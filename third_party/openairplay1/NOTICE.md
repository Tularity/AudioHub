# NOTICE

The `openairplay1` source in this directory is licensed under the MIT License
(see `LICENSE`). This file records third-party material and attribution that is
not covered by that grant.

## Protocol reference — shairport-sync (MIT)

The AirPlay 1 / RAOP protocol handling was developed with reference to
[shairport-sync](https://github.com/mikebrady/shairport-sync), copyright James
Laird and Mike Brady, under the MIT License. The RTP wire details,
resend-request encoding, `_raop._tcp` TXT records, and coarse frame-stuffing
approach were verified against or adapted from that project.

## AirPort Express RSA private key — third-party Apple material

`src/airport.pem` is the RSA private key extracted from Apple's AirPort
Express. It is not original to `openairplay1` or AudioHub and is not covered by
either project's open-source license. This copy comes from shairport-sync and
is embedded solely for AirPlay 1 interoperability: answering the
`Apple-Challenge` and decrypting the AES session key for audio streamed to the
receiver.

## ALAC decoding

ALAC decoding uses the `alac` crate by Ed Barnard, dual-licensed under MIT or
Apache-2.0.
