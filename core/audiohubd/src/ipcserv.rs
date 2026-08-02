//! Local IPC server: WebSocket on 127.0.0.1, token auth first frame, then
//! JSON request/response per core/audiohub-ipc/src/lib.rs (+ stats events).

use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{HandshakeError, Message, WebSocket};

use audiohub_ipc::{
    methods, DaemonInfo, DaemonSettings, OpenSessionParams, PeerState, PermissionKind, IPC_VERSION,
    MODE_A, MODE_B,
};
use audiohub_net::identity::{random_pin, PeerStore};

use crate::{conn, dlog, haldev, lk, DaemonInner, PairingMode};

pub(crate) fn accept_loop(inner: Arc<DaemonInner>, listener: TcpListener) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let i = inner.clone();
                let _ = std::thread::Builder::new()
                    .name("ahb-ipc-conn".into())
                    .spawn(move || {
                        // spec §8: one IPC client must not take the daemon down
                        if std::panic::catch_unwind(AssertUnwindSafe(|| client_thread(i, stream)))
                            .is_err()
                        {
                            dlog!("[audiohubd] ipc client panicked, connection dropped");
                        }
                    });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                dlog!("[audiohubd] ipc accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn daemon_info(inner: &DaemonInner) -> DaemonInfo {
    let (output_devices, virtual_cards) = crate::device_listing(inner);
    DaemonInfo {
        ipc_version: IPC_VERSION,
        name: inner.id.name.clone(),
        fingerprint: inner.id.fingerprint.clone(),
        control_port: inner.control_port,
        uptime_s: inner.start.elapsed().as_secs_f64(),
        output_devices,
        virtual_cards,
        // 站点级混音健康（规格 §3.5）。求和之后的量，归不到任何一条会话头上，
        // 所以走 daemon.status 而不是 SessionStats。
        mix_health: crate::build_mix_health(inner),
    }
}

/// The IPC endpoint is loopback-only but a page in a browser can still reach it
/// (WebSocket ignores same-origin policy), so a remote page the user happens to
/// visit must not be able to drive the daemon. The token is the real defense;
/// this is defense in depth against a token leak.
///
/// Absent Origin = a native client (ctl). The UI is a legitimate web client and
/// always sends one: `tauri://…` inside the shell, `http://127.0.0.1:…` in the
/// browser mode spec-ui.md §0 mandates for automated testing. Allow exactly
/// those two shapes; anything else (a real web site) is refused.
fn origin_allowed(origin: &str) -> bool {
    if origin.starts_with("tauri://") {
        return true;
    }
    let rest = match origin.split_once("://") {
        Some(("http", r)) | Some(("https", r)) => r,
        _ => return false,
    };
    let host = rest.split(':').next().unwrap_or("");
    // `tauri.localhost` is the WINDOWS webview's origin. Tauri only uses the
    // `tauri://` scheme on macOS/Linux; on Windows WebView2 serves the app from
    // `http://tauri.localhost`, so an allowlist written on a Mac silently locks
    // the Windows app out of its own daemon. Measured on the peer: the window
    // and the whole UI rendered, then sat on 「正在连接 AudioHub 服务…」 forever
    // while the daemon answered every handshake with 403.
    //
    // It is not a loopback LITERAL, but it is not routable either: WebView2
    // reserves the name internally and no DNS lookup happens, so a real site
    // cannot obtain this origin.
    host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "tauri.localhost"
}

fn reject_browser_origin(req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
    if let Some(origin) = req.headers().get("origin") {
        let ok = origin.to_str().map(origin_allowed).unwrap_or(false);
        if !ok {
            let mut err =
                ErrorResponse::new(Some("audiohub ipc rejects non-local origins".into()));
            *err.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
            return Err(err);
        }
    }
    Ok(resp)
}

fn accept_ws(stream: TcpStream) -> Option<WebSocket<TcpStream>> {
    match tungstenite::accept_hdr(stream, reject_browser_origin) {
        Ok(ws) => Some(ws),
        Err(HandshakeError::Interrupted(mut mid)) => {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(20));
                match mid.handshake() {
                    Ok(ws) => return Some(ws),
                    Err(HandshakeError::Interrupted(m)) => mid = m,
                    Err(_) => return None,
                }
            }
            None
        }
        Err(_) => None,
    }
}

