//! Safe ownership wrapper for macOS's private CoreMedia 802.1AS clock SPI.
//!
//! This module is intentionally isolated from the portable PTP observer. The
//! symbols below are private Apple SPI, so they are resolved at runtime: a
//! missing or incompatible implementation becomes a normal error rather than
//! an application load failure. The ABI was verified against macOS 26.x.

use core_foundation::base::{kCFAllocatorDefault, CFRelease, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use if_addrs::get_if_addrs;
use libc::{c_char, c_void, dlclose, dlerror, dlopen, dlsym, RTLD_LOCAL, RTLD_NOW};
use std::error::Error;
use std::ffi::CStr;
use std::fmt;
use std::mem;
use std::net::{IpAddr, Ipv4Addr};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CORE_MEDIA_PATH: &[u8] = b"/System/Library/Frameworks/CoreMedia.framework/CoreMedia\0";
const CM_TIME_VALID: u32 = 1;
const CM_TIME_HAS_BEEN_ROUNDED: u32 = 1 << 1;
const CM_TIME_NUMERIC_FLAGS: u32 = CM_TIME_VALID | CM_TIME_HAS_BEEN_ROUNDED;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// CoreMedia's ABI-stable, by-value representation of `CMTime`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type ClockRef = *mut c_void;
type CreateFn = unsafe extern "C" fn(*const c_void, *mut ClockRef) -> i32;
type AddIPv4PortFn = unsafe extern "C" fn(ClockRef, *const c_void, u32, *mut u64, *mut u16) -> i32;
type RemoveIPv4PortFn = unsafe extern "C" fn(ClockRef, *const c_void, u32) -> i32;
type OverridePortReceiveMatchingFn = unsafe extern "C" fn(ClockRef, u16, u64, u16) -> i32;
type GetHostTimeFn = unsafe extern "C" fn(ClockRef, CMTime, *mut u64) -> CMTime;
type IsLockedFn = unsafe extern "C" fn(ClockRef) -> u8;
type GetHostTimeClockFn = unsafe extern "C" fn() -> *const c_void;
type ClockGetTimeFn = unsafe extern "C" fn(*const c_void) -> CMTime;

#[derive(Clone, Copy)]
struct Symbols {
    create: CreateFn,
    add_ipv4_port: AddIPv4PortFn,
    remove_ipv4_port: RemoveIPv4PortFn,
    override_port_receive_matching: OverridePortReceiveMatchingFn,
    get_host_time: GetHostTimeFn,
    is_locked: IsLockedFn,
    get_host_time_clock: GetHostTimeClockFn,
    clock_get_time: ClockGetTimeFn,
}

struct CoreMediaLibrary(NonNull<c_void>);

impl CoreMediaLibrary {
    fn open() -> Result<(Self, Symbols), MacPtpError> {
        // SAFETY: CORE_MEDIA_PATH is a static NUL-terminated path. `dlopen`
        // returns an owned handle. Arc ownership keeps it alive until every
        // clock/port user is gone, then Inner::drop releases the clock first.
        let handle = unsafe {
            dlopen(
                CORE_MEDIA_PATH.as_ptr().cast::<c_char>(),
                RTLD_NOW | RTLD_LOCAL,
            )
        };
        let library = Self(
            NonNull::new(handle).ok_or_else(|| MacPtpError::LoadLibrary {
                detail: last_dl_error(),
            })?,
        );

        // SAFETY: each symbol is checked for null and cast to its verified
        // macOS ABI. Keeping `library` alive keeps every function pointer live.
        let symbols = unsafe {
            Symbols {
                create: library.symbol(b"CM8021ASClockCreate\0")?,
                add_ipv4_port: library.symbol(b"CM8021ASClockAddIPv4PortAndGetIdentity\0")?,
                remove_ipv4_port: library.symbol(b"CM8021ASClockRemoveIPv4Port\0")?,
                override_port_receive_matching: library
                    .symbol(b"CM8021ASClockOverridePortReceiveMatching\0")?,
                get_host_time: library.symbol(b"CM8021ASClockGetHostTimeForClockTime\0")?,
                is_locked: library.symbol(b"CM8021ASClockIsLocked\0")?,
                get_host_time_clock: library.symbol(b"CMClockGetHostTimeClock\0")?,
                clock_get_time: library.symbol(b"CMClockGetTime\0")?,
            }
        };
        Ok((library, symbols))
    }

    unsafe fn symbol<T: Copy>(&self, name: &'static [u8]) -> Result<T, MacPtpError> {
        debug_assert_eq!(name.last(), Some(&0));
        // Clear a stale loader error before asking for this symbol.
        let _ = dlerror();
        let address = dlsym(self.0.as_ptr(), name.as_ptr().cast::<c_char>());
        if address.is_null() {
            let symbol = CStr::from_bytes_with_nul_unchecked(name)
                .to_string_lossy()
                .into_owned();
            return Err(MacPtpError::MissingSymbol {
                symbol,
                detail: last_dl_error(),
            });
        }
        debug_assert_eq!(mem::size_of::<T>(), mem::size_of::<*mut c_void>());
        Ok(mem::transmute_copy::<*mut c_void, T>(&address))
    }
}

impl Drop for CoreMediaLibrary {
    fn drop(&mut self) {
        // SAFETY: this is the unique handle returned by `dlopen`.
        unsafe { dlclose(self.0.as_ptr()) };
    }
}

/// One process-local CoreMedia IEEE 802.1AS clock.
#[derive(Clone)]
pub(crate) struct MacPtpClock {
    inner: Arc<MacPtpClockInner>,
}

struct MacPtpClockInner {
    symbols: Symbols,
    clock: NonNull<c_void>,
    instant_epoch: Instant,
    host_epoch_ns: i128,
    // Declared last so the function pointers stay loaded through Inner::drop
    // and through the destruction of every preceding field.
    _library: CoreMediaLibrary,
}

// SAFETY: CoreMedia's 802.1AS clock is a Core Foundation object whose SPI
// serializes all mutable clock/port state internally. AudioHub never exposes
// the pointer, and every call uses an immutable Rust borrow. The Arc-owned
// dlopen handle remains alive until the last clock or port is dropped.
unsafe impl Send for MacPtpClockInner {}
unsafe impl Sync for MacPtpClockInner {}

impl MacPtpClock {
    pub(crate) fn new() -> Result<Self, MacPtpError> {
        let (library, symbols) = CoreMediaLibrary::open()?;
        let mut clock = std::ptr::null_mut();
        // SAFETY: output storage is valid and kCFAllocatorDefault is the
        // allocator accepted by the verified Create ABI.
        let status = unsafe { (symbols.create)(kCFAllocatorDefault.cast(), &mut clock) };
        if status != 0 {
            return Err(MacPtpError::CoreMedia {
                operation: "CM8021ASClockCreate",
                status,
            });
        }
        let clock = NonNull::new(clock).ok_or(MacPtpError::NullClock)?;

        // A close CoreMedia-host-time/Instant sample lets us convert the
        // result into Rust's opaque monotonic `Instant` domain.
        let before = Instant::now();
        // SAFETY: these public CoreMedia calls return a process-wide singleton
        // clock and query it without transferring ownership.
        let host_clock = unsafe { (symbols.get_host_time_clock)() };
        if host_clock.is_null() {
            // SAFETY: release the create-rule object before returning.
            unsafe { CFRelease(clock.as_ptr().cast::<c_void>() as CFTypeRef) };
            return Err(MacPtpError::NullHostClock);
        }
        let host_time = unsafe { (symbols.clock_get_time)(host_clock) };
        let host_epoch_ns = match validated_time_ns(host_time) {
            Ok(value) => value,
            Err(error) => {
                // SAFETY: release the create-rule object before returning.
                unsafe { CFRelease(clock.as_ptr().cast::<c_void>() as CFTypeRef) };
                return Err(error);
            }
        };
        let after = Instant::now();
        let instant_epoch = before + after.duration_since(before) / 2;

        Ok(Self {
            inner: Arc::new(MacPtpClockInner {
                symbols,
                clock,
                instant_epoch,
                host_epoch_ns,
                _library: library,
            }),
        })
    }

    /// Register the session's remote IPv4 peer on the interface owning
    /// `local_ip`. The peer address, not the local address, is passed to the
    /// SPI in network-byte numeric order.
    pub(crate) fn add_ipv4_port(
        &self,
        local_ip: Ipv4Addr,
        destination_peer: Ipv4Addr,
    ) -> Result<MacPtpPort, MacPtpError> {
        let interface_name = interface_name_for(local_ip)?;
        let interface = CFString::new(&interface_name);
        let destination_numeric = ipv4_network_numeric(destination_peer);
        let mut clock_identity = 0u64;
        let mut port_number = 0u16;
        // SAFETY: `self.clock` and `interface` remain valid for the call;
        // both output pointers reference initialized, suitably aligned values.
        let status = unsafe {
            (self.inner.symbols.add_ipv4_port)(
                self.inner.clock.as_ptr(),
                interface.as_concrete_TypeRef().cast(),
                destination_numeric,
                &mut clock_identity,
                &mut port_number,
            )
        };
        if status != 0 {
            return Err(MacPtpError::CoreMedia {
                operation: "CM8021ASClockAddIPv4PortAndGetIdentity",
                status,
            });
        }

        Ok(MacPtpPort {
            inner: Arc::clone(&self.inner),
            interface,
            destination_peer,
            destination_numeric,
            clock_identity,
            port_number,
        })
    }
}

impl MacPtpClockInner {
    fn is_locked(&self) -> bool {
        // SAFETY: the owned clock is alive; CoreMedia documents this query as
        // read-only and internally serializes the 802.1AS clock state.
        unsafe { (self.symbols.is_locked)(self.clock.as_ptr()) != 0 }
    }

    fn map_remote_time(
        &self,
        remote_time_ns: u64,
        expected_grandmaster: u64,
    ) -> Result<Instant, MacPtpError> {
        if !self.is_locked() {
            return Err(MacPtpError::NotLocked);
        }

        let remote_value = i64::try_from(remote_time_ns)
            .map_err(|_| MacPtpError::RemoteTimeOutOfRange(remote_time_ns))?;
        let remote_time = CMTime {
            value: remote_value,
            timescale: 1_000_000_000,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        let mut observed_grandmaster = 0u64;
        // SAFETY: the CMTime value and GM output pointer match the verified
        // by-value ABI. The returned CMTime is validated before use.
        let host_time = unsafe {
            (self.symbols.get_host_time)(
                self.clock.as_ptr(),
                remote_time,
                &mut observed_grandmaster,
            )
        };
        if observed_grandmaster != expected_grandmaster {
            return Err(MacPtpError::GrandmasterMismatch {
                expected: expected_grandmaster,
                observed: observed_grandmaster,
            });
        }
        let host_ns = validated_time_ns(host_time)?;
        instant_from_delta(self.instant_epoch, host_ns - self.host_epoch_ns)
    }
}

impl Drop for MacPtpClockInner {
    fn drop(&mut self) {
        // SAFETY: Create follows Core Foundation's create rule. Every port
        // owns this Inner through Arc, so this runs only after all registered
        // ports have called Remove in their own Drop implementations.
        unsafe { CFRelease(self.clock.as_ptr().cast::<c_void>() as CFTypeRef) };
    }
}

/// RAII registration of one remote IPv4 PTP peer.
pub(crate) struct MacPtpPort {
    inner: Arc<MacPtpClockInner>,
    interface: CFString,
    destination_peer: Ipv4Addr,
    destination_numeric: u32,
    clock_identity: u64,
    port_number: u16,
}

// SAFETY: `CFString` is immutable, and Remove uses the same thread-safe clock
// SPI as Add/query. The Arc keeps the Send + Sync inner clock and SPI loaded.
unsafe impl Send for MacPtpPort {}
unsafe impl Sync for MacPtpPort {}

impl MacPtpPort {
    /// Local clock identity reported to the AirPlay sender in SETUP.
    pub(crate) fn clock_identity(&self) -> u64 {
        self.clock_identity
    }

    /// Local PTP port number reported to the AirPlay sender in SETUP.
    pub(crate) fn port_number(&self) -> u16 {
        self.port_number
    }

    /// Restrict this local port to the authenticated sender's IEEE 1588
    /// sourcePortIdentity. AirPlay exchanges these logical identities in
    /// `timingPeerInfo`; neither port value is a UDP socket port.
    pub(crate) fn override_receive_matching(
        &self,
        remote_clock_identity: u64,
        remote_port: u16,
    ) -> Result<(), MacPtpError> {
        // SAFETY: this port belongs to the live clock, and all scalar values
        // have already been range-checked while parsing the peer plist. The
        // ABI matches APSTimeSyncNetworkClock's macOS 26.x wrapper.
        let status = unsafe {
            (self.inner.symbols.override_port_receive_matching)(
                self.inner.clock.as_ptr(),
                self.port_number,
                remote_clock_identity,
                remote_port,
            )
        };
        if status != 0 {
            return Err(MacPtpError::CoreMedia {
                operation: "CM8021ASClockOverridePortReceiveMatching",
                status,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    pub(crate) fn map_remote_time(
        &self,
        remote_time_ns: u64,
        expected_grandmaster: u64,
    ) -> Result<Instant, MacPtpError> {
        self.inner
            .map_remote_time(remote_time_ns, expected_grandmaster)
    }
}

impl Drop for MacPtpPort {
    fn drop(&mut self) {
        // SAFETY: this is exactly the tuple successfully registered by Add;
        // `interface` and the Arc-owned clock outlive this removal call.
        let status = unsafe {
            (self.inner.symbols.remove_ipv4_port)(
                self.inner.clock.as_ptr(),
                self.interface.as_concrete_TypeRef().cast(),
                self.destination_numeric,
            )
        };
        if status != 0 {
            log::warn!(
                "CM8021ASClockRemoveIPv4Port failed for {} with status {}",
                self.destination_peer,
                status
            );
        }
    }
}

#[derive(Debug)]
pub(crate) enum MacPtpError {
    LoadLibrary {
        detail: String,
    },
    MissingSymbol {
        symbol: String,
        detail: String,
    },
    CoreMedia {
        operation: &'static str,
        status: i32,
    },
    NullClock,
    NullHostClock,
    InterfaceEnumeration(std::io::Error),
    LocalInterfaceNotFound(Ipv4Addr),
    NotLocked,
    RemoteTimeOutOfRange(u64),
    InvalidTimeFlags(u32),
    InvalidTimescale(i32),
    UnsupportedEpoch(i64),
    TimeOverflow,
    GrandmasterMismatch {
        expected: u64,
        observed: u64,
    },
}

impl fmt::Display for MacPtpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadLibrary { detail } => write!(formatter, "cannot load CoreMedia: {detail}"),
            Self::MissingSymbol { symbol, detail } => {
                write!(
                    formatter,
                    "CoreMedia symbol {symbol} is unavailable: {detail}"
                )
            }
            Self::CoreMedia { operation, status } => {
                write!(formatter, "{operation} failed with OSStatus {status}")
            }
            Self::NullClock => write!(formatter, "CoreMedia created a null 802.1AS clock"),
            Self::NullHostClock => write!(formatter, "CoreMedia returned a null host clock"),
            Self::InterfaceEnumeration(error) => {
                write!(formatter, "cannot enumerate network interfaces: {error}")
            }
            Self::LocalInterfaceNotFound(address) => {
                write!(formatter, "no IPv4 interface owns local address {address}")
            }
            Self::NotLocked => write!(formatter, "the CoreMedia 802.1AS clock is not locked"),
            Self::RemoteTimeOutOfRange(value) => {
                write!(formatter, "remote PTP time {value}ns does not fit CMTime")
            }
            Self::InvalidTimeFlags(flags) => {
                write!(
                    formatter,
                    "CoreMedia returned invalid CMTime flags 0x{flags:x}"
                )
            }
            Self::InvalidTimescale(scale) => {
                write!(
                    formatter,
                    "CoreMedia returned invalid CMTime timescale {scale}"
                )
            }
            Self::UnsupportedEpoch(epoch) => {
                write!(
                    formatter,
                    "CoreMedia returned unsupported CMTime epoch {epoch}"
                )
            }
            Self::TimeOverflow => write!(formatter, "PTP/host time conversion overflowed"),
            Self::GrandmasterMismatch { expected, observed } => write!(
                formatter,
                "PTP grandmaster changed: expected {expected:016x}, observed {observed:016x}"
            ),
        }
    }
}

impl Error for MacPtpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InterfaceEnumeration(error) => Some(error),
            _ => None,
        }
    }
}

