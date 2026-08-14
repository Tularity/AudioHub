use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn digest(path: &Path) -> String {
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("cannot hash {}: {error}", path.display()));
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .unwrap_or_else(|error| panic!("cannot hash {}: {error}", path.display()));
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    format!("{:x}", hash.finalize())
}

fn main() {
    println!("cargo:rerun-if-env-changed=AUDIOHUB_WINDOWS_DRIVER_PACKAGE_DIR");
    println!("cargo:rerun-if-env-changed=AUDIOHUB_WINDOWS_DAEMON_PATH");
    let package = std::env::var_os("AUDIOHUB_WINDOWS_DRIVER_PACKAGE_DIR").map(PathBuf::from);
    let daemon = std::env::var_os("AUDIOHUB_WINDOWS_DAEMON_PATH").map(PathBuf::from);
    for (name, env_name) in [
        ("AudioHubVad.inf", "AUDIOHUB_EXPECTED_INF_SHA256"),
        ("AudioHubVad.sys", "AUDIOHUB_EXPECTED_SYS_SHA256"),
        ("AudioHubVad.cat", "AUDIOHUB_EXPECTED_CAT_SHA256"),
    ] {
        let value = package
            .as_deref()
            .map(|dir| dir.join(name))
            .filter(|path| path.is_file())
            .map(|path| {
                println!("cargo:rerun-if-changed={}", path.display());
                digest(&path)
            })
            .unwrap_or_default();
        println!("cargo:rustc-env={env_name}={value}");
    }
    let daemon_digest = daemon
        .filter(|path| path.is_file())
        .map(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            digest(&path)
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=AUDIOHUB_EXPECTED_DAEMON_SHA256={daemon_digest}");
}
