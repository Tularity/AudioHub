//! halbridge_win — the Windows half of the HAL bridge's control plane.
//!
//! The authority for everything in this file is
//! `drivers/windows-vad/Source/Inc/AudioHubIoctl.h`. This is a hand-maintained
//! mirror of it, and `test/tests/halwire_win.rs` pins the two against a third,
//! independently transcribed copy so that editing one side alone fails
//! `cargo test` rather than failing on a machine that has to be recovered from
//! a Hyper-V checkpoint.
//!
//! # Why this file is split the way it is
//!
//! [`wire`] compiles on EVERY platform. Encoding, decoding, the IOCTL
//! arithmetic, the peer-key whitelist and the UTF-16 clamp are pure functions
//! over byte buffers, and they are where the bugs live: a field at the wrong
//! offset, a surrogate pair cut in half, an IOCTL code off by one function
//! number. Gating them behind `cfg(windows)` would mean they are never
//! exercised on the machine this project is developed on, which is the same as
//! not testing them.
//!
//! [`rings`] compiles on every platform too, for the same reason and with more
//! force. It is the DATA PLANE: pointer arithmetic over an address, a pair of
//! free-running SPSC indices and two memcpys. Nothing in it is a Windows API
//! call — only the code that OBTAINS the address is, and that lives in
//! [`session`]. Gating the arithmetic behind `cfg(windows)` would mean the one
//! piece of this file that can silently corrupt audio, or memcpy past the end
//! of a kernel mapping, is never executed on the machine it is written on.
//!
//! [`transport`] is `cfg(windows)` and contains nothing but FFI plumbing.
//!
//! # No `windows`/`windows-sys` crate, on purpose
//!
//! The windows-gnu toolchain here has no `as.exe`, so anything that pulls in a
//! raw-dylib-based `windows-sys` (0.59+) cannot link at all — that has already
//! cost this project a build, via `dirs`, `gethostname` and `mio 1.2`. Every
//! function needed is an ordinary `kernel32` import that mingw resolves
//! natively, so they are declared by hand and `Cargo.toml` gains no dependency.
//!
//! # The one Windows-only hazard that has no macOS counterpart
//!
//! The control handle MUST be opened `FILE_FLAG_OVERLAPPED`. The I/O manager
//! serialises every request on a SYNCHRONOUS file object, and
//! `IOCTL_AUDIOHUB_CONTROL_PEND` is deliberately long-pending — so on a
//! synchronous handle it would park every subsequent `BIND_SET` behind itself
//! forever. The symptom is a deadlock, not an error code.

#![allow(dead_code)] // the transport half is cfg'd out on non-Windows hosts

// ---------------------------------------------------------------- wire

/// The frozen contract, as pure data. Compiled everywhere so it is testable
/// everywhere.
pub mod wire {
    /// `AudioHubIoctl.h:AUDIOHUB_WIN_PROTOCOL_VERSION`.
    ///
    /// An INDEPENDENT namespace from the macOS bridge's version 2. The two
    /// drivers share no message, no transport and no struct; unifying the
    /// numbers would only create the illusion that a version bump on one side
    /// means anything to the other.
    ///
    /// v2 grew `stage` / `nt_status` / `published` on the bind reply and
    /// `published` on the slot info. The bump is deliberate rather than a
    /// compatible append: v1's leading 16 bytes are byte-identical, so a v1
    /// daemon would keep believing that `status == OK` means both endpoints
    /// exist — the belief that produced `state=bound` alongside an empty
    /// `output_devices`.
    ///
    /// v3 was PER-PEER DEVICE NAMES, first attempt: a pin-name GUID derived
    /// from the peer fingerprint, with the composed label written under
    /// MediaCategories. The reply's trailing word became `flags`.
    ///
    /// v4 is the same feature through a mechanism that works in BOTH
    /// directions. v3's route names the microphone and cannot name the
    /// speaker: the endpoint builder hardcodes the name of any endpoint whose
    /// bridge pin carries `KSNODETYPE_SPEAKER`. v4 delivers the name as
    /// `PKEY_Device_DeviceDesc` under the interface's `EP\0` key instead.
    ///
    /// A bump for the same reason every time: the bad pairing is SILENT. A v3
    /// driver under a v4 daemon writes each speaker's name into a registry key
    /// nothing reads, finds no error, and answers OK with the fallback bit
    /// clear — so the daemon is told every device carries its peer's name
    /// while every speaker in the list reads the same generic word. Nothing in
    /// a v3 reply can say otherwise, and the version test is an equality test,
    /// so the mismatch refuses to bind instead of producing a device list the
    /// user cannot tell apart.
    ///
    /// v5 is THE DATA PLANE AND THE VOLUME NODE: [`IOCTL_MAP_RINGS`] and
    /// [`IOCTL_NOTIFY`], plus a `caps` word that finally answers
    /// [`CAP_DATAPLANE`] | [`CAP_VOLUME`].
    ///
    /// And the silent bad pairing here is the worst of the four. A v4 driver
    /// has NO volume node, so the audio engine inserts a software volume APO
    /// ahead of it and every sample reaching the ring is ALREADY attenuated by
    /// the user's slider. A v5 daemon then reads that same slider off the
    /// control plane and asks the peer to attenuate its REAL device by the
    /// same factor. Nothing errors and no device list looks wrong; the audio
    /// is simply quieter than it should be by the SQUARE of the setting, and
    /// the only place that shows is a level meter nobody is looking at.
    pub const PROTOCOL_VERSION: u32 = 5;

    /// `AudioHubIoctl.h:AUDIOHUB_WIN_MAX_SLOTS`, and equal to
    /// `halbridge::HAL_MAX_SLOTS`. The driver's `PcAddAdapterDevice` budget is
    /// derived from it and cannot be raised after `AddDevice`, so this is a
    /// build-time decision on both sides.
    pub const MAX_SLOTS: usize = 16;

    pub const PEERKEY_CHARS: usize = 16;
    pub const PEERKEY_BUF: usize = 40;
    /// UTF-16 code units INCLUDING the terminator.
    pub const DISPLAY_CHARS: usize = 128;

    /// `\\.\AudioHubVadCtl`.
    pub const CTL_PATH: &str = r"\\.\AudioHubVadCtl";

    // -- ring geometry ------------------------------------------------------
    //
    // Mirrors `drivers/windows-vad/Source/Inc/AudioHubRing.h`, which is itself
    // a literal port of the macOS bridge's layout. Every constant below is
    // therefore ALSO a `halbridge::HAL_*` constant, and the `const _` blocks
    // are what make a one-sided edit a BUILD failure rather than an audio
    // failure on a machine that has to be recovered from a checkpoint.
    //
    // These are the values the daemon will ACCEPT. The driver reports its own
    // in AH_MAP_REPLY and `rings::WinRings::attach` compares the two for
    // equality — a disagreement is a refusal, never an adaptation, because the
    // only way to "adapt" to a wrong capacity is to memcpy past the mapping.

    /// Samples start here, so the 40-byte header never shares a cache line
    /// with frame 0. Hard-coded on both sides rather than derived from a
    /// `sizeof`, which is why it needs pinning at all.
    pub const RING_DATA_OFFSET: usize = 64;
    pub const RING_SAMPLE_RATE: u32 = 48_000;
    /// 500 ms at 48 kHz.
    pub const RING_FRAMES: u32 = 24_000;
    pub const SPK_CHANNELS: u32 = 2;
    pub const MIC_CHANNELS: u32 = 1;
    /// `AUDIOHUB_SPK_BYTES` — the 16K-page-aligned mapped length of an OUT ring.
    pub const SPK_BYTES: usize = 196_608;
    /// `AUDIOHUB_MIC_BYTES`.
    pub const MIC_BYTES: usize = 98_304;
    /// `'AHR1'`. Written by the driver at ring creation, checked ONCE by the
    /// daemon at attach.
    pub const RING_MAGIC: u32 = 0x4148_5231;
    pub const RING_VERSION: u32 = 1;

    /// `AH_MAP_REPLY::va` is a fixed array of this many entries; `ring_count`
    /// says how many are meaningful. `AUDIOHUB_RING_INDEX(slot, dir)` is
    /// `slot * 2 + dir`, the SAME encoding the macOS bridge uses.
    pub const RING_SLOTS_MAX: usize = 2 * MAX_SLOTS;

    /// `AUDIOHUB_DIR_OUT` — the driver WRITES, this daemon READS (the virtual
    /// speaker).
    pub const DIR_OUT: usize = 0;
    /// `AUDIOHUB_DIR_IN` — this daemon WRITES, the driver READS (the virtual
    /// microphone).
    pub const DIR_IN: usize = 1;

    // One decision, three files. Break any of these and the build stops here
    // rather than in a DPC.
    const _: () = assert!(RING_DATA_OFFSET == crate::halbridge::HAL_RING_DATA_OFFSET);
    const _: () = assert!(RING_SAMPLE_RATE == crate::halbridge::HAL_SAMPLE_RATE);
    const _: () = assert!(RING_FRAMES == crate::halbridge::HAL_RING_FRAMES);
    const _: () = assert!(SPK_CHANNELS == crate::halbridge::HAL_SPK_CHANNELS);
    const _: () = assert!(MIC_CHANNELS == crate::halbridge::HAL_MIC_CHANNELS);
    const _: () = assert!(SPK_BYTES == crate::halbridge::HAL_SPK_BYTES);
    const _: () = assert!(MIC_BYTES == crate::halbridge::HAL_MIC_BYTES);
    const _: () = assert!(RING_MAGIC == crate::halbridge::HAL_RING_MAGIC);
    const _: () = assert!(RING_VERSION == crate::halbridge::HAL_RING_VERSION);
    // The bound that actually matters: the samples have to fit in the mapping.
    const _: () = assert!(
        SPK_BYTES >= RING_DATA_OFFSET + (RING_FRAMES as usize) * (SPK_CHANNELS as usize) * 4
    );
    const _: () = assert!(
        MIC_BYTES >= RING_DATA_OFFSET + (RING_FRAMES as usize) * (MIC_CHANNELS as usize) * 4
    );

    // -- IOCTL arithmetic ---------------------------------------------------

    const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
    const METHOD_BUFFERED: u32 = 0;
    const FILE_READ_DATA: u32 = 0x0001;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const AH_ACCESS: u32 = FILE_READ_DATA | FILE_WRITE_DATA;

