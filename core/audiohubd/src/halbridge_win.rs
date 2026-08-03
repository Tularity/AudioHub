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
    pub const PROTOCOL_VERSION: u32 = 4;

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

    const _: () = assert!(QUERY_SLOTS_REPLY_BYTES == 848);

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
        /// bit0 of `caps`: the driver has audio rings. 0 through M6-2.
        pub fn has_dataplane(&self) -> bool {
            self.caps & 0x1 != 0
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

    /// `\\.\AudioHubVadCtl` as a NUL-terminated UTF-16 path for `CreateFileW`.
    pub fn ctl_path_utf16() -> Vec<u16> {
        CTL_PATH.encode_utf16().chain(std::iter::once(0)).collect()
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

    use super::transport::{self, Handle, OpenFail, PendingCall};
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
        handle: Handle,
        pub session_id: u64,
        pub slot_count: u8,
        pub driver_protocol: u32,
        pub client_check: u32,
        pub caps: u32,
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
            };

            // Best effort: the inverted call carries nothing yet, and a driver
            // that refuses it is still perfectly usable for binding.
            s.pending = PendingCall::issue(&s.handle).unwrap_or(None);

            Ok(s)
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
        assert_eq!(PROTOCOL_VERSION, 4, "the layout above IS version 4");
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