fn interface_name_for(local_ip: Ipv4Addr) -> Result<String, MacPtpError> {
    get_if_addrs()
        .map_err(MacPtpError::InterfaceEnumeration)?
        .into_iter()
        .find(|interface| interface.ip() == IpAddr::V4(local_ip))
        .map(|interface| interface.name)
        .ok_or(MacPtpError::LocalInterfaceNotFound(local_ip))
}

fn ipv4_network_numeric(address: Ipv4Addr) -> u32 {
    u32::from_be_bytes(address.octets())
}

fn validated_time_ns(time: CMTime) -> Result<i128, MacPtpError> {
    // HasBeenRounded is compatible with a finite numeric value. Any other
    // flag includes indefinite/infinity semantics or an unknown future shape
    // and therefore cannot be scheduled as an Instant.
    if time.flags & CM_TIME_VALID == 0 || time.flags & !CM_TIME_NUMERIC_FLAGS != 0 {
        return Err(MacPtpError::InvalidTimeFlags(time.flags));
    }
    if time.timescale <= 0 {
        return Err(MacPtpError::InvalidTimescale(time.timescale));
    }
    if time.epoch != 0 {
        return Err(MacPtpError::UnsupportedEpoch(time.epoch));
    }
    i128::from(time.value)
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| value.checked_div(i128::from(time.timescale)))
        .ok_or(MacPtpError::TimeOverflow)
}

