<div align="center">

<img src="assets/logo.png" alt="" width="96">

# AudioHub

**Share audio devices between machines on a local network.**

[![build](https://img.shields.io/github/actions/workflow/status/Tularity/AudioHub/build.yml?branch=master&style=flat-square)](https://github.com/Tularity/AudioHub/actions/workflows/build.yml)
[![release](https://img.shields.io/github/v/release/Tularity/AudioHub?style=flat-square)](https://github.com/Tularity/AudioHub/releases)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square)](LICENSE)
![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey?style=flat-square)

</div>

Pair two computers and each can use the other's default microphone and default
speaker as if they were its own. No cloud, no relay, no NAT traversal — the two
machines talk directly over your own network or not at all.

Two decisions shape everything else:

- **No device enumeration.** AudioHub shares "the other machine's *default* input
  and *default* output". It never shows you a list of the peer's sound cards.
- **It will never ask you to switch your system output to a virtual device.** The
  usual loopback-driver workaround costs you your own monitoring. System audio
  capture here is a side tap; your output device and its volume stay as you left
  them.

## Install

Download the installer for your platform from
[Releases](https://github.com/Tularity/AudioHub/releases).

> **These builds are not signed for distribution.** The macOS disk image is
> ad-hoc signed and not notarised; the Windows installer is unsigned and carries
> an unsigned driver. Both operating systems will block them on first launch, and
> getting past that is a security decision you are making yourself. The
> [wiki](https://github.com/Tularity/AudioHub/wiki) walks through it per platform.

## Documentation

Everything about how it works — the three operating modes, the transport tiers,
latency, audio quality, volume, discovery and pairing, per-platform behaviour —
lives in the **[wiki](https://github.com/Tularity/AudioHub/wiki)**.

## Building

Requires **Rust** (stable), **Node 18+** and **Python 3**. The macOS app bundle
can only be built on macOS.

```sh
cargo build --release        # service + CLI -> target/release/audiohub
zsh scripts/build-app.sh     # macOS: .app -> .pkg -> .dmg
pwsh scripts/build-windows-installer.ps1   # Windows: NSIS installer
```

The default build is ad-hoc signed on macOS and unsigned on Windows. A
distributable macOS build additionally needs Developer ID Application and
Developer ID Installer identities plus a notary profile, and fails closed
without them.

## License

Licensed under the [Apache License 2.0](LICENSE).

The AirPlay compatibility path includes separately identified Apple-derived
material, including a widely leaked AirPort Express RSA private key. That asset
is not AudioHub-authored Apache-2.0 source, clean-room material, an Apple
license, or MFi approval. Read [NOTICE.md](NOTICE.md) and
`core/audiohub-airplay/PROVENANCE.md` before redistributing anything built here.

## Contributing

Single-author project; there is no contribution process yet. Issues describing a
reproducible problem are welcome — say which platform and which mode, and
include the output of `audiohub ctl status`.
