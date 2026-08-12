//! audiohubd binary: CLI per spec-m4a §1. stdout stays silent except the
//! single --json startup line; everything else goes to stderr.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// Same infallible stderr writer the daemon threads use: a failed write must
/// never panic a thread whose next statement is `shutdown()` (see lib.rs).
macro_rules! elog {
    ($($arg:tt)*) => { audiohubd::logln(format_args!($($arg)*)) };
}

fn usage() -> ! {
    elog!(
        "usage: audiohubd [--port N] [--ipc-port N] [--announce|--no-announce] [--secs N] [--json]"
    );
    std::process::exit(2);
}

fn main() {
    let mut port: u16 = 47810;
    let mut ipc_port: u16 = 0;
    // `None` = follow `settings.discovery_announce`, and it is the default
    // because this is the process the app and the autostart entries spawn with
    // no arguments at all. When announcing was a flag, that was the whole bug:
    // the shipping app never announced itself and 「同网段互见」 only ever
    // worked for someone running the CLI by hand.
    let mut announce: Option<bool> = None;
    let mut secs: f64 = 0.0;
    let mut json = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => match args.next().and_then(|v| v.parse::<u16>().ok()) {
                Some(v) => port = v,
                None => usage(),
            },
            "--ipc-port" => match args.next().and_then(|v| v.parse::<u16>().ok()) {
                Some(v) => ipc_port = v,
                None => usage(),
            },
            "--secs" => match args.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) if v >= 0.0 => secs = v,
                _ => usage(),
            },
            "--announce" => announce = Some(true),
            "--no-announce" => announce = Some(false),
            "--json" => json = true,
            _ => usage(),
        }
    }

    let handle = match audiohubd::start_daemon(audiohubd::DaemonCfg {
        control_port: port,
        ipc_port,
        config_dir: None, // env/platform default
        announce,
        hal_bridge: None, // production: AUDIOHUB_HAL_BRIDGE decides
        tx_throttle_kbps: None, // production: AUDIOHUB_TEST_TX_KBPS decides (normally unlimited)
        block_udp: None, // production: AUDIOHUB_TEST_BLOCK_UDP decides (normally nothing)
        announce_fault: false, // test-only knob; there is no production value for it
        airplay_advertise: true,
    }) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            elog!("[audiohubd] start failed: {e:#}");
            std::process::exit(1);
        }
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "ipc_port": handle.ipc_port,
                "control_port": handle.control_port,
            })
        );
        let _ = std::io::stdout().flush();
    } else {
        elog!(
            "[audiohubd] running: control_port={} ipc_port={} fp={}",
            handle.control_port, handle.ipc_port, handle.fingerprint
        );
    }

    if secs > 0.0 {
        let h = handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(secs));
            elog!("[audiohubd] --secs elapsed, shutting down");
            h.shutdown();
        });
    }
    // SIGTERM/SIGINT are handled inside start_daemon (see lib.rs): its watchdog
    // thread runs the normal shutdown path, so wait() returns on a signal too.
    handle.wait(); // also runs cleanup (Bye, remove ipc.json)
}