/// Length-checked constant-time compare: `==` on the token leaks a prefix-match
/// oracle to any local process that can retry the handshake.
fn token_matches(given: &str, expect: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (given.as_bytes(), expect.as_bytes());
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

fn read_text(ws: &mut WebSocket<TcpStream>, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Text(t)) => return Some(t),
            Ok(Message::Close(_)) => return None,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return None,
        }
    }
    None
}

fn reply_ok(id: &Value, result: Value) -> Message {
    Message::Text(json!({"id": id, "ok": true, "result": result}).to_string())
}

fn reply_err(id: &Value, error: &str) -> Message {
    Message::Text(json!({"id": id, "ok": false, "error": error}).to_string())
}

fn client_thread(inner: Arc<DaemonInner>, stream: TcpStream) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    if stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return;
    }
    let Some(mut ws) = accept_ws(stream) else { return };

    let authed = read_text(&mut ws, Duration::from_secs(5))
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("auth").and_then(Value::as_str).map(str::to_string))
        .map_or(false, |tok| token_matches(&tok, &inner.token));
    if !authed {
        let _ = ws.send(Message::Text(
            json!({"ok": false, "error": "auth failed"}).to_string(),
        ));
        let _ = ws.close(None);
        return;
    }
    if ws
        .send(Message::Text(
            json!({"ok": true, "daemon": daemon_info(&inner)}).to_string(),
        ))
        .is_err()
    {
        return;
    }
    if ws
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(100)))
        .is_err()
    {
        return;
    }

    let mut sub: Option<(Duration, Instant)> = None;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            return;
        }
        if let Some((iv, due)) = sub.as_mut() {
            if Instant::now() >= *due {
                *due = Instant::now() + *iv;
                let infos = crate::build_session_infos(&inner);
                if ws
                    .send(Message::Text(
                        json!({"event": "stats", "data": infos}).to_string(),
                    ))
                    .is_err()
                {
                    return;
                }
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(req) = serde_json::from_str::<Value>(&t) else { continue };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
                if method == methods::STATS_SUBSCRIBE {
                    let iv = params
                        .get("interval_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(1000)
                        .max(100);
                    sub = Some((Duration::from_millis(iv), Instant::now()));
                    if ws.send(reply_ok(&id, json!({}))).is_err() {
                        return;
                    }
                    continue;
                }
                let resp = dispatch(&inner, &method, &params);
                let frame = match &resp {
                    Ok(v) => reply_ok(&id, v.clone()),
                    Err(e) => reply_err(&id, e),
                };
                if ws.send(frame).is_err() {
                    return;
                }
                if method == methods::DAEMON_SHUTDOWN && resp.is_ok() {
                    let _ = ws.flush();
                    inner.begin_shutdown();
                    let _ = ws.close(None);
                    return;
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {} // ping/pong/binary
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
    }
}

fn dispatch(inner: &Arc<DaemonInner>, method: &str, params: &Value) -> Result<Value, String> {
    let r: anyhow::Result<Value> = (|| {
        Ok(match method {
            methods::DAEMON_STATUS => crate::status_with_hal(inner, daemon_info(inner))?,
            methods::DAEMON_SHUTDOWN => json!({}), // caller triggers after replying
            // spec-m4c §D: drives the SAME rebuild path a real default-device
            // change takes (mixer playback / mic source / VolumeState
            // renegotiation) without touching any system device.
            methods::DAEMON_SIMULATE_DEVICE_CHANGE => {
                let kind = params
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'kind'"))?;
                let counter = match kind {
                    audiohub_ipc::DEVICE_INPUT => &inner.dev_in_epoch,
                    audiohub_ipc::DEVICE_OUTPUT => &inner.dev_out_epoch,
                    other => anyhow::bail!(
                        "kind must be '{}' or '{}', got '{other}'",
                        audiohub_ipc::DEVICE_INPUT,
                        audiohub_ipc::DEVICE_OUTPUT
                    ),
                };
                let epoch = counter.fetch_add(1, Ordering::Relaxed) + 1;
                dlog!("[audiohubd] simulated default {kind} device change (epoch {epoch})");
                json!({ "kind": kind, "epoch": epoch })
            }
            // First-run permission gate. Pure query: it must never raise a
            // consent dialog, because the UI paints the gate page from it and
            // polls it while the user works through the rows.
            methods::DAEMON_PERMISSIONS => {
                serde_json::to_value(audiohub_core::permissions::probe_all())?
            }
            // The ONE method that deliberately prompts. Blocks this connection
            // while the dialog is up — other IPC clients each have their own
            // thread, so the daemon stays responsive — and answers with the
            // state as it stands afterwards, which may still be "unknown" if
            // the user has not answered yet.
            methods::DAEMON_REQUEST_PERMISSION => {
                // 'id' is the UI's spelling of the same field (app/ui sends
                // {id}); accepting both keeps the two halves independent.
                let raw = params
                    .get("kind")
                    .or_else(|| params.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'kind'"))?;
                let kind = PermissionKind::parse(raw).ok_or_else(|| {
                    anyhow::anyhow!(
                        "kind must be '{}', '{}' or '{}', got '{raw}'",
                        audiohub_ipc::KIND_MICROPHONE,
                        audiohub_ipc::KIND_LOCAL_NETWORK,
                        audiohub_ipc::KIND_SYSTEM_AUDIO
                    )
                })?;
                dlog!("[audiohubd] permission request: {raw} (may prompt)");
                let attempt = audiohub_core::permissions::request(kind);
                let mut state = audiohub_core::permissions::probe_one(kind);
                // A failed attempt is not an IPC error: the state is still the
                // answer the gate needs, and dropping it would leave the UI
                // with nothing to render. The reason rides along in the note.
                if let Err(e) = attempt {
                    dlog!("[audiohubd] permission request {raw} failed: {e:#}");
                    state.note = format!("{}（本次尝试：{e:#}）", state.note);
                }
                serde_json::to_value(state)?
            }
            methods::PEERS_LIST => serde_json::to_value(peer_states(inner)?)?,
            methods::PEERS_CONNECT => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let addr = params.get("addr").and_then(Value::as_str);
                let peer = conn::connect_peer(inner, sel, addr, conn::ConnectOrigin::User)?;
                let fp = peer.fingerprint.clone();
                serde_json::to_value(
                    peer_states(inner)?
                        .into_iter()
                        .find(|p| p.peer.fingerprint == fp)
                        .unwrap_or(PeerState {
                            peer,
                            online: true,
                            reconnecting: false,
                            retry_in_s: None,
                            hal_device: None,
                            hal_reason: None,
                            display_name: String::new(),
                        }),
                )?
            }
            methods::PEERS_DISCONNECT => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let fp = conn::disconnect_peer(inner, sel)?;
                json!({ "fingerprint": fp })
            }
            methods::PAIRING_ENABLE => {
                let pin = params
                    .get("pin")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(random_pin);
                // a PIN window is a brute-force window: cap it regardless of
                // what the caller asks for
                let ttl = params
                    .get("ttl_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(120)
                    .clamp(1, conn::MAX_PAIRING_TTL_S);
                lk(&inner.state).pairing = Some(PairingMode {
                    pin: pin.clone(),
                    until: Instant::now() + Duration::from_secs(ttl),
                    fails: 0,
                    in_flight: false,
                });
                json!({ "pin": pin, "ttl_s": ttl })
            }
            methods::PAIRING_DISABLE => {
                lk(&inner.state).pairing = None;
                json!({})
            }
            methods::DISCOVER_RUN => {
                let secs = params
                    .get("secs")
                    .and_then(Value::as_f64)
                    .unwrap_or(5.0)
                    .clamp(0.5, 60.0) as f32;
                let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
                serde_json::to_value(audiohub_net::discovery::browse(secs, &store)?)?
            }
            methods::SESSION_OPEN => {
                let p: OpenSessionParams = serde_json::from_value(params.clone())?;
                // The structural half of mode B (spec-m5b §6.1): in mode B the
                // SYSTEM's device selection is what opens sessions. A UI that
                // could also open one by peer would have put the peer picker
                // back, and every mode-B property would quietly stop holding.
                if let Some(why) =
                    haldev::refuse_ui_session(haldev::effective_mode(inner), p.override_mode)
                {
                    anyhow::bail!("{why}");
                }
                serde_json::to_value(conn::open_session(inner, &p)?)?
            }
            methods::SESSION_CLOSE => {
                let id = params
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("missing 'id'"))? as u32;
                conn::close_session(inner, id)?;
                json!({})
            }
            methods::SESSION_LIST => serde_json::to_value(crate::build_session_infos(inner))?,
            methods::SESSION_SET_VOLUME => {
                let id = params
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("missing 'id'"))? as u32;
                let scalar = params
                    .get("scalar")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| anyhow::anyhow!("missing 'scalar'"))?
                    as f32;
                // Omitted 'muted' travels as None all the way to the provider,
                // which then leaves its mute control untouched. Resolving it to
                // a cached value here was the bug: before the provider's first
                // VolumeState the cache is empty, so `ctl set-volume --scalar`
                // unmuted a deliberately-muted machine.
                let muted = params.get("muted").and_then(Value::as_bool);
                conn::set_session_volume(inner, id, scalar, muted)?;
                json!({})
            }
            methods::SETTINGS_GET => serde_json::to_value(settings_view(inner))?,
            methods::SETTINGS_SET => {
                let mut changed = false;
                {
                    let mut s = lk(&inner.settings);
                    if let Some(m) = params.get("consumer_mode").and_then(Value::as_str) {
                        if m != MODE_A && m != MODE_B {
                            anyhow::bail!("consumer_mode must be '{MODE_A}' or '{MODE_B}'");
                        }
                        changed |= s.consumer_mode != m;
                        s.consumer_mode = m.to_string();
                    }
                    for (key, field) in [
                        ("remove_virtual_on_disconnect", 0u8),
                        ("mark_offline_devices", 1u8),
                    ] {
                        if let Some(v) = params.get(key).and_then(Value::as_bool) {
                            let slot = match field {
                                0 => &mut s.remove_virtual_on_disconnect,
                                _ => &mut s.mark_offline_devices,
                            };
                            changed |= *slot != v;
                            *slot = v;
                        }
                    }
                    for (key, field) in [("latency", 0u8), ("quality", 1u8)] {
                        if let Some(v) = params.get(key).and_then(Value::as_str) {
                            let slot = match field {
                                0 => &mut s.latency,
                                _ => &mut s.quality,
                            };
                            changed |= slot.as_str() != v;
                            *slot = v.to_string();
                        }
                    }
                    if changed {
                        // Persisted before it is answered: a UI that reads back
                        // what it just wrote must never see the old value, and
                        // a daemon killed a moment later must come back in the
                        // mode the user chose.
                        s.save(&inner.cfg_dir)?;
                    }
                }
                if changed {
                    dlog!("[audiohubd] settings changed; the device coordinator will reconcile");
                }
                serde_json::to_value(settings_view(inner))?
            }
            // The initiator half of M3 pairing, moved out of the CLI: a pairing
            // done in another process wrote paired_peers.json behind the
            // daemon's back, and the device coordinator only noticed on its
            // next re-read. Pairing here makes the devices appear at once.
            methods::PEERS_PAIR => {
                let addr = params
                    .get("addr")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'addr'"))?;
                let pin = params
                    .get("pin")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'pin'"))?;
                let fp = conn::pair_with(inner, addr, pin)?;
                serde_json::to_value(
                    peer_states(inner)?
                        .into_iter()
                        .find(|p| p.peer.fingerprint == fp)
                        .ok_or_else(|| anyhow::anyhow!("paired peer {fp} vanished from the store"))?,
                )?
            }
            methods::PEERS_UNPAIR => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let fp = conn::resolve_fingerprint(inner, sel)?;
                conn::forget_peer(inner, &fp);
                json!({ "fingerprint": fp })
            }
            methods::PEERS_SET_ALIAS => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let fp = conn::resolve_fingerprint(inner, sel)?;
                let alias = params
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                {
                    let _g = lk(&inner.store_lock);
                    let mut store = PeerStore::load_at(Some(&inner.cfg_dir))?;
                    if !store.set_alias(&fp, alias) {
                        anyhow::bail!("no paired peer {fp}");
                    }
                    store.save()?;
                }
                // The coordinator turns this into an in-place rename at the same
                // UID on its next pass — no new AudioObjectID, no device-list
                // change, so an application's remembered selection survives it.
                let display = peer_states(inner)?
                    .into_iter()
                    .find(|p| p.peer.fingerprint == fp)
                    .map(|p| p.display_name)
                    .unwrap_or_default();
                json!({ "fingerprint": fp, "display_name": display })
            }
            other => anyhow::bail!("unknown method '{other}'"),
        })
    })();
    r.map_err(|e| format!("{e:#}"))
}

