# audiohub-airplay

Cross-platform, audio-only AirPlay 2 ingress for AudioHub.

The receiver under `src/protocol` is implemented and owned by AudioHub. It does
not link or vendor another AirPlay receiver engine. The implementation was
written from protocol observations and documented behavioural references;
source influences and opaque interoperability data are recorded in
`PROVENANCE.md` and the repository-level `NOTICE.md`.

The current discovery profile advertises independently implemented realtime
type-96 ALAC and buffered type-103 AAC-LC paths. Real macOS Music capability
tests established that its native transient-pairing route requires the PTP
capability combination. The receiver binds the standard UDP 319/320 ports and
passively observes the sender's two-step Sync/Follow_Up timeline on Windows.
On macOS those sockets belong to the system, so AudioHub registers the
authenticated peer and IEEE 1588 logical port with the system TimeSync service
through a dynamically resolved CoreMedia adapter. Buffered RTP timestamps are
mapped through the authenticated rate anchor onto the local monotonic clock.
This is a single-receiver path, not a BMCA participant or a claim of multiroom
precision. Grouping, screen/video, metadata, persistent pairing, and
remote-control streams remain unadvertised.

This crate never advertises through mDNS itself. It returns a complete
`_airplay._tcp` service descriptor to the daemon, which owns registration. The
control port defaults to `0`; startup selects a free ephemeral port and reports
the concrete port to the caller before it returns.

Decoded PCM stays full-scale. The only sample-domain operations are stereo to
mono conversion and streaming 44.1 kHz to 48 kHz resampling. AirPlay volume
arrives as a dB event for the daemon to map to the receiver machine's real
system output control; it is deliberately never multiplied into PCM.

Realtime media uses a bounded UDP jitter/retransmission engine, validates NTP
timing, and delivers ordered ALAC PCM into the existing clock-servo bus.
Buffered media owns a separate bounded TCP framer, authenticated AAC decoder,
pause/flush state machine, and bounded PTP-to-RTP presentation mapper. The
bounded multi-reader bus drops old history for a stale reader and never lets
that reader stall reception. Kernel receive timestamps, output-device clock
feedback, BMCA, delay measurement, and multiroom drift servo remain future
precision work and are not advertised as implemented.

## Protocol implementation and provenance

The AirPlay 2 engine is AudioHub source. Behavioural references, fixed
revisions, acquisition methods, and separately attributed opaque FairPlay
compatibility records are listed in `PROVENANCE.md` and the root `NOTICE.md`.

Generic cryptography, plist, integer, async-runtime, and codec crates are
ordinary package dependencies under their own licenses; none is an AirPlay
receiver engine. Apple-derived compatibility material remains separately
identified and is not represented as AudioHub-authored Apache-2.0 source. In
particular, the receiver bundles a leaked AirPort Express RSA key behind an
isolated `AppleResponseProvider` because current Apple Music verifies an
`Apple-Response` before entering the AirPlay 2 control flow; this makes the
complete receiver not clean-room and carries the distribution risks stated in
the root `NOTICE.md`.
