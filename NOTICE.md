# AudioHub third-party notices

AudioHub's original source is licensed under Apache License 2.0. The following
interoperability material is separately identified because AudioHub's license
cannot grant rights that belong to its original owner.

## FairPlay `fp-setup` compatibility records

`core/audiohub-airplay/src/protocol/fairplay_compat.hex` contains four
142-byte responses and one 12-byte header used by the AirPlay 2 `fp-setup`
compatibility exchange. The records were copied from `rtsp.c` in
shairport-sync at commit `08af668a5d17b4714da38981dea4c9039263a4cc`.

The surrounding shairport-sync source is MIT licensed. Its complete notice is
reproduced below:

Copyright (c) James Laird 2013

Modifications, including those associated with audio synchronization,
multithreading and metadata handling copyright (c) Mike Brady 2014--2026

All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The byte records themselves are widely circulated, reverse-engineered,
Apple-derived FairPlay interoperability data. They are not original to
shairport-sync or AudioHub and are not represented as covered by AudioHub's
Apache-2.0 license. Their inclusion is solely for interoperability with audio
sent to a receiver controlled by the user. Copyright, anti-circumvention,
patent, certification or other rights may apply independently of the
open-source licenses.

AudioHub does not include OmgHax/FairPlay session-key decryption code. It does,
however, bundle the separately identified AirPort Express RSA private key
described below; therefore the complete receiver must not be described as a
clean-room implementation or as containing only AudioHub-authored material.

## AirPort Express Apple-Challenge compatibility key

`core/audiohub-airplay/src/protocol/airport_express_private_key.pem` is the
widely leaked Apple/AirPort Express 2048-bit RSA private key used to answer the
legacy `Apple-Challenge` authenticity check that current Apple Music performs
before entering its AirPlay 2 control flow. The PEM was copied from
`common.c` in shairport-sync at commit
`08af668a5d17b4714da38981dea4c9039263a4cc` (donor file SHA-256
`b89676c3daebac78de206fad1be5331c690a3a0e864750b31531ee36450cf058`),
then mechanically rewrapped to canonical 64-character PEM lines without
changing its DER key material. The local PEM SHA-256 is
`1a04883a6086368301382c6bae4074296929da9000b399dc616848f622293363`;
the extracted public-key DER SHA-256 is
`e14bc3d4793ca6f8be156fc07c4d41ef22faa082fb88308f3d8426daa5a39ede`.

The donor `common.c` carries these copyright lines under the same MIT
permission and warranty terms reproduced above:

Copyright (c) James Laird 2013

`vol2attn` copyright (c) Mike Brady 2014

Further changes copyright (c) Mike Brady 2014--2025

That MIT grant cannot establish ownership of or a license to Apple's leaked
key. The key is not AudioHub-authored Apache-2.0 material. Bundling or using it
is not an Apple license, MFi certification, patent grant, anti-circumvention
authorization, or assurance that distribution is lawful in every jurisdiction.
Copyright, contractual, trademark and export controls may apply independently
as well. The independently written Rust request validation, byte-layout and
PKCS#1 v1.5 provider boundary do not remove this separate compatibility-asset
risk.

## st3fan/openairplay2 structural reference

AudioHub's stable identity and discovery module organization was informed by
`st3fan/openairplay2` at commit
`7cf2cced8d48e5b3c063ecddc6802aa979f0bebd`. That project is not linked or
vendored. Its complete MIT notice is:

MIT License

Copyright (c) 2026 Stefan Arentz

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## ALAC decoder

The self-developed AirPlay 2 protocol engine uses the general-purpose Rust
crate `alac` version 0.5.0 to decode ALAC frames. The crate is available under
MIT OR Apache-2.0; its crates.io registry checksum is
`498a34d3cad5f3b23cc217ab489424ebcfffed186e30ad5ac02624e50df2c2b8`.
It is a codec primitive and does not provide AirPlay discovery, pairing,
control, timing, retransmission, decryption, or receiver-session logic.

## AAC-LC decoder

The self-developed buffered AirPlay 2 path uses the general-purpose Rust
`symphonia` AAC decoder, version 0.5.5, with default features disabled and only
the AAC feature enabled. Symphonia is available under MPL-2.0. AudioHub does
not modify Symphonia source; the applicable package notices and complete
license text are included in `THIRD-PARTY-LICENSES.html`. Symphonia provides
codec primitives only and is not an AirPlay receiver engine.

## macOS CoreMedia PTP platform adapter

On macOS, AudioHub dynamically calls the operating system's private
`CM8021ASClock*` CoreMedia SPI so Apple's TimeSync service can receive the
AirPlay 2 PTP packets owned by the system and convert the sender timeline to
host time. The Rust wrapper, protocol integration, validation and resource
management are original AudioHub code; no third-party implementation is
copied or linked for this adapter.

These entry points are private Apple API. Apple provides no public source or
binary compatibility commitment for them, and using them may be incompatible
with Mac App Store review policy. AudioHub resolves every required symbol at
runtime and fails closed if the platform capability is unavailable. This
notice is disclosure of a platform compatibility boundary, not a grant of
rights or an Apple endorsement.
