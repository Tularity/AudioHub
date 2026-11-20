//! Release guard for the test-only `airplay-test-hooks` feature. A release
//! path sets AUDIOHUB_RELEASE_BUILD=1; a receiver built that way must not
//! contain the PTP fault-injection hooks.

fn main() {
    println!("cargo:rerun-if-env-changed=AUDIOHUB_RELEASE_BUILD");
    let release = std::env::var("AUDIOHUB_RELEASE_BUILD").is_ok_and(|v| v == "1");
    let hooks = std::env::var_os("CARGO_FEATURE_AIRPLAY_TEST_HOOKS").is_some();
    if release && hooks {
        panic!(
            "audiohub-airplay: the test-only `airplay-test-hooks` feature is enabled in a \
             release build (AUDIOHUB_RELEASE_BUILD=1). Build without that feature."
        );
    }
}
