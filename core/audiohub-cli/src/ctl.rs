use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use audiohub_ipc::{methods, IpcEndpoint, OpenSessionParams};
use audiohub_net::identity::LocalIdentity;

// exit codes per spec §5: cannot connect/auth = 3, method error = 2, other = 4
use crate::{emit_json, info, DEFAULT_PORT, EXIT_CHECK_FAILED, EXIT_NO_TRAFFIC};

#[derive(Subcommand)]
pub enum M4Cmd {
    /// run the AudioHub daemon in the foreground
    Daemon {
        /// control (TCP) + media (UDP) port
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// local IPC WebSocket port (0 = random)
        #[arg(long, default_value_t = 0)]
        ipc_port: u16,
        /// announce over mDNS while running
        #[arg(long)]
        announce: bool,
        /// stop after N seconds (0 = run until killed)
        #[arg(long, default_value_t = 0.0)]
        secs: f32,
    },
    /// control a running daemon over local IPC
    Ctl {
        #[command(subcommand)]
        cmd: CtlCmd,
    },
    /// read (and optionally set) THIS machine's default output volume
    Volume {
        /// output device by name (default: the system default output) — the
        /// only way to reach a virtual device's own control without making it
        /// the system default
        #[arg(long)]
        device: Option<String>,
        /// new scalar 0.0..=1.0
        #[arg(long)]
        set: Option<f32>,
        /// mute the target output
        #[arg(long, conflicts_with = "unmute")]
        mute: bool,
        /// unmute the target output
        #[arg(long)]
        unmute: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum CtlKind {
    Mic,
    Spk,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum CtlSource {
    Tone,
    Mic,
    /// mirror what the source machine is playing (spec-m4b §B2)
    Sysaudio,
    /// what an app played into THIS peer's virtual speaker on macOS
    /// (spec-m5b §5.4); needs the HAL bridge. The device is named after the
    /// peer and there is one per peer — the daemon resolves which ring to read
    /// from the fingerprint, so this source never names a slot or a device.
    Halspk,
}

#[derive(Subcommand)]
pub enum CtlCmd {
    /// daemon info
    Status,
    /// list paired peers with online state
    Peers,
    /// establish (verify + encrypt) control channel to a paired peer
    Connect {
        /// peer fingerprint (unique prefix allowed)
        #[arg(long)]
        peer: String,
        /// IP[:PORT] override for the peer
        #[arg(long)]
        addr: Option<String>,
    },
    /// drop the control channel to a peer and stop reconnecting to it
    Disconnect {
        /// peer fingerprint (unique prefix allowed)
        #[arg(long)]
        peer: String,
    },
    /// enable pairing mode on the daemon
    PairEnable {
        /// 6 ascii digits; random if omitted
        #[arg(long)]
        pin: Option<String>,
        /// seconds pairing mode stays active
        #[arg(long, default_value_t = 120)]
        ttl: u64,
    },
    /// disable pairing mode
    PairDisable,
    /// mDNS browse via the daemon
    Discover {
        #[arg(long, default_value_t = 5.0)]
        secs: f32,
    },
    /// open a media session with a paired peer
    Open {
        /// peer fingerprint (unique prefix allowed)
        #[arg(long)]
        peer: String,
        /// mic = consume peer's microphone; spk = send audio to peer's output
        #[arg(long, value_enum)]
        kind: CtlKind,
        /// audio source for locally-originated / provider streams
        #[arg(long, value_enum)]
        source: Option<CtlSource>,
        /// tone source frequency
        #[arg(long, default_value_t = 1000.0)]
        freq: f32,
        /// --source sysaudio: capture backend id (default "auto")
        #[arg(long)]
        backend: Option<String>,
        /// mic: also play received audio locally
        #[arg(long)]
        monitor: bool,
        /// mic: ALSO render the peer's audio into this named output device
        /// (a virtual card, so any app can select it as an input)
        #[arg(long)]
        bridge: Option<String>,
        /// mic: ALSO feed the peer's audio to the virtual microphone this
        /// peer owns, through the HAL bridge (spec-m5b §5.4)
        #[arg(long)]
        hal: bool,
        /// open the session even in mode B, where sessions are otherwise
        /// driven by the system's device selection (probe/diagnostic use)
        #[arg(long = "override")]
        override_mode: bool,
        /// receiver-side tone verification frequency (probe)
        #[arg(long, default_value_t = 1000.0)]
        verify_freq: f32,
        /// sender-side simulated loss percent (probe)
        #[arg(long, default_value_t = 0.0)]
        loss: f32,
        /// spk: mirror this side's volume onto the peer's output device
        #[arg(long)]
        volume_sync: bool,
    },
    /// close a session by id
    Close {
        #[arg(long)]
        id: u32,
    },
    /// drive the peer's output volume on a spk session (needs --volume-sync)
    SetVolume {
        #[arg(long)]
        id: u32,
        /// 0.0..=1.0
        #[arg(long)]
        scalar: f32,
        /// also mute the peer's output (omit to keep the current mute state)
        #[arg(long, conflicts_with = "unmute")]
        mute: bool,
        /// also unmute the peer's output
        #[arg(long)]
        unmute: bool,
    },
    /// list active sessions with stats
    Sessions,
    /// drive the default-device rebuild path without touching real devices
    SimulateDeviceChange {
        /// which default device to pretend changed
        #[arg(long, value_parser = ["input", "output"])]
        kind: String,
    },
    /// read the daemon's settings, or change them
    ///
    /// With no flags this is a plain read. `--mode b` is what turns the
    /// per-peer virtual devices on; the daemon owns this setting, so it
    /// survives a restart and every client sees the same value.
    ///
    /// The three modes are mutually exclusive (plan §13): `share` is the only
    /// one in which OTHER machines may use this one, and `a`/`b` are the two
    /// ways this machine uses others. Switching away from `share` closes
    /// whatever peers had open on us and tells them; switching to `share` or
    /// `a` removes every virtual device.
    Settings {
        /// "share" (be used by others), "a" (driverless system capture) or
        /// "b" (per-peer virtual devices)
        #[arg(long, value_parser = ["share", "a", "b"])]
        mode: Option<String>,
        /// remove a peer's virtual devices while it is disconnected
        #[arg(long)]
        remove_virtual_on_disconnect: Option<bool>,
        /// append "（离线）" to a disconnected peer's device names
        #[arg(long)]
        mark_offline_devices: Option<bool>,
    },
    /// plan §15：某一台对端、某一个方向的延迟与音质档。
    ///
    /// **必须能从这里写。** 在此之前档位只有 UI 一条写入路径，于是「档位到底
    /// 有没有被下发」这件事无法在不开窗口的情况下验证——而这一整条回路的失效
    /// 恰恰是无声的。
    ///
    /// 不带 `--latency` / `--quality` 就是**只读**这台对端的四个档。
    PeerTransport {
        /// fingerprint or a unique prefix of one
        #[arg(long)]
        peer: String,
        /// 本机视角：`recv` = 我收这台对端的音，`send` = 我发给它。
        ///
        /// 两个方向的执行器在**不同的机器上**（延迟在接收端的 jitter buffer，
        /// 音质在发送端的阶梯），所以这个参数没有默认值：挑一个默认方向去写
        /// 就是替用户决定了他改的是哪一半。
        #[arg(long, value_parser = ["recv", "send"])]
        dir: Option<String>,
        /// END-TO-END latency target: "auto", or one of the ladder's
        /// milliseconds (see `latency_stops_ms` in `ctl settings --json`).
        ///
        /// This is the TOTAL, network segment included — not a buffer size.
        /// `--latency 200` on a link that measures 105 ms means the daemon
        /// deliberately ADDS ~95 ms. Watch it work in
        /// `ctl status --json | jq '.latency_guard.servo.by_stream'`.
        #[arg(long)]
        latency: Option<String>,
        /// wire quality: "auto", or a stop id. A stop id spells out BOTH
        /// dimensions: `pcm<kHz>k<depth>` where depth is 16 / 24 / 32f
        /// (e.g. "pcm48k16", "pcm48k24", "pcm48k32f", "pcm32k16"). See
        /// `quality_stops` in `ctl settings --json` for the live table.
        /// The old depth-less ids ("pcm48k", …) are REFUSED, not translated:
        /// the translation had to be mirrored in the UI and one of the read
        /// paths there forgot to, so one stored value rendered two ways.
        /// Opus stops are listed but refused — this build cannot deliver them.
        #[arg(long)]
        quality: Option<String>,
    },
    /// pair with a peer that has pairing mode enabled
    Pair {
        /// host or host:port
        #[arg(long)]
        addr: String,
        /// the 6-digit PIN the other side is showing
        #[arg(long)]
        pin: String,
    },
    /// remove a pairing, its sessions and its virtual devices (both sides)
    Unpair {
        /// fingerprint or a unique prefix of one
        #[arg(long)]
        peer: String,
    },
    /// give a peer a local name; its virtual devices are renamed in place
    Alias {
        #[arg(long)]
        peer: String,
        /// omit to clear the alias and fall back to the peer's computer name
        #[arg(long)]
        alias: Option<String>,
    },
    /// stop the daemon
    Shutdown,
}

pub fn dispatch(cmd: M4Cmd, json: bool) -> Result<i32> {
    match cmd {
        M4Cmd::Daemon {
            port,
            ipc_port,
            announce,
            secs,
        } => cmd_daemon(port, ipc_port, announce, secs, json),
        M4Cmd::Ctl { cmd } => cmd_ctl(cmd, json),
        M4Cmd::Volume { device, set, mute, unmute } => {
            cmd_volume(device, set, mute, unmute, json)
        }
    }
}

/// Local device volume, no daemon involved: the same audiohub-core calls the
/// provider side of a volume_sync'd session uses.
fn cmd_volume(
    device: Option<String>,
    set: Option<f32>,
    mute: bool,
    unmute: bool,
    json: bool,
) -> Result<i32> {
    use audiohub_core::volume;
    let dev = device.as_deref();
    if let Some(s) = set {
        if !(0.0..=1.0).contains(&s) {
            return Err(anyhow!("--set must be within 0.0..=1.0"));
        }
        volume::set_output_volume(dev, s)?;
    }
    if mute || unmute {
        volume::set_output_mute(dev, mute)?;
    }
    let v = volume::get_output_volume(dev)?;
    match dev {
        // The no --device line and object are what the regression suite parses;
        // a named device only ever adds to them.
        None => {
            info(&format!(
                "output volume scalar={:.3} muted={} adjustable={}",
                v.scalar, v.muted, v.adjustable
            ));
            emit_json(json, &v);
        }
        Some(name) => {
            info(&format!(
                "output volume device={name:?} scalar={:.3} muted={} adjustable={}",
                v.scalar, v.muted, v.adjustable
            ));
            emit_json(
                json,
                &json!({
                    "device": name,
                    "scalar": v.scalar,
                    "muted": v.muted,
                    "adjustable": v.adjustable,
                }),
            );
        }
    }
    Ok(0)
}

fn cmd_daemon(port: u16, ipc_port: u16, announce: bool, secs: f32, json: bool) -> Result<i32> {
    #[allow(unused_mut)]
    let mut handle = audiohubd::start_daemon(audiohubd::DaemonCfg {
        control_port: port,
        ipc_port,
        config_dir: None, // resolve via AUDIOHUB_CONFIG_DIR / platform default, same as ctl
        announce,
        hal_bridge: None, // production: AUDIOHUB_HAL_BRIDGE decides
    })?;
    info(&format!(
        "daemon running: control_port={port} ipc_port={} config_dir={}",
        handle.ipc_port,
        LocalIdentity::config_dir().display()
    ));
    emit_json(
        json,
        &json!({"ok": true, "ipc_port": handle.ipc_port, "control_port": port}),
    );

    let deadline = (secs > 0.0).then(|| Instant::now() + Duration::from_secs_f32(secs));
    let ipc_json = LocalIdentity::config_dir().join("ipc.json");
    let mut seen_ipc_json = false;
    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                info("--secs elapsed; shutting down");
                break;
            }
        }
        // the daemon deletes ipc.json on exit (e.g. ctl shutdown) — stop waiting then
        if ipc_json.exists() {
            seen_ipc_json = true;
        } else if seen_ipc_json {
            info("daemon exited (ipc.json removed)");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = handle.shutdown();
    Ok(0)
}

fn cmd_ctl(cmd: CtlCmd, json: bool) -> Result<i32> {
    let (method, params) = request_for(&cmd)?;
    // read timeout must outlast daemon-side blocking work (discover browse, connect+handshake)
    let read_timeout = match &cmd {
        CtlCmd::Discover { secs } => Duration::from_secs_f32(secs + 20.0),
        // A pair is a full SPAKE2 exchange against another machine; it needs
        // the same room a connect does.
        CtlCmd::Connect { .. } | CtlCmd::Open { .. } | CtlCmd::Pair { .. } => {
            Duration::from_secs(30)
        }
        _ => Duration::from_secs(15),
    };
    let mut client = match IpcClient::connect(read_timeout) {
        Ok(c) => c,
        Err(e) => {
            info(&format!("ipc connect failed: {e:#}"));
            return Ok(EXIT_NO_TRAFFIC);
        }
    };
    let code = match client.call(method, params) {
        Ok(result) => {
            summarize(&cmd, &result);
            emit_json(json, &result);
            0
        }
        Err(CallError::Method(msg)) => {
            info(&format!("{method} error: {msg}"));
            EXIT_CHECK_FAILED
        }
        Err(CallError::Transport(_)) if matches!(cmd, CtlCmd::Shutdown) => {
            // daemon may drop the connection while dying — request was delivered
            info("connection closed during shutdown (assuming daemon exited)");
            emit_json(json, &json!({}));
            0
        }
        Err(CallError::Transport(e)) => return Err(e),
    };
    client.close();
    Ok(code)
}

fn validate_pin(pin: &str) -> Result<()> {
    if pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(anyhow!("--pin must be exactly 6 ascii digits"))
    }
}

fn request_for(cmd: &CtlCmd) -> Result<(&'static str, Value)> {
    Ok(match cmd {
        CtlCmd::Status => (methods::DAEMON_STATUS, json!({})),
        CtlCmd::Peers => (methods::PEERS_LIST, json!({})),
        CtlCmd::Connect { peer, addr } => {
            let mut p = json!({ "peer": peer });
            if let Some(a) = addr {
                p["addr"] = json!(a);
            }
            (methods::PEERS_CONNECT, p)
        }
        CtlCmd::PairEnable { pin, ttl } => {
            let mut p = json!({ "ttl_s": ttl });
            if let Some(pin) = pin {
                validate_pin(pin)?;
                p["pin"] = json!(pin);
            }
            (methods::PAIRING_ENABLE, p)
        }
        CtlCmd::PairDisable => (methods::PAIRING_DISABLE, json!({})),
        CtlCmd::Discover { secs } => (methods::DISCOVER_RUN, json!({ "secs": secs })),
        CtlCmd::Disconnect { peer } => (methods::PEERS_DISCONNECT, json!({ "peer": peer })),
        CtlCmd::Open {
            peer,
            kind,
            source,
            freq,
            backend,
            monitor,
            bridge,
            hal,
            override_mode,
            verify_freq,
            loss,
            volume_sync,
        } => {
            if backend.is_some() && !matches!(source, Some(CtlSource::Sysaudio)) {
                return Err(anyhow!("--backend is only meaningful with --source sysaudio"));
            }
            if bridge.is_some() && !matches!(kind, CtlKind::Mic) {
                return Err(anyhow!("--bridge is only meaningful with --kind mic"));
            }
            if *hal && !matches!(kind, CtlKind::Mic) {
                return Err(anyhow!("--hal is only meaningful with --kind mic"));
            }
            let params = OpenSessionParams {
                peer: peer.clone(),
                kind: match kind {
                    CtlKind::Mic => audiohub_ipc::KIND_MIC,
                    CtlKind::Spk => audiohub_ipc::KIND_SPK,
                }
                .to_string(),
                source: source.map(|s| {
                    match s {
                        CtlSource::Tone => audiohub_ipc::SOURCE_TONE,
                        CtlSource::Mic => audiohub_ipc::SOURCE_MIC,
                        CtlSource::Sysaudio => audiohub_ipc::SOURCE_SYSAUDIO,
                        CtlSource::Halspk => audiohub_ipc::SOURCE_HAL_SPEAKER,
                    }
                    .to_string()
                }),
                freq: Some(*freq),
                backend: backend.clone(),
                monitor: *monitor,
                verify_freq: Some(*verify_freq),
                simulate_loss_pct: Some(*loss),
                volume_sync: *volume_sync,
                bridge: bridge.clone(),
                hal: *hal,
                // NOT defaulted to true: `ctl` is a UI too, and in mode B a
                // session opened by peer instead of by device selection is the
                // thing mode B exists to remove. Probes ask for it explicitly.
                override_mode: *override_mode,
            };
            (methods::SESSION_OPEN, serde_json::to_value(params)?)
        }
        CtlCmd::Close { id } => (methods::SESSION_CLOSE, json!({ "id": id })),
        CtlCmd::SetVolume { id, scalar, mute, unmute } => {
            if !(0.0..=1.0).contains(scalar) {
                return Err(anyhow!("--scalar must be within 0.0..=1.0"));
            }
            let mut p = json!({ "id": id, "scalar": scalar });
            // no flag = leave the peer's mute state alone (the daemon reuses
            // whatever the session last knew)
            if *mute || *unmute {
                p["muted"] = json!(*mute);
            }
            (methods::SESSION_SET_VOLUME, p)
        }
        CtlCmd::Sessions => (methods::SESSION_LIST, json!({})),
        CtlCmd::SimulateDeviceChange { kind } => (
            methods::DAEMON_SIMULATE_DEVICE_CHANGE,
            json!({ "kind": kind }),
        ),
        // 传输档位（`latency` / `quality`）**必须能从这里写**。
        //
        // 在此之前它们只有 UI 一条写入路径，于是「档位到底有没有被下发」这件事
        // 无法在不开窗口的情况下验证——而这一整条回路的失效恰恰是无声的。
        // 用户实测 `ctl settings --latency 200` 报 `unexpected argument`，
        // 那正是这两行缺失的样子。
        //
        // **不在本地做档位校验**，尽管 `LATENCY_STOPS_MS` 就在 `audiohub-ipc`
        // 里、抬手可及。理由是版本错位：CLI 与 daemon 是两个可以独立更新的
        // 二进制，一个旧 CLI 拿自己那份旧表去挡，会把一个 daemon 明明支持的
        // 新档位判死，而错误信息还会言之凿凿地列出一张过时的表。校验权归
        // daemon 独有（`ipcserv.rs` 的 `LatencyTarget::parse`），它同时也是
        // UI / 任何第三方客户端唯一的那道关——多一道本地关不会更安全，
        // 只会多一处可以与它分歧的地方。
        CtlCmd::Settings {
            mode,
            remove_virtual_on_disconnect,
            mark_offline_devices,
        } => {
            let mut p = serde_json::Map::new();
            if let Some(m) = mode {
                p.insert("mode".into(), json!(m));
            }
            if let Some(v) = remove_virtual_on_disconnect {
                p.insert("remove_virtual_on_disconnect".into(), json!(v));
            }
            if let Some(v) = mark_offline_devices {
                p.insert("mark_offline_devices".into(), json!(v));
            }
            // A read and a write are the same call with no fields to change,
            // so `settings` with no flags cannot accidentally write anything.
            let method = if p.is_empty() {
                methods::SETTINGS_GET
            } else {
                methods::SETTINGS_SET
            };
            (method, Value::Object(p))
        }
        CtlCmd::PeerTransport { peer, dir, latency, quality } => {
            if latency.is_none() && quality.is_none() {
                // 只读：走 peers.list，回包里挑这一台。写一个「读」的专用方法
                // 只会多一处可以与 `peers.list` 分歧的地方。
                (methods::PEERS_LIST, json!({}))
            } else {
                let dir = dir.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "写档位必须带 --dir recv|send：两个方向的执行器在不同的机器上，\
                         挑一个默认方向就是替你决定了改的是哪一半"
                    )
                })?;
                let mut p = serde_json::Map::new();
                p.insert("peer".into(), json!(peer));
                p.insert("dir".into(), json!(dir));
                if let Some(v) = latency {
                    p.insert("latency".into(), json!(v));
                }
                if let Some(v) = quality {
                    p.insert("quality".into(), json!(v));
                }
                (methods::PEERS_SET_TRANSPORT, Value::Object(p))
            }
        }
        CtlCmd::Pair { addr, pin } => {
            (methods::PEERS_PAIR, json!({ "addr": addr, "pin": pin }))
        }
        CtlCmd::Unpair { peer } => (methods::PEERS_UNPAIR, json!({ "peer": peer })),
        CtlCmd::Alias { peer, alias } => (
            methods::PEERS_SET_ALIAS,
            match alias {
                Some(a) => json!({ "peer": peer, "alias": a }),
                None => json!({ "peer": peer, "alias": null }),
            },
        ),
        CtlCmd::Shutdown => (methods::DAEMON_SHUTDOWN, json!({})),
    })
}

