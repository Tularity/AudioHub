//! MAC address discovery via `/sys/class/net`.
//!
//! The MAC ends up in two places that clients cross-check: the mDNS service
//! name (`<MAC>@<Name>`) and the Apple-Response signature buffer.

use std::fs;
use std::path::Path;

pub fn parse(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(mac)
}

/// The MAC of the first operationally-up non-loopback interface, falling
/// back to any non-loopback interface with a non-zero address.
pub fn discover() -> Option<[u8; 6]> {
    discover_in(Path::new("/sys/class/net"))
}

fn discover_in(net_dir: &Path) -> Option<[u8; 6]> {
    let mut candidates: Vec<(String, [u8; 6], bool)> = Vec::new();
    let entries = fs::read_dir(net_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let Some(mac) = fs::read_to_string(entry.path().join("address"))
            .ok()
            .as_deref()
            .and_then(parse)
        else {
            continue;
        };
        if mac == [0u8; 6] {
            continue;
        }
        let up = fs::read_to_string(entry.path().join("operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false);
        candidates.push((name, mac, up));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates
        .iter()
        .find(|(_, _, up)| *up)
        .or_else(|| candidates.first())
        .map(|(_, mac, _)| *mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mac() {
        assert_eq!(
            parse("aa:bb:cc:dd:ee:ff\n"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse("00:11:22:33:44:55"),
            Some([0, 0x11, 0x22, 0x33, 0x44, 0x55])
        );
        assert_eq!(parse("not-a-mac"), None);
        assert_eq!(parse("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse("aa:bb:cc:dd:ee:ff:00"), None);
    }

    #[test]
    fn prefers_up_interface_and_skips_loopback() {
        let dir = std::env::temp_dir().join(format!("openairplay-mac-test-{}", std::process::id()));
        let make = |name: &str, addr: &str, state: &str| {
            let d = dir.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("address"), addr).unwrap();
            fs::write(d.join("operstate"), state).unwrap();
        };
        make("lo", "00:00:00:00:00:00", "unknown");
        make("eth0", "aa:00:00:00:00:01", "down");
        make("wlan0", "aa:00:00:00:00:02", "up");
        assert_eq!(discover_in(&dir), Some([0xaa, 0, 0, 0, 0, 2]));
        fs::remove_dir_all(&dir).unwrap();
    }
}
