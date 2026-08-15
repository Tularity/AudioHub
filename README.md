# AudioHub

**Share audio devices between machines on a local network.** Pair two computers,
and each can use the other's default microphone and default speaker as if they
were its own.

You have two machines on one desk. The good microphone is plugged into one, the
good speakers into the other, and the meeting is on a third. Moving cables is the
usual answer. AudioHub is the other one.

It is not a conferencing app and not a music streamer. It moves raw audio between
two machines you own, over your own network, with the latency and quality knobs
exposed rather than hidden. There is no cloud component, no relay, and no NAT
traversal — if the two machines cannot reach each other, AudioHub does not route
around it.

> **Project status: pre-release, and not ready for general use.** It has never
> been packaged, signed, or published. What follows is deliberately specific
> about what works and what does not — see [Maturity](#maturity).

---

## How it works

Two deliberate non-goals shape the design:

- **No device enumeration.** AudioHub shares "the other machine's *default* input
  and *default* output". It does not show you a list of the peer's sound cards.
- **AudioHub will never ask you to switch your system output to a virtual
  device.** The usual loopback-driver workaround — point your output at a virtual
  card and lose your own monitoring — is the exact experience this project treats
  as unacceptable. System audio capture is a side tap; your output device and its
  volume stay as you left them.

There are three ways to use a pair of machines, described in full under
[Operating Modes](https://github.com/Tularity/AudioHub/wiki/Operating-Modes):

| Mode | What it does | Needs a driver |
|---|---|:---:|
| **Share** | Push this machine's microphone to a peer; play what a peer sends | No |
| **Consumer mode A** | Mirror this machine's system audio to the peer's speaker; monitor the peer's microphone in-app or bridge it to a third-party virtual sound card | No |
| **Consumer mode B** | Inject "peer's microphone / peer's speaker" into this machine's system device list, so *any* application can select them | Yes |

AudioHub runs as two processes: a background **service** that owns the devices
and the network, and a **UI** that is a thin client over local IPC. The CLI
speaks the same IPC contract. Closing the window does not stop the audio.

Discovery is automatic on the local network (mDNS), with manual `IP[:port]` entry
always available as an alternative that also works across subnets. Pairing is a
one-time PIN exchange; once it succeeds, trust is bidirectional. The default port
is **47810**, TCP and UDP sharing the same number.

### Deeper reading

The wiki is the real documentation. Each page is written to be read on its own.

| Page | What it covers |
|---|---|
| [Operating Modes](https://github.com/Tularity/AudioHub/wiki/Operating-Modes) | Share mode, mode A, mode B, and why exactly one may be active |
| [Transport Tiers](https://github.com/Tularity/AudioHub/wiki/Transport-Tiers) | Tier 0 (UDP) / Tier 1 (TCP media) / Tier 2 (single-connection multiplex) |
| [Audio Quality](https://github.com/Tularity/AudioHub/wiki/Audio-Quality) | The six-rung quality ladder and how AUTO moves along it |
| [Latency](https://github.com/Tularity/AudioHub/wiki/Latency) | Measured end-to-end latency and where the milliseconds actually go |
| [Volume](https://github.com/Tularity/AudioHub/wiki/Volume) | Why loudness and slider positions cannot both match across two machines |
| [Share Protocols](https://github.com/Tularity/AudioHub/wiki/Share-Protocols) | Audio-only AirPlay 2 reception, local playback, password and bidirectional volume behaviour |
| [Platform Notes](https://github.com/Tularity/AudioHub/wiki/Platform-Notes) | macOS permissions, Windows driver status |
| [Glossary](https://github.com/Tularity/AudioHub/wiki/Glossary) | Jitter buffer, underrun, PLC, tier, taper, and friends |

---

## Platform support

**macOS** and **Windows 10 version 2004 or later.** Nothing older is considered.

| | macOS | Windows |
|---|---|---|
| Share mode | Yes | Yes |
| Mode A (driverless) | Yes; system audio capture needs macOS 14.2+ | Yes |
| Mode B (virtual devices) | Yes — user-space plugin, ordinary signing | **Unsigned; end users cannot install it** |
| Third-party bridge card | BlackHole | VB-Cable |

### The Windows driver, stated plainly

Windows offers no user-mode audio driver framework, so a virtual audio device
requires a **kernel-mode driver**. That is a platform fact, not a choice.

The driver is written and it works: on a test-signing machine it loads, the
device pair appears in the system audio device list, audio crosses machines
through it at +57.6 dB SNR, the volume topology composes exactly once, and Driver
Verifier passes a ten-minute soak.

**It is not signed, and it will not be.** Shipping it would require an EV
certificate and attestation signing, which is out of scope for this project. On
an ordinary machine — Secure Boot on, test signing off — Windows refuses to load
it. **Windows users cannot install mode B**, and the UI greys it out. The
supported Windows path is mode A plus a third-party signed virtual sound card.

macOS has no equivalent problem: its driver is a user-space Core Audio Server
Plugin, and ordinary code signing is enough.

---

## Maturity

This section exists so nobody is misled by the fact that the rest of it works.

**What works:** share mode both directions; mode A; mode B on macOS; pairing and
discovery; the quality ladder and AUTO; volume synchronisation; audio-only
AirPlay 2 reception with fixed local playback and a classic-DACP reverse-volume
request path; the CLI and its probes.

**What does not, or is not proven:**

- **End-to-end latency is about 150 ms**, measured macOS ↔ Windows. The
  architectural floor is about **91 ms**. Sub-40 ms is not reachable in this
  shape, and an early "under 30 ms" target has been formally withdrawn. See
  [Latency](https://github.com/Tularity/AudioHub/wiki/Latency).
- **Automatic Tier 0 → Tier 1 downgrade has not been exercised between two
  machines.** It is implemented — 600 ms of UDP silence, 3 s of keepalive
  silence, or a peer announcement all trigger it, and it re-probes after an
  hour — but the trigger has only been seen in tests, never on a real link.
- **Tier 2 has never been exercised between two machines.** It is implemented and
  unproven.
- **The Windows virtual driver is unsigned** and cannot be installed by end
  users, as above. Its microphone direction has no cross-machine evidence yet.
- **No release.** The packaging chains exist and are wired up — a macOS
  driver-pkg → pkg → dmg sequence with notarisation enforced in release mode,
  and a per-machine Windows NSIS installer — but nothing has been published and
  there are no downloadable binaries. The default local build stays ad-hoc
  signed; a release build additionally requires Developer ID Application and
  Developer ID Installer identities.
- **Installing on macOS takes one administrator authorization.** The App copies
  its service to `/Library/Application Support/AudioHub/service/`, re-signs it
  with a machine-local certificate so the Local Network grant survives updates,
  and then runs it **as the interactive user** — nothing stays resident as root.
- Real iOS playback and sender-to-receiver AirPlay volume have been exercised.
  Receiver-to-sender volume requests reach the macOS 26 AirPlaySender's internal
  DACP notification, but that sender does not update its Control Center slider;
  current iOS consumption of the reverse direction still needs final device
  re-verification. Broader UI walkthroughs on both desktop platforms remain
  pending.

Measured figures throughout the wiki are dated and scoped: they are what was
observed on specific machines on a specific date, not a specification anyone
promises to meet. Where a value cannot be read, the UI prefixes the total with
**≥** and names the missing term rather than quietly substituting zero.

---

## Building

Requires **Rust** (stable), **Node 18+**, and **Python 3** (icon generation uses
only system tools beyond it). Building the macOS app bundle requires macOS.

```sh
git clone https://github.com/Tularity/AudioHub.git
cd AudioHub
cargo build --release          # service + CLI -> target/release/audiohub
```

The macOS `.app` bundle:

```sh
zsh scripts/build-app.sh
```

That wrapper refuses to run if a process is currently executing from any image
the build would overwrite — replacing a running bundle in place invalidates its
code identity and, with it, the Local Network permission the service depends on.
It then delegates to `app/build-app.sh`, which builds the frontend, builds the
service, regenerates icons, stages the service as a Tauri sidecar, and bundles.

That default build is **ad-hoc signed only**; Gatekeeper will quarantine a copy
that has been downloaded, though a locally built bundle runs fine. The release
packaging scripts take a different path and require real Developer ID identities.

Frontend only:

```sh
cd app/frontend
npm install
npm run build        # tsc -b && vite build
npm run typecheck    # tsc -b --force
npm test             # vitest
```

## Running

The UI starts the background service on demand. To drive it without the UI:

```sh
target/release/audiohub daemon           # run the service in the foreground
target/release/audiohub ctl status       # talk to a running service
target/release/audiohub ctl peers
target/release/audiohub discover         # browse mDNS for peers
target/release/audiohub ctl pair --help  # pair with a peer in pairing mode
target/release/audiohub ctl open --help  # open a media session
target/release/audiohub ctl shutdown
```

Every command accepts `--json` and will then emit exactly one JSON line on
stdout, which is what the regression scripts consume.

`audiohub probe` holds the diagnostics used to verify real audio paths —
`probe devices`, `probe tone`, `probe loopback`, `probe capture`, `probe sysaudio`
for system-audio capture backends, and `probe winvad` to drive the Windows driver
directly, bypassing both the service and the network. Run any of them with
`--help`.

---

## Repository layout

```
core/audiohub-core    device I/O, resampling, mixing, volume
core/audiohub-airplay AudioHub-owned AirPlay 2 audio ingress and local playback
core/audiohub-net     transport, tiers, crypto, discovery
core/audiohub-ipc     the local IPC contract shared by UI and CLI
core/audiohubd        the background service
core/audiohub-cli     the `audiohub` binary
app/frontend          UI (React + Vite + TypeScript)
app/src-tauri         UI shell (Tauri 2)
drivers/macos-hal     Core Audio Server Plugin (user space)
drivers/windows-vad   kernel-mode virtual audio driver (PortCls/KMDF, unsigned)
scripts/              build and safety wrappers
```

---

## License

Licensed under the [Apache License 2.0](LICENSE).

The AirPlay compatibility path includes separately identified Apple-derived
material, including a widely leaked AirPort Express RSA private key. That asset
is not AudioHub-authored Apache-2.0 source, clean-room material, an Apple
license, or MFi approval. Review [NOTICE.md](NOTICE.md) and
`core/audiohub-airplay/PROVENANCE.md` before redistribution.

## Contributing

There is no contribution process yet — this is a single-author project that has
not made a release. Issues describing a reproducible problem are welcome; please
say which platform and which mode, and include `audiohub ctl status --json`.