enum CallError {
    Method(String),
    Transport(anyhow::Error),
}

struct IpcClient {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl IpcClient {
    fn connect(read_timeout: Duration) -> Result<IpcClient> {
        let path = LocalIdentity::config_dir().join("ipc.json");
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read {} (is the daemon running?)", path.display()))?;
        let ep: IpcEndpoint = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", path.display()))?;
        if ep.ipc_version != audiohub_ipc::IPC_VERSION {
            bail!(
                "ipc.json version {} != supported {}",
                ep.ipc_version,
                audiohub_ipc::IPC_VERSION
            );
        }
        let (mut ws, _resp) = tungstenite::connect(format!("ws://127.0.0.1:{}/", ep.port))
            .with_context(|| format!("websocket connect to 127.0.0.1:{}", ep.port))?;
        match ws.get_ref() {
            MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(read_timeout))?,
            _ => {}
        }
        ws.send(Message::Text(json!({ "auth": ep.token }).to_string()))
            .context("send auth frame")?;
        let hello = read_json(&mut ws).context("read auth response")?;
        if hello.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!("auth rejected: {hello}");
        }
        Ok(IpcClient { ws })
    }

    fn call(&mut self, method: &str, params: Value) -> std::result::Result<Value, CallError> {
        self.ws
            .send(Message::Text(
                json!({"id": 1, "method": method, "params": params}).to_string(),
            ))
            .map_err(|e| CallError::Transport(e.into()))?;
        loop {
            let v = read_json(&mut self.ws).map_err(CallError::Transport)?;
            if v.get("id").and_then(Value::as_u64) != Some(1) {
                continue; // unsolicited event frame — not ours
            }
            return if v.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(CallError::Method(
                    v.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                ))
            };
        }
    }

    fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}

