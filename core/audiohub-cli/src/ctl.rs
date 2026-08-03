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
