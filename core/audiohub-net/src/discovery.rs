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

pub fn browse(secs: f32, store: &PeerStore) -> Result<Vec<DiscoveredPeer>> {
    let my_fp = LocalIdentity::load_or_create().ok().map(|i| i.fingerprint);
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
            Err(_) => break, // timeout at deadline or channel closed
        };
        if let ServiceEvent::ServiceResolved(info) = ev {
            let fingerprint = info.get_property_val_str("fp").map(str::to_string);
            if let (Some(fp), Some(me)) = (fingerprint.as_deref(), my_fp.as_deref()) {
                if fp == me {
                    continue;
                }
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