fn read_json(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<Value> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).context("parse ipc frame"),
            Ok(Message::Close(_)) => bail!("ipc connection closed"),
            Ok(_) => {} // ping/pong/binary — ignore
            Err(e) => return Err(anyhow!("ipc read: {e}")),
        }
    }
}

fn val_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("?")
}

fn val_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn val_f64(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(f64::NAN)
}

fn val_bool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn summarize(cmd: &CtlCmd, v: &Value) {
    match cmd {
        CtlCmd::Status => {
            info(&format!(
                "daemon {} fp={} control_port={} uptime={:.1}s ipc_version={}",
                val_str(v, "name"),
                val_str(v, "fingerprint"),
                val_u64(v, "control_port"),
                val_f64(v, "uptime_s"),
                val_u64(v, "ipc_version"),
            ));
            let outs = v
                .get("output_devices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            info(&format!(
                "output devices: {}",
                outs.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
            for c in v
                .get("virtual_cards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                info(&format!(
                    "virtual card {:<12} present={} kind={} name={}",
                    val_str(&c, "id"),
                    val_bool(&c, "present"),
                    val_str(&c, "kind"),
                    val_str(&c, "name"),
                ));
            }
        }
        CtlCmd::Peers => {
            let items = v.as_array().cloned().unwrap_or_default();
            for p in &items {
                let retry = match (val_bool(p, "reconnecting"), p.get("retry_in_s")) {
                    (true, Some(s)) if s.is_number() => {
                        format!(" reconnecting(retry in {:.1}s)", val_f64(p, "retry_in_s"))
                    }
                    (true, _) => " reconnecting".to_string(),
                    _ => String::new(),
                };
                info(&format!(
                    "peer {} fp={} addr={}:{} online={}{}",
                    val_str(p, "name"),
                    val_str(p, "fingerprint"),
                    p.get("last_addr").and_then(Value::as_str).unwrap_or("-"),
                    val_u64(p, "port"),
                    val_bool(p, "online"),
                    retry,
                ));
            }
            info(&format!("{} paired peer(s)", items.len()));
        }
        CtlCmd::Disconnect { .. } => info(&format!(
            "disconnected {} (reconnect loop stopped)",
            val_str(v, "fingerprint")
        )),
        CtlCmd::Connect { .. } => info(&format!(
            "connected {} ({}) addr={}:{} online={}",
            val_str(v, "name"),
            val_str(v, "fingerprint"),
            v.get("last_addr").and_then(Value::as_str).unwrap_or("-"),
            val_u64(v, "port"),
            val_bool(v, "online"),
        )),
        CtlCmd::PairEnable { .. } => {
            eprintln!("[audiohub] ==========================");
            eprintln!("[audiohub]        PIN: {}", val_str(v, "pin"));
            eprintln!("[audiohub] ==========================");
            info("pairing mode enabled");
        }
        CtlCmd::PairDisable => info("pairing mode disabled"),
        CtlCmd::Discover { .. } => {
            let items = v.as_array().cloned().unwrap_or_default();
            for p in &items {
                info(&format!(
                    "found instance={} name={:?} fp={:?} addrs={} port={} paired={}",
                    val_str(p, "instance"),
                    p.get("name").and_then(Value::as_str),
                    p.get("fingerprint").and_then(Value::as_str),
                    p.get("addrs").map(|a| a.to_string()).unwrap_or_default(),
                    val_u64(p, "port"),
                    val_bool(p, "paired"),
                ));
            }
            info(&format!("{} peer(s) discovered", items.len()));
        }
        CtlCmd::Open { .. } => info(&format!(
            "session {} {}/{} peer={}({}) rate={} ch={}",
            val_u64(v, "id"),
            val_str(v, "kind"),
            val_str(v, "dir"),
            val_str(v, "peer_name"),
            val_str(v, "peer_fingerprint"),
            val_u64(v, "sample_rate"),
            val_u64(v, "channels"),
        )),
        CtlCmd::Close { id } => info(&format!("closed session {id}")),
        CtlCmd::SetVolume { id, scalar, .. } => {
            info(&format!("session {id} volume request sent (scalar {scalar:.3})"))
        }
        CtlCmd::Sessions => {
            let items = v.as_array().cloned().unwrap_or_default();
            for s in &items {
                let st = s.get("stats").cloned().unwrap_or(Value::Null);
                let mut extra = String::new();
                if let Some(vd) = st.get("verdict").filter(|x| !x.is_null()) {
                    extra.push_str(&format!(
                        " verify[{:.0}Hz detected={} snr={:.1}dB]",
                        val_f64(vd, "freq_hz"),
                        val_bool(vd, "detected"),
                        val_f64(vd, "snr_db"),
                    ));
                }
                if let Some(vol) = st.get("volume").filter(|x| !x.is_null()) {
                    extra.push_str(&format!(
                        " volume[{:.3} muted={} adjustable={}]",
                        val_f64(vol, "scalar"),
                        val_bool(vol, "muted"),
                        val_bool(vol, "adjustable"),
                    ));
                }
                if let Some(mv) = st.get("mix_verdicts").and_then(Value::as_array) {
                    let parts: Vec<String> = mv
                        .iter()
                        .map(|vd| {
                            format!("{:.0}Hz:{}", val_f64(vd, "freq_hz"), val_bool(vd, "detected"))
                        })
                        .collect();
                    extra.push_str(&format!(" mix[{}]", parts.join(",")));
                }
                info(&format!(
                    "session {} {}/{} peer={} recv={} lost={} loss={:.2}% jitter={:.2}ms jb={} sent={} rung={} rung_changes={}{}",
                    val_u64(s, "id"),
                    val_str(s, "kind"),
                    val_str(s, "dir"),
                    val_str(s, "peer_name"),
                    val_u64(&st, "received"),
                    val_u64(&st, "lost"),
                    val_f64(&st, "loss_pct"),
                    val_f64(&st, "jitter_ms"),
                    val_u64(&st, "jb_depth_frames"),
                    val_u64(&st, "sent_packets"),
                    val_u64(&st, "rung"),
                    val_u64(&st, "rung_changes"),
                    extra,
                ));
            }
            info(&format!("{} session(s)", items.len()));
        }
        CtlCmd::SimulateDeviceChange { kind } => info(&format!(
            "simulated default-{kind} device change (epoch {})",
            v.get("epoch").and_then(|e| e.as_u64()).unwrap_or(0)
        )),
        CtlCmd::Settings { .. } => {
            info(&format!(
                "mode={} effective_mode={} remove_virtual_on_disconnect={} \
                 mark_offline_devices={} virtual devices {}/{}",
                val_str(v, "mode"),
                val_str(v, "effective_mode"),
                val_bool(v, "remove_virtual_on_disconnect"),
                val_bool(v, "mark_offline_devices"),
                val_u64(v, "hal_used"),
                val_u64(v, "hal_capacity"),
            ));
            // plan §15：延迟与音质**不再是全局设置**，所以这里不再印它们。
            // 印一个「代表值」正是 §14 裁定 1 那个「不管取哪条都在替另一条
            // 撒谎」的命令行版本——每对端两个方向，一共四个，它们互不相等。
            info("  (延迟与音质已改为每对端 × 每方向：`ctl peer-transport --peer <fp>` 读，\
                  加 --dir recv|send --latency/--quality 写)");
            if val_str(v, "mode") == "b" && val_str(v, "effective_mode") != "b" {
                info("  (mode B is not in force: no HAL bridge on this daemon)");
            }
            // The one fact a `share`-mode reader most needs and cannot infer
            // from the line above: this machine will refuse `ctl open`.
            if val_str(v, "effective_mode") == "share" {
                info("  (share mode: this machine serves peers and does not open sessions of \
                      its own — plan §13)");
            }
        }
        CtlCmd::PeerTransport { peer, dir, latency, quality } => {
            if latency.is_none() && quality.is_none() {
                // 回包是 peers.list：挑出这一台，四个档位并排印出来。
                let row = v
                    .as_array()
                    .and_then(|a| {
                        a.iter().find(|p| {
                            val_str(p, "fingerprint").starts_with(peer.as_str())
                        })
                    })
                    .cloned()
                    .unwrap_or(Value::Null);
                if row.is_null() {
                    info(&format!("no paired peer matching '{peer}'"));
                    return;
                }
                let t = row.get("transport").cloned().unwrap_or(Value::Null);
                let cell = |d: &str, k: &str| -> String {
                    t.get(d)
                        .and_then(|x| x.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or("—")
                        .to_string()
                };
                info(&format!(
                    "{} ({})",
                    val_str(&row, "display_name"),
                    val_str(&row, "fingerprint")
                ));
                info(&format!(
                    "  recv (this machine receives): latency={} quality={}",
                    cell("recv", "latency"),
                    cell("recv", "quality")
                ));
                info(&format!(
                    "  send (this machine sends):    latency={} quality={}",
                    cell("send", "latency"),
                    cell("send", "quality")
                ));
                // 对端推来的两个：**只有本机是提供者时才有**，而且必须与上面
                // 四个分开印。合成一栏之后「这个 300 是我设的还是对端要求的」
                // 就再也答不出来。
                let pushed = |k: &str| {
                    t.get(k).and_then(Value::as_str).map(str::to_string)
                };
                match (pushed("peer_rx_latency"), pushed("peer_tx_quality")) {
                    (None, None) => {}
                    (l, q) => info(&format!(
                        "  pushed BY THIS PEER (it is the consumer): \
                         our-receive-latency={} our-send-quality={}",
                        l.unwrap_or_else(|| "未设定".into()),
                        q.unwrap_or_else(|| "未设定".into())
                    )),
                }
                info(
                    "  (延迟档是**目标**：设 300 时 daemon 会主动把缓冲填到 300，\
                     不是「只能做到这么慢」。执行器：延迟在接收端、音质在发送端)",
                );
            } else {
                let d = dir.as_deref().unwrap_or("?");
                let cell = |k: &str| {
                    v.get(if d == "recv" { "recv" } else { "send" })
                        .and_then(|x| x.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or("—")
                };
                info(&format!(
                    "{peer} {d}: latency={} quality={}",
                    cell("latency"),
                    cell("quality")
                ));
                info(
                    "  (已即时生效：本地那半边灌进每条流的原子量，交叉的那半边推给对端。\
                     `ctl status --json | jq '.latency_guard.servo.by_stream'` 看回路)",
                );
            }
        }
        CtlCmd::Pair { addr, .. } => info(&format!(
            "paired with {} at {addr} ({})",
            val_str(v, "display_name"),
            val_str(v, "fingerprint")
        )),
        CtlCmd::Unpair { .. } => info(&format!(
            "unpaired {}; its virtual devices are being removed",
            val_str(v, "fingerprint")
        )),
        CtlCmd::Alias { .. } => info(&format!(
            "{} is now shown as {:?}",
            val_str(v, "fingerprint"),
            val_str(v, "display_name")
        )),
        CtlCmd::Shutdown => info("daemon shutdown requested"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// 走**真正的命令行解析**，不是直接构造 `CtlCmd`。
    ///
    /// 这一条是承重的：`--latency` 缺失时构造出来的 `CtlCmd::Settings` 照样
    /// 可以带一个 `latency` 字段，而用户在终端里拿到的是 `unexpected argument`。
    /// 只有从 `argv` 出发才测得到那个缺口。
    fn req(args: &[&str]) -> (&'static str, Value) {
        let cli = crate::Cli::try_parse_from(args)
            .unwrap_or_else(|e| panic!("{args:?} 解析不了：{e}"));
        let crate::TopCmd::M4(M4Cmd::Ctl { cmd }) = cli.cmd else {
            panic!("{args:?} 没有落到 ctl 子命令上")
        };
        request_for(&cmd).expect("request_for")
    }

    fn settings(args: &[&str]) -> (&'static str, Value) {
        let mut v = vec!["audiohub", "ctl", "settings"];
        v.extend_from_slice(args);
        req(&v)
    }

    fn pt(args: &[&str]) -> (&'static str, Value) {
        let mut v = vec!["audiohub", "ctl", "peer-transport", "--peer", "ab12"];
        v.extend_from_slice(args);
        req(&v)
    }

    /// **`settings.set` 收得下的每一个字段，命令行都必须够得到。**
    ///
    /// 这条测试就是用户实测那个 `unexpected argument '--latency' found` 的
    /// 机械化版本。判据不是「`--latency` 在不在」——那样加第三个字段时它照旧
    /// 全绿；判据是 [`SETTINGS_WRITABLE_KEYS`] 这张契约表**逐项**都能从 argv
    /// 出发被送进 `settings.set` 的 params 里。
    #[test]
    fn every_writable_setting_is_reachable_from_the_command_line() {
        // 每个键给一个合法取值。取值本身不重要（daemon 才是校验方），
        // 重要的是这个 flag 存在、并且落进同名的 params 键。
        let sample: &[(&str, &str, Value)] = &[
            ("mode", "--mode=share", json!("share")),
            (
                "remove_virtual_on_disconnect",
                "--remove-virtual-on-disconnect=true",
                json!(true),
            ),
            ("mark_offline_devices", "--mark-offline-devices=false", json!(false)),
        ];
        for key in audiohub_ipc::SETTINGS_WRITABLE_KEYS {
            let (_, flag, want) = sample
                .iter()
                .find(|(k, _, _)| k == key)
                .unwrap_or_else(|| {
                    panic!(
                        "settings.set 收得下 '{key}'，但命令行没有对应的 flag —— \
                         这正是 `--latency` 消失了整整一轮的那个缺口"
                    )
                });
            let (method, params) = settings(&[flag]);
            assert_eq!(method, methods::SETTINGS_SET, "{flag} 没走写入路径");
            assert_eq!(
                params.get(*key),
                Some(want),
                "{flag} 没有落到 params['{key}'] 上"
            );
        }
    }

    /// **档位表里的每一档都送得出去，原样。**
    ///
    /// 本地不做校验（版本错位时旧 CLI 会挡掉 daemon 支持的新档，理由见
    /// `request_for` 里那段注释），所以这里断言的是「不篡改」：`0` 不许变成
    /// `"min"`，`200` 不许变成 `200`（数字）——daemon 读的是字符串。
    #[test]
    fn every_latency_stop_goes_out_verbatim() {
        let mut wire: Vec<String> = vec![audiohub_ipc::LATENCY_AUTO.to_string()];
        wire.extend(audiohub_ipc::LATENCY_STOPS_MS.iter().map(|m| m.to_string()));
        for v in wire {
            let (method, params) = pt(&["--dir=send", &format!("--latency={v}")]);
            assert_eq!(method, methods::PEERS_SET_TRANSPORT);
            assert_eq!(
                params.get("latency").and_then(Value::as_str),
                Some(v.as_str()),
                "档位 {v} 在路上被改写了"
            );
        }
        // 档位表以外的值同样**原样送出**：拒绝是 daemon 的事，不是 CLI 的。
        // 本地先挡的话，一个旧 CLI 会把新 daemon 的新档判死。
        let (_, params) = pt(&["--dir=recv", "--latency=137"]);
        assert_eq!(params.get("latency").and_then(Value::as_str), Some("137"));
    }

    #[test]
    fn every_quality_stop_goes_out_verbatim() {
        let mut ids: Vec<String> = vec![audiohub_ipc::QUALITY_AUTO.to_string()];
        ids.extend(
            audiohub_ipc::transport::quality_stops()
                .into_iter()
                .map(|q| q.id),
        );
        for id in ids {
            let (method, params) = pt(&["--dir=send", &format!("--quality={id}")]);
            assert_eq!(method, methods::PEERS_SET_TRANSPORT);
            assert_eq!(
                params.get("quality").and_then(Value::as_str),
                Some(id.as_str()),
                "质量档 {id} 在路上被改写了"
            );
        }
    }

    /// **plan §15：`settings` 上的两个旧 flag 必须真的消失。**
    ///
    /// 留着一个「收下但不发」的 flag 会让旧脚本继续跑、继续报成功、
    /// 什么都不发生——本项目栽过六次的那个形状。判据是 argv **解析失败**，
    /// 不是 params 里没有那个键：后者一个「解析了再丢掉」的实现照样通过。
    #[test]
    fn the_global_stop_flags_are_gone_from_settings() {
        for bad in ["--latency=200", "--quality=pcm32k16"] {
            assert!(
                crate::Cli::try_parse_from(["audiohub", "ctl", "settings", bad]).is_err(),
                "`ctl settings {bad}` 还能解析：旧脚本会继续对着空气说话"
            );
        }
    }

    /// **写档位必须点名方向。** 两个方向的执行器在不同的机器上，挑一个默认
    /// 方向就是替用户决定了他改的是哪一半——而错的那一半是静默无效的。
    #[test]
    fn writing_a_stop_without_a_direction_is_refused() {
        let cli = crate::Cli::try_parse_from([
            "audiohub", "ctl", "peer-transport", "--peer", "ab12", "--latency", "200",
        ])
        .expect("argv 本身是合法的");
        let crate::TopCmd::M4(M4Cmd::Ctl { cmd }) = cli.cmd else { panic!() };
        assert!(request_for(&cmd).is_err(), "缺 --dir 却被放行了");
    }

    /// 不带 `--latency` / `--quality` 是**只读**，走 `peers.list`，
    /// 一个字段都不写。
    #[test]
    fn peer_transport_without_stops_cannot_write_anything() {
        let (method, _) = pt(&[]);
        assert_eq!(method, methods::PEERS_LIST, "一条只读命令走了写入路径");
        // 带了 --dir 但没带任何档位，同样是只读：`--dir` 本身不是一次写入。
        let (method, _) = pt(&["--dir=send"]);
        assert_eq!(method, methods::PEERS_LIST);
    }

    /// 不带 flag 仍然是**读**，而且 params 是空的。
    ///
    /// 反向对照：没有它，一个「把所有字段都填上默认值」的实现会让
    /// `audiohub ctl settings` 这条纯查询命令悄悄改写用户的设置。
    #[test]
    fn settings_without_flags_cannot_write_anything() {
        let (method, params) = settings(&[]);
        assert_eq!(method, methods::SETTINGS_GET);
        assert_eq!(params, json!({}), "一条只读命令带上了字段");
    }

    /// 两个档位一起给时**互不吞没**，也不牵连别的字段。
    #[test]
    fn the_transport_stops_travel_together_without_inventing_fields() {
        let (method, params) = pt(&["--dir=recv", "--latency=auto", "--quality=pcm48k24"]);
        assert_eq!(method, methods::PEERS_SET_TRANSPORT);
        assert_eq!(params.get("latency").and_then(Value::as_str), Some("auto"));
        assert_eq!(params.get("quality").and_then(Value::as_str), Some("pcm48k24"));
        assert_eq!(params.get("dir").and_then(Value::as_str), Some("recv"));
        assert_eq!(params.get("peer").and_then(Value::as_str), Some("ab12"));
        // 没给的字段一个都不许出现：这是 patch 语义，凭空补一个键就是替用户
        // 改了一项他没碰过的设置——而另一个方向的档位在**另一台机器**上执行。
        assert_eq!(params.as_object().map(|o| o.len()), Some(4));
    }
}