    /// The `CTL_CODE` macro, reproduced. There are no Windows headers in this
    /// build, so this arithmetic is the only thing standing between the daemon
    /// and a driver that answers `STATUS_INVALID_DEVICE_REQUEST` to everything.
    pub const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
        (device << 16) | (access << 14) | (function << 2) | method
    }

    const fn ah_ioctl(function: u32) -> u32 {
        ctl_code(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, AH_ACCESS)
    }

    pub const IOCTL_HELLO: u32 = ah_ioctl(0x800);
    pub const IOCTL_BIND_SET: u32 = ah_ioctl(0x801);
    pub const IOCTL_BIND_CLEAR: u32 = ah_ioctl(0x802);
    pub const IOCTL_QUERY_SLOTS: u32 = ah_ioctl(0x803);
    pub const IOCTL_CONTROL_PEND: u32 = ah_ioctl(0x804);
    /// The data plane's ONE system call: maps every ring the driver owns into
    /// this process and hands back the user virtual addresses. After it, audio
    /// moves with two memcpys and two 64-bit stores per tick — no IOCTL, no
    /// copy by the I/O manager, no system call at all.
    pub const IOCTL_MAP_RINGS: u32 = ah_ioctl(0x805);
    /// The return leg of volume sync: the far peer's real device moved, so the
    /// virtual endpoint's node must follow. Touches no sample — the rings carry
    /// full scale and the far side does the attenuating.
    pub const IOCTL_NOTIFY: u32 = ah_ioctl(0x806);

    // The two new codes as LITERALS, transcribed from the C_ASSERTs at the
    // bottom of AudioHubIoctl.h. `ah_ioctl` is a macro on both sides, so this
    // is the only place the arithmetic gets checked against a fixed number
    // rather than against itself.
    const _: () = assert!(IOCTL_MAP_RINGS == 0x0022_E014);
    const _: () = assert!(IOCTL_NOTIFY == 0x0022_E018);

    // -- status codes -------------------------------------------------------

    pub const STATUS_OK: u32 = 0;
    pub const STATUS_BAD_VERSION: u32 = 1;
    pub const STATUS_BAD_ARGUMENT: u32 = 2;
    pub const STATUS_STALE_SESSION: u32 = 3;
    pub const STATUS_NO_ADAPTER: u32 = 4;
    pub const STATUS_CAPACITY: u32 = 5;
    pub const STATUS_INTERNAL: u32 = 6;
    pub const STATUS_NOT_BOUND: u32 = 7;
    /// A bind failed AND the rollback that should have made it "nothing at
    /// all" also failed, so one half of a device pair is still published.
    ///
    /// The ONLY status under which `published` may be neither 0 nor
    /// [`PUB_BOTH`]. It exists so that "the driver said OK but only the
    /// microphone appeared" stops being expressible.
    pub const STATUS_PARTIAL: u32 = 8;

    pub fn status_label(v: u32) -> &'static str {
        match v {
            STATUS_OK => "ok",
            STATUS_BAD_VERSION => "protocol version mismatch",
            STATUS_BAD_ARGUMENT => "bad argument",
            STATUS_STALE_SESSION => "stale session",
            STATUS_NO_ADAPTER => "the devnode has not started yet",
            STATUS_CAPACITY => "slot out of range",
            STATUS_INTERNAL => "driver internal error",
            STATUS_NOT_BOUND => "slot not bound",
            STATUS_PARTIAL => "one half of the device pair is still published",
            _ => "unknown status",
        }
    }

    // -- slot states --------------------------------------------------------

    pub const SLOT_FREE: u32 = 0;
    pub const SLOT_BOUND: u32 = 1;
    pub const SLOT_DELISTED: u32 = 2;

    // -- bind ops and flags -------------------------------------------------

    pub const BIND_CLEAR: u32 = 0;
    pub const BIND_SET: u32 = 1;
    pub const BINDFLAG_ONLINE: u32 = 0x1;

    // -- fault injection ----------------------------------------------------
    //
    // NEVER set by the daemon. `audiohub probe winvad` and the regression
    // harness set them; the driver logs every use at D_ERROR.
    //
    // They exist because "the driver must report a half-failed install
    // honestly" cannot be tested without a way to MAKE one half fail — and a
    // test that can only walk the happy path is how the original defect
    // survived a full acceptance run.

    /// SET: fail the speaker half.
    pub const BINDFLAG_FAIL_RENDER: u32 = 0x100;
    /// SET: fail the microphone half (after the speaker half succeeded, so
    /// this is the rollback test).
    pub const BINDFLAG_FAIL_CAPTURE: u32 = 0x200;
    /// SET: leave the partial install in place so `published` can be observed.
    pub const BINDFLAG_SKIP_ROLLBACK: u32 = 0x400;
    /// CLEAR: unregister the physical connection through the TOPOLOGY port
    /// even when it was registered from the WAVE port — i.e. reproduce the
    /// M6-2 speaker-loss defect on demand.
    pub const BINDFLAG_LEGACY_UNBIND: u32 = 0x800;
    /// SET: fail the per-peer pin-name write, so the fallback path and
    /// [`BINDREPLY_FLAG_NAME_FALLBACK`] can be observed without having to
    /// break the registry by hand.
    pub const BINDFLAG_FAIL_ENDPOINT_NAME: u32 = 0x1000;

    pub const BINDFLAG_DEBUG_MASK: u32 = BINDFLAG_FAIL_RENDER
        | BINDFLAG_FAIL_CAPTURE
        | BINDFLAG_SKIP_ROLLBACK
        | BINDFLAG_LEGACY_UNBIND
        | BINDFLAG_FAIL_ENDPOINT_NAME;

    // -- bind reply flags ---------------------------------------------------

    /// Set alongside a SUCCESSFUL bind when the peer's own name could not be
    /// made to appear, so the endpoints carry the INF's generic direction
    /// words instead.
    ///
    /// A warning on an OK reply, not a failure: a device with a generic name
    /// is enormously better than no device. Not silent either — with two peers
    /// paired it means two identically named speakers, and the user needs
    /// somewhere that explains why.
    pub const BINDREPLY_FLAG_NAME_FALLBACK: u32 = 0x1;

    // -- failure stages -----------------------------------------------------

    pub const STAGE_NONE: u32 = 0;
    pub const STAGE_REFSTRINGS: u32 = 1;
    pub const STAGE_INSTALL_RENDER: u32 = 2;
    pub const STAGE_INSTALL_CAPTURE: u32 = 3;
    pub const STAGE_VERIFY: u32 = 4;
    pub const STAGE_ROLLBACK: u32 = 5;
    pub const STAGE_DISCONNECT: u32 = 6;
    pub const STAGE_UNREGISTER: u32 = 7;
    pub const STAGE_ENDPOINT_NAME: u32 = 8;

    pub fn stage_label(v: u32) -> &'static str {
        match v {
            STAGE_NONE => "none",
            STAGE_REFSTRINGS => "building the interface reference strings",
            STAGE_INSTALL_RENDER => "installing the speaker filters",
            STAGE_INSTALL_CAPTURE => "installing the microphone filters",
            STAGE_VERIFY => "verifying the installed filters",
            STAGE_ROLLBACK => "rolling a failed install back",
            STAGE_DISCONNECT => "unregistering the physical connection",
            STAGE_UNREGISTER => "unregistering the subdevice",
            STAGE_ENDPOINT_NAME => "applying the per-peer device name",
            _ => "unknown stage",
        }
    }

    // -- published mask -----------------------------------------------------

    pub const PUB_RENDER: u32 = 0x1;
    pub const PUB_CAPTURE: u32 = 0x2;
    pub const PUB_BOTH: u32 = PUB_RENDER | PUB_CAPTURE;

    /// "render+capture" / "render only" / "capture only" / "nothing".
    pub fn published_label(v: u32) -> &'static str {
        match v & PUB_BOTH {
            0 => "nothing",
            PUB_RENDER => "the speaker only",
            PUB_CAPTURE => "the microphone only",
            _ => "both",
        }
    }

    // -- caller-identity levels --------------------------------------------

    pub const CLIENT_CHECK_NONE: u32 = 0;
    pub const CLIENT_CHECK_ACL_ONLY: u32 = 1;
    pub const CLIENT_CHECK_IMAGEPATH: u32 = 2;
    pub const CLIENT_CHECK_SIGNATURE: u32 = 3;

    // -- struct sizes -------------------------------------------------------
    //
    // Transcribed from the C_ASSERTs in AudioHubIoctl.h. Nothing here may be
    // "corrected" to make a test pass: if the header changed, re-derive from
    // the header and the failure is then a deliberate contract change.

    pub const HELLO_REQUEST_BYTES: usize = 16;
    pub const HELLO_REPLY_BYTES: usize = 40;
    pub const BIND_REQUEST_BYTES: usize = 320;
    pub const BIND_REPLY_BYTES: usize = 32;
    pub const SLOT_INFO_BYTES: usize = 52;
    pub const QUERY_SLOTS_REPLY_BYTES: usize = 16 + SLOT_INFO_BYTES * MAX_SLOTS;
    pub const CONTROL_EVENT_BYTES: usize = 24;
    pub const MAP_REQUEST_BYTES: usize = 24;
    /// 40 bytes of geometry, then 32 `u64` addresses.
    pub const MAP_REPLY_BYTES: usize = 40 + 8 * RING_SLOTS_MAX;
    pub const NOTIFY_REQUEST_BYTES: usize = 24;
    pub const NOTIFY_REPLY_BYTES: usize = 8;

    const _: () = assert!(QUERY_SLOTS_REPLY_BYTES == 848);
    const _: () = assert!(MAP_REPLY_BYTES == 296);

    // -- capabilities -------------------------------------------------------

    /// `AH_CAP_DATAPLANE`: [`IOCTL_MAP_RINGS`] works and the
    /// `sample_rate`/`out_channels`/`in_channels` fields of the hello reply
    /// mean something. Clear through M6-2.
    pub const CAP_DATAPLANE: u32 = 0x1;
    /// `AH_CAP_VOLUME`: every endpoint carries a `KSNODETYPE_VOLUME` node, so
    /// the audio engine does NOT insert a software volume APO ahead of the
    /// driver and the samples in the rings are FULL SCALE.
    ///
    /// A daemon that syncs volume to a peer while this bit is CLEAR is
    /// applying the user's setting twice — once in the APO on the way into the
    /// ring, once on the peer's real device — and the result is quieter than
    /// asked for by the SQUARE of the setting, which nothing reports.
    pub const CAP_VOLUME: u32 = 0x2;

    // -- helpers ------------------------------------------------------------

    fn put_u32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u64(buf: &mut [u8], at: usize, v: u64) {
        buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn get_u32(buf: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
    }

    fn get_u64(buf: &[u8], at: usize) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[at..at + 8]);
        u64::from_le_bytes(b)
    }

    /// Exactly 16 lowercase hex digits — the same whitelist the driver applies
    /// in `AhIsValidPeerKey`.
    ///
    /// This is checked on BOTH sides deliberately. The peer key becomes a
    /// device-interface reference string, whose documented constraint is that
    /// it "must not contain any path separator characters"; a daemon that sends
    /// something else would be asking the kernel to build a malformed interface
    /// name, and the daemon is the side that can report it usefully.
    pub fn valid_peer_key(s: &str) -> bool {
        s.len() == PEERKEY_CHARS
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }

    /// UTF-16LE encode, truncated to at most `max_units - 1` code units and
    /// always NUL-terminated, padded out to `max_units`.
    ///
    /// The truncation NEVER splits a surrogate pair. Chinese sits in the BMP so
    /// it cannot trip this, but macOS lets a user put an emoji in the computer
    /// name, and half a surrogate pair is not valid UTF-16 — it would be
    /// written straight into a persistent registry property.
    ///
    /// This is the UTF-16 counterpart of `haldev::clamp_utf8`, which truncates
    /// on a CHARACTER boundary for the same reason on the macOS side.
    pub fn clamp_utf16(s: &str, max_units: usize) -> Vec<u16> {
        assert!(max_units >= 1);
        let mut out: Vec<u16> = Vec::with_capacity(max_units);
        for ch in s.chars() {
            let need = ch.len_utf16();
            if out.len() + need > max_units - 1 {
                // Stopping at the CHARACTER, not at the code unit: a `break`
                // one unit later is exactly the split-surrogate bug.
                break;
            }
            let mut buf = [0u16; 2];
            out.extend_from_slice(ch.encode_utf16(&mut buf));
        }
        out.push(0);
        out.resize(max_units, 0);
        out
    }

    /// The daemon's half of "the receiver always terminates what it receives":
    /// an ASCII field is padded with NULs and can never be over-long.
    fn put_ascii(buf: &mut [u8], at: usize, len: usize, s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(len.saturating_sub(1));
        buf[at..at + len].fill(0);
        buf[at..at + n].copy_from_slice(&bytes[..n]);
    }

    fn put_utf16(buf: &mut [u8], at: usize, units: &[u16]) {
        for (i, u) in units.iter().enumerate() {
            buf[at + i * 2..at + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
    }

    // -- messages -----------------------------------------------------------

    pub fn encode_hello_request(client_pid: u32) -> [u8; HELLO_REQUEST_BYTES] {
        let mut b = [0u8; HELLO_REQUEST_BYTES];
        put_u32(&mut b, 0, PROTOCOL_VERSION);
        put_u32(&mut b, 4, 0); // flags MBZ
        put_u32(&mut b, 8, client_pid);
        put_u32(&mut b, 12, 0); // reserved MBZ
        b
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HelloReply {
        pub status: u32,
        pub protocol_version: u32,
        pub slot_count: u32,
        pub caps: u32,
        pub session_id: u64,
        pub sample_rate: u32,
        pub out_channels: u32,
        pub in_channels: u32,
        pub client_check: u32,
    }

    impl HelloReply {
        /// [`CAP_DATAPLANE`]: the driver has audio rings. Clear through M6-2.
        pub fn has_dataplane(&self) -> bool {
            self.caps & CAP_DATAPLANE != 0
        }

        /// [`CAP_VOLUME`]: the endpoints carry a hardware volume node, so the
        /// samples in the rings are full scale and relaying the user's slider
        /// to the peer attenuates ONCE.
        ///
        /// The daemon must not sync volume while this is clear: the audio
        /// engine's own APO has already applied the setting upstream of the
        /// ring, and applying it again on the peer's real device is a squared
        /// attenuation that nothing in either device list shows.
        pub fn has_volume(&self) -> bool {
            self.caps & CAP_VOLUME != 0
        }
    }

    pub fn decode_hello_reply(b: &[u8]) -> Option<HelloReply> {
        if b.len() != HELLO_REPLY_BYTES {
            return None;
        }
        Some(HelloReply {
            status: get_u32(b, 0),
            protocol_version: get_u32(b, 4),
            slot_count: get_u32(b, 8),
            caps: get_u32(b, 12),
            session_id: get_u64(b, 16),
            sample_rate: get_u32(b, 24),
            out_channels: get_u32(b, 28),
            in_channels: get_u32(b, 32),
            client_check: get_u32(b, 36),
        })
    }

    /// One `Bind` on the wire.
    ///
    /// Note what is NOT here compared with the macOS `AudioHubBindMsg`:
    /// `out_uid` / `in_uid` are gone. On macOS the UID is the device's
    /// identity and the daemon must supply it; on Windows the identity is the
    /// device-interface reference string, which the driver derives from
    /// `peer_key`. The daemon still keeps its UIDs — they are how the rest of
    /// the daemon, the UI and the regression scripts name a device — it just
    /// has nothing to tell the kernel about them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BindRequest<'a> {
        pub op: u32,
        pub slot: u8,
        pub flags: u32,
        pub generation: u32,
        pub session_id: u64,
        pub peer_key: &'a str,
        /// The DISAMBIGUATED PEER NAME exactly as [`crate::haldev::display_names`]
        /// produced it — bare, e.g. `"WIN-30"` or `"WIN-30 (2)"`. No
        /// `AudioHub – ` prefix and no direction suffix.
        ///
        /// [`encode_bind_request`] composes the prefix on the way out, through
        /// [`crate::haldev::device_name_stem`], which is the same function the macOS
        /// names are built from.
        ///
        /// The composition lives HERE and not at the call site on purpose.
        /// M6-2 shipped with the call site passing this field straight through,
        /// so every Windows endpoint was labelled `WIN-IR01HVEFU7G` with no
        /// AudioHub anywhere in it — and the encoder's own test passed, because
        /// it was handed an input that already carried the prefix. With the
        /// prefix applied here there is no call site left that can forget it,
        /// and the test below feeds the encoder what the daemon really produces.
        pub peer_display: &'a str,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BindEncodeError {
        BadPeerKey,
        SlotOutOfRange,
    }

    pub fn encode_bind_request(
        req: &BindRequest<'_>,
    ) -> Result<[u8; BIND_REQUEST_BYTES], BindEncodeError> {
        if !valid_peer_key(req.peer_key) {
            return Err(BindEncodeError::BadPeerKey);
        }
        if (req.slot as usize) >= MAX_SLOTS {
            return Err(BindEncodeError::SlotOutOfRange);
        }
        let mut b = [0u8; BIND_REQUEST_BYTES];
        put_u32(&mut b, 0, req.op);
        put_u32(&mut b, 4, req.slot as u32);
        put_u32(&mut b, 8, req.flags);
        put_u32(&mut b, 12, req.generation);
        put_u64(&mut b, 16, req.session_id);
        put_ascii(&mut b, 24, PEERKEY_BUF, req.peer_key);
        // A Clear carries no name; prefixing an empty one would put a bare
        // "AudioHub – " into the driver's log and nowhere useful.
        let display = if req.peer_display.is_empty() {
            String::new()
        } else {
            crate::haldev::device_name_stem(req.peer_display)
        };
        put_utf16(&mut b, 64, &clamp_utf16(&display, DISPLAY_CHARS));
        Ok(b)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BindReply {
        pub status: u32,
        pub slot: u32,
        pub generation: u32,
        pub state: u32,
        /// [`STAGE_NONE`] unless something failed.
        pub stage: u32,
        /// The raw kernel NTSTATUS of that stage. Diagnostics only: `status`
        /// alone decides what the daemon DOES, this decides what it can SAY.
        pub nt_status: u32,
        /// [`PUB_RENDER`] | [`PUB_CAPTURE`] as they stand after the call.
        pub published: u32,
        /// [`BINDREPLY_FLAG_NAME_FALLBACK`]. Degradations that leave the
        /// call successful but not wholly true.
        pub flags: u32,
    }

    impl BindReply {
        /// The invariant a successful SET must satisfy. Checked on this side
        /// too: the point of the field is that BOTH ends can assert it, so a
        /// driver that regresses is caught by the daemon rather than by a
        /// human looking at the system's device list.
        pub fn set_is_whole(&self) -> bool {
            self.status == STATUS_OK && self.published == PUB_BOTH
        }

        /// The bind worked, but the devices carry the INF's generic direction
        /// words rather than this peer's name.
        pub fn endpoint_name_fell_back(&self) -> bool {
            self.flags & BINDREPLY_FLAG_NAME_FALLBACK != 0
        }

        /// One line a status view can show verbatim.
        pub fn failure_text(&self) -> String {
            format!(
                "{} while {} (NTSTATUS 0x{:08x}); published: {}",
                status_label(self.status),
                stage_label(self.stage),
                self.nt_status,
                published_label(self.published),
            )
        }
    }

    /// Did this bind actually do what it claimed?
    ///
    /// `Ok(())` means the daemon may believe the reply. `Err(text)` means it
    /// must not, and carries the sentence to log and to publish over IPC.
    ///
    /// THIS IS THE PRODUCTION DECISION, not a description of it. `bind_call`
    /// on Windows calls exactly this function, so the tests below exercise the
    /// shipped path rather than a second copy of the rule that can drift from
    /// it — which is the failure mode that let the original defect through:
    /// the encoder was tested with a well-formed input while the PRODUCER of
    /// that input was never checked.
    pub fn bind_outcome(is_set: bool, r: &BindReply) -> Result<(), String> {
        if r.status != STATUS_OK {
            return Err(r.failure_text());
        }
        let want = if is_set { PUB_BOTH } else { 0 };
        if r.published != want {
            return Err(format!(
                "the driver reported success but published {} — treating it as a failure",
                published_label(r.published)
            ));
        }
        Ok(())
    }

    pub fn decode_bind_reply(b: &[u8]) -> Option<BindReply> {
        if b.len() != BIND_REPLY_BYTES {
            return None;
        }
        Some(BindReply {
            status: get_u32(b, 0),
            slot: get_u32(b, 4),
            generation: get_u32(b, 8),
            state: get_u32(b, 12),
            stage: get_u32(b, 16),
            nt_status: get_u32(b, 20),
            published: get_u32(b, 24),
            flags: get_u32(b, 28),
        })
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SlotInfo {
        pub state: u32,
        pub generation: u32,
        /// Empty when the slot is free.
        pub peer_key: String,
        /// [`PUB_RENDER`] | [`PUB_CAPTURE`] — the driver's own account of the
        /// filters it currently holds for this slot. A slot that says
        /// [`SLOT_BOUND`] with anything but [`PUB_BOTH`] is the failure this
        /// field was added to make detectable from user mode.
        pub published: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct QuerySlotsReply {
        pub status: u32,
        pub slot_count: u32,
        pub session_id: u64,
        pub slots: Vec<SlotInfo>,
    }

    pub fn decode_query_slots_reply(b: &[u8]) -> Option<QuerySlotsReply> {
        if b.len() != QUERY_SLOTS_REPLY_BYTES {
            return None;
        }
        let mut slots = Vec::with_capacity(MAX_SLOTS);
        for i in 0..MAX_SLOTS {
            let at = 16 + i * SLOT_INFO_BYTES;
            let key = &b[at + 8..at + 8 + PEERKEY_BUF];
            let end = key.iter().position(|&c| c == 0).unwrap_or(PEERKEY_BUF);
            slots.push(SlotInfo {
                state: get_u32(b, at),
                generation: get_u32(b, at + 4),
                // Lossy on purpose: a driver that somehow produced non-UTF-8
                // here must not be able to take the reconcile pass down. The
                // value is compared, never re-sent.
                peer_key: String::from_utf8_lossy(&key[..end]).into_owned(),
                published: get_u32(b, at + 8 + PEERKEY_BUF),
            });
        }
        Some(QuerySlotsReply {
            status: get_u32(b, 0),
            slot_count: get_u32(b, 4),
            session_id: get_u64(b, 8),
            slots,
        })
    }

    pub const EVENT_NONE: u32 = 0;
    pub const EVENT_VOLUME: u32 = 1;
    pub const EVENT_IOSTATE: u32 = 2;
    pub const EVENT_SLOT: u32 = 3;

    pub const EVFLAG_INPUT: u32 = 0x1;
    pub const EVFLAG_MUTED: u32 = 0x2;
    pub const EVFLAG_RUNNING: u32 = 0x4;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ControlEvent {
        pub kind: u32,
        pub slot: u32,
        pub generation: u32,
        pub flags: u32,
        pub scalar_q16: u32,
        pub state: u32,
    }

    impl ControlEvent {
        pub fn input(&self) -> bool {
            self.flags & EVFLAG_INPUT != 0
        }
        pub fn muted(&self) -> bool {
            self.flags & EVFLAG_MUTED != 0
        }
        pub fn running(&self) -> bool {
            self.flags & EVFLAG_RUNNING != 0
        }
        /// 16.16 fixed point, clamped. Fixed point rather than float because
        /// kernel code must not touch the FPU without saving its state.
        pub fn scalar(&self) -> f32 {
            (self.scalar_q16 as f32 / 65536.0).clamp(0.0, 1.0)
        }
    }

    pub fn decode_control_event(b: &[u8]) -> Option<ControlEvent> {
        if b.len() != CONTROL_EVENT_BYTES {
            return None;
        }
        Some(ControlEvent {
            kind: get_u32(b, 0),
            slot: get_u32(b, 4),
            generation: get_u32(b, 8),
            flags: get_u32(b, 12),
            scalar_q16: get_u32(b, 16),
            state: get_u32(b, 20),
        })
    }

    // -- MAP_RINGS ----------------------------------------------------------

    /// `AH_MAP_REQUEST`. `wake_event` is a user-mode HANDLE the driver
    /// references once and signals from the DPC whenever it moved audio, or 0
    /// for none.
    ///
    /// 0 is FULLY SUPPORTED and not a degraded mode: the mixer is driven by
    /// its own 10 ms tick, and the event is an accelerator, never the mechanism
    /// by which data becomes visible. A daemon that treated a missing event as
    /// a reason to give up on the data plane would be trading working audio for
    /// a latency optimisation.
    pub fn encode_map_request(session_id: u64, wake_event: u64) -> [u8; MAP_REQUEST_BYTES] {
        let mut b = [0u8; MAP_REQUEST_BYTES];
        put_u64(&mut b, 0, session_id);
        put_u64(&mut b, 8, wake_event);
        // Checked again HERE, for equality. An unversioned second call is how
        // a stale client would map the rings of a driver it never handshook
        // with.
        put_u32(&mut b, 16, PROTOCOL_VERSION);
        put_u32(&mut b, 20, 0); // flags MBZ
        b
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MapReply {
        pub status: u32,
        /// `2 * slot_count`. Entries of `va` past this are 0.
        pub ring_count: u32,
        pub data_offset: u32,
        pub capacity_frames: u32,
        pub sample_rate: u32,
        pub spk_channels: u32,
        pub mic_channels: u32,
        /// Mapped length of an OUT ring.
        pub spk_bytes: u32,
        /// Mapped length of an IN ring.
        pub mic_bytes: u32,
        /// Indexed `slot * 2 + dir`. Even entries are speakers (the driver
        /// writes), odd are microphones (this daemon writes). Always
        /// [`RING_SLOTS_MAX`] long; only the first `ring_count` are meaningful.
        pub va: Vec<u64>,
    }

    pub fn decode_map_reply(b: &[u8]) -> Option<MapReply> {
        if b.len() != MAP_REPLY_BYTES {
            return None;
        }
        let va = (0..RING_SLOTS_MAX).map(|i| get_u64(b, 40 + i * 8)).collect();
        Some(MapReply {
            status: get_u32(b, 0),
            ring_count: get_u32(b, 4),
            data_offset: get_u32(b, 8),
            capacity_frames: get_u32(b, 12),
            sample_rate: get_u32(b, 16),
            spk_channels: get_u32(b, 20),
            mic_channels: get_u32(b, 24),
            spk_bytes: get_u32(b, 28),
            mic_bytes: get_u32(b, 32),
            // offset 36 is `reserved`, MBZ: it keeps `va` 8-byte aligned
            // without the compiler inserting a hole nobody named.
            va,
        })
    }

    impl MapReply {
        /// Every geometry field against this daemon's own constants.
        ///
        /// A MISMATCH IS A REFUSAL, never an adaptation. The daemon maps this
        /// memory read/write and then indexes it with arithmetic bounded by
        /// these numbers; accepting a capacity the driver invented would aim a
        /// memcpy past the end of the mapping. And "adapt to whatever the
        /// driver says" is not available either — the media plane above is
        /// fixed at 48 kHz / 2ch out / 1ch in all the way to the socket.
        pub fn geometry_error(&self) -> Option<String> {
            let want = [
                ("data_offset", self.data_offset as u64, RING_DATA_OFFSET as u64),
                ("capacity_frames", self.capacity_frames as u64, RING_FRAMES as u64),
                ("sample_rate", self.sample_rate as u64, RING_SAMPLE_RATE as u64),
                ("spk_channels", self.spk_channels as u64, SPK_CHANNELS as u64),
                ("mic_channels", self.mic_channels as u64, MIC_CHANNELS as u64),
                ("spk_bytes", self.spk_bytes as u64, SPK_BYTES as u64),
                ("mic_bytes", self.mic_bytes as u64, MIC_BYTES as u64),
            ];
            for (name, got, expect) in want {
                if got != expect {
                    return Some(format!(
                        "the driver's rings report {name} = {got}, this daemon is built for {expect}"
                    ));
                }
            }
            if self.ring_count == 0 || self.ring_count as usize > RING_SLOTS_MAX {
                return Some(format!(
                    "the driver reported {} rings (this daemon supports 1..={RING_SLOTS_MAX})",
                    self.ring_count
                ));
            }
            if self.ring_count % 2 != 0 {
                return Some(format!(
                    "the driver reported {} rings, which is not 2 per slot",
                    self.ring_count
                ));
            }
            None
        }
    }

    // -- NOTIFY -------------------------------------------------------------

    pub const NOTIFYFLAG_MUTED: u32 = 0x1;
    /// The virtual MICROPHONE, else the speaker.
    pub const NOTIFYFLAG_INPUT: u32 = 0x2;

    /// 16.16 fixed point, `0x10000 == 1.0 == full scale`.
    ///
    /// Fixed point rather than float because the kernel consumes this at
    /// DISPATCH_LEVEL, where touching the FPU without bracketing it in
    /// `KeSaveExtendedProcessorState` is not allowed.
    ///
    /// SATURATES at `0x10000`. `1.0 * 65536.0` is exactly 65536, so the clamp
    /// on the way in is what actually bounds this; the saturation is the belt
    /// to that suspenders, and it is the value the driver compares against its
    /// own stored level.
    pub fn scalar_to_q16(scalar: f32) -> u32 {
        let q = (scalar.clamp(0.0, 1.0) * 65536.0).round();
        // `as` on a float is already saturating in Rust, but the min is what
        // says 0x10000 is the ceiling rather than leaving it to rounding.
        (q as u32).min(0x1_0000)
    }

    /// The inverse, clamped. Used on anything the DRIVER sent: a value above
    /// full scale is relayed to a peer's real device, so it is bounded here
    /// rather than trusted.
    pub fn q16_to_scalar(q: u32) -> f32 {
        (q as f32 / 65536.0).min(1.0)
    }

    /// `AH_NOTIFY_REQUEST`.
    ///
    /// `generation` is dropped by the driver if it does not match the slot's
    /// current stamp: a late notify for a slot's PREVIOUS tenant must not move
    /// the next peer's slider.
    pub fn encode_notify_request(
        session_id: u64,
        slot: u8,
        generation: u32,
        input: bool,
        muted: bool,
        scalar: f32,
    ) -> [u8; NOTIFY_REQUEST_BYTES] {
        let mut b = [0u8; NOTIFY_REQUEST_BYTES];
        put_u64(&mut b, 0, session_id);
        put_u32(&mut b, 8, slot as u32);
        put_u32(&mut b, 12, generation);
        put_u32(
            &mut b,
            16,
            (if input { NOTIFYFLAG_INPUT } else { 0 }) | (if muted { NOTIFYFLAG_MUTED } else { 0 }),
        );
        put_u32(&mut b, 20, scalar_to_q16(scalar));
        b
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NotifyReply {
        pub status: u32,
        /// 1 when the stored level actually changed and an event was raised, 0
        /// when it was already this value.
        ///
        /// Observable ON PURPOSE: "the driver suppressed my echo" and "the
        /// driver ignored me" are otherwise the same silence. Loop suppression
        /// is the DRIVER's job — applying a value equal to the stored one
        /// raises no event — and without a way to see it happen, a sync that
        /// silently did nothing looks exactly like one that worked.
        pub applied: u32,
    }

    impl NotifyReply {
        pub fn applied(&self) -> bool {
            self.applied != 0
        }
    }

    pub fn decode_notify_reply(b: &[u8]) -> Option<NotifyReply> {
        if b.len() != NOTIFY_REPLY_BYTES {
            return None;
        }
        Some(NotifyReply { status: get_u32(b, 0), applied: get_u32(b, 4) })
    }

    /// `\\.\AudioHubVadCtl` as a NUL-terminated UTF-16 path for `CreateFileW`.
    pub fn ctl_path_utf16() -> Vec<u16> {
        CTL_PATH.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

// ---------------------------------------------------------------- rings

/// The data plane: the shared-memory audio rings, as pure arithmetic over an
/// address.
///
/// A FAITHFUL SIBLING of `halbridge::platform::RingMem` on macOS and of
/// `AhRingWrite`/`AhRingRead` in `AudioHubRing.h`. The same 40-byte header,
/// the same free-running SPSC indices, the same `wrapping_sub`, the same
/// `.min(capacity)` clamps, the same "a reader more than a capacity behind
/// jumps to the newest window rather than replaying stale audio", the same
/// "a full ring drops the tail rather than waiting". Three implementations of
/// one algorithm; a difference in any of them is an audible defect nobody can
/// localise.
///
/// # Why there is no `Drop` that unmaps
///
/// On macOS `RingMem::drop` calls `mach_vm_deallocate`, because the mapping is
/// this task's and nothing else will take it away. On Windows the mapping is
/// torn down by the driver in `IRP_MJ_CLEANUP`, which runs in the context of
/// the process closing the control handle — the only context in which
/// `MmUnmapLockedPages` is safe at all ("if the context is incorrect, the
/// unmapping operation could delete the address range of a random process").
/// There is therefore no user-mode call to make here. A `Drop` that tried to
/// `VirtualFree` or `UnmapViewOfFile` this range would be freeing memory it
/// never allocated, against an allocator that never handed it out.
///
/// What that costs is that the mapping's LIFETIME is the control handle's, not
/// this struct's — so [`WinRings::detach`] must run, and its write lock must be
/// acquired, BEFORE the handle closes. That is the whole reason the `RwLock` is
/// here; see its own note.
pub mod rings {
    use super::wire;
    use crate::{rd, wr};
    use anyhow::{anyhow, Result};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::RwLock;

    /// `AUDIOHUB_RING_HEADER`. Byte-for-byte the macOS bridge's header, which
    /// is why one set of constants covers both.
    #[repr(C)]
    struct RingHeader {
        magic: u32,
        version: u32,
        sample_rate: u32,
        channels: u32,
        capacity_frames: u32,
        reserved: u32,
        /// Producer only.
        write_idx: AtomicU64,
        /// Consumer only.
        read_idx: AtomicU64,
    }

    // The C_ASSERTs from AudioHubRing.h, restated where Rust can check them.
    const _: () = assert!(std::mem::size_of::<RingHeader>() == 40);
    const _: () = assert!(std::mem::offset_of!(RingHeader, write_idx) == 24);
    const _: () = assert!(std::mem::offset_of!(RingHeader, read_idx) == 32);
    const _: () = assert!(std::mem::size_of::<RingHeader>() <= wire::RING_DATA_OFFSET);

    /// One ring, already mapped by the driver into this process.
    ///
    /// Everything here is arithmetic; nothing owns anything. See the module
    /// note for why there is no `Drop`.
    struct RingMem {
        addr: usize,
        data_offset: usize,
        capacity: u32,
        channels: u32,
    }

    impl RingMem {
        /// Validates one ring's header against the contract and takes it.
        ///
        /// The header check happens HERE and only here. `hdr()` forms a
        /// `&RingHeader`, which is a data race by the letter of the model the
        /// moment the driver is running — so it may only be done at attach,
        /// before anything is using the ring, and the hot paths below address
        /// the two indices by OFFSET so they never form a reference to
        /// anything but the `AtomicU64` itself.
        fn map(va: u64, rep: &wire::MapReply, want_channels: u32, what: &str) -> Result<RingMem> {
            if va == 0 {
                return Err(anyhow!("the driver mapped no memory for the {what} ring"));
            }
            let addr = usize::try_from(va)
                .map_err(|_| anyhow!("the {what} ring's address {va:#x} is not an address"))?;
            // An unaligned `AtomicU64` is undefined behaviour, not a slow path.
            // The kernel maps on a page boundary, so this can only fail if the
            // reply is not describing a mapping at all.
            if addr % std::mem::align_of::<RingHeader>() != 0 {
                return Err(anyhow!(
                    "the {what} ring is at {addr:#x}, which is not 8-byte aligned"
                ));
            }

            let me = RingMem {
                addr,
                data_offset: rep.data_offset as usize,
                capacity: rep.capacity_frames,
                channels: want_channels,
            };

            // Cross-check the MEMORY against what the reply claimed. If these
            // disagree, the reply described a different object than the address
            // points at, and no amount of arithmetic recovers from that.
            let h = me.hdr();
            if h.magic != wire::RING_MAGIC || h.version != wire::RING_VERSION {
                return Err(anyhow!(
                    "the {what} ring header is magic {:#x} v{}, expected {:#x} v{}",
                    h.magic,
                    h.version,
                    wire::RING_MAGIC,
                    wire::RING_VERSION
                ));
            }
            if h.channels != want_channels {
                return Err(anyhow!(
                    "the {what} ring header says {}ch, this daemon needs {want_channels}ch",
                    h.channels
                ));
            }
            if h.capacity_frames != rep.capacity_frames {
                return Err(anyhow!(
                    "the {what} ring header says {} frames, the reply said {}",
                    h.capacity_frames,
                    rep.capacity_frames
                ));
            }
            if h.sample_rate != rep.sample_rate {
                return Err(anyhow!(
                    "the {what} ring header says {}Hz, the reply said {}Hz",
                    h.sample_rate,
                    rep.sample_rate
                ));
            }
            Ok(me)
        }

        /// ATTACH-TIME AND TESTS ONLY — see [`RingMem::map`].
        fn hdr(&self) -> &RingHeader {
            unsafe { &*(self.addr as *const RingHeader) }
        }

        /// The two indices, addressed by offset so no reference to the
        /// surrounding struct is ever created. `AtomicU64` is the only type in
        /// the header that both sides may touch while the ring is live.
        fn w_idx(&self) -> &AtomicU64 {
            const OFF: usize = std::mem::offset_of!(RingHeader, write_idx);
            unsafe { &*((self.addr + OFF) as *const AtomicU64) }
        }

        fn r_idx(&self) -> &AtomicU64 {
            const OFF: usize = std::mem::offset_of!(RingHeader, read_idx);
            unsafe { &*((self.addr + OFF) as *const AtomicU64) }
        }

        fn data(&self) -> *mut f32 {
            (self.addr + self.data_offset) as *mut f32
        }

        /// Producer. Returns frames written; a full ring drops the tail.
        fn write(&self, src: &[f32], frames: usize) -> usize {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let w = self.w_idx().load(Ordering::Relaxed);
            let r = self.r_idx().load(Ordering::Acquire);
            // `read_idx` belongs to the DRIVER, so it can hold anything: a
            // consumer that reset its index legitimately reads ahead of us for
            // an instant. The clamp is what stops the unsigned subtraction from
            // reporting a 2^64-sized backlog and `cap - used` from underflowing.
            let used = (w.wrapping_sub(r) as usize).min(cap);
            let count = frames.min(cap - used).min(src.len() / ch);
            if count == 0 {
                return 0;
            }
            let start = (w % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), self.data().add(start * ch), first * ch);
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(first * ch),
                        self.data(),
                        (count - first) * ch,
                    );
                }
            }
            self.w_idx().store(w + count as u64, Ordering::Release);
            count
        }

        /// Consumer. Returns frames read; the caller zero-fills the rest.
        fn read(&self, dst: &mut [f32], frames: usize) -> usize {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            // More than a full buffer behind means we stalled: jump to the
            // newest window rather than replay half a second of stale audio.
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let count = frames.min(avail).min(dst.len() / ch);
            if count == 0 {
                return 0;
            }
            // wrapping, not plain `-`: the driver rewinds its indices to 0 when
            // it recreates a ring, which leaves our read_idx ahead of write_idx
            // for one pass. `w - avail` would panic there in a debug build;
            // wrapping re-converges on the next read.
            let effective = w.wrapping_sub(avail as u64);
            let start = (effective % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data().add(start * ch),
                    dst.as_mut_ptr(),
                    first * ch,
                );
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        self.data(),
                        dst.as_mut_ptr().add(first * ch),
                        (count - first) * ch,
                    );
                }
            }
            self.r_idx()
                .store(effective.wrapping_add(count as u64), Ordering::Release);
            count
        }

        /// Consumer, 只窥不取: identical sampling to [`RingMem::read`] but it
        /// does not write `read_idx`.
        ///
        /// Returns `(frames, base)`. The base MUST be handed back to
        /// [`RingMem::advance`] — `read()` is one indivisible "compute the
        /// index, copy, store", and if `advance` recomputed the index itself,
        /// whatever the producer pushed in between would be counted as consumed.
        fn peek(&self, dst: &mut [f32], frames: usize) -> (usize, u64) {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let effective = w.wrapping_sub(avail as u64);
            let count = frames.min(avail).min(dst.len() / ch);
            if count == 0 {
                return (0, effective);
            }
            let start = (effective % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data().add(start * ch),
                    dst.as_mut_ptr(),
                    first * ch,
                );
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        self.data(),
                        dst.as_mut_ptr().add(first * ch),
                        (count - first) * ch,
                    );
                }
            }
            (count, effective)
        }

        /// Moves `read_idx` `frames` on from a [`RingMem::peek`]'s base.
        fn advance(&self, base: u64, frames: usize) {
            self.r_idx()
                .store(base.wrapping_add(frames as u64), Ordering::Release);
        }

        /// Consumer-side deliberate discard: moves `read_idx` on by at most
        /// `frames` and returns how many were really dropped. Moves no samples.
        fn drop_frames(&self, frames: usize) -> usize {
            let cap = self.capacity as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let effective = w.wrapping_sub(avail as u64);
            let n = frames.min(avail);
            self.r_idx()
                .store(effective.wrapping_add(n as u64), Ordering::Release);
            n
        }

        /// Frames occupying the ring right now. The SAME expression as `read`'s
        /// `avail` and `write`'s `used` — including the `min(cap)` clamp and the
        /// wrapping — but it MOVES NO INDEX.
        ///
        /// Observing must not consume: the HAL ring is the one stage whose
        /// residency ceiling is exactly 500 ms, and an "observation" that
        /// advanced `read_idx` would zero the very quantity it claims to
        /// measure.
        fn readable(&self) -> u32 {
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            (w.wrapping_sub(r) as usize).min(self.capacity as usize) as u32
        }

        /// Consumer-side backlog drop; only ever called on the ring this
        /// process consumes.
        fn flush_consumer(&self) {
            self.r_idx()
                .store(self.w_idx().load(Ordering::Acquire), Ordering::Release);
        }
    }

    struct RingPair {
        spk: RingMem,
        mic: RingMem,
    }

    /// EVERY slot's mappings, present only while a driver session is attached.
    ///
    /// A single `Option`, so attach and detach stay whole-set atomic: a
    /// handshake either installs all `slot_count` pairs or none, and a detach
    /// takes them all away at once. A per-slot `Option` would let a caller read
    /// slot 3 of the previous driver while slot 4 belongs to the new one.
    ///
    /// The lock is NOT what makes the rings safe to use concurrently — the
    /// free-running SPSC indices do that, and both audio callers take it
    /// shared. It exists because the mapping GOES AWAY when the control handle
    /// closes (`IRP_MJ_CLEANUP`), so a detach has to wait for the mixer and the
    /// tx engine to be out of the pages before `CloseHandle` runs. Exactly the
    /// macOS reason with a different verb: there `mach_vm_deallocate`, here the
    /// kernel doing it for us the moment we stop holding the handle.
    ///
    /// A boxed SLICE rather than `[RingPair; MAX_SLOTS]`: a driver built with a
    /// smaller pool reports a smaller `ring_count` and hands over exactly that
    /// many pairs, and indexing a fixed array would then read mappings that
    /// were never made.
    pub struct WinRings {
        inner: RwLock<Option<Box<[RingPair]>>>,
    }

    // The pointers are into a mapping owned by the driver and live exactly as
    // long as the control handle does, which the RwLock is what serialises; the
    // SPSC discipline (one reader thread, one writer thread per ring) is
    // documented on the public methods.
    unsafe impl Send for WinRings {}
    unsafe impl Sync for WinRings {}

    impl Default for WinRings {
        fn default() -> WinRings {
            WinRings::new()
        }
    }

    impl WinRings {
        pub fn new() -> WinRings {
            WinRings { inner: RwLock::new(None) }
        }

        /// Takes the whole reply and installs every pair it describes.
        ///
        /// ALL OR NOTHING: the pairs are built into a local `Vec` and only
        /// published once every one of them validated. A partial install would
        /// be worse than no install — the mixer would find silence on some
        /// slots and audio on others, with nothing anywhere saying why.
        ///
        /// Returns the number of SLOTS installed.
        pub fn attach(&self, rep: &wire::MapReply) -> Result<usize> {
            if let Some(e) = rep.geometry_error() {
                return Err(anyhow!("{e}"));
            }
            let slots = rep.ring_count as usize / 2;
            if rep.va.len() < rep.ring_count as usize {
                return Err(anyhow!(
                    "the reply claims {} rings but carries {} addresses",
                    rep.ring_count,
                    rep.va.len()
                ));
            }
            let mut pairs = Vec::with_capacity(slots);
            for slot in 0..slots {
                let spk = RingMem::map(
                    rep.va[slot * 2 + wire::DIR_OUT],
                    rep,
                    wire::SPK_CHANNELS,
                    &format!("slot {slot} speaker"),
                )?;
                let mic = RingMem::map(
                    rep.va[slot * 2 + wire::DIR_IN],
                    rep,
                    wire::MIC_CHANNELS,
                    &format!("slot {slot} microphone"),
                )?;
                pairs.push(RingPair { spk, mic });
            }
            *wr(&self.inner) = Some(pairs.into_boxed_slice());
            Ok(slots)
        }

        /// Drops every mapping.
        ///
        /// MUST run before the control handle closes: taking the write lock is
        /// what waits for the mixer and the tx engine to be out of the pages,
        /// and after `CloseHandle` those pages are not ours to touch.
        pub fn detach(&self) {
            *wr(&self.inner) = None;
        }

        pub fn attached(&self) -> bool {
            rd(&self.inner).is_some()
        }

        /// Hands the mapping over to another `WinRings`, leaving this one
        /// detached.
        ///
        /// `Session::open` is the only thing that can build the mapping — it is
        /// the only thing holding the reply — but the mixer and the tx engine
        /// reach the rings through `platform::Rings`, on a path that must never
        /// take the session mutex. This is that handover, and it is a MOVE:
        /// afterwards exactly one `WinRings` names those pages.
        pub fn move_into(&self, dst: &WinRings) {
            *wr(&dst.inner) = wr(&self.inner).take();
        }

        /// 0 with no driver attached (or a slot this driver does not have): the
        /// caller zero-fills, so a missing driver is silence, never a stall.
        pub fn read_spk(&self, slot: usize, dst: &mut [f32], frames: usize) -> usize {
            match rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                Some(p) => p.spk.read(dst, frames),
                None => 0,
            }
        }

        /// `None` distinguishes "no ring at all" from "the ring was full",
        /// which are the same number of frames accepted but very different
        /// diagnoses.
        pub fn write_mic(&self, slot: usize, mono: &[f32]) -> Option<usize> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.mic.write(mono, mono.len()))
        }

        pub fn flush_spk_consumer(&self, slot: usize) {
            if let Some(p) = rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                p.spk.flush_consumer();
            }
        }

        /// 只窥不取. `None` = no driver attached / no such slot.
        pub fn peek_spk(&self, slot: usize, dst: &mut [f32], frames: usize) -> Option<(usize, u64)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.spk.peek(dst, frames))
        }

        /// Moves the read pointer on from a [`WinRings::peek_spk`] base.
        pub fn advance_spk(&self, slot: usize, base: u64, frames: usize) {
            if let Some(p) = rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                p.spk.advance(base, frames);
            }
        }

        /// Drops at most `frames`, returning how many really went.
        pub fn drop_spk(&self, slot: usize, frames: usize) -> usize {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.spk.drop_frames(frames))
                .unwrap_or(0)
        }

        /// `(readable frames, capacity frames)`, READ ONLY: it does not move
        /// `read_idx`, so observing through it cannot change what it measures.
        ///
        /// `None` = no driver attached / no such slot ⇒ this stage does not
        /// exist. NOT 0 ms.
        pub fn spk_readable(&self, slot: usize) -> Option<(u32, u32)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| (p.spk.readable(), p.spk.capacity))
        }

        /// `(occupied frames, capacity frames)` of the MIC ring, READ ONLY.
        ///
        /// The same `w - r` expression means different things in the two
        /// directions: on the speaker ring we are the consumer and it is
        /// "readable"; on the microphone ring we are the producer and the same
        /// number is "backlog the driver has not taken yet" — which is exactly
        /// the queue the frame we are writing now has to wait behind. Hence
        /// `occupied`, not `readable`.
        pub fn mic_occupied(&self, slot: usize) -> Option<(u32, u32)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| (p.mic.readable(), p.mic.capacity))
        }
    }

    // ------------------------------------------------------------ ring tests
    //
    // These run on macOS, which is the ONLY reason any of the arithmetic above
    // is exercised at all before it reaches a kernel mapping. The rings are
    // ordinary aligned heap allocations shaped exactly like the driver's, with
    // real headers, handed to the real `attach` at the real geometry.
    //
    // Every assertion below is on DATA THAT ROUND-TRIPPED — a ramp written by
    // one side and read back by the other — never on a constant this file
    // could redefine on both sides at once.

    #[cfg(test)]
    mod tests {
        use super::*;

        /// One ring's worth of correctly shaped, 8-byte-aligned memory with a
        /// valid header, standing in for the driver's non-paged allocation.
        ///
        /// `Vec<u64>` rather than `Vec<u8>`: the header holds two `AtomicU64`
        /// and an unaligned atomic is undefined behaviour, not a slow path.
        struct FakeRing {
            buf: Vec<u64>,
            channels: u32,
        }

        impl FakeRing {
            fn new(channels: u32, bytes: usize) -> FakeRing {
                let mut me = FakeRing { buf: vec![0u64; bytes / 8], channels };
                let h = me.hdr();
                h.magic = wire::RING_MAGIC;
                h.version = wire::RING_VERSION;
                h.sample_rate = wire::RING_SAMPLE_RATE;
                h.channels = channels;
                h.capacity_frames = wire::RING_FRAMES;
                h.reserved = 0;
                me
            }

            fn spk() -> FakeRing {
                FakeRing::new(wire::SPK_CHANNELS, wire::SPK_BYTES)
            }

            fn mic() -> FakeRing {
                FakeRing::new(wire::MIC_CHANNELS, wire::MIC_BYTES)
            }

            fn addr(&self) -> u64 {
                self.buf.as_ptr() as usize as u64
            }

            #[allow(clippy::mut_from_ref)]
            fn hdr(&mut self) -> &mut RingHeader {
                unsafe { &mut *(self.buf.as_mut_ptr() as *mut RingHeader) }
            }

            /// A `RingMem` view for the OTHER side of the ring — the driver.
            /// Built through the shipped `RingMem::map`, so the test producer
            /// also exercises the real validation.
            fn view(&self, rep: &wire::MapReply) -> RingMem {
                RingMem::map(self.addr(), rep, self.channels, "test").expect("valid fake ring")
            }

            /// Plants raw index values, standing in for a driver that reset its
            /// ring underneath a live mapping. Nothing in the shipped producer
            /// can put `write_idx` more than a capacity ahead of `read_idx`, so
            /// this is the only way to reach the "consumer stalled" branch.
            fn set_indices(&mut self, w: u64, r: u64) {
                let h = self.hdr();
                h.write_idx.store(w, Ordering::SeqCst);
                h.read_idx.store(r, Ordering::SeqCst);
            }

            fn indices(&mut self) -> (u64, u64) {
                let h = self.hdr();
                (h.write_idx.load(Ordering::SeqCst), h.read_idx.load(Ordering::SeqCst))
            }
        }

        fn good_reply(rings: &[&FakeRing]) -> wire::MapReply {
            let mut va = vec![0u64; wire::RING_SLOTS_MAX];
            for (i, r) in rings.iter().enumerate() {
                va[i] = r.addr();
            }
            wire::MapReply {
                status: wire::STATUS_OK,
                ring_count: rings.len() as u32,
                data_offset: wire::RING_DATA_OFFSET as u32,
                capacity_frames: wire::RING_FRAMES,
                sample_rate: wire::RING_SAMPLE_RATE,
                spk_channels: wire::SPK_CHANNELS,
                mic_channels: wire::MIC_CHANNELS,
                spk_bytes: wire::SPK_BYTES as u32,
                mic_bytes: wire::MIC_BYTES as u32,
                va,
            }
        }

        /// Stereo ramp: channel 0 carries the absolute frame index, channel 1
        /// carries index + 0.5, so a swapped channel or an off-by-one frame is
        /// a visibly different NUMBER rather than a plausible one.
        ///
        /// `f32` is exact for integers below 2^24, and every index used here is
        /// far under that, so equality comparison is legitimate.
        fn stereo_ramp(from: u64, frames: usize) -> Vec<f32> {
            let mut v = Vec::with_capacity(frames * 2);
            for i in 0..frames {
                let n = (from + i as u64) as f32;
                v.push(n);
                v.push(n + 0.5);
            }
            v
        }

        fn mono_ramp(from: u64, frames: usize) -> Vec<f32> {
            (0..frames).map(|i| (from + i as u64) as f32).collect()
        }

        /// Asserts a stereo buffer is exactly the ramp starting at `from`.
        fn assert_stereo_ramp(got: &[f32], from: u64, frames: usize) {
            assert_eq!(got.len(), frames * 2, "frame count");
            for i in 0..frames {
                let n = (from + i as u64) as f32;
                assert_eq!(got[i * 2], n, "frame {i} left (want the ramp from {from})");
                assert_eq!(got[i * 2 + 1], n + 0.5, "frame {i} right");
            }
        }

        // -- attach ---------------------------------------------------------

        #[test]
        fn attach_installs_one_pair_per_slot_and_detach_takes_them_all_away() {
            let (s0, m0, s1, m1) =
                (FakeRing::spk(), FakeRing::mic(), FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&s0, &m0, &s1, &m1]);

            let r = WinRings::new();
            assert!(!r.attached());
            assert_eq!(r.attach(&rep).expect("two well formed slots"), 2);
            assert!(r.attached());
            assert_eq!(r.spk_readable(0).map(|(_, c)| c), Some(wire::RING_FRAMES));
            assert_eq!(r.spk_readable(1).map(|(_, c)| c), Some(wire::RING_FRAMES));
            // A slot past what this driver reported is not a ring.
            assert_eq!(r.spk_readable(2), None);
            assert_eq!(r.mic_occupied(2), None);

            r.detach();
            assert!(!r.attached());
            assert_eq!(r.spk_readable(0), None);
        }

        /// THE index-encoding test. `AUDIOHUB_RING_INDEX(slot, dir)` is
        /// `slot * 2 + dir`, and getting it wrong routes peer A's audio to peer
        /// B's device — which sounds like working audio to whoever is testing
        /// with one peer paired.
        ///
        /// Each ring gets a ramp from a DIFFERENT base, so the assertion names
        /// which ring the data came out of, not merely that some data did.
        #[test]
        fn each_slot_reads_and_writes_its_own_pair_of_rings() {
            let (s0, m0, s1, m1) =
                (FakeRing::spk(), FakeRing::mic(), FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&s0, &m0, &s1, &m1]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();

            // The driver writes a distinct ramp into each speaker ring.
            s0.view(&rep).write(&stereo_ramp(1000, 64), 64);
            s1.view(&rep).write(&stereo_ramp(7000, 64), 64);

            let mut dst = vec![0.0f32; 64 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 64), 64);
            assert_stereo_ramp(&dst, 1000, 64);
            assert_eq!(r.read_spk(1, &mut dst, 64), 64);
            assert_stereo_ramp(&dst, 7000, 64);

            // And the daemon's microphone writes land in each slot's OWN mic
            // ring, which the driver reads back.
            assert_eq!(r.write_mic(0, &mono_ramp(20, 32)), Some(32));
            assert_eq!(r.write_mic(1, &mono_ramp(90, 32)), Some(32));
            let mut mono = vec![0.0f32; 32];
            assert_eq!(m0.view(&rep).read(&mut mono, 32), 32);
            assert_eq!(mono, mono_ramp(20, 32));
            assert_eq!(m1.view(&rep).read(&mut mono, 32), 32);
            assert_eq!(mono, mono_ramp(90, 32));
        }

        /// Every header field the contract pins, corrupted one at a time. A
        /// rejected attach must leave NOTHING installed: a half-built set is
        /// how one slot ends up pointing at another driver's pages.
        #[test]
        fn attach_refuses_a_ring_whose_header_disagrees_with_the_contract() {
            for (what, mutate) in [
                ("magic", (|h: &mut RingHeader| h.magic = 0x4148_5232) as fn(&mut RingHeader)),
                ("version", |h| h.version = 2),
                ("channels", |h| h.channels = 1),
                ("capacity", |h| h.capacity_frames = wire::RING_FRAMES - 1),
                ("sample rate", |h| h.sample_rate = 44_100),
            ] {
                let (mut spk, mic) = (FakeRing::spk(), FakeRing::mic());
                mutate(spk.hdr());
                let rep = good_reply(&[&spk, &mic]);
                let r = WinRings::new();
                let e = r
                    .attach(&rep)
                    .expect_err(&format!("a ring with a bad {what} must be refused"));
                assert!(
                    format!("{e:#}").contains("speaker"),
                    "the error must name the ring: {e:#}"
                );
                assert!(!r.attached(), "a refused attach must install nothing ({what})");
            }
        }

        /// The SECOND ring of the pair is validated too. The first version of
        /// the loop above only ever corrupted the speaker, so a `map` call that
        /// forgot to check the microphone would have survived it.
        #[test]
        fn attach_refuses_a_bad_microphone_ring_as_well_as_a_bad_speaker() {
            let (spk, mut mic) = (FakeRing::spk(), FakeRing::mic());
            mic.hdr().magic = 0;
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            let e = r.attach(&rep).expect_err("a bad microphone ring is still a bad ring");
            assert!(format!("{e:#}").contains("microphone"), "{e:#}");
            assert!(!r.attached());
        }

        /// A ring the driver did not map. `va == 0` decoded as an address is a
        /// null dereference on the first memcpy.
        #[test]
        fn attach_refuses_a_null_or_unaligned_address() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());

            let mut rep = good_reply(&[&spk, &mic]);
            rep.va[1] = 0;
            assert!(WinRings::new().attach(&rep).is_err(), "va = 0 is not an address");

            let mut rep = good_reply(&[&spk, &mic]);
            rep.va[0] += 4; // 4-aligned, not 8: an unaligned AtomicU64.
            let e = WinRings::new().attach(&rep).expect_err("unaligned is UB, not a slow path");
            assert!(format!("{e:#}").contains("aligned"), "{e:#}");
        }

        /// The geometry gate, exercised through the shipped `attach` rather
        /// than by calling `geometry_error` directly.
        #[test]
        fn attach_refuses_a_reply_whose_geometry_is_not_this_daemons() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let base = good_reply(&[&spk, &mic]);

            for (what, mutate) in [
                ("capacity_frames", (|r: &mut wire::MapReply| r.capacity_frames = 12_000)
                    as fn(&mut wire::MapReply)),
                ("sample_rate", |r| r.sample_rate = 44_100),
                ("data_offset", |r| r.data_offset = 40),
                ("spk_channels", |r| r.spk_channels = 1),
                ("mic_channels", |r| r.mic_channels = 2),
                ("spk_bytes", |r| r.spk_bytes = 1024),
                ("mic_bytes", |r| r.mic_bytes = 1024),
                ("ring_count = 0", |r| r.ring_count = 0),
                ("an odd ring_count", |r| r.ring_count = 3),
                ("ring_count past the pool", |r| r.ring_count = 64),
            ] {
                let mut rep = base.clone();
                mutate(&mut rep);
                let rings = WinRings::new();
                assert!(
                    rings.attach(&rep).is_err(),
                    "{what} must be refused, never adapted to"
                );
                assert!(!rings.attached());
            }
        }

        // -- transfer -------------------------------------------------------

        /// The basic round trip in the direction the DRIVER produces: it writes
        /// stereo frames, the daemon reads them back byte-identical and in
        /// order.
        #[test]
        fn the_speaker_ring_round_trips_stereo_frames_in_order() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            assert_eq!(driver.write(&stereo_ramp(0, 480), 480), 480);
            let mut dst = vec![0.0f32; 480 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 480), 480);
            assert_stereo_ramp(&dst, 0, 480);

            // The ring is now empty, and an empty ring reads NOTHING rather
            // than replaying what it just handed over.
            let mut again = vec![-1.0f32; 480 * 2];
            assert_eq!(r.read_spk(0, &mut again, 480), 0);
            assert_eq!(again[0], -1.0, "an empty read must not touch the buffer");

            // The next write continues the sequence.
            assert_eq!(driver.write(&stereo_ramp(480, 480), 480), 480);
            assert_eq!(r.read_spk(0, &mut dst, 480), 480);
            assert_stereo_ramp(&dst, 480, 480);
        }

        /// The other direction: the daemon writes mono, the driver reads it.
        #[test]
        fn the_microphone_ring_round_trips_mono_frames_in_order() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = mic.view(&rep);

            assert_eq!(r.write_mic(0, &mono_ramp(0, 480)), Some(480));
            assert_eq!(r.write_mic(0, &mono_ramp(480, 120)), Some(120));

            let mut dst = vec![0.0f32; 600];
            assert_eq!(driver.read(&mut dst, 600), 600);
            assert_eq!(dst, mono_ramp(0, 600), "both writes, contiguous, in order");
        }

        /// The wrap. A ring whose read/write straddles the physical end of the
        /// buffer must produce ONE contiguous sequence — a broken `first` or
        /// `start` gives audio that is intact for 499 ms and then jumps.
        #[test]
        fn a_transfer_that_straddles_the_end_of_the_buffer_stays_contiguous() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            // Park both indices just short of the physical end.
            let cap = wire::RING_FRAMES as usize;
            let lead = cap - 100;
            assert_eq!(driver.write(&stereo_ramp(0, lead), lead), lead);
            let mut sink = vec![0.0f32; lead * 2];
            assert_eq!(r.read_spk(0, &mut sink, lead), lead);

            // 200 frames from absolute index `lead`: 100 before the wrap, 100
            // after it.
            assert_eq!(driver.write(&stereo_ramp(lead as u64, 200), 200), 200);
            let mut dst = vec![0.0f32; 200 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 200), 200);
            assert_stereo_ramp(&dst, lead as u64, 200);

            // And the same wrap on the write side of the MIC ring.
            let mic_drv = mic.view(&rep);
            assert_eq!(r.write_mic(0, &mono_ramp(0, lead)), Some(lead));
            let mut msink = vec![0.0f32; lead];
            assert_eq!(mic_drv.read(&mut msink, lead), lead);
            assert_eq!(r.write_mic(0, &mono_ramp(lead as u64, 200)), Some(200));
            let mut mdst = vec![0.0f32; 200];
            assert_eq!(mic_drv.read(&mut mdst, 200), 200);
            assert_eq!(mdst, mono_ramp(lead as u64, 200));
        }

        /// A full ring DROPS THE TAIL rather than waiting or overwriting: the
        /// producer is a DPC and a DPC that waits is a system-wide glitch.
        ///
        /// What survives must be the HEAD of the sequence. An implementation
        /// that overwrote instead would leave the tail and lose the head, which
        /// is the same frame count and the opposite audio.
        #[test]
        fn a_full_ring_drops_the_tail_and_keeps_what_was_already_there() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            let cap = wire::RING_FRAMES as usize;
            let over = cap + 500;
            assert_eq!(
                driver.write(&stereo_ramp(0, over), over),
                cap,
                "only the capacity fits"
            );
            // Not one more frame, either.
            assert_eq!(driver.write(&stereo_ramp(cap as u64, 10), 10), 0);

            let mut dst = vec![0.0f32; cap * 2];
            assert_eq!(r.read_spk(0, &mut dst, cap), cap);
            assert_stereo_ramp(&dst, 0, cap);
        }

        /// A consumer that stalled more than a capacity behind must resume at
        /// the NEWEST full window, not replay half a second of stale audio.
        ///
        /// The indices are planted by hand because nothing in the shipped
        /// producer can reach this state — `write` refuses to run more than a
        /// capacity ahead. The state is real all the same: it is what a driver
        /// that recreated its ring leaves behind under a live mapping.
        #[test]
        fn a_consumer_more_than_a_capacity_behind_jumps_to_the_newest_window() {
            let (mut spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            let cap = wire::RING_FRAMES as usize;
            // Fill, drain 4000, refill 4000: the ring now physically holds
            // absolute frames 4000..28000 and write_idx is 28000.
            assert_eq!(driver.write(&stereo_ramp(0, cap), cap), cap);
            let mut sink = vec![0.0f32; 4000 * 2];
            assert_eq!(r.read_spk(0, &mut sink, 4000), 4000);
            assert_eq!(driver.write(&stereo_ramp(cap as u64, 4000), 4000), 4000);

            // Now pretend we never consumed anything at all.
            let (w, _) = spk.indices();
            assert_eq!(w, (cap + 4000) as u64);
            spk.set_indices(w, 0);

            let mut dst = vec![0.0f32; 480 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 480), 480);
            // The oldest frame still IN the ring is w - capacity = 4000.
            assert_stereo_ramp(&dst, 4000, 480);

            // And the read index landed on the newest window, so the next read
            // continues from there rather than crawling forward from 0.
            let (_, rd_after) = spk.indices();
            assert_eq!(rd_after, 4480);
        }

        /// The mixer's `peek` / `advance` split: peeking twice must hand over
        /// the SAME audio, and the base from the peek is what advance commits.
        ///
        /// Recomputing the index inside `advance` instead of using the base is
        /// the defect this shape exists to prevent — whatever the producer
        /// pushed between the two calls would be counted as consumed and never
        /// heard.
        #[test]
        fn peek_does_not_consume_and_advance_commits_exactly_what_was_peeked() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            driver.write(&stereo_ramp(0, 300), 300);

            let mut a = vec![0.0f32; 100 * 2];
            let (n, base) = r.peek_spk(0, &mut a, 100).expect("attached");
            assert_eq!(n, 100);
            assert_stereo_ramp(&a, 0, 100);
            assert_eq!(r.spk_readable(0), Some((300, wire::RING_FRAMES)), "peek consumed nothing");

            let mut b = vec![0.0f32; 100 * 2];
            let (n2, base2) = r.peek_spk(0, &mut b, 100).unwrap();
            assert_eq!((n2, base2), (100, base), "a second peek sees the same window");
            assert_eq!(a, b);

            // The producer pushes MORE between the peek and the advance. The
            // committed position must still be base + 100.
            driver.write(&stereo_ramp(300, 50), 50);
            r.advance_spk(0, base, 100);
            assert_eq!(r.spk_readable(0), Some((250, wire::RING_FRAMES)));

            let mut c = vec![0.0f32; 250 * 2];
            let (n3, _) = r.peek_spk(0, &mut c, 250).unwrap();
            assert_eq!(n3, 250);
            assert_stereo_ramp(&c, 100, 250);
        }

        /// The peek/advance split in the ONE state where the base and
        /// `read_idx` actually differ — and therefore the only state in which
        /// passing the base can be shown to matter.
        ///
        /// THE TEST ABOVE DOES NOT COVER THIS, and believing it did is how the
        /// defect would ship. On a healthy ring `effective == read_idx`, so an
        /// `advance` that recomputed from `read_idx` gives the identical
        /// answer and the assertion passes with the bug in place — measured, by
        /// injecting exactly that change. The two diverge only once the
        /// consumer is more than a capacity behind: the peek jumps forward to
        /// the newest window, and an advance that started from `read_idx`
        /// instead would leave the reader permanently behind. Every peek would
        /// jump, every advance would crawl, and the backlog would never clear.
        ///
        /// The other wrong implementation — recomputing `effective` from the
        /// CURRENT `write_idx` — is covered by the second half: the producer
        /// moves on between the peek and the advance, and the committed
        /// position must still be the one that was peeked, not a newer window
        /// whose frames nobody has seen.
        #[test]
        fn advance_commits_the_peeked_base_and_not_wherever_read_idx_happens_to_be() {
            let (mut spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            let cap = wire::RING_FRAMES as usize;
            // The ring physically holds absolute frames 4000..28000.
            assert_eq!(driver.write(&stereo_ramp(0, cap), cap), cap);
            let mut sink = vec![0.0f32; 4000 * 2];
            assert_eq!(r.read_spk(0, &mut sink, 4000), 4000);
            assert_eq!(driver.write(&stereo_ramp(cap as u64, 4000), 4000), 4000);
            // ...and now we are a whole capacity behind, having "consumed"
            // nothing.
            spk.set_indices(28_000, 0);

            let mut buf = vec![0.0f32; 480 * 2];
            let (n, base) = r.peek_spk(0, &mut buf, 480).expect("attached");
            assert_eq!(n, 480);
            assert_eq!(base, 4_000, "the peek jumped, so the base is NOT read_idx (which is 0)");
            assert_stereo_ramp(&buf, 4_000, 480);
            assert_eq!(spk.indices().1, 0, "peek still consumed nothing");

            // The producer keeps running between the peek and the advance.
            spk.set_indices(29_000, 0);

            r.advance_spk(0, base, 480);
            assert_eq!(
                spk.indices().1,
                4_480,
                "the commit must be the peeked base + 480 — neither read_idx + 480 (480) \
                 nor a freshly recomputed window (5480, which would skip 1000 frames \
                 nobody has seen)"
            );
        }

        /// 治法 A: drop frames without moving a sample, and the audio that
        /// follows is the audio that was NOT dropped.
        #[test]
        fn dropping_frames_skips_them_and_leaves_the_rest_intact() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            spk.view(&rep).write(&stereo_ramp(0, 200), 200);

            assert_eq!(r.drop_spk(0, 60), 60);
            assert_eq!(r.spk_readable(0), Some((140, wire::RING_FRAMES)));

            let mut dst = vec![0.0f32; 140 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 140), 140);
            assert_stereo_ramp(&dst, 60, 140, /* the first 60 are gone */);

            // Asking for more than is there drops only what is there.
            spk.view(&rep).write(&stereo_ramp(200, 10), 10);
            assert_eq!(r.drop_spk(0, 1_000), 10);
            assert_eq!(r.spk_readable(0), Some((0, wire::RING_FRAMES)));
        }

        /// The flush the bind path arms: everything queued goes, and what the
        /// producer writes AFTERWARDS is what the next peer hears. Replaying
        /// the previous tenant's audio into a freshly bound slot is the exact
        /// defect this exists to prevent.
        #[test]
        fn flushing_the_speaker_consumer_discards_the_backlog_only() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            let driver = spk.view(&rep);

            driver.write(&stereo_ramp(0, 500), 500);
            r.flush_spk_consumer(0);
            assert_eq!(r.spk_readable(0), Some((0, wire::RING_FRAMES)));

            driver.write(&stereo_ramp(9000, 64), 64);
            let mut dst = vec![0.0f32; 64 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 64), 64);
            assert_stereo_ramp(&dst, 9000, 64, /* not the flushed 0..500 */);
        }

        /// Depth is REPORTED, never consumed. Reading it a hundred times must
        /// leave the audio exactly where it was.
        #[test]
        fn observing_depth_does_not_consume_in_either_direction() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();

            spk.view(&rep).write(&stereo_ramp(0, 700), 700);
            r.write_mic(0, &mono_ramp(0, 300));
            for _ in 0..100 {
                assert_eq!(r.spk_readable(0), Some((700, wire::RING_FRAMES)));
                assert_eq!(r.mic_occupied(0), Some((300, wire::RING_FRAMES)));
            }

            // Still all there, still in order.
            let mut dst = vec![0.0f32; 700 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 700), 700);
            assert_stereo_ramp(&dst, 0, 700);

            // And the mic depth falls as the DRIVER drains it, which is what
            // makes it a backlog measurement rather than a write counter.
            let mut mdst = vec![0.0f32; 120];
            assert_eq!(mic.view(&rep).read(&mut mdst, 120), 120);
            assert_eq!(mdst, mono_ramp(0, 120));
            assert_eq!(r.mic_occupied(0), Some((180, wire::RING_FRAMES)));
        }

        /// A partial read fills what it can and reports it, so the caller knows
        /// how much to zero-fill.
        #[test]
        fn a_short_ring_hands_over_what_it_has_and_says_so() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let r = WinRings::new();
            r.attach(&rep).unwrap();
            spk.view(&rep).write(&stereo_ramp(0, 37), 37);

            let mut dst = vec![-1.0f32; 480 * 2];
            assert_eq!(r.read_spk(0, &mut dst, 480), 37);
            assert_stereo_ramp(&dst[..37 * 2], 0, 37);
            assert_eq!(dst[37 * 2], -1.0, "the rest is the caller's to fill");
        }

        /// With no driver attached every method reports ABSENCE, and the two
        /// depth readings report `None` rather than 0 — "this stage does not
        /// exist" is not "this stage is empty", and a status view that showed
        /// 0 ms would be inventing a measurement.
        #[test]
        fn a_detached_ring_set_reports_absence_and_never_a_zero_measurement() {
            let r = WinRings::new();
            let mut dst = vec![-1.0f32; 480 * 2];

            assert_eq!(r.read_spk(0, &mut dst, 480), 0);
            assert_eq!(dst[0], -1.0);
            assert_eq!(r.write_mic(0, &mono_ramp(0, 10)), None);
            assert_eq!(r.peek_spk(0, &mut dst, 480), None);
            assert_eq!(r.drop_spk(0, 480), 0);
            assert_eq!(r.spk_readable(0), None);
            assert_eq!(r.mic_occupied(0), None);
            r.flush_spk_consumer(0); // must not panic
            r.advance_spk(0, 0, 480); // must not panic
        }

        /// The handover `Session::open` -> `platform::Rings` is a MOVE: after
        /// it exactly one `WinRings` names the pages, so a later detach on the
        /// session's own copy cannot pull the mapping out from under the mixer.
        #[test]
        fn move_into_transfers_the_mapping_and_leaves_the_source_detached() {
            let (spk, mic) = (FakeRing::spk(), FakeRing::mic());
            let rep = good_reply(&[&spk, &mic]);
            let src = WinRings::new();
            src.attach(&rep).unwrap();
            spk.view(&rep).write(&stereo_ramp(4242, 64), 64);

            let dst_rings = WinRings::new();
            src.move_into(&dst_rings);

            assert!(!src.attached(), "the source gave the mapping away");
            assert_eq!(src.spk_readable(0), None);
            assert!(dst_rings.attached());

            let mut dst = vec![0.0f32; 64 * 2];
            assert_eq!(dst_rings.read_spk(0, &mut dst, 64), 64);
            assert_stereo_ramp(&dst, 4242, 64);
        }
    }
}

