use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap::Subcommand;

use audiohub_net::discovery::{self, AnnounceGuard};
use audiohub_net::identity::{LocalIdentity, PeerStore};
use audiohub_net::pairing::{pair_initiator, pair_responder, verify_initiator, verify_responder};

use crate::{emit_json, info, DEFAULT_PORT, EXIT_CHECK_FAILED, EXIT_NO_TRAFFIC};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand)]
pub enum M3Cmd {
    /// show/initialize local identity
    Id,
    /// browse mDNS for AudioHub peers
    Discover {
        #[arg(long, default_value_t = 5.0)]
        secs: f32,
    },
    /// register mDNS service for a while
    Announce {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = 30.0)]
        secs: f32,
    },
    /// listen for one incoming pairing
    PairListen {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = 60.0)]
        secs: f32,
        /// 6 ascii digits; random if omitted
        #[arg(long)]
        pin: Option<String>,
        /// also announce over mDNS while listening
        #[arg(long)]
        announce: bool,
    },
    /// initiate pairing with a listener
    Pair {
        /// IP[:PORT], port defaults to 47810
        #[arg(long)]
        to: String,
        #[arg(long)]
        pin: String,
        /// port the daemon for THIS config dir will listen on, advertised to
        /// the peer so it can dial back. Defaults to 47810, which is a guess:
        /// this command has no listener of its own. Pass the real one when the
        /// daemon here will not use the default, or 0 to advertise nothing.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        listen_port: u16,
    },
    /// list paired peers
    Peers,
    /// remove paired peer(s)
    Unpair {
        #[arg(long, conflicts_with = "all")]
        fingerprint: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// accept one mutual verification
    VerifyListen {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = 60.0)]
        secs: f32,
    },
    /// initiate mutual verification
    Verify {
        /// IP[:PORT], port defaults to 47810
        #[arg(long)]
        to: String,
    },
}

pub fn dispatch(cmd: M3Cmd, json: bool) -> Result<i32> {
    match cmd {
        M3Cmd::Id => cmd_id(json),
        M3Cmd::Discover { secs } => cmd_discover(secs, json),
        M3Cmd::Announce { port, secs } => cmd_announce(port, secs, json),
        M3Cmd::PairListen {
            port,
            secs,
            pin,
            announce,
        } => cmd_pair_listen(port, secs, pin, announce, json),
        M3Cmd::Pair { to, pin, listen_port } => cmd_pair(&to, &pin, listen_port, json),
        M3Cmd::Peers => cmd_peers(json),
        M3Cmd::Unpair { fingerprint, all } => cmd_unpair(fingerprint, all, json),
        M3Cmd::VerifyListen { port, secs } => cmd_verify_listen(port, secs, json),
        M3Cmd::Verify { to } => cmd_verify(&to, json),
    }
}

fn validate_pin(pin: &str) -> Result<()> {
    if pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(anyhow!("--pin must be exactly 6 ascii digits"))
    }
}

fn random_pin() -> String {
    audiohub_net::identity::random_pin()
}

fn parse_target(to: &str) -> Result<SocketAddr> {
    if let Ok(sa) = to.parse::<SocketAddr>() {
        return Ok(sa);
    }
    if let Ok(ip) = to.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    let with_port = if to.contains(':') {
        to.to_string()
    } else {
        format!("{to}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve --to address: {to}"))
}

