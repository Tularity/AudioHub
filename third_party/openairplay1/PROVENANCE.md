# openairplay1 provenance

This directory vendors the `openairplay1` library from:

- Upstream: https://github.com/st3fan/openairplay1
- Baseline commit: `797837d69d7fe33cb7af6cd7460931599079eaf2`
- Baseline commit date: 2026-08-04
- Imported for AudioHub: 2026-08-11
- Upstream code license: MIT; see `LICENSE`
- Third-party material: see `NOTICE.md`, especially the Apple-derived AirPort
  Express key in `src/airport.pem`, which is not covered by the MIT grant

The initial `Cargo.toml`, `LICENSE`, and `src/` contents were copied exactly
from that commit. `NOTICE.md` carries the relevant upstream attribution in a
self-contained form. AudioHub then applied a deliberately narrow receiver-edge
hardening delta:

- bounded RTSP connections, request parsing, bodies, and idle time;
- challenge-first Digest authentication with issued nonces, a fixed realm,
  exact request-URI binding, and redacted authentication logs;
- narrow authenticated metadata/artwork body caps plus state-aware RTSP idle
  handling that keeps quiet streaming control connections alive;
- strict production RAOP ALAC format validation;
- audio, control, and timing UDP datagrams bound to the authenticated RTSP
  peer IP (including IPv4-mapped IPv6 normalization), so another LAN host
  cannot inject packets into an active session;
- bounded protocol, player, and event queues;
- session-scoped finite volume events only.

The three upstream golden decoder fixtures are kept under `src/testdata/`;
the two binary fixtures are base64-encoded for reviewable patch transport and
decode byte-for-byte in unit tests.

The upstream repository remains the source of provenance. This copy is used as
a path dependency so the reviewed security behavior is reproducible on macOS
and Windows instead of depending on an unpublished fork revision.
