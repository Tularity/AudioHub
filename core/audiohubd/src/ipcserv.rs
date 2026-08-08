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
    methods, DaemonInfo, DaemonSettings, LatencyTarget, Mode, OpenSessionParams, PeerState,
    PermissionKind, QualityTarget, IPC_VERSION, LATENCY_AUTO, LATENCY_STOPS_MS, MODE_A, MODE_B,
    MODE_SHARE,
};
use audiohub_net::identity::{random_pin, PeerStore};

use crate::{conn, dlog, haldev, lk, DaemonInner, PairingMode, DIR_RECV, DIR_SEND};

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

/// The whole IPC surface, minus the WebSocket. Tests drive this directly so an
/// assertion exercises the SAME dispatcher a real client reaches — a test that
/// called `conn::open_session` by hand would skip the mode gate that lives here
/// and pass while the product refused nothing.
#[cfg(test)]
pub(crate) fn dispatch_for_test(
    inner: &Arc<DaemonInner>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    dispatch(inner, method, params)
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
                            // 这条分支只在「刚连上就从 store 里消失了」时走到，
                            // 那时既没有窗口也没有样本。`None` = 还没测出来，
                            // 与「测出来是 0」是两回事。
                            net_ms: None,
                            rtt_ms: None,
                            reconnecting: false,
                            // `online: true` on this arm, and the third state
                            // is only meaningful while a peer is not connected.
                            awaiting_inbound: false,
                            retry_in_s: None,
                            hal_device: None,
                            hal_reason: None,
                            display_name: String::new(),
                            // 同上：这一刻它已经不在 store 里，没有档位可报。
                            transport: Default::default(),
                            // This arm only runs when the peer vanished from
                            // the store between connecting and listing, so
                            // there is no channel to have heard a mode on.
                            // "Unknown", never "usable".
                            peer_mode: None,
                            peer_unusable: false,
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
                    haldev::refuse_using_others(haldev::effective_mode(inner), p.override_mode)
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
                // Set only when the MODE moved, which is what drives the §13
                // transition below. Any other setting changing must not close
                // anybody's sessions.
                let mut mode_changed = false;
                {
                    let mut s = lk(&inner.settings);
                    if let Some(m) = params.get("mode").and_then(Value::as_str) {
                        let m = Mode::parse(m).ok_or_else(|| {
                            anyhow::anyhow!(
                                "mode must be '{MODE_SHARE}', '{MODE_A}' or '{MODE_B}'"
                            )
                        })?;
                        mode_changed = s.mode != m;
                        changed |= mode_changed;
                        s.mode = m;
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
                    // plan §15：`latency` / `quality` **不再在这里**。
                    //
                    // 不是「忽略」而是**拒绝**：一个旧客户端（或旧脚本）传
                    // `{"latency":"300"}` 过来时，静默收下再什么都不做，正是
                    // 本项目栽过六次的那个形状——那次的原话是「`settings.latency`
                    // 从未被读过」。这里让它报错，调用方立刻知道要改用
                    // `peers.set_transport`。
                    for gone in ["latency", "quality"] {
                        if params.get(gone).is_some() {
                            anyhow::bail!(
                                "'{gone}' 已改为每对端 × 每方向的设置（plan §15）：\
                                 请用 '{}'（参数 peer / dir / latency / quality）",
                                audiohub_ipc::methods::PEERS_SET_TRANSPORT
                            );
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
                if mode_changed {
                    // plan §13 推论 2/3, run BEFORE the reply so a client that
                    // reads back `settings.get` (or `peers.list`) right after
                    // this call cannot observe the old sessions still open.
                    //
                    // `effective_mode` and not the requested one: a machine
                    // that asked for B without a driver is running A, and it is
                    // A's permissions that the sessions have to satisfy.
                    // Virtual devices are removed by the ordinary reconcile —
                    // `compute_desired` stops desiring them the moment the mode
                    // is no longer B (see `haldev::no_device_reason`).
                    conn::announce_mode(inner, haldev::effective_mode(inner));
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
            // plan §15：延迟与质量改为**每对端 × 每方向**。
            //
            // 写入口在这里做**严格校验**：`{"latency":"opus999"}` 被收下、写盘、
            // 原样回显而媒体面一个字节没变，正是本项目栽过六次的形状。
            methods::PEERS_SET_TRANSPORT => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let fp = conn::resolve_fingerprint(inner, sel)?;
                let dir = params
                    .get("dir")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'dir' ('recv' | 'send')"))?;
                if dir != DIR_RECV && dir != DIR_SEND {
                    anyhow::bail!("dir 必须是 '{DIR_RECV}'（本机收）或 '{DIR_SEND}'（本机发），收到 '{dir}'");
                }
                let mut t = lk(&inner.peer_transport).get(&fp);
                {
                    let slot = if dir == DIR_RECV { &mut t.recv } else { &mut t.send };
                    if let Some(v) = params.get("latency").and_then(Value::as_str) {
                        let parsed = LatencyTarget::parse(v).ok_or_else(|| {
                            anyhow::anyhow!(
                                "latency 必须是 '{LATENCY_AUTO}' 或档位表里的毫秒数 {:?}，收到 '{v}'",
                                LATENCY_STOPS_MS
                            )
                        })?;
                        // 存规范拼写而不是用户给的原样：旧的 "min" 与新的 "0"
                        // 是同一档，盘上留两种写法只会让下一个读者以为是两档。
                        slot.latency = parsed.as_wire();
                    }
                    if let Some(v) = params.get("quality").and_then(Value::as_str) {
                        let parsed = QualityTarget::parse(v).ok_or_else(|| {
                            let avail: Vec<String> = audiohub_ipc::transport::quality_stops()
                                .into_iter()
                                .filter(|q| q.available)
                                .map(|q| q.id)
                                .collect();
                            anyhow::anyhow!(
                                "quality '{v}' 本 build 给不了；可选 {avail:?}\
                                 （Opus 三档在档位表里可见但尚未实现）"
                            )
                        })?;
                        slot.quality = parsed.as_wire();
                    }
                }
                {
                    let mut store = lk(&inner.peer_transport);
                    store.set(&fp, t);
                    // 落盘在回包之前：一个刚写完就 `peers.list` 的客户端不许
                    // 读到旧值，一个下一刻被杀掉的 daemon 要带着新档位回来。
                    store.save(&inner.cfg_dir)?;
                }
                // **「修改即生效」的那两行。** 本地那半边灌进本机每条流的原子量，
                // 交叉的那半边推给对端。都不重开流：重开 = 新 stream_id + 新
                // media salt + JB 重建 + 重新预缓冲，听感上是一次明显的断续。
                crate::publish_targets(inner, &crate::snapshot_sessions(inner));
                conn::push_transport(inner, &fp);
                serde_json::to_value(peer_transport_view(inner, &lk(&inner.state), &fp))?
            }
            // plan §16.2「手动覆盖恒可用」的执行入口。**热切换**：不重启
            // daemon，也不要求用户改文件——P3 之前钉一个 tier 要「停 daemon →
            // 改 peer_transport.json → 起 daemon」，因为活着的 daemon 手里是
            // 那张表的内存副本，改盘对它无效。
            methods::PEERS_SET_TIER => {
                let sel = params
                    .get("peer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing 'peer'"))?;
                let fp = conn::resolve_fingerprint(inner, sel)?;
                let v = params.get("tier").and_then(Value::as_str).ok_or_else(|| {
                    anyhow::anyhow!("missing 'tier' ('auto' | 'tier0' | 'tier1' | 'tier2')")
                })?;
                // 与 §15 两个档位串同一条纪律：写入口严格校验，未知值**拒绝**
                // 而不是收下。收下一个执行不了的字符串、写盘、再原样回显，
                // 正是本仓栽过六次的那个形状。
                let tier = crate::peer_transport::TransportTier::parse(v).ok_or_else(|| {
                    anyhow::anyhow!("tier 必须是 'auto'、'tier0'、'tier1' 或 'tier2'，收到 '{v}'")
                })?;
                // 拨号策略与 tier 同一个入口、同一次落盘：两者一起决定「这台
                // 对端怎么连」，而分成两个方法就意味着两次落盘、两次拆连接，
                // 中间那一刻是一个用户从没要求过的组合（tier2 + 仍然会拨号）。
                // **并列而不是从 tier 推**：tier 2 的隧道不一定单向，单向的通路
                // 也不一定是 tier 2。
                let dial = match params.get("dial_policy").and_then(Value::as_str) {
                    Some(d) => Some(crate::peer_transport::DialPolicy::parse(d).ok_or_else(
                        || {
                            anyhow::anyhow!(
                                "dial_policy 必须是 'both'、'outbound_only' 或 \
                                 'inbound_only'，收到 '{d}'"
                            )
                        },
                    )?),
                    None => None,
                };
                // P6：URL 形态的对端地址。**与 tier / dial_policy 同一个入口**，
                // 理由同上：三者一起决定「这台对端怎么连」，分开写就会出现一个
                // 用户从没要求过的中间态（比如地址已经是 ws:// 而 tier 还是 0）。
                // 写入口严格校验：`""` 表示清除，其余必须解析得出来——收下一个
                // 拨不动的字符串再原样回显，正是本仓栽过的那个形状。
                let endpoint = match params.get("endpoint").and_then(Value::as_str) {
                    Some("") => Some(String::new()),
                    Some(u) => {
                        let parsed = crate::wsshell::WsUrl::parse(u)
                            .map_err(|e| anyhow::anyhow!("endpoint 解析失败：{e:#}"))?;
                        // 拒在写入口而不是拨号时：一个存得下、拨不动的地址会在
                        // 界面上看起来完全正常，直到用户去连它。
                        parsed.require_plaintext()?;
                        Some(u.to_string())
                    }
                    None => None,
                };
                let prev = lk(&inner.peer_transport).tier(&fp);
                let prev_dial = lk(&inner.peer_transport).dial_policy(&fp);
                let prev_endpoint = lk(&inner.peer_transport).get(&fp).endpoint;
                {
                    let mut t = lk(&inner.peer_transport).get(&fp);
                    t.transport_tier = tier.as_wire().to_string();
                    if let Some(d) = dial {
                        t.dial_policy = d.as_wire().to_string();
                    }
                    if let Some(u) = &endpoint {
                        t.endpoint = u.clone();
                        t.endpoint_reset_from = None;
                    }
                    let mut store = lk(&inner.peer_transport);
                    store.set(&fp, t);
                    // 落盘在生效之前：这一步之后连接就要被拆掉，而重连会重新
                    // 读这张表。顺序反过来，一次「改档 + 立刻崩」就会让重连
                    // 带着旧档回来，而用户看到的是新档。
                    store.save(&inner.cfg_dir)?;
                }
                let changed = prev != tier
                    || dial.is_some_and(|d| d != prev_dial)
                    || endpoint.as_ref().is_some_and(|u| *u != prev_endpoint);
                let applied = if changed {
                    conn::retier(inner, &fp)
                } else {
                    // 值没变就什么都不做：拆一条健康的连接去应用一个没有变化
                    // 的设置，是拿一次真实的断流换零收益。
                    conn::Retier::Stored
                };
                json!({
                    "fingerprint": fp,
                    "tier": tier.as_wire(),
                    "previous": prev.as_wire(),
                    "dial_policy": dial.unwrap_or(prev_dial).as_wire(),
                    "previous_dial_policy": prev_dial.as_wire(),
                    "endpoint": endpoint.clone().unwrap_or(prev_endpoint),
                    "applied": applied.as_wire(),
                })
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
        mode: s.mode,
        effective_mode: haldev::effective_mode(inner),
        remove_virtual_on_disconnect: s.remove_virtual_on_disconnect,
        mark_offline_devices: s.mark_offline_devices,
        // 档表随每次 `settings.get` 一起发：前端不许自己写一份。
        // 两边各存一份表，分歧不会有任何报错——只会有一个选不中的档。
        //
        // ⚠ **档表留下、档位走了**（plan §15）：档表是这台机器的能力，
        // 档位是用户对某一台对端某一个方向的选择，见 `PeerState::transport`。
        latency_stops_ms: LATENCY_STOPS_MS.to_vec(),
        quality_stops: audiohub_ipc::transport::quality_stops(),
        hal_capacity: capacity as u8,
        hal_used: used as u8,
    }
}

/// plan §15：一台对端的四个档位 + 它推给本机的那两个。
///
/// **两组来源分开报，绝不合并。** 本机存的那份在共享模式下对任何链路都不生效，
/// 但照存不误（切回 A/B 时它是这台对端的既有设置）；对端推来的那两个只在本机
/// 是**提供者**的会话上存在。UI 靠这两组的分离才回答得了「这个 300 是我设的
/// 还是对端要求的」——而那正是本次事故里两台机器的任何界面都答不出来的问题。
fn peer_transport_view(
    inner: &Arc<DaemonInner>,
    st: &crate::DaemonState,
    fp: &str,
) -> audiohub_ipc::PeerTransportView {
    let mine = lk(&inner.peer_transport).get(fp);
    let mut v = audiohub_ipc::PeerTransportView {
        recv: audiohub_ipc::PeerTransportDir {
            latency: mine.recv.latency.clone(),
            quality: mine.recv.quality.clone(),
            quality_reset_from: mine.recv.quality_reset_from.clone(),
            latency_reset_from: mine.recv.latency_reset_from.clone(),
        },
        send: audiohub_ipc::PeerTransportDir {
            latency: mine.send.latency.clone(),
            quality: mine.send.quality.clone(),
            quality_reset_from: mine.send.quality_reset_from.clone(),
            latency_reset_from: mine.send.latency_reset_from.clone(),
        },
        peer_rx_latency: None,
        peer_tx_quality: None,
        tier: mine.tier().as_wire().to_string(),
        tier_reset_from: mine.transport_tier_reset_from.clone(),
        dial_policy: mine.dial_policy().as_wire().to_string(),
        dial_policy_reset_from: mine.dial_policy_reset_from.clone(),
        endpoint: mine.endpoint.clone(),
        endpoint_reset_from: mine.endpoint_reset_from.clone(),
    };
    for e in st.sessions.values() {
        if e.conn.fp != fp || e.origin != crate::SessionOrigin::Peer {
            continue;
        }
        // `None` 保持 `None`：「对端没表态」与「对端要求 auto」在执行上相同、
        // 在界面上不同（「未设定 · 按自动运行」vs 一个真的被选中的档）。
        if let Some(t) = *lk(&e.pushed.rx_latency) {
            v.peer_rx_latency = Some(t.as_wire());
        }
        if let Some(t) = *lk(&e.pushed.tx_quality) {
            v.peer_tx_quality = Some(t.as_wire());
        }
    }
    v
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
            // Only from a LIVE channel (plan §13 推论 1). A dead conn's cell
            // still holds whatever the peer last said, and reporting that would
            // put a stale "usable"/"unusable" badge on an offline machine.
            let live = st
                .conns
                .get(&p.fingerprint)
                .filter(|c| c.alive.load(Ordering::SeqCst));
            let cell = live
                .map(|c| *crate::lk(&c.peer_mode))
                .unwrap_or(crate::PeerModeCell::Unheard);
            // 网络单程估计**只在连接活着时**给：拿上一次的读数冒充现在，
            // 与 `peer_mode` 那条规矩同源。
            //
            // ⚠ 这一句是**防御性**的，不是承重的，缺陷注入 I17 已经证明：
            // `ClockFilter` 挂在 `ConnShared` 上、每条连接一个新的，于是重连
            // 必然从空窗口起步，`estimate()` 恒为 `None`——陈旧值根本活不到
            // 被读出来。留着它是因为这个前提是**别处**的实现细节：哪天有人把
            // 时钟窗口提到 per-peer 复用，这一句就立刻从防御变成承重。
            // 删掉它不会有任何测试变红，所以这段注释就是它的保险丝。
            let clock = live.and_then(|c| crate::lk(&c.clock).estimate());
            PeerState {
                net_ms: clock.map(|e| e.min_rtt_us as f64 / 2000.0),
                rtt_ms: clock.map(|e| e.last_rtt_us as f64 / 1000.0),
                online: live.is_some(),
                peer_mode: cell.mode(),
                peer_unusable: cell.unusable(),
                reconnecting,
                // The third state, and it is only meaningful while the peer is
                // not connected: an inbound-only peer with a live channel is
                // simply online, and reporting both would ask the UI to choose.
                awaiting_inbound: live.is_none()
                    && !lk(&inner.peer_transport).dial_policy(&p.fingerprint).may_dial(),
                retry_in_s,
                hal_device: hal.peer_device(&p.fingerprint),
                hal_reason: hal.reasons.get(&p.fingerprint).cloned(),
                display_name: hal
                    .display
                    .get(&p.fingerprint)
                    .cloned()
                    .unwrap_or_else(|| haldev::base_name(p)),
                transport: peer_transport_view(inner, &st, &p.fingerprint),
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
