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
    /// what an app played into the macOS virtual device "AudioHub Speaker"
    /// (spec-round2 §B2); needs the HAL bridge
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
        /// mic: ALSO feed the peer's audio to the macOS virtual device
        /// "AudioHub Microphone" through the HAL bridge (spec-round2 §B2)
        #[arg(long)]
        hal: bool,
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
        CtlCmd::Connect { .. } | CtlCmd::Open { .. } => Duration::from_secs(30),
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
        CtlCmd::Shutdown => info("daemon shutdown requested"),
    }
}