// ---------------------------------------------------------------- transport

#[cfg(windows)]
pub mod transport {
    //! `CreateFileW` / `DeviceIoControl`, declared by hand.

    use super::wire;
    use anyhow::{anyhow, Result};
    use std::ffi::c_void;

    pub const INVALID_HANDLE_VALUE: isize = -1;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    /// THE one that stops a deadlock. See the module docs.
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;

    pub const ERROR_FILE_NOT_FOUND: u32 = 2;
    pub const ERROR_PATH_NOT_FOUND: u32 = 3;
    pub const ERROR_ACCESS_DENIED: u32 = 5;
    pub const ERROR_SHARING_VIOLATION: u32 = 32;
    pub const ERROR_BUSY: u32 = 170;
    pub const ERROR_OPERATION_ABORTED: u32 = 995;
    /// What `GetOverlappedResult` reports for a request that has NOT completed
    /// when `bWait` is FALSE. NOT the same number as ERROR_IO_PENDING, which is
    /// what `DeviceIoControl` reports at issue time — confusing the two makes a
    /// still-outstanding IRP look like a failure.
    pub const ERROR_IO_INCOMPLETE: u32 = 996;
    pub const ERROR_IO_PENDING: u32 = 997;

    #[repr(C)]
    #[derive(Default)]
    pub struct Overlapped {
        pub internal: usize,
        pub internal_high: usize,
        pub offset: u32,
        pub offset_high: u32,
        pub event: isize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sa: *mut c_void,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn DeviceIoControl(
            h: isize,
            code: u32,
            inbuf: *const c_void,
            inlen: u32,
            outbuf: *mut c_void,
            outlen: u32,
            returned: *mut u32,
            ov: *mut Overlapped,
        ) -> i32;
        fn GetOverlappedResult(h: isize, ov: *mut Overlapped, n: *mut u32, wait: i32) -> i32;
        fn CreateEventW(sa: *mut c_void, manual: i32, initial: i32, name: *const u16) -> isize;
        fn WaitForSingleObject(h: isize, ms: u32) -> u32;
        fn CancelIoEx(h: isize, ov: *mut Overlapped) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn GetLastError() -> u32;
        fn GetCurrentProcessId() -> u32;
    }

