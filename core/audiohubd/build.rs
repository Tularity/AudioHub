//! Release guard for test-only features (docs/physical-presentation-1.0.2-design.md
//! §8.2 item 1). A release path sets AUDIOHUB_RELEASE_BUILD=1; a daemon built
//! that way must not contain the device-facts injection path, so the build
//! stops here instead of producing a binary that can bypass the physical
//! presentation gate.

fn main() {
    println!("cargo:rerun-if-env-changed=AUDIOHUB_RELEASE_BUILD");
    let release = std::env::var("AUDIOHUB_RELEASE_BUILD").is_ok_and(|v| v == "1");
    let injection = std::env::var_os("CARGO_FEATURE_FACTS_INJECTION").is_some();
    if release && injection {
        panic!(
            "audiohubd: the test-only `facts-injection` feature is enabled in a release build \
             (AUDIOHUB_RELEASE_BUILD=1). Shipped binaries must not contain the injection path; \
             build without that feature."
        );
    }
}
