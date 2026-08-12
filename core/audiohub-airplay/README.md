# audiohub-airplay

Cross-platform, audio-only AirPlay ingress for AudioHub.

The production default is AirPlay 1 (RAOP) only. That is the protocol used by
macOS system-wide audio output today. AirPlay 2 support is present behind the
non-default Cargo feature `experimental-airplay2` and a runtime opt-in because
`openairplay2` 0.5 supports buffered AAC but not the realtime ALAC/type-96
stream selected by macOS system output. Production builds therefore do not
even compile the incomplete AP2 engine.

This crate never advertises through mDNS itself. It returns complete service
descriptors to the daemon, which owns the independent `_raop._tcp` and
`_airplay._tcp` registrations. Both control ports default to `0`; startup
selects currently free ephemeral ports and reports the concrete ports to the
caller before it returns.

Decoded PCM stays full-scale. The only sample-domain operations are stereo to
mono format conversion and streaming 44.1 kHz to 48 kHz resampling. AirPlay
volume arrives as a dB event for the daemon to map to the receiver machine's
real system output control; it is deliberately never multiplied into PCM.
The protocol sink blocks against a wall-clock 48 kHz pacer (as required by the
upstream sink contract); the bounded multi-reader bus itself drops old history
for a stale reader and never lets that reader stall reception.

## Upstream protocol engines

- `openairplay1`, pinned to upstream commit
  `797837d69d7fe33cb7af6cd7460931599079eaf2` for RAOP Digest password support
  (MIT for its own code; its Apple-derived AirPort Express key is separately
  identified in the vendored `NOTICE.md` and is not covered by that grant).
- `openairplay2` 0.5.0 for experimental buffered AirPlay 2 support (MIT).

The open-source dependency licenses are compatible with AudioHub's Apache-2.0
license. The AirPort key remains separately attributed third-party material.