    /// An owned kernel handle. `Drop` is the only place `CloseHandle` is
    /// called, so a bail-out on any error path cannot leak one.
    pub struct Handle(isize);

    impl Handle {
        pub fn raw(&self) -> isize {
            self.0
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE && self.0 != 0 {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A manual-reset event, one per in-flight request.
    struct Event(isize);

    impl Event {
        fn new() -> Result<Event> {
            let h = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
            if h == 0 {
                return Err(anyhow!("CreateEventW failed: {}", unsafe { GetLastError() }));
            }
            Ok(Event(h))
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The data plane's wake event: handed to the driver ONCE in
    /// `IOCTL_MAP_RINGS` and signalled from the WaveRT timer DPC whenever it
    /// moved audio, so the mixer can wait instead of poll.
    ///
    /// AUTO-RESET, unlike [`Event`]. A manual-reset event here would stay
    /// signalled after the first DPC and turn every subsequent wait into a
    /// spin; auto-reset is consumed by the wait that observes it, which is what
    /// makes "wait for the next tick's worth of audio" expressible at all.
    ///
    /// The driver takes its own reference to the underlying object, so closing
    /// this handle does not invalidate the driver's — but the daemon may want
    /// to wait on it for as long as the mapping lives, so the `Session` holds
    /// it and `Drop` is the only `CloseHandle`.
    pub struct WakeEvent(isize);

    impl WakeEvent {
        pub fn new() -> Result<WakeEvent> {
            let h = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
            if h == 0 {
                return Err(anyhow!("CreateEventW failed: {}", unsafe { GetLastError() }));
            }
            Ok(WakeEvent(h))
        }

        /// What goes in `AH_MAP_REQUEST::wake_event`. A HANDLE is process-
        /// relative and the driver resolves it against the CALLER's handle
        /// table inside the IOCTL, which is why it may be sent as a bare
        /// number.
        pub fn raw(&self) -> u64 {
            self.0 as usize as u64
        }

        /// Blocks until the driver signals, or the timeout expires. `true` when
        /// it was signalled.
        pub fn wait(&self, timeout_ms: u32) -> bool {
            unsafe { WaitForSingleObject(self.0, timeout_ms) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for WakeEvent {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub fn current_pid() -> u32 {
        unsafe { GetCurrentProcessId() }
    }

    /// Why `open_control` failed.
    ///
    /// Classified rather than collapsed into one error string because the
    /// daemon has to answer a question the text cannot: is the DRIVER there?
    /// "Not loaded" is the ordinary state of every machine without the driver
    /// installed and must stay silent; "refused" and "busy" both mean the
    /// driver is present and something is wrong, which a status line has to
    /// show.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OpenFail {
        NotLoaded,
        Refused,
        Busy,
        Other(u32),
    }

    impl OpenFail {
        pub fn driver_present(self) -> bool {
            !matches!(self, OpenFail::NotLoaded)
        }

        pub fn text(self) -> String {
            match self {
                OpenFail::NotLoaded => {
                    r"the AudioHub driver is not loaded (\\.\AudioHubVadCtl does not exist)".into()
                }
                OpenFail::Refused => {
                    "the driver refused this process: not an interactive logon, or this \
                     executable's path does not match AudioHubDaemonImage in the device \
                     software key"
                        .into()
                }
                OpenFail::Busy => {
                    "another daemon already holds the driver's control session".into()
                }
                OpenFail::Other(e) => {
                    format!(r"CreateFileW(\\.\AudioHubVadCtl) failed: error {e}")
                }
            }
        }
    }

    /// Opens the control device.
    ///
    /// `FILE_FLAG_OVERLAPPED` is NOT optional: without it the I/O manager
    /// serialises this handle, and the pending `CONTROL_PEND` IRP would queue
    /// every later `BIND_SET` behind itself permanently.
    pub fn open_control() -> std::result::Result<Handle, OpenFail> {
        let path = wire::ctl_path_utf16();
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                0,
            )
        };
        if h == INVALID_HANDLE_VALUE || h == 0 {
            return Err(match unsafe { GetLastError() } {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => OpenFail::NotLoaded,
                ERROR_ACCESS_DENIED => OpenFail::Refused,
                ERROR_BUSY | ERROR_SHARING_VIOLATION => OpenFail::Busy,
                e => OpenFail::Other(e),
            });
        }
        Ok(Handle(h))
    }

    /// One synchronous round trip on an OVERLAPPED handle: issue, wait on this
    /// request's own event, collect.
    ///
    /// Each call brings its own `OVERLAPPED` and its own event, which is what
    /// makes concurrent requests on one handle safe — and what keeps this
    /// looking synchronous to the layer above while the long-pending
    /// `CONTROL_PEND` stays outstanding beside it.
    pub fn ioctl(h: &Handle, code: u32, input: &[u8], output: &mut [u8], timeout_ms: u32) -> Result<u32> {
        let ev = Event::new()?;
        let mut ov = Overlapped {
            event: ev.0,
            ..Default::default()
        };
        let mut returned: u32 = 0;

        let ok = unsafe {
            DeviceIoControl(
                h.0,
                code,
                if input.is_empty() {
                    std::ptr::null()
                } else {
                    input.as_ptr() as *const c_void
                },
                input.len() as u32,
                if output.is_empty() {
                    std::ptr::null_mut()
                } else {
                    output.as_mut_ptr() as *mut c_void
                },
                output.len() as u32,
                &mut returned,
                &mut ov,
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(anyhow!("DeviceIoControl(0x{code:08X}) failed: error {err}"));
            }
            match unsafe { WaitForSingleObject(ev.0, timeout_ms) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => {
                    // The IRP is still with the driver and it owns `ov` and the
                    // buffers. Cancelling and then WAITING for the cancellation
                    // to land is mandatory: returning while the kernel still
                    // holds pointers into our stack is a use-after-free that
                    // corrupts whatever the next frame puts there.
                    unsafe { CancelIoEx(h.0, &mut ov) };
                    unsafe { WaitForSingleObject(ev.0, u32::MAX) };
                    return Err(anyhow!("DeviceIoControl(0x{code:08X}) timed out"));
                }
                other => {
                    unsafe { CancelIoEx(h.0, &mut ov) };
                    unsafe { WaitForSingleObject(ev.0, u32::MAX) };
                    return Err(anyhow!("WaitForSingleObject failed: {other}"));
                }
            }
            let got = unsafe { GetOverlappedResult(h.0, &mut ov, &mut returned, 1) };
            if got == 0 {
                let err = unsafe { GetLastError() };
                return Err(anyhow!(
                    "GetOverlappedResult(0x{code:08X}) failed: error {err}"
                ));
            }
        }

        Ok(returned)
    }

    /// Issues `CONTROL_PEND` and leaves it outstanding. Returns the machinery
    /// the caller must keep alive until the IRP completes.
    ///
    /// M6-2 has nothing to report over this, but the call exists now so that
    /// adding volume or IO state later does not force a protocol version bump.
    pub struct PendingCall {
        _event: Event,
        ov: Box<Overlapped>,
        pub buf: Box<[u8; wire::CONTROL_EVENT_BYTES]>,
        /// A copy of the device handle, so `Drop` can cancel WITHOUT the caller
        /// having to remember to. Never closed here — `Session` owns it, and a
        /// `PendingCall` cannot outlive the `Session` that made it.
        h: isize,
    }

    impl Drop for PendingCall {
        /// Cancel, then WAIT for the cancellation to land.
        ///
        /// While the IRP is outstanding the kernel holds pointers into `ov` and
        /// `buf`; freeing them first is a use-after-free that corrupts whatever
        /// the allocator hands out next — a crash with no relationship to this
        /// code. `ov` and `buf` are boxed for exactly this reason: their
        /// addresses must not move while the driver holds them.
        fn drop(&mut self) {
            unsafe { CancelIoEx(self.h, self.ov.as_mut()) };
            let mut n: u32 = 0;
            unsafe { GetOverlappedResult(self.h, self.ov.as_mut(), &mut n, 1) };
        }
    }

    impl PendingCall {
        pub fn issue(h: &Handle) -> Result<Option<PendingCall>> {
            let ev = Event::new()?;
            let mut ov = Box::new(Overlapped {
                event: ev.0,
                ..Default::default()
            });
            let mut buf = Box::new([0u8; wire::CONTROL_EVENT_BYTES]);
            let mut returned: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    h.0,
                    wire::IOCTL_CONTROL_PEND,
                    std::ptr::null(),
                    0,
                    buf.as_mut_ptr() as *mut c_void,
                    wire::CONTROL_EVENT_BYTES as u32,
                    &mut returned,
                    ov.as_mut() as *mut Overlapped,
                )
            };
            if ok != 0 {
                // Completed inline — nothing is outstanding.
                return Ok(None);
            }
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(anyhow!("IOCTL_CONTROL_PEND failed: error {err}"));
            }
            Ok(Some(PendingCall { _event: ev, ov, buf, h: h.raw() }))
        }

        /// Non-blocking poll. `Ok(None)` = still pending.
        ///
        /// Both 996 and 997 count as "still pending". `GetOverlappedResult`
        /// with `bWait = FALSE` reports ERROR_IO_INCOMPLETE (996), not the
        /// ERROR_IO_PENDING (997) that `DeviceIoControl` returned at issue
        /// time. Accepting only 997 turned every poll of a healthy outstanding
        /// IRP into an error — and the caller reacts to an error by dropping
        /// this object, i.e. by freeing buffers the kernel is still holding.
        pub fn poll(&mut self, h: &Handle) -> Result<Option<wire::ControlEvent>> {
            let mut n: u32 = 0;
            let got = unsafe { GetOverlappedResult(h.raw(), self.ov.as_mut(), &mut n, 0) };
            if got == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_IO_INCOMPLETE || err == ERROR_IO_PENDING {
                    return Ok(None);
                }
                if err == ERROR_OPERATION_ABORTED {
                    return Err(anyhow!("the driver cancelled the pending control call"));
                }
                return Err(anyhow!("pending control call failed: error {err}"));
            }
            if n as usize != wire::CONTROL_EVENT_BYTES {
                return Err(anyhow!("pending control call returned {n} bytes"));
            }
            wire::decode_control_event(self.buf.as_ref())
                .map(Some)
                .ok_or_else(|| anyhow!("undecodable control event"))
        }

    }
}

// ---------------------------------------------------------------- session

#[cfg(windows)]
pub mod session {
    //! One control session: open, handshake, bind, query.
    //!
    //! Deliberately holds no threads and no interior mutability — the caller
    //! (`halbridge::platform`) owns the service thread, exactly as the macOS
    //! side does.