/// `DaemonSettings` = what is stored plus what is derived. `effective_mode`,
/// `hal_capacity` and `hal_used` are computed here every time rather than
/// persisted, so the two ends cannot drift apart about which mode is live.
fn settings_view(inner: &Arc<DaemonInner>) -> DaemonSettings {
    let s = lk(&inner.settings).clone();
    let (capacity, used) = {
        let st = lk(&inner.haldev);
        (st.capacity, st.table.used())
    };
    DaemonSettings {
        consumer_mode: s.consumer_mode.clone(),
        effective_mode: haldev::effective_mode(inner).to_string(),
        remove_virtual_on_disconnect: s.remove_virtual_on_disconnect,
        mark_offline_devices: s.mark_offline_devices,
        latency: s.latency,
        quality: s.quality,
        hal_capacity: capacity as u8,
        hal_used: used as u8,
    }
}

fn peer_states(inner: &Arc<DaemonInner>) -> anyhow::Result<Vec<PeerState>> {
    let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
    let recon = crate::reconnect::snapshot(inner);
    let hal = lk(&inner.haldev);
    let st = lk(&inner.state);
    Ok(store
        .list()
        .iter()
        .map(|p| {
            let (reconnecting, retry_in_s) =
                recon.get(&p.fingerprint).copied().unwrap_or((false, None));
            PeerState {
                online: st
                    .conns
                    .get(&p.fingerprint)
                    .map_or(false, |c| c.alive.load(Ordering::SeqCst)),
                reconnecting,
                retry_in_s,
                hal_device: hal.peer_device(&p.fingerprint),
                hal_reason: hal.reasons.get(&p.fingerprint).cloned(),
                display_name: hal
                    .display
                    .get(&p.fingerprint)
                    .cloned()
                    .unwrap_or_else(|| haldev::base_name(p)),
                peer: p.clone(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;
    use audiohub_ipc::PermissionKind;

    /// The gate page reads these field names; a rename here is a UI break.
    /// Doubles as proof that probing never prompts — this runs deviceless in
    /// CI and would hang on a dialog otherwise.
    #[test]
    fn permissions_serialize_to_the_documented_shape() {
        let v = serde_json::to_value(audiohub_core::permissions::probe_all()).unwrap();
        let rows = v.as_array().expect("array");
        assert_eq!(rows.len(), 3);
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                audiohub_ipc::KIND_MICROPHONE,
                audiohub_ipc::KIND_LOCAL_NETWORK,
                audiohub_ipc::KIND_SYSTEM_AUDIO
            ]
        );
        for row in rows {
            for f in ["kind", "granted", "required", "why", "settings_url", "note"] {
                assert!(row.get(f).is_some(), "missing {f} in {row}");
            }
            assert!(row["required"].is_boolean());
            // tri-state: true / false / null, never a string
            assert!(row["granted"].is_boolean() || row["granted"].is_null());
        }
    }

    /// `daemon.request_permission` refuses anything it cannot map, so a typo
    /// can never silently prompt for the wrong thing.
    #[test]
    fn only_the_three_documented_kinds_parse() {
        for k in [
            audiohub_ipc::KIND_MICROPHONE,
            audiohub_ipc::KIND_LOCAL_NETWORK,
            audiohub_ipc::KIND_SYSTEM_AUDIO,
        ] {
            assert!(PermissionKind::parse(k).is_some(), "{k}");
        }
        assert!(PermissionKind::parse("Microphone").is_none());
        assert!(PermissionKind::parse("mic").is_none());
    }

    #[test]
    fn only_local_and_tauri_origins_pass() {
        // the shell (custom protocol) and the browser test mode spec-ui.md §0
        // mandates must both work
        assert!(origin_allowed("tauri://localhost"));
        assert!(origin_allowed("http://127.0.0.1:47994"));
        assert!(origin_allowed("http://localhost:8080"));
        assert!(origin_allowed("https://127.0.0.1"));
        // a page the user merely visits must not be able to drive the daemon
        assert!(!origin_allowed("https://evil.example"));
        assert!(!origin_allowed("http://127.0.0.1.evil.example"));
        assert!(!origin_allowed("file://"));
        assert!(!origin_allowed("null"));
        assert!(!origin_allowed(""));
    }
}