/// Nonblocking accept loop, 100ms poll ticks until deadline.
fn accept_until(listener: &TcpListener, deadline: Instant) -> Result<Option<(TcpStream, SocketAddr)>> {
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        match listener.accept() {
            Ok(pair) => return Ok(Some(pair)),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn cmd_id(json: bool) -> Result<i32> {
    let id = LocalIdentity::load_or_create()?;
    let dir = LocalIdentity::config_dir();
    info(&format!(
        "identity: name={} fingerprint={} config_dir={}",
        id.name,
        id.fingerprint,
        dir.display()
    ));
    emit_json(
        json,
        &serde_json::json!({
            "name": id.name,
            "fingerprint": id.fingerprint,
            "config_dir": dir.display().to_string(),
        }),
    );
    Ok(0)
}

fn cmd_discover(secs: f32, json: bool) -> Result<i32> {
    let store = PeerStore::load()?;
    info(&format!("browsing {} for {secs}s", discovery::SERVICE_TYPE));
    let peers = discovery::browse(secs, &store)?;
    for p in &peers {
        info(&format!(
            "found instance={} name={:?} fp={:?} addrs={:?} port={} paired={}",
            p.instance, p.name, p.fingerprint, p.addrs, p.port, p.paired
        ));
    }
    info(&format!("{} peer(s) discovered", peers.len()));
    emit_json(json, &peers);
    Ok(0)
}

fn cmd_announce(port: u16, secs: f32, json: bool) -> Result<i32> {
    let id = LocalIdentity::load_or_create()?;
    let guard = discovery::announce(&id, port)?;
    info(&format!(
        "announcing name={} fp={} port={port} for {secs}s",
        id.name, id.fingerprint
    ));
    std::thread::sleep(Duration::from_secs_f32(secs));
    drop(guard);
    emit_json(json, &serde_json::json!({"ok": true}));
    Ok(0)
}

fn cmd_pair_listen(
    port: u16,
    secs: f32,
    pin: Option<String>,
    do_announce: bool,
    json: bool,
) -> Result<i32> {
    let pin = match pin {
        Some(p) => {
            validate_pin(&p)?;
            p
        }
        None => random_pin(),
    };
    let id = LocalIdentity::load_or_create()?;
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    let _announce_guard: Option<AnnounceGuard> = if do_announce {
        Some(discovery::announce(&id, port)?)
    } else {
        None
    };

    eprintln!("[audiohub] ==========================");
    eprintln!("[audiohub]        PIN: {pin}");
    eprintln!("[audiohub] ==========================");
    info(&format!("pair-listen on 0.0.0.0:{port} for {secs}s"));

    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    let (mut stream, peer_addr) = match accept_until(&listener, deadline)? {
        Some(pair) => pair,
        None => {
            info("timeout: no pairing connection");
            emit_json(json, &serde_json::json!({"ok": false, "error": "timeout"}));
            return Ok(EXIT_NO_TRAFFIC);
        }
    };
    drop(listener); // one pairing per invocation
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(TCP_READ_TIMEOUT))?;
    info(&format!("pairing with {peer_addr}"));

    match pair_responder(&mut stream, &pin, &id) {
        Ok(mut outcome) => {
            // Responder persists BEFORE sending the final Ok frame (spec §2.5):
            // pair_responder leaves the Ok to us precisely so the initiator only
            // trusts a peer that has already durably recorded it.
            outcome.peer.last_addr = Some(peer_addr.ip().to_string());
            let mut store = PeerStore::load()?;
            store.upsert(outcome.peer.clone());
            store.save()?;
            audiohub_net::control::write_frame(
                &mut stream,
                &audiohub_net::control::ControlMsg::Ok {},
            )?;
            info(&format!(
                "paired with {} ({})",
                outcome.peer.name, outcome.peer.fingerprint
            ));
            emit_json(json, &serde_json::json!({"ok": true, "peer": outcome.peer}));
            Ok(0)
        }
        Err(e) => {
            info(&format!("pairing failed: {e:#}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("{e:#}")}),
            );
            Ok(EXIT_CHECK_FAILED)
        }
    }
}

fn cmd_pair(to: &str, pin: &str, listen_port: u16, json: bool) -> Result<i32> {
    validate_pin(pin)?;
    let target = parse_target(to)?;
    let id = LocalIdentity::load_or_create()?;
    info(&format!("connecting to {target}"));
    let mut stream = match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            info(&format!("connect failed: {e}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("connect failed: {e}")}),
            );
            return Ok(EXIT_NO_TRAFFIC);
        }
    };
    stream.set_read_timeout(Some(TCP_READ_TIMEOUT))?;

    // The initiator advertises the port a daemon for this config dir WOULD
    // listen on. This process has no listener, so the default is a guess, and a
    // wrong guess is not harmless: the responder records it as the address to
    // dial us back on, and when this pairing is over loopback to a daemon that
    // owns the default port the guess names that daemon's own socket. Hence
    // --listen-port, and hence the responder's refusal to record an
    // advertisement that points at itself.
    match pair_initiator(&mut stream, pin, &id, listen_port) {
        Ok(mut outcome) => {
            outcome.peer.last_addr = Some(target.ip().to_string());
            outcome.peer.port = target.port();
            let mut store = PeerStore::load()?;
            store.upsert(outcome.peer.clone());
            store.save()?;
            info(&format!(
                "paired with {} ({})",
                outcome.peer.name, outcome.peer.fingerprint
            ));
            emit_json(json, &serde_json::json!({"ok": true, "peer": outcome.peer}));
            Ok(0)
        }
        Err(e) => {
            info(&format!("pairing failed: {e:#}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("{e:#}")}),
            );
            Ok(EXIT_CHECK_FAILED)
        }
    }
}

fn cmd_peers(json: bool) -> Result<i32> {
    let store = PeerStore::load()?;
    for p in store.list() {
        info(&format!(
            "peer {} fp={} last_addr={:?} port={} added_unix={}",
            p.name, p.fingerprint, p.last_addr, p.port, p.added_unix
        ));
    }
    info(&format!("{} paired peer(s)", store.list().len()));
    emit_json(json, &store.list());
    Ok(0)
}

fn cmd_unpair(fingerprint: Option<String>, all: bool, json: bool) -> Result<i32> {
    if fingerprint.is_none() && !all {
        return Err(anyhow!("unpair requires --fingerprint FP or --all"));
    }
    let mut store = PeerStore::load()?;
    let removed = if all {
        let n = store.list().len();
        store.clear();
        n
    } else {
        let fp = fingerprint.unwrap();
        if store.remove_by_fingerprint(&fp) {
            1
        } else {
            0
        }
    };
    store.save()?;
    info(&format!("removed {removed} peer(s)"));
    emit_json(json, &serde_json::json!({"removed": removed}));
    Ok(0)
}

fn cmd_verify_listen(port: u16, secs: f32, json: bool) -> Result<i32> {
    let id = LocalIdentity::load_or_create()?;
    let store = PeerStore::load()?;
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    info(&format!("verify-listen on 0.0.0.0:{port} for {secs}s"));

    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    let (mut stream, peer_addr) = match accept_until(&listener, deadline)? {
        Some(pair) => pair,
        None => {
            info("timeout: no verify connection");
            emit_json(json, &serde_json::json!({"ok": false, "error": "timeout"}));
            return Ok(EXIT_NO_TRAFFIC);
        }
    };
    drop(listener);
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(TCP_READ_TIMEOUT))?;
    info(&format!("verifying {peer_addr}"));

    match verify_responder(&mut stream, &id, &store) {
        Ok(peer) => {
            info(&format!("verified {} ({})", peer.name, peer.fingerprint));
            emit_json(json, &serde_json::json!({"ok": true, "peer": peer}));
            Ok(0)
        }
        Err(e) => {
            info(&format!("verify failed: {e:#}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("{e:#}")}),
            );
            Ok(EXIT_CHECK_FAILED)
        }
    }
}

fn cmd_verify(to: &str, json: bool) -> Result<i32> {
    let target = parse_target(to)?;
    let id = LocalIdentity::load_or_create()?;
    let store = PeerStore::load()?;
    info(&format!("connecting to {target}"));
    let mut stream = match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            info(&format!("connect failed: {e}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("connect failed: {e}")}),
            );
            return Ok(EXIT_NO_TRAFFIC);
        }
    };
    stream.set_read_timeout(Some(TCP_READ_TIMEOUT))?;

    match verify_initiator(&mut stream, &id, &store) {
        Ok(peer) => {
            info(&format!("verified {} ({})", peer.name, peer.fingerprint));
            emit_json(json, &serde_json::json!({"ok": true, "peer": peer}));
            Ok(0)
        }
        Err(e) => {
            info(&format!("verify failed: {e:#}"));
            emit_json(
                json,
                &serde_json::json!({"ok": false, "error": format!("{e:#}")}),
            );
            Ok(EXIT_CHECK_FAILED)
        }
    }
}