    use super::rings;
    use super::transport::{self, Handle, OpenFail, PendingCall, WakeEvent};
    use super::wire;
    use anyhow::{anyhow, Result};

    /// Control-plane calls are a few per second and the driver answers them
    /// with a mutex held at PASSIVE_LEVEL, so a second is generous. A timeout
    /// at all is the point: a driver that wedges must not wedge the daemon.
    const IOCTL_TIMEOUT_MS: u32 = 1_000;

    /// Why a session could not be established, keeping the one bit the caller
    /// cannot recover from the text: whether the driver is there at all.
    #[derive(Debug, Clone)]
    pub enum SessionError {
        Open(OpenFail),
        /// The device opened but the handshake did not complete. The driver is
        /// definitely present.
        Handshake(String),
        /// The driver speaks a different protocol. Carried separately because
        /// the daemon reports the driver's number in `daemon.status`.
        Version(u32),
    }

    impl SessionError {
        pub fn driver_present(&self) -> bool {
            match self {
                SessionError::Open(f) => f.driver_present(),
                _ => true,
            }
        }

        pub fn driver_protocol(&self) -> Option<u32> {
            match self {
                SessionError::Version(v) => Some(*v),
                _ => None,
            }
        }

        pub fn text(&self) -> String {
            match self {
                SessionError::Open(f) => f.text(),
                SessionError::Handshake(s) => s.clone(),
                SessionError::Version(v) => format!(
                    "driver speaks protocol {v}, this daemon speaks {}",
                    wire::PROTOCOL_VERSION
                ),
            }
        }
    }

