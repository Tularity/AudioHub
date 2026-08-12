use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use audiohub_ipc::methods;

use crate::{halbridge, ipcserv, start_daemon, DaemonCfg, DaemonHandle};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn temp_config(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "audiohub-airplay-{label}-{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

struct IsolatedDaemon {
    handle: DaemonHandle,
    config: PathBuf,
}

impl IsolatedDaemon {
    fn start(label: &str) -> Self {
        let config = temp_config(label);
        let handle = start_daemon(DaemonCfg {
            control_port: 0,
            ipc_port: 0,
            config_dir: Some(config.clone()),
            announce: Some(false),
            announce_fault: false,
            airplay_advertise: false,
            hal_bridge: Some(halbridge::HalBridgeMode::Off),
            tx_throttle_kbps: Some(0),
            block_udp: None,
        })
        .expect("start isolated daemon");
        Self { handle, config }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        ipcserv::dispatch_for_test(self.handle.inner_for_test(), method, &params)
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        self.handle.shutdown();
        self.handle.wait();
        let _ = std::fs::remove_dir_all(&self.config);
    }
}

#[test]
fn listener_follows_share_mode_and_shutdown_joins_events() {
    let daemon = IsolatedDaemon::start("lifecycle");

    let error = daemon
        .call(methods::SETTINGS_SET, json!({ "airplay_enabled": "yes" }))
        .expect_err("a non-boolean AirPlay switch must not be silently ignored");
    assert!(error.contains("airplay_enabled must be a boolean"));

    let enabled = daemon.ok(methods::SETTINGS_SET, json!({ "airplay_enabled": true }));
    assert_eq!(
        enabled.get("airplay_enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        enabled.get("airplay_listening").and_then(Value::as_bool),
        Some(true)
    );
    let port = enabled
        .get("airplay_raop_port")
        .and_then(Value::as_u64)
        .expect("actual ephemeral RAOP port");
    assert!(port > 0);
    assert!(enabled
        .get("airplay_airplay2_port")
        .is_some_and(Value::is_null));
    assert!(daemon
        .handle
        .inner_for_test()
        .airplay
        .event_thread_for_test());

    let away = daemon.ok(methods::SETTINGS_SET, json!({ "mode": "a" }));
    assert_eq!(
        away.get("airplay_enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        away.get("airplay_listening").and_then(Value::as_bool),
        Some(false)
    );
    assert!(away
        .get("airplay_error")
        .and_then(Value::as_str)
        .is_some_and(|error| error.contains("共享模式")));
    assert!(!daemon
        .handle
        .inner_for_test()
        .airplay
        .event_thread_for_test());
    assert_eq!(
        daemon.ok(methods::AIRPLAY_SESSIONS_LIST, json!({})),
        json!([])
    );

    let restored = daemon.ok(methods::SETTINGS_SET, json!({ "mode": "share" }));
    assert_eq!(
        restored.get("airplay_listening").and_then(Value::as_bool),
        Some(true)
    );
    assert!(restored
        .get("airplay_raop_port")
        .and_then(Value::as_u64)
        .is_some_and(|port| port > 0));
    assert!(daemon
        .handle
        .inner_for_test()
        .airplay
        .event_thread_for_test());
    // Drop calls controller.stop(); that stops the runtime before joining this
    // thread. If its receiver/sender ownership leaks, this test hangs in wait().
}

#[test]
fn configured_but_unreadable_password_fails_closed() {
    let daemon = IsolatedDaemon::start("secret-fail-closed");
    std::fs::write(daemon.config.join("airplay-password"), [0xff]).expect("write invalid secret");

    let view = daemon.ok(methods::SETTINGS_SET, json!({ "airplay_enabled": true }));
    assert_eq!(
        view.get("airplay_enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        view.get("airplay_password_set").and_then(Value::as_bool),
        Some(true),
        "presence must remain visible without exposing secret contents"
    );
    assert_eq!(
        view.get("airplay_listening").and_then(Value::as_bool),
        Some(false)
    );
    assert!(view
        .get("airplay_error")
        .and_then(Value::as_str)
        .is_some_and(|error| error.contains("密码读取失败")));
    assert!(view.get("airplay_raop_port").is_some_and(Value::is_null));
}

#[test]
fn failed_password_commit_cannot_persist_an_enabled_open_receiver() {
    let daemon = IsolatedDaemon::start("secret-commit-failure");
    // A directory at the secret path makes the atomic replacement fail on
    // every platform without relying on permissions (tests may run as root).
    std::fs::create_dir(daemon.config.join("airplay-password"))
        .expect("install unwritable secret target");

    let error = daemon
        .call(
            methods::SETTINGS_SET,
            json!({ "airplay_enabled": true, "airplay_password": "must-not-open" }),
        )
        .expect_err("secret commit must fail");
    assert!(
        error.contains("rename") || error.contains("directory"),
        "{error}"
    );

    let view = daemon.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(
        view.get("airplay_enabled").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        view.get("airplay_listening").and_then(Value::as_bool),
        Some(false)
    );
    let stored = std::fs::read_to_string(daemon.config.join("settings.json")).unwrap_or_default();
    assert!(
        !stored.contains("\"airplay_enabled\": true"),
        "failed secret write persisted an exposed receiver: {stored}"
    );
}

#[test]
fn successful_new_secret_is_removed_when_settings_rename_fails() {
    let daemon = IsolatedDaemon::start("settings-rename-failure");
    // A directory at the final settings path lets the unique secret rename
    // succeed and the settings temp write succeed, then fails only the final
    // settings rename. This proves the compensating secret rollback rather than
    // merely exercising validation or the first write.
    std::fs::create_dir(daemon.config.join("settings.json"))
        .expect("install settings rename failure");

    let error = daemon
        .call(
            methods::SETTINGS_SET,
            json!({ "airplay_enabled": true, "airplay_password": "new-protection" }),
        )
        .expect_err("settings rename must fail after the secret commit");
    assert!(
        error.contains("rename to") || error.contains("directory"),
        "{error}"
    );

    let view = daemon.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(
        view.get("airplay_enabled").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        view.get("airplay_password_set").and_then(Value::as_bool),
        Some(false),
        "the newly committed secret was not rolled back"
    );
    assert_eq!(
        view.get("airplay_listening").and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        !daemon.config.join("airplay-password").exists(),
        "a failed settings commit left the new password behind"
    );
}

#[test]
fn settings_write_failure_restores_the_previous_secret_and_live_settings() {
    let daemon = IsolatedDaemon::start("settings-write-rollback");
    let before = daemon.ok(
        methods::SETTINGS_SET,
        json!({
            "airplay_enabled": true,
            "airplay_name": "Protected",
            "airplay_password": "old-protection"
        }),
    );
    assert_eq!(
        before.get("airplay_listening").and_then(Value::as_bool),
        Some(true)
    );
    // StoredSettings::save writes this fixed staging path before its atomic
    // rename. A directory here fails that write after the replacement password
    // has already committed.
    std::fs::create_dir(daemon.config.join("settings.json.tmp"))
        .expect("install settings write failure");

    let error = daemon
        .call(
            methods::SETTINGS_SET,
            json!({ "airplay_name": "Must Roll Back", "airplay_password": "new-protection" }),
        )
        .expect_err("settings write must fail after the secret commit");
    assert!(
        error.contains("write") || error.contains("directory"),
        "{error}"
    );

    let view = daemon.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(
        view.get("airplay_enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        view.get("airplay_name").and_then(Value::as_str),
        Some("Protected")
    );
    assert_eq!(
        view.get("airplay_listening").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        crate::settings::load_airplay_password(&daemon.config)
            .expect("read rolled-back password")
            .as_deref(),
        Some("old-protection")
    );
}

#[test]
fn disable_and_clear_never_removes_protection_before_settings_commit() {
    let daemon = IsolatedDaemon::start("disable-clear-fail-closed");
    daemon.ok(
        methods::SETTINGS_SET,
        json!({ "airplay_enabled": true, "airplay_password": "keep-until-disabled" }),
    );
    std::fs::create_dir(daemon.config.join("settings.json.tmp"))
        .expect("install settings write failure");

    let error = daemon
        .call(
            methods::SETTINGS_SET,
            json!({ "airplay_enabled": false, "airplay_password": "" }),
        )
        .expect_err("disable settings write must fail before clearing the secret");
    assert!(
        error.contains("write") || error.contains("directory"),
        "{error}"
    );

    // All three truths remain on the old protected state: live settings, the
    // receiver, and restart state on disk. Clearing the secret before the failed
    // write would make the last assertion an exposed receiver on next startup.
    let view = daemon.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(
        view.get("airplay_enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        view.get("airplay_password_set").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        view.get("airplay_listening").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        crate::settings::load_airplay_password(&daemon.config)
            .expect("read preserved password")
            .as_deref(),
        Some("keep-until-disabled")
    );
    let stored = std::fs::read_to_string(daemon.config.join("settings.json"))
        .expect("read last committed settings");
    assert!(
        stored.contains("\"airplay_enabled\": true"),
        "failed disable unexpectedly changed restart state: {stored}"
    );
}
