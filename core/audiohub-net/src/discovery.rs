//! LAN discovery: one mDNS service type, announced by every daemon that has
//! not been told to keep quiet, browsed on demand.
//!
//! # What goes on the wire, and why the fingerprint is on it
//!
//! The announcement carries the instance name (this machine's human name), the
//! control port, and a TXT record of `v` / `fp` / `name` / `port`. `fp` is the
//! **identity fingerprint** — a hash of this machine's ed25519 PUBLIC key (see
//! `identity.rs`), never the secret.
//!
//! Publishing it is a deliberate decision, not an oversight, and it costs
//! something: a passive listener on the same segment learns that this machine
//! runs AudioHub and gets a stable identifier for it that survives DHCP.
//! Three reasons it is still the right shape:
//!
//!  1. **It is not a secret in the first place.** Anyone who can open a TCP
//!     connection to the control port receives the same public key in the Noise
//!     handshake, unauthenticated, before any pairing check. Withholding it
//!     from the TXT record would hide it from nobody who wanted it.
//!  2. **Two things the interface promises are computed from it**: which entry
//!     in the scan list is *this machine* (filtered out — see [`browse`]), and
//!     which are **already paired** (the 已配对 tag, `PeerStore::find`).
//!     Without `fp` neither question has an answer: names are not unique and
//!     addresses change, so a truncated or rotating value degrades both from
//!     "correct" to "usually right", which is the worse failure — it is wrong
//!     silently.
//!  3. **The cost has an off switch that is honoured** (`discovery_announce`,
//!     default on). A user who does not want to be enumerable turns announcing
//!     off and keeps every other capability: manual IP pairing, incoming
//!     connections, and browsing others all work with it off, because none of
//!     them reads our own announcement.
//!
//! What is deliberately NOT published: no key material, no session state, no
//! device names, no peer list. The record says "an AudioHub is here, this is
//! what to call it and where to knock" and nothing else.
//!
//! # Why `mdns-sd` on both platforms
//!
//! It is a pure-Rust responder + browser already in this crate's dependency
//! set, so this costs no new dependency on either platform. The alternatives
//! were worse in opposite directions: macOS Bonjour would bind us to a system
//! daemon and its entitlements, and Windows has no system responder for
//! arbitrary service types at all, so it would have to be `mdns-sd` there
//! regardless — and then the two platforms would be two code paths with two
//! sets of bugs for one behaviour.

use crate::identity::{LocalIdentity, PeerStore};
use anyhow::{Context, Result};
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_audiohub._udp.local.";