    pub struct Session {
        /// DECLARED FIRST ON PURPOSE. Rust drops a struct's fields in
        /// declaration order, and the outstanding inverted-call IRP must be
        /// cancelled and waited for BEFORE its handle is closed — the kernel is
        /// holding pointers into this object's buffers until then. With
        /// `handle` first, `CloseHandle` would run while that IRP was still
        /// live.
        pending: Option<PendingCall>,
        /// The mapped data plane, empty until `MAP_RINGS` succeeds and empty
        /// again the moment `platform::attach` takes it (see
        /// [`rings::WinRings::move_into`]).
        ///
        /// DECLARED BEFORE `handle` for the same reason `pending` is: the
        /// mapping's lifetime is the control handle's, because the driver tears
        /// it down in `IRP_MJ_CLEANUP`. Nothing may still be inside those pages
        /// when `CloseHandle` runs.
        pub rings: rings::WinRings,
        /// The event the driver signals from its timer DPC after it moved
        /// audio. An ACCELERATOR: the mixer runs on its own 10 ms tick and a
        /// session whose event could not be created is fully functional.
        wake: Option<WakeEvent>,
        handle: Handle,
        pub session_id: u64,
        pub slot_count: u8,
        pub driver_protocol: u32,
        pub client_check: u32,
        pub caps: u32,
        /// Why this session carries no audio, or `None` when it does.
        ///
        /// A named reason rather than a bare bool, and reported rather than
        /// logged once: a control plane that binds devices which then stay
        /// silent is exactly the shape of failure this protocol keeps being
        /// versioned to make impossible to hide.
        pub dataplane_off: Option<String>,
    }

    impl Session {
        /// Opens the device and completes the handshake.
        pub fn open() -> std::result::Result<Session, SessionError> {
            let handle = transport::open_control().map_err(SessionError::Open)?;

            let req = wire::encode_hello_request(transport::current_pid());
            let mut out = [0u8; wire::HELLO_REPLY_BYTES];
            let n = transport::ioctl(&handle, wire::IOCTL_HELLO, &req, &mut out, IOCTL_TIMEOUT_MS)
                .map_err(|e| SessionError::Handshake(format!("{e:#}")))?;
            if n as usize != wire::HELLO_REPLY_BYTES {
                return Err(SessionError::Handshake(format!("hello reply was {n} bytes")));
            }
            let rep = wire::decode_hello_reply(&out)
                .ok_or_else(|| SessionError::Handshake("undecodable hello reply".into()))?;

            if rep.status == wire::STATUS_BAD_VERSION
                || rep.protocol_version != wire::PROTOCOL_VERSION
            {
                return Err(SessionError::Version(rep.protocol_version));
            }
            if rep.status != wire::STATUS_OK {
                return Err(SessionError::Handshake(format!(
                    "driver refused the handshake: {}",
                    wire::status_label(rep.status)
                )));
            }
            if rep.slot_count == 0 {
                return Err(SessionError::Handshake("driver reports zero slots".into()));
            }

            // The DRIVER's number, capped by ours. A driver built with a
            // smaller pool must show up as a visible capacity limit, not as a
            // refused handshake.
            let slot_count = (rep.slot_count as usize).min(wire::MAX_SLOTS) as u8;

            let mut s = Session {
                handle,
                session_id: rep.session_id,
                slot_count,
                driver_protocol: rep.protocol_version,
                client_check: rep.client_check,
                caps: rep.caps,
                pending: None,
                rings: rings::WinRings::new(),
                wake: None,
                dataplane_off: None,
            };

            // THE DATA PLANE. Two outcomes, and the difference between them is
            // deliberate:
            //
            //   * the driver does not claim AH_CAP_DATAPLANE — a CLEAN DEGRADE.
            //     That is an honest, older driver (every M6-2 build says this),
            //     the control plane it does implement works perfectly, and a
            //     daemon that refused to talk to it would take away working
            //     device publication to punish a missing feature. The reason is
            //     named so a status view can say WHY there is no audio.
            //
            //   * the driver claims it and then cannot deliver it — FATAL. A
            //     geometry that disagrees with this build is not something to
            //     adapt to: the media plane above is fixed at 48 kHz / 2ch out
            //     / 1ch in all the way to the socket, and the arithmetic that
            //     indexes these rings is bounded by these constants. The only
            //     available "adaptation" is to memcpy past the end of a kernel
            //     mapping. Same argument as the protocol version's equality
            //     test, and the same remedy: refuse loudly rather than publish
            //     devices that carry the wrong audio.
            if rep.has_dataplane() {
                s.map_rings()
                    .map_err(|e| SessionError::Handshake(format!("{e:#}")))?;
            } else {
                s.dataplane_off = Some(
                    "the driver does not implement the data plane (AH_CAP_DATAPLANE is \
                     clear), so its devices will publish but carry no audio"
                        .to_string(),
                );
            }

            // Best effort: a driver that refuses the inverted call is still
            // perfectly usable for binding.
            s.pending = PendingCall::issue(&s.handle).unwrap_or(None);

            Ok(s)
        }

        /// `IOCTL_MAP_RINGS`: the data plane's one and only system call.
        ///
        /// After this returns, audio moves with two memcpys and two 64-bit
        /// stores per tick. Nothing on the audio path calls into the kernel
        /// again.
        fn map_rings(&mut self) -> Result<()> {
            // Best effort, and NOT an error when it fails. The event lets the
            // driver's DPC wake the mixer instead of making it wait for its own
            // tick; the tick is what actually moves the audio. Trading a
            // working data plane for a latency optimisation would be the wrong
            // way round.
            let wake = WakeEvent::new().ok();
            let req =
                wire::encode_map_request(self.session_id, wake.as_ref().map_or(0, |w| w.raw()));

            let mut out = [0u8; wire::MAP_REPLY_BYTES];
            let n = transport::ioctl(
                &self.handle,
                wire::IOCTL_MAP_RINGS,
                &req,
                &mut out,
                IOCTL_TIMEOUT_MS,
            )?;
            if n as usize != wire::MAP_REPLY_BYTES {
                return Err(anyhow!("the map reply was {n} bytes"));
            }
            let rep =
                wire::decode_map_reply(&out).ok_or_else(|| anyhow!("undecodable map reply"))?;
            if rep.status != wire::STATUS_OK {
                return Err(anyhow!(
                    "the driver refused to map its rings: {}",
                    wire::status_label(rep.status)
                ));
            }

            // Validates every geometry field against this build's constants and
            // every ring's header against the reply, then installs all of them
            // or none.
            let slots = self.rings.attach(&rep)?;

            // The two halves of the handshake have to agree about capacity. A
            // driver that reports more slots than it mapped rings for would let
            // the daemon bind a peer to a slot whose devices appear in the
            // system list and are permanently silent — and nothing downstream
            // could tell that apart from a peer that is simply not sending.
            if slots < self.slot_count as usize {
                self.rings.detach();
                return Err(anyhow!(
                    "the driver reported {} slots but mapped rings for only {slots}",
                    self.slot_count
                ));
            }

            self.wake = wake;
            Ok(())
        }

        /// True once the rings are mapped. False on a clean degrade, with
        /// [`Session::dataplane_off`] carrying the reason.
        pub fn has_dataplane(&self) -> bool {
            self.rings.attached()
        }

        /// [`wire::CAP_VOLUME`]: the endpoints carry a hardware volume node, so
        /// the samples in the rings are FULL SCALE.
        ///
        /// The caller must not relay the user's slider to the peer while this
        /// is false — the audio engine's own APO has already applied it
        /// upstream of the ring, and applying it again on the peer's real
        /// device attenuates by the square of the setting.
        pub fn has_volume(&self) -> bool {
            self.caps & wire::CAP_VOLUME != 0
        }

        /// `IOCTL_NOTIFY`: the far peer's real device moved, so make the
        /// virtual endpoint's volume node follow.
        ///
        /// Returns `applied`: true when the driver's stored level actually
        /// changed and it raised `KSEVENT_CONTROL_CHANGE`, false when the value
        /// was already this one. LOOP SUPPRESSION IS THE DRIVER'S JOB — without
        /// it every sync would bounce (daemon sets, driver events, daemon reads
        /// its own echo and sets again) — and this bool is the only way to tell
        /// a suppressed echo from a notify that was ignored.
        ///
        /// Touches no sample. The rings carry full scale; the far side does the
        /// attenuating.
        pub fn notify(
            &self,
            slot: u8,
            generation: u32,
            input: bool,
            muted: bool,
            scalar: f32,
        ) -> Result<bool> {
            let req = wire::encode_notify_request(
                self.session_id,
                slot,
                generation,
                input,
                muted,
                scalar,
            );
            let mut out = [0u8; wire::NOTIFY_REPLY_BYTES];
            let n = transport::ioctl(
                &self.handle,
                wire::IOCTL_NOTIFY,
                &req,
                &mut out,
                IOCTL_TIMEOUT_MS,
            )?;
            if n as usize != wire::NOTIFY_REPLY_BYTES {
                return Err(anyhow!("the notify reply was {n} bytes"));
            }
            let rep = wire::decode_notify_reply(&out)
                .ok_or_else(|| anyhow!("undecodable notify reply"))?;
            if rep.status != wire::STATUS_OK {
                return Err(anyhow!(
                    "the driver refused the volume notify: {}",
                    wire::status_label(rep.status)
                ));
            }
            Ok(rep.applied())
        }

        /// True when the driver could only enforce the DACL — i.e. nobody has
        /// written `AudioHubDaemonImage` into the device software key. Surfaced
        /// rather than silently accepted: a degraded check that nobody can see
        /// is the same as no check.
        pub fn identity_check_degraded(&self) -> bool {
            self.client_check < wire::CLIENT_CHECK_IMAGEPATH
        }

        /// `peer_display` is the BARE disambiguated peer name ("WIN-30"). The
        /// `AudioHub – ` prefix is added by `wire::encode_bind_request`; adding
        /// it here as well would double it.
        pub fn bind_set(
            &self,
            slot: u8,
            peer_key: &str,
            peer_display: &str,
            online: bool,
        ) -> Result<wire::BindReply> {
            self.bind_set_with(slot, peer_key, peer_display, online, 0)
        }

        /// `debug_flags` is `wire::BINDFLAG_FAIL_*` / `SKIP_ROLLBACK`, and is
        /// ALWAYS 0 on the daemon's own path — only `probe winvad` and the
        /// regression harness pass anything else. Kept as a separate entry
        /// point so that a stray argument cannot reach production code by
        /// being defaulted wrongly at one call site.
        pub fn bind_set_with(
            &self,
            slot: u8,
            peer_key: &str,
            peer_display: &str,
            online: bool,
            debug_flags: u32,
        ) -> Result<wire::BindReply> {
            let req = wire::BindRequest {
                op: wire::BIND_SET,
                slot,
                flags: (if online { wire::BINDFLAG_ONLINE } else { 0 })
                    | (debug_flags & wire::BINDFLAG_DEBUG_MASK),
                // Set carries 0: the DRIVER allocates the generation and
                // returns it in the reply.
                generation: 0,
                session_id: self.session_id,
                peer_key,
                peer_display,
            };
            let buf = wire::encode_bind_request(&req)
                .map_err(|e| anyhow!("cannot encode bind set: {e:?}"))?;
            self.bind_call(wire::IOCTL_BIND_SET, &buf)
        }

        pub fn bind_clear(&self, slot: u8, generation: u32) -> Result<wire::BindReply> {
            self.bind_clear_with(slot, generation, 0)
        }

        pub fn bind_clear_with(
            &self,
            slot: u8,
            generation: u32,
            debug_flags: u32,
        ) -> Result<wire::BindReply> {
            // A Clear carries no name, but the peer key field still has to pass
            // the driver's whitelist, so send a well-formed placeholder. The
            // driver identifies the slot by index and generation, never by key,
            // on this path.
            let req = wire::BindRequest {
                op: wire::BIND_CLEAR,
                slot,
                flags: debug_flags & wire::BINDFLAG_DEBUG_MASK,
                generation,
                session_id: self.session_id,
                peer_key: "0000000000000000",
                peer_display: "",
            };
            let buf = wire::encode_bind_request(&req)
                .map_err(|e| anyhow!("cannot encode bind clear: {e:?}"))?;
            self.bind_call(wire::IOCTL_BIND_CLEAR, &buf)
        }

        fn bind_call(&self, code: u32, buf: &[u8]) -> Result<wire::BindReply> {
            let mut out = [0u8; wire::BIND_REPLY_BYTES];
            let n = transport::ioctl(&self.handle, code, buf, &mut out, IOCTL_TIMEOUT_MS)?;
            if n as usize != wire::BIND_REPLY_BYTES {
                return Err(anyhow!("bind reply was {n} bytes"));
            }
            wire::decode_bind_reply(&out).ok_or_else(|| anyhow!("undecodable bind reply"))
        }

        /// The driver's own account of its slots. This is what makes a slot the
        /// daemon leaked (Set with no Clear, then a restart) detectable at all:
        /// on macOS the equivalent state arrives pushed as `CTL_BIND_STATE`.
        pub fn query_slots(&self) -> Result<wire::QuerySlotsReply> {
            let mut out = [0u8; wire::QUERY_SLOTS_REPLY_BYTES];
            let n = transport::ioctl(
                &self.handle,
                wire::IOCTL_QUERY_SLOTS,
                &[],
                &mut out,
                IOCTL_TIMEOUT_MS,
            )?;
            if n as usize != wire::QUERY_SLOTS_REPLY_BYTES {
                return Err(anyhow!("query reply was {n} bytes"));
            }
            wire::decode_query_slots_reply(&out).ok_or_else(|| anyhow!("undecodable query reply"))
        }

        /// Drains whatever the driver has pushed, and re-arms. Empty through
        /// M6-2.
        pub fn poll_events(&mut self) -> Vec<wire::ControlEvent> {
            let mut out = Vec::new();
            loop {
                let Some(p) = self.pending.as_mut() else { return out };
                match p.poll(&self.handle) {
                    Ok(Some(ev)) => {
                        out.push(ev);
                        self.pending = PendingCall::issue(&self.handle).unwrap_or(None);
                    }
                    Ok(None) => return out,
                    Err(_) => {
                        self.pending = None;
                        return out;
                    }
                }
            }
        }
    }

    // No `Drop for Session`: `PendingCall` cancels and waits in its own Drop,
    // and the field order above guarantees that runs before the handle closes.
    // A second cancel here would only re-cancel an IRP that has completed.
}

// ---------------------------------------------------------------- tests
//
// Everything below runs on EVERY platform, including the macOS box this is
// developed on. That is the whole reason `wire` is not cfg'd: the encoding is
// where the defects are, and a test that only runs on the target machine is a
// test that runs after the mistake has already cost a driver reinstall.

#[cfg(test)]
mod tests {
    use super::wire::*;

    // -- IOCTL arithmetic ---------------------------------------------------

    /// The five literals are transcribed from the C_ASSERTs in
    /// AudioHubIoctl.h. They are NOT computed here from `ctl_code`, or this
    /// test would only prove `ctl_code` equals itself.
    #[test]
    fn ioctl_codes_match_the_header_literals() {
        assert_eq!(IOCTL_HELLO, 0x0022_E000);
        assert_eq!(IOCTL_BIND_SET, 0x0022_E004);
        assert_eq!(IOCTL_BIND_CLEAR, 0x0022_E008);
        assert_eq!(IOCTL_QUERY_SLOTS, 0x0022_E00C);
        assert_eq!(IOCTL_CONTROL_PEND, 0x0022_E010);
        assert_eq!(IOCTL_MAP_RINGS, 0x0022_E014);
        assert_eq!(IOCTL_NOTIFY, 0x0022_E018);
    }

    /// Function codes below 0x800 are reserved for Microsoft; a code that
    /// drifted into that range would collide with a system IOCTL rather than
    /// fail cleanly.
    #[test]
    fn ioctl_function_codes_are_in_the_vendor_range() {
        for code in [
            IOCTL_HELLO,
            IOCTL_BIND_SET,
            IOCTL_BIND_CLEAR,
            IOCTL_QUERY_SLOTS,
            IOCTL_CONTROL_PEND,
            IOCTL_MAP_RINGS,
            IOCTL_NOTIFY,
        ] {
            let function = (code >> 2) & 0xFFF;
            assert!(function >= 0x800, "function 0x{function:03X} is reserved");
            assert_eq!(code & 0x3, 0, "METHOD_BUFFERED is method 0");
            assert_eq!(code >> 16, 0x22, "FILE_DEVICE_UNKNOWN");
        }
    }

    /// An INDEPENDENT check on the arithmetic itself, against published values
    /// that nothing in this project can quietly redefine.
    ///
    /// The `access` term MUST be exercised with a non-zero value. The first
    /// version of this test used only `IOCTL_STORAGE_QUERY_PROPERTY`, whose
    /// access is `FILE_ANY_ACCESS` (0) — so `access << 14` and `access << 13`
    /// produced the same answer and a mutation of the shift survived the whole
    /// suite. Found by `regress/audit-halwire-win-injection.py`, which is what
    /// that script is for.
    #[test]
    fn ctl_code_reproduces_known_system_ioctls() {
        // IOCTL_STORAGE_QUERY_PROPERTY = 0x002D1400
        //   device 0x2D (MASS_STORAGE), function 0x500, BUFFERED, ANY access.
        assert_eq!(ctl_code(0x2D, 0x500, 0, 0), 0x002D_1400);
        // IOCTL_DISK_SET_PARTITION_INFO = 0x0007C008
        //   device 0x07 (DISK), function 0x002, BUFFERED, READ|WRITE (3).
        //   This one pins the access shift.
        assert_eq!(ctl_code(0x07, 0x002, 0, 3), 0x0007_C008);
        // IOCTL_DISK_GET_DRIVE_GEOMETRY = 0x00070000
        //   function 0, ANY access — pins the device shift on its own.
        assert_eq!(ctl_code(0x07, 0x000, 0, 0), 0x0007_0000);
        // IOCTL_SERIAL_SET_BAUD_RATE = 0x001B0004
        //   device 0x1B (SERIAL_PORT), function 1, BUFFERED, ANY access.
        assert_eq!(ctl_code(0x1B, 0x001, 0, 0), 0x001B_0004);
    }

    // -- struct layout ------------------------------------------------------