fn instant_from_delta(epoch: Instant, delta_ns: i128) -> Result<Instant, MacPtpError> {
    let magnitude =
        u64::try_from(delta_ns.unsigned_abs()).map_err(|_| MacPtpError::TimeOverflow)?;
    let duration = Duration::from_nanos(magnitude);
    if delta_ns >= 0 {
        epoch.checked_add(duration).ok_or(MacPtpError::TimeOverflow)
    } else {
        epoch.checked_sub(duration).ok_or(MacPtpError::TimeOverflow)
    }
}

fn last_dl_error() -> String {
    // SAFETY: `dlerror` returns either null or a process-owned NUL-terminated
    // error string valid until the next dynamic loader call on this thread.
    unsafe {
        let error = dlerror();
        if error.is_null() {
            "unknown dynamic loader error".to_owned()
        } else {
            CStr::from_ptr(error).to_string_lossy().into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_destination_in_network_byte_numeric_order() {
        assert_eq!(
            ipv4_network_numeric(Ipv4Addr::new(10, 130, 32, 30)),
            0x0a82_201e
        );
    }

    #[test]
    fn validates_and_scales_core_media_time() {
        let time = CMTime {
            value: 15,
            timescale: 4,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        assert_eq!(validated_time_ns(time).unwrap(), 3_750_000_000);
        assert_eq!(
            validated_time_ns(CMTime {
                flags: CM_TIME_VALID | CM_TIME_HAS_BEEN_ROUNDED,
                ..time
            })
            .unwrap(),
            3_750_000_000
        );
    }

    #[test]
    fn rejects_nonzero_epoch_and_nonvalid_flags() {
        let base = CMTime {
            value: 1,
            timescale: 1,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        assert!(matches!(
            validated_time_ns(CMTime { epoch: 1, ..base }),
            Err(MacPtpError::UnsupportedEpoch(1))
        ));
        assert!(matches!(
            validated_time_ns(CMTime { flags: 0, ..base }),
            Err(MacPtpError::InvalidTimeFlags(0))
        ));
        assert!(matches!(
            validated_time_ns(CMTime {
                flags: CM_TIME_VALID | (1 << 2),
                ..base
            }),
            Err(MacPtpError::InvalidTimeFlags(5))
        ));
    }

    #[test]
    fn applies_signed_instant_delta() {
        let epoch = Instant::now();
        assert_eq!(
            instant_from_delta(epoch, 42).unwrap(),
            epoch + Duration::from_nanos(42)
        );
        assert_eq!(
            instant_from_delta(epoch, -42).unwrap(),
            epoch - Duration::from_nanos(42)
        );
    }

    #[test]
    fn can_load_private_spi_and_create_clock() {
        let _clock = MacPtpClock::new().expect("macOS 26 CoreMedia SPI should be available");
    }

    #[test]
    fn owned_handles_are_send_and_sync() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<MacPtpClock>();
        require_send_sync::<MacPtpPort>();
    }

    #[test]
    #[ignore = "requires a macOS AirPlay sender plus explicit IPv4 environment variables"]
    fn registers_and_overrides_a_live_ipv4_port() {
        let local_ip = std::env::var("AUDIOHUB_PTP_LOCAL_IPV4")
            .expect("AUDIOHUB_PTP_LOCAL_IPV4 is required")
            .parse()
            .expect("local address must be IPv4");
        let peer = std::env::var("AUDIOHUB_PTP_PEER_IPV4")
            .expect("AUDIOHUB_PTP_PEER_IPV4 is required")
            .parse()
            .expect("peer address must be IPv4");
        let remote_clock = std::env::var("AUDIOHUB_PTP_REMOTE_CLOCK_ID")
            .map(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap())
            .unwrap_or(0x5273_7ec2_e765_0008);
        let remote_port = std::env::var("AUDIOHUB_PTP_REMOTE_PORT")
            .map(|value| value.parse().unwrap())
            .unwrap_or(0x800c);
        let clock = MacPtpClock::new().expect("CoreMedia clock creation should succeed");
        let port = clock
            .add_ipv4_port(local_ip, peer)
            .expect("CoreMedia peer registration should succeed");
        port.override_receive_matching(remote_clock, remote_port)
            .expect("CoreMedia receive matching override should succeed");
    }

    #[test]
    #[ignore = "requires a macOS AirPlay sender plus explicit IPv4 environment variables"]
    fn registers_live_ipv4_peer() {
        let local_ip = std::env::var("AUDIOHUB_PTP_LOCAL_IPV4")
            .expect("AUDIOHUB_PTP_LOCAL_IPV4 is required")
            .parse()
            .expect("local address must be IPv4");
        let peer = std::env::var("AUDIOHUB_PTP_PEER_IPV4")
            .expect("AUDIOHUB_PTP_PEER_IPV4 is required")
            .parse()
            .expect("peer address must be IPv4");
        let clock = MacPtpClock::new().expect("CoreMedia clock creation should succeed");
        let port = clock
            .add_ipv4_port(local_ip, peer)
            .expect("CoreMedia peer registration should succeed");
        assert_ne!(port.clock_identity(), 0);
        assert_ne!(port.port_number(), 0);
        for _ in 0..20 {
            if port.is_locked() {
                let lower_bound = Instant::now() - Duration::from_millis(25);
                // This live topology uses the local Mac as the PTP
                // grandmaster, so its current host time is also a valid
                // current remote/network timestamp for an end-to-end map.
                let host_clock = unsafe { (port.inner.symbols.get_host_time_clock)() };
                let host_time = unsafe { (port.inner.symbols.clock_get_time)(host_clock) };
                let remote_time_ns = u64::try_from(validated_time_ns(host_time).unwrap()).unwrap();
                let mapped = port
                    .map_remote_time(remote_time_ns, port.clock_identity())
                    .expect("locked same-GM time conversion should succeed");
                let upper_bound = Instant::now() + Duration::from_millis(25);
                assert!(mapped >= lower_bound && mapped <= upper_bound);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("CoreMedia clock did not lock to the active peer within two seconds");
    }
}