pub struct AnnounceGuard {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for AnnounceGuard {
    fn drop(&mut self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
        let _ = self.daemon.shutdown();
    }
}

fn local_host_name() -> String {
    let h = crate::identity::local_hostname();
    let base = h.trim_end_matches('.');
    let base = base.strip_suffix(".local").unwrap_or(base);
    format!("{base}.local.")
}

pub fn announce(id: &LocalIdentity, port: u16) -> Result<AnnounceGuard> {
    let daemon = ServiceDaemon::new().context("start mdns daemon")?;
    let host = local_host_name();
    let port_str = port.to_string();
    let props = [
        ("v", "1"),
        ("fp", id.fingerprint.as_str()),
        ("name", id.name.as_str()),
        ("port", port_str.as_str()),
    ];
    let info = ServiceInfo::new(SERVICE_TYPE, &id.name, &host, "", port, &props[..])
        .context("build mdns service info")?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    // mdns-sd swallows per-interface 5353 bind failures inside its daemon
    // thread; without this confirmation a broken environment "succeeds" with
    // zero sockets. Wait for the daemon to actually announce us.
    let monitor = daemon.monitor().context("monitor mdns daemon")?;
    daemon.register(info).context("register mdns service")?;
    let confirm_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let now = Instant::now();
        if now >= confirm_deadline {
            let _ = daemon.shutdown();
            anyhow::bail!("mdns announce not confirmed within 3s (no usable 5353 socket?)");
        }
        match monitor.recv_timeout(confirm_deadline - now) {
            Ok(DaemonEvent::Announce(name, _)) if name == fullname => break,
            Ok(DaemonEvent::Error(e)) => {
                let _ = daemon.shutdown();
                anyhow::bail!("mdns daemon error during announce: {e}");
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    Ok(AnnounceGuard { daemon, fullname })
}

#[derive(Debug, serde::Serialize)]
pub struct DiscoveredPeer {
    pub instance: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub fingerprint: Option<String>,
    pub name: Option<String>,
    pub paired: bool,
}

fn instance_label(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
        .unwrap_or(fullname)
        .to_string()
}

/// `self_fp` is the fingerprint of **the identity this caller runs as**, and it
/// is a parameter rather than something looked up in here.
///
/// It used to be `LocalIdentity::load_or_create()`, i.e. whatever identity the
/// ambient config dir happens to hold. That was invisible while nobody
/// announced: with zero announcements on the network, "did we filter ourselves
/// out correctly" never had an observable answer. Now that every daemon
/// announces it has two:
///
///   - a daemon running on an explicit `config_dir` (tests, a second instance,
///     `AUDIOHUB_CONFIG_DIR`) announces identity A and filtered against
///     identity B, so it listed **itself** as a discovered peer;
///   - `load_or_create` *creates* `identity.json` when it is missing, so a
///     plain `discover` could mint a stray identity in the platform default
///     directory as a side effect of reading.
pub fn browse(secs: f32, store: &PeerStore, self_fp: &str) -> Result<Vec<DiscoveredPeer>> {
    let daemon = ServiceDaemon::new().context("start mdns daemon")?;
    let receiver = daemon.browse(SERVICE_TYPE).context("browse mdns")?;
    let deadline = Instant::now() + Duration::from_secs_f32(secs.max(0.0));
    let mut found: BTreeMap<String, DiscoveredPeer> = BTreeMap::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let ev = match receiver.recv_timeout(deadline - now) {
            Ok(ev) => ev,
            // The deadline, the normal exit. Asking the receiver rather than
            // matching the error variant keeps `flume` — mdns-sd's channel
            // crate, not ours — out of this crate's dependency list.
            Err(_) if !receiver.is_disconnected() => break,
            // The mDNS daemon dropped its end before we were done. Reporting
            // `Ok(vec![])` here would say "nobody is on this network", which is
            // the one answer we cannot support: we stopped listening. Anything
            // already resolved is still true, so it is only the empty case that
            // has to become an error.
            Err(_) => {
                if found.is_empty() {
                    let _ = daemon.shutdown();
                    anyhow::bail!("mdns daemon stopped before the scan finished");
                }
                break;
            }
        };
        // NOTE for the next audit: there is no `ServiceEvent::Error` to
        // forward. mdns-sd 0.13 defines exactly five browse events
        // (SearchStarted / ServiceFound / ServiceResolved / ServiceRemoved /
        // SearchStopped), and its one `DaemonEvent::Error` is raised from a
        // single call site — `register_service`, when the service TYPE is too
        // long. Our type is a compile-time constant of legal length, so that
        // arm is unreachable for us on the announce side too. Subscribing the
        // monitor here would add a branch nothing can ever take.
        if let ServiceEvent::ServiceResolved(info) = ev {
            let fingerprint = info.get_property_val_str("fp").map(str::to_string);
            if fingerprint.as_deref() == Some(self_fp) {
                continue;
            }
            let mut addrs: Vec<IpAddr> = info.get_addresses().iter().cloned().collect();
            addrs.sort();
            let paired = fingerprint
                .as_deref()
                .map(|fp| store.find(fp).is_some())
                .unwrap_or(false);
            let fullname = info.get_fullname().to_string();
            found.insert(
                fullname.clone(),
                DiscoveredPeer {
                    instance: instance_label(&fullname),
                    addrs,
                    port: info.get_port(),
                    fingerprint,
                    name: info.get_property_val_str("name").map(str::to_string),
                    paired,
                },
            );
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}