    #[test]
    fn struct_sizes_match_the_header() {
        assert_eq!(HELLO_REQUEST_BYTES, 16);
        assert_eq!(HELLO_REPLY_BYTES, 40);
        assert_eq!(BIND_REQUEST_BYTES, 320);
        // v2: +stage +nt_status +published +reserved
        // v3: that trailing reserved word became `flags` — same SIZE, new
        //     meaning, which is precisely why it needed a version and not an
        //     append: the bytes cannot tell the two apart.
        assert_eq!(BIND_REPLY_BYTES, 32);
        // v2: +published
        assert_eq!(SLOT_INFO_BYTES, 52);
        assert_eq!(QUERY_SLOTS_REPLY_BYTES, 848);
        assert_eq!(CONTROL_EVENT_BYTES, 24);
        // v5: the data plane's two messages. Sizes transcribed from the
        // C_ASSERTs, not computed here.
        assert_eq!(MAP_REQUEST_BYTES, 24);
        assert_eq!(MAP_REPLY_BYTES, 296);
        assert_eq!(NOTIFY_REQUEST_BYTES, 24);
        assert_eq!(NOTIFY_REPLY_BYTES, 8);
        assert_eq!(PROTOCOL_VERSION, 5, "the layout above IS version 5");
    }

    /// Every added field is read from the offset the C header asserts, by
    /// writing a distinguishable value at that exact byte and reading it back
    /// through the decoder. A field that shifts by four bytes still passes a
    /// size check; it does not pass this.
    #[test]
    fn bind_reply_v2_fields_decode_from_the_header_offsets() {
        let mut b = [0u8; BIND_REPLY_BYTES];
        b[0..4].copy_from_slice(&STATUS_PARTIAL.to_le_bytes());
        b[4..8].copy_from_slice(&3u32.to_le_bytes());
        b[8..12].copy_from_slice(&99u32.to_le_bytes());
        b[12..16].copy_from_slice(&SLOT_FREE.to_le_bytes());
        b[16..20].copy_from_slice(&STAGE_INSTALL_RENDER.to_le_bytes());
        b[20..24].copy_from_slice(&0xC000_0001u32.to_le_bytes()); // STATUS_UNSUCCESSFUL
        b[24..28].copy_from_slice(&PUB_CAPTURE.to_le_bytes());

        let r = decode_bind_reply(&b).expect("well formed");
        assert_eq!(r.status, STATUS_PARTIAL);
        assert_eq!(r.slot, 3);
        assert_eq!(r.generation, 99);
        assert_eq!(r.state, SLOT_FREE);
        assert_eq!(r.stage, STAGE_INSTALL_RENDER);
        assert_eq!(r.nt_status, 0xC000_0001);
        assert_eq!(r.published, PUB_CAPTURE);
        assert!(!r.set_is_whole());

        let text = r.failure_text();
        assert!(text.contains("speaker filters"), "{text}");
        assert!(text.contains("0xc0000001"), "{text}");
        assert!(text.contains("microphone only"), "{text}");
    }

    /// The gate `bind_call` actually runs, on every combination that matters.
    ///
    /// The half-published SET is the exact shape M6-2 shipped: `status: ok`,
    /// `state: bound`, and no speaker in the system list. It must come back as
    /// an Err whose text names what IS published, because that sentence is
    /// what reaches `daemon.status` and therefore the only thing anybody
    /// upstream can act on.
    #[test]
    fn the_gate_rejects_every_reply_that_did_not_do_what_it_said() {
        let ok_set = BindReply {
            status: STATUS_OK,
            slot: 0,
            generation: 1,
            state: SLOT_BOUND,
            stage: STAGE_NONE,
            nt_status: 0,
            published: PUB_BOTH,
            flags: 0,
        };
        assert!(bind_outcome(true, &ok_set).is_ok());

        // A naming fallback is NOT a failed bind: the devices exist. It is
        // reported separately (`endpoint_name_fell_back`), never by turning a
        // working device pair into an error the coordinator would retry.
        let named_generically = BindReply {
            flags: BINDREPLY_FLAG_NAME_FALLBACK,
            ..ok_set
        };
        assert!(bind_outcome(true, &named_generically).is_ok());
        assert!(named_generically.endpoint_name_fell_back());
        assert!(!ok_set.endpoint_name_fell_back());

        // A SET with only the microphone: the defect under repair.
        let half = BindReply { published: PUB_CAPTURE, ..ok_set };
        let e = bind_outcome(true, &half).expect_err("half a pair is not a bind");
        assert!(e.contains("microphone only"), "{e}");

        // And only the speaker, symmetrically.
        let half = BindReply { published: PUB_RENDER, ..ok_set };
        assert!(bind_outcome(true, &half).is_err());

        // Nothing at all, still claiming OK.
        let none = BindReply { published: 0, ..ok_set };
        assert!(bind_outcome(true, &none).is_err());

        // A CLEAR is whole when NOTHING is left.
        let ok_clear = BindReply { state: SLOT_FREE, published: 0, ..ok_set };
        assert!(bind_outcome(false, &ok_clear).is_ok());
        // A CLEAR that left half the pair behind is a failure, not a success:
        // the next SET would then be repairing a slot nobody knows is broken.
        let leftover = BindReply { state: SLOT_FREE, published: PUB_RENDER, ..ok_set };
        assert!(bind_outcome(false, &leftover).is_err());

        // A non-OK status is reported with the driver's own stage and NTSTATUS
        // rather than a generic "refused".
        let failed = BindReply {
            status: STATUS_PARTIAL,
            stage: STAGE_ROLLBACK,
            nt_status: 0xC000_0017,
            published: PUB_RENDER,
            ..ok_set
        };
        let e = bind_outcome(true, &failed).expect_err("partial is not success");
        assert!(e.contains("rolling a failed install back"), "{e}");
        assert!(e.contains("0xc0000017"), "{e}");
    }

    /// THE invariant, as an assertion rather than a comment: a SET that says
    /// OK while publishing only one half is NOT whole, and the daemon must be
    /// able to see that from the reply alone.
    ///
    /// This is the exact shape M6-2 shipped — `status: ok`, `state: bound`,
    /// and no speaker in the system list. In v1 the reply had no field that
    /// could express it, so the daemon believed the status word.
    #[test]
    fn a_set_that_publishes_one_half_is_not_whole_however_ok_it_claims_to_be() {
        let whole = BindReply {
            status: STATUS_OK,
            slot: 0,
            generation: 1,
            state: SLOT_BOUND,
            stage: STAGE_NONE,
            nt_status: 0,
            published: PUB_BOTH,
            flags: 0,
        };
        assert!(whole.set_is_whole());

        for half in [PUB_RENDER, PUB_CAPTURE, 0] {
            let r = BindReply { published: half, ..whole };
            assert!(
                !r.set_is_whole(),
                "published 0x{half:x} with status ok must NOT count as a bound device pair"
            );
        }
    }

    /// The debug bits are one contiguous, non-overlapping block, and none of
    /// them collides with `BINDFLAG_ONLINE`. A collision would make the daemon
    /// inject a fault every time it marked a peer online.
    #[test]
    fn the_fault_injection_bits_cannot_collide_with_the_online_flag() {
        assert_eq!(BINDFLAG_DEBUG_MASK & BINDFLAG_ONLINE, 0);
        let bits = [
            BINDFLAG_FAIL_RENDER,
            BINDFLAG_FAIL_CAPTURE,
            BINDFLAG_SKIP_ROLLBACK,
            BINDFLAG_LEGACY_UNBIND,
            BINDFLAG_FAIL_ENDPOINT_NAME,
        ];
        let mut seen = 0u32;
        for b in bits {
            assert_eq!(b.count_ones(), 1, "each flag is a single bit");
            assert_eq!(seen & b, 0, "flags overlap");
            seen |= b;
        }
        assert_eq!(seen, BINDFLAG_DEBUG_MASK);
    }

    /// Field offsets, checked by WRITING a distinguishable value into each
    /// field and reading the raw bytes back at the offset the C header asserts.
    /// A field that moved shows up here even though every size still matches.
    #[test]
    fn bind_request_fields_land_at_the_header_offsets() {
        let req = BindRequest {
            op: 0x1111_1111,
            slot: 7,
            flags: 0x3333_3333,
            generation: 0x4444_4444,
            session_id: 0x5555_5555_6666_6666,
            peer_key: "0123456789abcdef",
            peer_display: "AB",
        };
        let b = encode_bind_request(&req).expect("valid");

        assert_eq!(&b[0..4], &0x1111_1111u32.to_le_bytes());
        assert_eq!(&b[4..8], &7u32.to_le_bytes());
        assert_eq!(&b[8..12], &0x3333_3333u32.to_le_bytes());
        assert_eq!(&b[12..16], &0x4444_4444u32.to_le_bytes());
        // session_id at 16, and its 8 bytes must be little-endian.
        assert_eq!(&b[16..24], &0x5555_5555_6666_6666u64.to_le_bytes());
        // peer_key at 24, ASCII, NUL padded to 40.
        assert_eq!(&b[24..40], b"0123456789abcdef");
        assert!(b[40..64].iter().all(|&c| c == 0), "peer_key must be NUL padded");
        // display at 64, UTF-16LE — and carrying the composed name, not the
        // bare input: "AudioHub \u{2013} AB".
        let want: Vec<u16> = "AudioHub \u{2013} AB".encode_utf16().collect();
        for (i, u) in want.iter().enumerate() {
            assert_eq!(&b[64 + i * 2..66 + i * 2], &u.to_le_bytes(), "code unit {i}");
        }
        let end = 64 + want.len() * 2;
        assert!(b[end..320].iter().all(|&c| c == 0), "display must be NUL terminated and padded");
    }

    #[test]
    fn hello_reply_fields_decode_from_the_header_offsets() {
        let mut b = [0u8; HELLO_REPLY_BYTES];
        b[0..4].copy_from_slice(&1u32.to_le_bytes()); // status
        b[4..8].copy_from_slice(&2u32.to_le_bytes()); // protocol_version
        b[8..12].copy_from_slice(&3u32.to_le_bytes()); // slot_count
        b[12..16].copy_from_slice(&4u32.to_le_bytes()); // caps
        b[16..24].copy_from_slice(&0xDEAD_BEEF_CAFE_F00Du64.to_le_bytes());
        b[24..28].copy_from_slice(&48_000u32.to_le_bytes());
        b[28..32].copy_from_slice(&2u32.to_le_bytes());
        b[32..36].copy_from_slice(&1u32.to_le_bytes());
        b[36..40].copy_from_slice(&2u32.to_le_bytes()); // client_check

        let r = decode_hello_reply(&b).expect("well formed");
        assert_eq!(r.status, 1);
        assert_eq!(r.protocol_version, 2);
        assert_eq!(r.slot_count, 3);
        assert_eq!(r.caps, 4);
        assert_eq!(r.session_id, 0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(r.sample_rate, 48_000);
        assert_eq!(r.out_channels, 2);
        assert_eq!(r.in_channels, 1);
        assert_eq!(r.client_check, CLIENT_CHECK_IMAGEPATH);
    }

    #[test]
    fn a_reply_of_the_wrong_length_is_rejected_rather_than_read() {
        assert!(decode_hello_reply(&[0u8; HELLO_REPLY_BYTES - 1]).is_none());
        assert!(decode_hello_reply(&[0u8; HELLO_REPLY_BYTES + 1]).is_none());
        assert!(decode_bind_reply(&[0u8; BIND_REPLY_BYTES - 1]).is_none());
        // A v1-sized reply must be REFUSED, not read: its 16 bytes decode
        // perfectly as the first four fields, and the missing `published`
        // would silently read as 0 — "nothing published" — on a bind that
        // actually worked.
        assert!(decode_bind_reply(&[0u8; 16]).is_none(), "a v1 reply is not a v2 reply");
        assert!(decode_query_slots_reply(&[0u8; QUERY_SLOTS_REPLY_BYTES - 1]).is_none());
        assert!(decode_query_slots_reply(&[0u8; 784]).is_none(), "the v1 size is refused");
        assert!(decode_control_event(&[0u8; 23]).is_none());
    }

    // -- peer key -----------------------------------------------------------

    #[test]
    fn peer_key_whitelist_matches_the_driver() {
        assert!(valid_peer_key("b47382dc90267042")); // 30-win, measured
        assert!(valid_peer_key("ec8b4544a5249276")); // win-audio-debug, measured
        assert!(valid_peer_key("0000000000000000"));

        assert!(!valid_peer_key(""), "empty");
        assert!(!valid_peer_key("b47382dc9026704"), "15 chars");
        assert!(!valid_peer_key("b47382dc902670420"), "17 chars");
        assert!(!valid_peer_key("B47382DC90267042"), "uppercase hex is not the format we emit");
        assert!(!valid_peer_key("b47382dc9026704g"), "g is not hex");
        // The ones that would corrupt a device-interface reference string.
        assert!(!valid_peer_key(r"..\..\evil\aaaa"));
        assert!(!valid_peer_key("b47382dc\09026704"));
    }

    #[test]
    fn encode_refuses_a_bad_peer_key_instead_of_sending_it() {
        let bad = BindRequest {
            op: BIND_SET,
            slot: 0,
            flags: 0,
            generation: 0,
            session_id: 1,
            peer_key: "not-hex",
            peer_display: "x",
        };
        assert_eq!(encode_bind_request(&bad), Err(BindEncodeError::BadPeerKey));
    }

    #[test]
    fn encode_refuses_a_slot_past_the_pool() {
        let bad = BindRequest {
            op: BIND_SET,
            slot: MAX_SLOTS as u8,
            flags: 0,
            generation: 0,
            session_id: 1,
            peer_key: "0123456789abcdef",
            peer_display: "x",
        };
        assert_eq!(encode_bind_request(&bad), Err(BindEncodeError::SlotOutOfRange));
    }

    // -- clamp_utf16 --------------------------------------------------------

    #[test]
    fn clamp_utf16_always_terminates_and_fills() {
        let v = clamp_utf16("hi", 8);
        assert_eq!(v.len(), 8);
        assert_eq!(&v[..3], &[b'h' as u16, b'i' as u16, 0]);
        assert!(v[2..].iter().all(|&u| u == 0));
    }

    #[test]
    fn clamp_utf16_encodes_chinese_in_the_bmp_one_unit_each() {
        let v = clamp_utf16("客厅 Mac", DISPLAY_CHARS);
        assert_eq!(v[0], 0x5BA2); // 客
        assert_eq!(v[1], 0x5385); // 厅
        assert_eq!(v[2], 0x0020);
        assert_eq!(v[3], b'M' as u16);
        let end = v.iter().position(|&u| u == 0).unwrap();
        assert_eq!(end, 6, "6 code units of content");
    }

    /// THE reason this function exists rather than a `.take(n)` over
    /// `encode_utf16()`. An emoji is a surrogate PAIR; cutting between its two
    /// code units yields a lone surrogate, which is not valid UTF-16 — and it
    /// would go straight into a PERSISTENT registry property, where nothing
    /// downstream reports it.
    #[test]
    fn clamp_utf16_never_splits_a_surrogate_pair() {
        // "ab" + U+1F3B5 MUSICAL NOTE (D83C DFB5). With max_units = 4 there is
        // room for 3 content units, and the emoji needs 2 — so a naive
        // truncation keeps D83C alone.
        let s = "ab\u{1F3B5}";
        let v = clamp_utf16(s, 4);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], b'a' as u16);
        assert_eq!(v[1], b'b' as u16);
        assert_eq!(v[2], 0, "the emoji did not fit, so NOTHING of it was written");
        assert!(
            !v.iter().any(|u| (0xD800..0xE000).contains(u)),
            "a lone surrogate escaped: {v:04X?}"
        );
        // And the whole buffer must round-trip as valid UTF-16.
        let end = v.iter().position(|&u| u == 0).unwrap();
        assert_eq!(String::from_utf16(&v[..end]).unwrap(), "ab");
    }

    #[test]
    fn clamp_utf16_keeps_a_surrogate_pair_that_does_fit() {
        let v = clamp_utf16("ab\u{1F3B5}", 5);
        let end = v.iter().position(|&u| u == 0).unwrap();
        assert_eq!(end, 4);
        assert_eq!(String::from_utf16(&v[..end]).unwrap(), "ab\u{1F3B5}");
    }

    #[test]
    fn clamp_utf16_truncates_a_long_name_and_still_terminates() {
        let long = "漢".repeat(500);
        let v = clamp_utf16(&long, DISPLAY_CHARS);
        assert_eq!(v.len(), DISPLAY_CHARS);
        assert_eq!(v[DISPLAY_CHARS - 1], 0, "the last unit is always the terminator");
        let end = v.iter().position(|&u| u == 0).unwrap();
        assert_eq!(end, DISPLAY_CHARS - 1);
        assert_eq!(String::from_utf16(&v[..end]).unwrap().chars().count(), 127);
    }

    /// A 128-code-unit name must lose exactly its last character, not be
    /// silently accepted and overrun.
    #[test]
    fn clamp_utf16_is_exclusive_of_the_terminator_slot() {
        let s: String = std::iter::repeat('x').take(DISPLAY_CHARS).collect();
        let v = clamp_utf16(&s, DISPLAY_CHARS);
        let end = v.iter().position(|&u| u == 0).unwrap();
        assert_eq!(end, DISPLAY_CHARS - 1);
    }

    // -- bind request semantics --------------------------------------------

    /// Decodes the `display` field back out of an encoded bind request.
    fn wire_display(peer_display: &str) -> String {
        let req = BindRequest {
            op: BIND_SET,
            slot: 0,
            flags: BINDFLAG_ONLINE,
            generation: 0,
            session_id: 9,
            peer_key: "0123456789abcdef",
            peer_display,
        };
        let b = encode_bind_request(&req).unwrap();
        let units: Vec<u16> = b[64..320]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&u| u == 0).unwrap();
        String::from_utf16(&units[..end]).unwrap()
    }

    /// THE test the old one was not.
    ///
    /// Its predecessor fed the encoder `"AudioHub – WIN-30"` and asserted it
    /// came back unchanged — so it measured the encoder while the PRODUCER of
    /// that string went unchecked. The producer is `haldev`, which emits the
    /// BARE peer name, and every Windows endpoint duly came out labelled
    /// `WIN-IR01HVEFU7G`. The test stayed green throughout.
    ///
    /// The input here is therefore exactly what `haldev::display_names`
    /// produces. Take the prefixing out of `encode_bind_request` and this goes
    /// red, which is the whole point.
    #[test]
    fn the_wire_name_is_composed_from_the_bare_peer_name() {
        assert_eq!(wire_display("WIN-30"), "AudioHub – WIN-30");
        // Disambiguated names survive intact: the suffix haldev appends to a
        // duplicate host name is part of the peer's name, not decoration.
        assert_eq!(wire_display("WIN-30 (2)"), "AudioHub – WIN-30 (2)");
    }

    /// And the composed name is the macOS name minus the direction word.
    ///
    /// This is the cross-platform pin: the driver appends the direction word
    /// itself, so what goes on the wire has to be the exact prefix of what
    /// macOS calls the same device. Change the prefix on one side alone and
    /// this fails.
    #[test]
    fn the_wire_name_is_the_macos_name_without_its_direction_word() {
        let (out, mic) = crate::haldev::device_names("WIN-30", false);
        let stem = wire_display("WIN-30");
        assert_eq!(out, format!("{stem} 扬声器"));
        assert_eq!(mic, format!("{stem} 麦克风"));
        // Nothing directional may ride along on the wire: the driver appends
        // its own word to it and would produce "... 扬声器 扬声器".
        assert!(!stem.contains('扬'));
        assert!(!stem.contains('麦'));
    }

    /// A Clear carries no name, and must not carry a lone prefix either.
    #[test]
    fn a_clear_sends_an_empty_name_not_a_lone_prefix() {
        assert_eq!(wire_display(""), "");
    }

    /// A peer name long enough to overflow the field still arrives with the
    /// prefix on it: truncation takes from the END, so the part that says this
    /// is an AudioHub device is the part that survives.
    #[test]
    fn an_over_long_peer_name_keeps_the_prefix() {
        let long: String = std::iter::repeat('x').take(DISPLAY_CHARS * 2).collect();
        let got = wire_display(&long);
        assert!(got.starts_with("AudioHub – "), "{got}");
        assert_eq!(got.chars().count(), DISPLAY_CHARS - 1);
    }

    /// The EN DASH in the name prefix is U+2013, not a hyphen — and it must
    /// survive the UTF-16 encoding as a single code unit.
    #[test]
    fn the_en_dash_survives_the_encoding() {
        let v = clamp_utf16("AudioHub – x", DISPLAY_CHARS);
        assert_eq!(v[9], 0x2013, "U+2013 EN DASH, not 0x2D HYPHEN-MINUS");
    }

    // -- query slots --------------------------------------------------------

    #[test]
    fn query_slots_decodes_every_slot_and_trims_the_key() {
        let mut b = [0u8; QUERY_SLOTS_REPLY_BYTES];
        b[0..4].copy_from_slice(&STATUS_OK.to_le_bytes());
        b[4..8].copy_from_slice(&(MAX_SLOTS as u32).to_le_bytes());
        b[8..16].copy_from_slice(&77u64.to_le_bytes());
        // slot 3 is bound
        let at = 16 + 3 * SLOT_INFO_BYTES;
        b[at..at + 4].copy_from_slice(&SLOT_BOUND.to_le_bytes());
        b[at + 4..at + 8].copy_from_slice(&42u32.to_le_bytes());
        b[at + 8..at + 8 + 16].copy_from_slice(b"b47382dc90267042");
        b[at + 8 + PEERKEY_BUF..at + 12 + PEERKEY_BUF].copy_from_slice(&PUB_BOTH.to_le_bytes());
        // slot 5: bound, but only the microphone half is really there. This is
        // the state the driver must never produce and the daemon must be able
        // to read back if it ever does.
        let at5 = 16 + 5 * SLOT_INFO_BYTES;
        b[at5..at5 + 4].copy_from_slice(&SLOT_BOUND.to_le_bytes());
        b[at5 + 8..at5 + 8 + 16].copy_from_slice(b"ec8b4544a5249276");
        b[at5 + 8 + PEERKEY_BUF..at5 + 12 + PEERKEY_BUF].copy_from_slice(&PUB_CAPTURE.to_le_bytes());

        let r = decode_query_slots_reply(&b).expect("well formed");
        assert_eq!(r.status, STATUS_OK);
        assert_eq!(r.session_id, 77);
        assert_eq!(r.slots.len(), MAX_SLOTS);
        assert_eq!(r.slots[3].state, SLOT_BOUND);
        assert_eq!(r.slots[3].generation, 42);
        assert_eq!(r.slots[3].peer_key, "b47382dc90267042");
        assert_eq!(r.slots[3].published, PUB_BOTH);
        assert_eq!(r.slots[5].published, PUB_CAPTURE, "half a pair is visible from user mode");
        assert_eq!(r.slots[0].state, SLOT_FREE);
        assert_eq!(r.slots[0].peer_key, "", "a free slot reports no key");
        assert_eq!(r.slots[0].published, 0);
    }

    // -- control event ------------------------------------------------------

    #[test]
    fn control_event_decodes_flags_and_fixed_point_volume() {
        let mut b = [0u8; CONTROL_EVENT_BYTES];
        b[0..4].copy_from_slice(&EVENT_VOLUME.to_le_bytes());
        b[4..8].copy_from_slice(&5u32.to_le_bytes());
        b[8..12].copy_from_slice(&11u32.to_le_bytes());
        b[12..16].copy_from_slice(&(EVFLAG_INPUT | EVFLAG_MUTED).to_le_bytes());
        b[16..20].copy_from_slice(&(0x8000u32).to_le_bytes()); // 0.5 in 16.16

        let e = decode_control_event(&b).expect("well formed");
        assert_eq!(e.kind, EVENT_VOLUME);
        assert_eq!(e.slot, 5);
        assert_eq!(e.generation, 11);
        assert!(e.input());
        assert!(e.muted());
        assert!(!e.running());
        assert!((e.scalar() - 0.5).abs() < 1e-6);
    }

    /// A driver reporting a scalar above 1.0 must be clamped, not trusted: the
    /// value is relayed to a peer's real device.
    #[test]
    fn control_event_clamps_an_out_of_range_scalar() {
        let mut b = [0u8; CONTROL_EVENT_BYTES];
        b[16..20].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(decode_control_event(&b).unwrap().scalar(), 1.0);
    }

    // -- MAP_RINGS ----------------------------------------------------------

    /// Every field at the byte offset the C header asserts, checked by writing
    /// a distinguishable value into each and reading the RAW BYTES back. A
    /// field that shifted by four bytes still passes every size check; it does
    /// not pass this.
    #[test]
    fn map_request_fields_land_at_the_header_offsets() {
        let b = encode_map_request(0x1122_3344_5566_7788, 0x0000_00AA_BBCC_DDEE);
        assert_eq!(b.len(), 24);
        assert_eq!(&b[0..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&b[8..16], &0x0000_00AA_BBCC_DDEEu64.to_le_bytes());
        assert_eq!(&b[16..20], &PROTOCOL_VERSION.to_le_bytes(), "checked again here");
        assert_eq!(&b[20..24], &0u32.to_le_bytes(), "flags MBZ");
    }

    /// A wake event of 0 means "do not signal me", and it must go across as a
    /// real zero rather than being turned into something the driver would try
    /// to reference.
    #[test]
    fn map_request_carries_no_wake_event_as_zero() {
        let b = encode_map_request(1, 0);
        assert_eq!(&b[8..16], &[0u8; 8]);
    }

    #[test]
    fn map_reply_fields_decode_from_the_header_offsets() {
        let mut b = [0u8; MAP_REPLY_BYTES];
        b[0..4].copy_from_slice(&STATUS_OK.to_le_bytes());
        b[4..8].copy_from_slice(&32u32.to_le_bytes()); // ring_count
        b[8..12].copy_from_slice(&64u32.to_le_bytes()); // data_offset
        b[12..16].copy_from_slice(&24_000u32.to_le_bytes()); // capacity_frames
        b[16..20].copy_from_slice(&48_000u32.to_le_bytes()); // sample_rate
        b[20..24].copy_from_slice(&2u32.to_le_bytes()); // spk_channels
        b[24..28].copy_from_slice(&1u32.to_le_bytes()); // mic_channels
        b[28..32].copy_from_slice(&196_608u32.to_le_bytes()); // spk_bytes @28
        b[32..36].copy_from_slice(&98_304u32.to_le_bytes()); // mic_bytes
        b[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved MBZ
        // va at 40, and it is 8-byte quantities: a 4-byte read here would pick
        // up the top half of the previous entry on every index but the first.
        for i in 0..RING_SLOTS_MAX {
            let v = 0x0000_7F00_0000_0000u64 + (i as u64) * 0x4000;
            b[40 + i * 8..48 + i * 8].copy_from_slice(&v.to_le_bytes());
        }

        let r = decode_map_reply(&b).expect("well formed");
        assert_eq!(r.status, STATUS_OK);
        assert_eq!(r.ring_count, 32);
        assert_eq!(r.data_offset, 64);
        assert_eq!(r.capacity_frames, 24_000);
        assert_eq!(r.sample_rate, 48_000);
        assert_eq!(r.spk_channels, 2);
        assert_eq!(r.mic_channels, 1);
        assert_eq!(r.spk_bytes, 196_608);
        assert_eq!(r.mic_bytes, 98_304);
        assert_eq!(r.va.len(), RING_SLOTS_MAX);
        for i in 0..RING_SLOTS_MAX {
            assert_eq!(
                r.va[i],
                0x0000_7F00_0000_0000u64 + (i as u64) * 0x4000,
                "va[{i}]"
            );
        }
        assert_eq!(r.geometry_error(), None, "this IS the contract geometry");
    }

    /// A reply of the wrong length is REFUSED, not read. The dangerous case is
    /// a short one: the leading fields would decode perfectly and the missing
    /// addresses would silently read as 0 — which `attach` would then report as
    /// "the driver mapped no memory", blaming the driver for a decode bug.
    #[test]
    fn a_map_reply_of_the_wrong_length_is_rejected_rather_than_read() {
        assert!(decode_map_reply(&[0u8; MAP_REPLY_BYTES - 1]).is_none());
        assert!(decode_map_reply(&[0u8; MAP_REPLY_BYTES + 1]).is_none());
        assert!(decode_map_reply(&[0u8; 40]).is_none(), "geometry with no addresses");
        assert!(decode_notify_reply(&[0u8; 4]).is_none());
        assert!(decode_notify_reply(&[0u8; 12]).is_none());
    }

    /// The geometry gate names the field that disagrees, one at a time — a
    /// message that only said "geometry mismatch" would leave whoever reads
    /// `daemon.status` with nothing to act on.
    #[test]
    fn the_geometry_gate_names_the_field_that_disagrees() {
        let good = MapReply {
            status: STATUS_OK,
            ring_count: 32,
            data_offset: RING_DATA_OFFSET as u32,
            capacity_frames: RING_FRAMES,
            sample_rate: RING_SAMPLE_RATE,
            spk_channels: SPK_CHANNELS,
            mic_channels: MIC_CHANNELS,
            spk_bytes: SPK_BYTES as u32,
            mic_bytes: MIC_BYTES as u32,
            va: vec![1; RING_SLOTS_MAX],
        };
        assert_eq!(good.geometry_error(), None);

        for (field, bad) in [
            ("data_offset", MapReply { data_offset: 40, ..good.clone() }),
            ("capacity_frames", MapReply { capacity_frames: 12_000, ..good.clone() }),
            ("sample_rate", MapReply { sample_rate: 44_100, ..good.clone() }),
            ("spk_channels", MapReply { spk_channels: 1, ..good.clone() }),
            ("mic_channels", MapReply { mic_channels: 2, ..good.clone() }),
            ("spk_bytes", MapReply { spk_bytes: 65_536, ..good.clone() }),
            ("mic_bytes", MapReply { mic_bytes: 65_536, ..good.clone() }),
        ] {
            let e = bad.geometry_error().unwrap_or_else(|| panic!("{field} must be refused"));
            assert!(e.contains(field), "the message must name {field}: {e}");
        }

        // And the ring count, which is a different kind of wrong: not a
        // geometry the daemon cannot consume, but a count it cannot index.
        for n in [0u32, 3, 33, 64, u32::MAX] {
            assert!(
                MapReply { ring_count: n, ..good.clone() }.geometry_error().is_some(),
                "ring_count {n} must be refused"
            );
        }
        for n in [2u32, 4, 32] {
            assert_eq!(MapReply { ring_count: n, ..good.clone() }.geometry_error(), None);
        }
    }

    // -- NOTIFY -------------------------------------------------------------

    #[test]
    fn notify_request_fields_land_at_the_header_offsets() {
        let b = encode_notify_request(0x0102_0304_0506_0708, 9, 0x1234_5678, true, true, 0.5);
        assert_eq!(b.len(), 24);
        assert_eq!(&b[0..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&b[8..12], &9u32.to_le_bytes(), "slot at 8");
        assert_eq!(&b[12..16], &0x1234_5678u32.to_le_bytes(), "generation at 12");
        assert_eq!(&b[16..20], &(NOTIFYFLAG_INPUT | NOTIFYFLAG_MUTED).to_le_bytes());
        assert_eq!(&b[20..24], &0x8000u32.to_le_bytes(), "scalar_q16 at 20");
    }

    /// The two flags are independent single bits and neither is implied by the
    /// other. A muted speaker and an unmuted microphone must be distinguishable
    /// on the wire, or a mute would follow the wrong endpoint.
    #[test]
    fn the_notify_flags_are_independent_bits() {
        assert_eq!(NOTIFYFLAG_MUTED.count_ones(), 1);
        assert_eq!(NOTIFYFLAG_INPUT.count_ones(), 1);
        assert_eq!(NOTIFYFLAG_MUTED & NOTIFYFLAG_INPUT, 0);

        let flags = |input, muted| {
            let b = encode_notify_request(1, 0, 0, input, muted, 1.0);
            u32::from_le_bytes([b[16], b[17], b[18], b[19]])
        };
        assert_eq!(flags(false, false), 0);
        assert_eq!(flags(true, false), NOTIFYFLAG_INPUT);
        assert_eq!(flags(false, true), NOTIFYFLAG_MUTED);
        assert_eq!(flags(true, true), NOTIFYFLAG_INPUT | NOTIFYFLAG_MUTED);
    }

    /// 16.16 fixed point, ROUND TRIPPED rather than compared with a literal on
    /// both sides: the value written into the request is decoded back with
    /// `q16_to_scalar` and must land within half a step of what went in.
    ///
    /// A step is 1/65536 ≈ 1.5e-5, so the tolerance below is the quantiser's,
    /// not a fudge factor — it is exactly what a 16.16 encoding costs.
    #[test]
    fn the_notify_scalar_round_trips_through_16_16_fixed_point() {
        for &v in &[0.0f32, 0.01, 0.1, 0.25, 1.0 / 3.0, 0.5, 0.75, 0.9, 0.99, 1.0] {
            let b = encode_notify_request(1, 0, 0, false, false, v);
            let q = u32::from_le_bytes([b[20], b[21], b[22], b[23]]);
            let back = q16_to_scalar(q);
            assert!(
                (back - v).abs() <= 1.0 / 131_072.0,
                "{v} round-tripped as {back} (q={q})"
            );
        }
        // The two ends of the range are EXACT, because they are the two values
        // a user actually lands on and an off-by-one at full scale is audible.
        assert_eq!(q16_to_scalar(scalar_to_q16(0.0)), 0.0);
        assert_eq!(q16_to_scalar(scalar_to_q16(1.0)), 1.0);
        assert_eq!(scalar_to_q16(1.0), 0x1_0000, "0x10000 IS 1.0");
        assert_eq!(scalar_to_q16(0.5), 0x8000);
    }

    /// Out of range in either direction is CLAMPED, not wrapped. This number
    /// reaches a peer's real device: a scalar that wrapped to a huge integer
    /// through `as u32` would be full volume where the user asked for silence.
    #[test]
    fn an_out_of_range_notify_scalar_is_clamped_at_both_ends() {
        assert_eq!(scalar_to_q16(-0.5), 0);
        assert_eq!(scalar_to_q16(-1e9), 0);
        assert_eq!(scalar_to_q16(f32::NEG_INFINITY), 0);
        assert_eq!(scalar_to_q16(1.5), 0x1_0000);
        assert_eq!(scalar_to_q16(1e9), 0x1_0000);
        assert_eq!(scalar_to_q16(f32::INFINITY), 0x1_0000);
        // NaN clamps to 0 (`f32::clamp` on NaN yields NaN, and `NaN as u32` is
        // 0 in Rust) — silence rather than an arbitrary level.
        assert_eq!(scalar_to_q16(f32::NAN), 0);

        // And the decode side, on anything the DRIVER might send.
        assert_eq!(q16_to_scalar(0x1_0000), 1.0);
        assert_eq!(q16_to_scalar(0xFFFF_FFFF), 1.0);
        assert_eq!(q16_to_scalar(0), 0.0);
    }

    #[test]
    fn notify_reply_decodes_status_and_the_applied_flag() {
        let mut b = [0u8; NOTIFY_REPLY_BYTES];
        b[0..4].copy_from_slice(&STATUS_OK.to_le_bytes());
        b[4..8].copy_from_slice(&1u32.to_le_bytes());
        let r = decode_notify_reply(&b).expect("well formed");
        assert_eq!(r.status, STATUS_OK);
        assert!(r.applied(), "the stored level changed");

        b[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(
            !decode_notify_reply(&b).unwrap().applied(),
            "the driver suppressed the echo, which is not the same as ignoring it"
        );

        b[0..4].copy_from_slice(&STATUS_STALE_SESSION.to_le_bytes());
        assert_eq!(decode_notify_reply(&b).unwrap().status, STATUS_STALE_SESSION);
    }

    // -- capabilities -------------------------------------------------------

    /// The two capability bits are independent, and reading one for the other
    /// is the SQUARED-ATTENUATION defect: a driver with rings but no volume
    /// node hands over pre-attenuated samples, and a daemon that took
    /// `has_dataplane` as licence to sync volume would apply the setting twice.
    #[test]
    fn the_capability_bits_are_independent_and_read_from_their_own_bit() {
        assert_eq!(CAP_DATAPLANE.count_ones(), 1);
        assert_eq!(CAP_VOLUME.count_ones(), 1);
        assert_eq!(CAP_DATAPLANE & CAP_VOLUME, 0);

        let hello = |caps: u32| {
            let mut b = [0u8; HELLO_REPLY_BYTES];
            b[12..16].copy_from_slice(&caps.to_le_bytes());
            decode_hello_reply(&b).unwrap()
        };
        assert!(!hello(0).has_dataplane() && !hello(0).has_volume());
        assert!(hello(CAP_DATAPLANE).has_dataplane());
        assert!(!hello(CAP_DATAPLANE).has_volume(), "rings do not imply a volume node");
        assert!(hello(CAP_VOLUME).has_volume());
        assert!(!hello(CAP_VOLUME).has_dataplane(), "a volume node does not imply rings");
        let both = hello(CAP_DATAPLANE | CAP_VOLUME);
        assert!(both.has_dataplane() && both.has_volume());
        // An unknown future bit must move neither answer.
        assert!(!hello(0x8000_0000).has_dataplane() && !hello(0x8000_0000).has_volume());
    }

    // -- ring geometry ------------------------------------------------------

    /// The geometry is DERIVED where it can be, so the numbers cannot merely
    /// agree with themselves: the byte sizes must be big enough for the frames
    /// they claim to hold and 16K-page aligned, which is what makes them legal
    /// for `MmMapLockedPagesSpecifyCache`.
    #[test]
    fn the_ring_byte_sizes_hold_the_frames_they_claim_and_are_page_aligned() {
        for (bytes, ch, what) in
            [(SPK_BYTES, SPK_CHANNELS, "speaker"), (MIC_BYTES, MIC_CHANNELS, "microphone")]
        {
            let need = RING_DATA_OFFSET + (RING_FRAMES as usize) * (ch as usize) * 4;
            assert!(bytes >= need, "the {what} ring is {bytes} bytes, it needs {need}");
            assert!(bytes - need < 16_384, "the {what} ring is over-allocated by a whole page");
            assert_eq!(bytes % 16_384, 0, "the {what} ring must be 16K-page aligned");
        }
        // 500 ms, which is what the whole latency budget is written against.
        assert_eq!(RING_FRAMES, (RING_SAMPLE_RATE / 1000) * 500);
        assert_eq!(RING_SLOTS_MAX, 2 * MAX_SLOTS);
        assert_eq!(DIR_OUT, 0);
        assert_eq!(DIR_IN, 1);
    }

    // -- capacity agreement -------------------------------------------------

    /// `AUDIOHUB_WIN_MAX_SLOTS`, `HAL_MAX_SLOTS` and the driver's
    /// `g_MaxAudioHubMiniports` are three copies of one decision. The driver's
    /// budget reaches `PcAddAdapterDevice` once and cannot be raised, so a
    /// disagreement shows up as the SECOND peer failing to publish.
    #[test]
    fn slot_capacity_agrees_with_the_rest_of_the_bridge() {
        assert_eq!(MAX_SLOTS, crate::halbridge::HAL_MAX_SLOTS);
        assert_eq!(MAX_SLOTS * 4, 64, "g_MaxAudioHubMiniports in minipairs.h");
    }

    #[test]
    fn the_control_path_is_the_one_the_driver_publishes() {
        assert_eq!(CTL_PATH, r"\\.\AudioHubVadCtl");
        let u = ctl_path_utf16();
        assert_eq!(*u.last().unwrap(), 0, "CreateFileW needs a NUL");
        assert_eq!(u.len(), CTL_PATH.chars().count() + 1);
    }
}
