use anyhow::{anyhow, bail, Context, Result};
use audiohub_security::{secure_private_directory, secure_private_file};
use base64::prelude::*;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// CSPRNG-backed 6-digit pairing PIN (SystemTime-derived PINs collapse to
/// ~1000 possible values on macOS where nanos have only µs resolution).
pub fn random_pin() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 4];
    rand_core::OsRng.fill_bytes(&mut b);
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

/// The HUMAN-READABLE computer name, without the `gethostname` crate
/// (raw-dylib windows deps — see the Cargo.toml note).
///
/// On macOS this is `scutil --get ComputerName` ("客厅 Mac"), not `hostname`
/// ("keting-mac.local"): this string ends up as the name of a virtual audio
/// device in somebody else's 系统设置 › 声音, where a DNS-shaped label with the
/// spaces punched out is not what the user calls that machine. `hostname` stays
/// as the fallback for the case where scutil is unavailable.
///
/// Read once per daemon start rather than once per identity creation, so a
/// machine renamed after AudioHub was first run reports its new name.
pub fn local_hostname() -> String {
    #[cfg(windows)]
    {
        if let Some(n) = std::env::var_os("COMPUTERNAME") {
            let n = n.to_string_lossy().into_owned();
            if !n.is_empty() {
                return n;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
        {
            let n = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !n.is_empty() {
                return n;
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(out) = std::process::Command::new("hostname").output() {
            let n = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !n.is_empty() {
                return n;
            }
        }
    }
    "audiohub-host".to_string()
}

/// Hand-rolled platform config root (the `dirs` crate drags in a windows-sys
/// version our gnu toolchain cannot link — see Cargo.toml note).
fn platform_config_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

#[derive(Clone)]
pub struct LocalIdentity {
    pub name: String,
    pub fingerprint: String,
    signing_key: SigningKey,
}

/// Where the name in force came from. Reported to the UI so 「恢复默认」 can be
/// armed only when there is actually something to restore, and so the field can
/// be locked when `AUDIOHUB_NAME` is what is being displayed.
///
/// Without it the interface cannot tell "the user set a name that happens to
/// equal the host name" apart from "following the host name" — two states in
/// which that button must behave differently.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameSource {
    /// `AUDIOHUB_NAME` is set. Read-only in the UI.
    Env,
    /// `identity.json` carries a `name_override` the user typed.
    Custom,
    /// Following `local_hostname()`.
    Hostname,
}

impl NameSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NameSource::Env => "env",
            NameSource::Custom => "custom",
            NameSource::Hostname => "hostname",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    version: u32,
    name: String,
    secret_b64: String,
    /// The user's chosen name (plan §7.1 / user instruction 2026-08-10 #9).
    ///
    /// `serde(default)` so an identity.json written before this field existed
    /// still loads — a daemon that cannot read its own identity file is a
    /// daemon that generates a new key and silently breaks every pairing.
    ///
    /// `skip_serializing_if` keeps the file byte-identical to the old shape
    /// when no override is set, so nothing downgrades badly either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name_override: Option<String>,
}

/// Reads `identity.json` if it is there and parseable. `None` covers both "no
/// file yet" and "unreadable", because every caller here treats them the same:
/// there is no stored override to honour.
fn read_identity_file(path: &Path) -> Option<IdentityFile> {
    secure_private_file(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let file: IdentityFile = serde_json::from_slice(&bytes).ok()?;
    (file.version == 1).then_some(file)
}

/// Trims, drops control characters, and clamps to 48 chars; empty means "no
/// override".
///
/// The UI does this too (`app/frontend/src/lib/identityName.ts`), and that is
/// deliberate rather than duplicated work: this string becomes the name of two
/// virtual audio devices in **every peer's** system sound settings, and the CLI,
/// an older frontend and a hand-edited config file all reach this function
/// without passing through the UI's copy.
/// The name-priority rule, as a pure function so it can be tested without
/// touching the process environment.
///
/// `AUDIOHUB_NAME` > stored override > computer name > the file's own copy.
///
/// ⚠ **The env var must stay at the top.** regress runs several daemons on one
/// host and tells them apart with it; letting a value stored in identity.json
/// outrank it puts every one of those runs on whatever name happened to be
/// saved, and the misattribution is silent — every log line, every device name
/// and every peer list entry agrees on the wrong machine.
fn resolve_name(env: &str, stored: Option<&str>, hostname: &str, file_name: &str) -> String {
    for candidate in [
        sanitize_name(env),
        stored.and_then(sanitize_name),
        sanitize_name(hostname),
        sanitize_name(file_name),
    ] {
        if let Some(n) = candidate {
            return n;
        }
    }
    String::new()
}

fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(48).collect())
}

impl LocalIdentity {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_at(None)
    }

    /// `dir`, when given, overrides AUDIOHUB_CONFIG_DIR / platform default
    /// (lets one process host several isolated daemon instances).
    pub fn load_or_create_at(dir: Option<&Path>) -> Result<Self> {
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        secure_private_directory(&base)
            .with_context(|| format!("secure private directory {}", base.display()))?;
        let path = base.join("identity.json");
        match secure_private_file(&path) {
            Ok(()) => {
                let bytes =
                    std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                let file: IdentityFile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", path.display()))?;
                if file.version != 1 {
                    bail!("unsupported identity.json version {}", file.version);
                }
                // The name is NOT simply taken from the file: it is what peers put
                // on the virtual devices they publish for this machine, so a Mac
                // renamed after AudioHub first ran must announce the new name. The
                // file's `name` is only the last fallback.
                //
                // Priority lives in `resolve_name` (pure, and tested there).
                let name = resolve_name(
                    &std::env::var("AUDIOHUB_NAME").unwrap_or_default(),
                    file.name_override.as_deref(),
                    &local_hostname(),
                    &file.name,
                );
                return Self::from_parts(&name, &file.secret_b64);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("secure {}", path.display())),
        }
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        let mut name = local_hostname();
        if name.is_empty() {
            name = "audiohub".to_string();
        }
        let file = IdentityFile {
            version: 1,
            name: name.clone(),
            secret_b64: BASE64_STANDARD.encode(signing_key.to_bytes()),
            name_override: None,
        };
        write_atomic(&path, serde_json::to_string_pretty(&file)?.as_bytes(), true)?;
        Ok(Self::from_key(name, signing_key))
    }

    /// Where the name currently in force came from.
    ///
    /// Recomputed from the environment and the file rather than cached on the
    /// struct: `AUDIOHUB_NAME` is read at load time, but the override can be
    /// rewritten by `settings.set` while the daemon runs, and a cached copy on
    /// a value that lives inside an `Arc<DaemonInner>` cannot be updated.
    pub fn name_source_at(dir: Option<&Path>) -> NameSource {
        if !std::env::var("AUDIOHUB_NAME").unwrap_or_default().trim().is_empty() {
            return NameSource::Env;
        }
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        match read_identity_file(&base.join("identity.json"))
            .and_then(|f| f.name_override)
            .as_deref()
            .and_then(sanitize_name)
        {
            Some(_) => NameSource::Custom,
            None => NameSource::Hostname,
        }
    }

    /// The stored override as the UI's input box should show it: the user's own
    /// text when there is one, otherwise the name actually in force.
    pub fn name_override_at(dir: Option<&Path>) -> Option<String> {
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        read_identity_file(&base.join("identity.json"))?
            .name_override
            .as_deref()
            .and_then(sanitize_name)
    }

    /// Writes (or clears, with `None` / blank) the user's chosen machine name
    /// and returns the name that is now in force.
    ///
    /// The signing key is read back and written out unchanged — this rewrites
    /// identity.json, and identity.json is where the private key lives, so the
    /// `secret = true` (0600) argument to `write_atomic` is not optional.
    ///
    /// Note what this does NOT do: it does not reconnect to anybody. A peer
    /// refreshes its copy of this name from the next `VerifyResponse`, so the
    /// rename lands on its virtual devices when it next connects. The UI states
    /// that consequence rather than pretending it is instant.
    pub fn set_name_at(dir: Option<&Path>, name: Option<&str>) -> Result<String> {
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        let path = base.join("identity.json");
        let mut file = read_identity_file(&path)
            .ok_or_else(|| anyhow!("no usable identity.json at {}", path.display()))?;
        file.name_override = name.and_then(sanitize_name);
        write_atomic(&path, serde_json::to_string_pretty(&file)?.as_bytes(), true)?;
        Ok(Self::load_or_create_at(dir)?.name)
    }

    /// Throws away this machine's signing key and writes a fresh one, returning
    /// the new fingerprint.
    ///
    /// ⚠ **Every existing pairing dies with the old key.** The caller must have
    /// already told each paired peer (`SessionMsg::Unpaired`) and cleared the
    /// trust store: a peer that is not told keeps a pair of virtual devices
    /// bearing this machine's name, permanently offline and permanently
    /// redialling, and nothing in its interface can explain them. plan §7.1
    /// names that exact failure for ordinary unpairing, and swapping the key is
    /// worse than unpairing — the peer cannot even recognise us afterwards.
    ///
    /// The chosen name (`name_override`) is deliberately preserved: the user
    /// asked to reset an identity key, not to rename their computer.
    pub fn reset_key_at(dir: Option<&Path>) -> Result<String> {
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        let path = base.join("identity.json");
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        let previous = read_identity_file(&path);
        let file = IdentityFile {
            version: 1,
            name: previous
                .as_ref()
                .map(|f| f.name.clone())
                .unwrap_or_else(local_hostname),
            secret_b64: BASE64_STANDARD.encode(signing_key.to_bytes()),
            name_override: previous.and_then(|f| f.name_override),
        };
        write_atomic(&path, serde_json::to_string_pretty(&file)?.as_bytes(), true)?;
        Ok(fingerprint_of(&signing_key.verifying_key().to_bytes()))
    }

    pub fn from_parts(name: &str, secret_b64: &str) -> Result<Self> {
        let bytes = BASE64_STANDARD
            .decode(secret_b64)
            .context("decode secret_b64")?;
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("identity secret must be 32 bytes"))?;
        Ok(Self::from_key(name.to_string(), SigningKey::from_bytes(&seed)))
    }

    fn from_key(name: String, signing_key: SigningKey) -> Self {
        let fingerprint = fingerprint_of(&signing_key.verifying_key().to_bytes());
        LocalIdentity {
            name,
            fingerprint,
            signing_key,
        }
    }

    pub fn config_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("AUDIOHUB_CONFIG_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        platform_config_root()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AudioHub")
    }

    pub fn public_key_b64(&self) -> String {
        BASE64_STANDARD.encode(self.signing_key.verifying_key().to_bytes())
    }

    pub(crate) fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.signing_key.sign(msg).to_bytes().to_vec()
    }
}

pub fn fingerprint_of(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify_sig(public_key_b64: &str, msg: &[u8], sig: &[u8]) -> bool {
    let Ok(pk) = BASE64_STANDARD.decode(public_key_b64) else {
        return false;
    };
    let Ok(pk) = <[u8; 32]>::try_from(pk.as_slice()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(sig) else {
        return false;
    };
    vk.verify_strict(msg, &Signature::from_bytes(&sig)).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedPeer {
    /// The peer's own computer name, refreshed from every `VerifyResponse` —
    /// so a peer that renames its Mac is renamed here on its next connection.
    pub name: String,
    pub fingerprint: String,
    pub public_key_b64: String,
    pub last_addr: Option<String>,
    pub port: u16,
    pub added_unix: u64,
    /// A name the LOCAL user chose for this peer. It overrides `name`
    /// everywhere the peer is displayed, including on its virtual devices
    /// (spec-m5b §5.3). `serde(default)` so a store written before this field
    /// existed still loads.
    #[serde(default)]
    pub alias: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    peers: Vec<PairedPeer>,
}

pub struct PeerStore {
    peers: Vec<PairedPeer>,
    path: PathBuf,
}

impl PeerStore {
    pub fn load() -> Result<Self> {
        Self::load_at(None)
    }

    /// `dir` overrides AUDIOHUB_CONFIG_DIR / platform default, see
    /// `LocalIdentity::load_or_create_at`.
    pub fn load_at(dir: Option<&Path>) -> Result<Self> {
        let base = dir
            .map(Path::to_path_buf)
            .unwrap_or_else(LocalIdentity::config_dir);
        let path = base.join("paired_peers.json");
        let peers = if path.exists() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let file: StoreFile = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            file.peers
        } else {
            Vec::new()
        };
        Ok(PeerStore { peers, path })
    }

    pub fn save(&self) -> Result<()> {
        let file = StoreFile {
            version: 1,
            peers: self.peers.clone(),
        };
        write_atomic(&self.path, serde_json::to_string_pretty(&file)?.as_bytes(), false)
    }

    /// Writes `peer`, KEEPING the local alias and the original `added_unix`
    /// when the incoming record does not carry them.
    ///
    /// Every caller builds its `PairedPeer` from the wire (pairing, verify),
    /// where neither exists: a plain overwrite would drop the name the user
    /// chose on the peer's next connection, and reset the pairing time that
    /// decides which of two identically-named peers gets the ` (2)` suffix —
    /// so a reconnect could rename BOTH machines' virtual devices.
    pub fn upsert(&mut self, mut peer: PairedPeer) {
        match self
            .peers
            .iter_mut()
            .find(|p| p.fingerprint == peer.fingerprint)
        {
            Some(existing) => {
                if peer.alias.is_none() {
                    peer.alias = existing.alias.clone();
                }
                if peer.added_unix == 0 || existing.added_unix != 0 {
                    peer.added_unix = existing.added_unix;
                }
                if peer.name.trim().is_empty() {
                    peer.name = existing.name.clone();
                }
                *existing = peer;
            }
            None => self.peers.push(peer),
        }
    }

    /// Sets (or clears, with `None`) the local alias. `false` = no such peer.
    pub fn set_alias(&mut self, fp: &str, alias: Option<String>) -> bool {
        match self.peers.iter_mut().find(|p| p.fingerprint == fp) {
            Some(p) => {
                p.alias = alias.filter(|a| !a.trim().is_empty());
                true
            }
            None => false,
        }
    }

    pub fn remove_by_fingerprint(&mut self, fp: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.fingerprint != fp);
        self.peers.len() != before
    }

    pub fn clear(&mut self) {
        self.peers.clear();
    }

    pub fn list(&self) -> &[PairedPeer] {
        &self.peers
    }

    pub fn find(&self, fp: &str) -> Option<&PairedPeer> {
        self.peers.iter().find(|p| p.fingerprint == fp)
    }
}

fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    secure_private_directory(dir)
        .with_context(|| format!("secure private directory {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        if secret {
            secure_private_file(&tmp).with_context(|| format!("secure {}", tmp.display()))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    if secret {
        secure_private_file(path).with_context(|| format!("secure {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod name_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let p = std::env::temp_dir().join(format!("ahb-name-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    #[test]
    fn the_environment_variable_outranks_everything_including_a_stored_override() {
        // The rule regress depends on. Tested through the pure function rather
        // than by setting the variable: `std::env::set_var` is process-global
        // and cargo runs these tests on threads, so a test that set it would
        // rename every daemon the other tests in this binary create.
        assert_eq!(
            resolve_name("from-env", Some("stored"), "host", "file"),
            "from-env"
        );
        // Blank / whitespace-only is "unset", not "an empty name".
        assert_eq!(resolve_name("   ", Some("stored"), "host", "file"), "stored");
        assert_eq!(resolve_name("", None, "host", "file"), "host");
        assert_eq!(resolve_name("", None, "", "file"), "file");
        assert_eq!(resolve_name("", None, "", ""), "");
    }

    #[test]
    fn the_priority_sanitises_whichever_candidate_wins() {
        // Every level reaches a peer's device list, so no level may skip the
        // scrub — including the env var, which regress sets by hand.
        assert_eq!(resolve_name("a\nb", Some("stored"), "host", "file"), "ab");
        assert_eq!(resolve_name("", Some(" pad "), "host", "file"), "pad");
    }

    #[test]
    fn a_fresh_identity_follows_the_computer_name() {
        let dir = scratch("fresh");
        let id = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        assert_eq!(id.name, local_hostname());
        assert_eq!(
            LocalIdentity::name_source_at(Some(&dir)),
            NameSource::Hostname,
            "nothing was overridden yet"
        );
        assert_eq!(LocalIdentity::name_override_at(Some(&dir)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_override_outranks_the_computer_name_and_survives_a_reload() {
        let dir = scratch("override");
        LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        let now = LocalIdentity::set_name_at(Some(&dir), Some("客厅 Mac")).expect("set");
        assert_eq!(now, "客厅 Mac");
        // Reloaded from disk, not from the value we just returned: the whole
        // point of the field is that it outlives the process.
        let again = LocalIdentity::load_or_create_at(Some(&dir)).expect("reload");
        assert_eq!(again.name, "客厅 Mac");
        assert_eq!(LocalIdentity::name_source_at(Some(&dir)), NameSource::Custom);
        assert_eq!(
            LocalIdentity::name_override_at(Some(&dir)),
            Some("客厅 Mac".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_the_override_goes_back_to_the_computer_name() {
        let dir = scratch("clear");
        LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        LocalIdentity::set_name_at(Some(&dir), Some("temporary")).expect("set");
        // Blank means "clear the override", NOT "set the name to empty" — an
        // empty machine name would reach the peer as two nameless audio devices.
        let back = LocalIdentity::set_name_at(Some(&dir), Some("   ")).expect("clear");
        assert_eq!(back, local_hostname());
        assert_eq!(LocalIdentity::name_source_at(Some(&dir)), NameSource::Hostname);
        assert_eq!(LocalIdentity::name_override_at(Some(&dir)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stored_name_is_stripped_of_control_characters_and_clamped() {
        let dir = scratch("sanitize");
        LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        let got = LocalIdentity::set_name_at(Some(&dir), Some("  living\nroom\u{7f}  ")).expect("set");
        assert_eq!(got, "livingroom", "a pasted newline reaches every peer's device list");
        let long = "x".repeat(80);
        let clamped = LocalIdentity::set_name_at(Some(&dir), Some(&long)).expect("set");
        assert_eq!(clamped.chars().count(), 48);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_keeps_the_signing_key_and_therefore_the_fingerprint() {
        // Rename must not be a quiet re-pairing. Rewriting identity.json is how
        // the key gets lost, so this asserts the one property that makes the
        // rewrite safe.
        let dir = scratch("keeps-key");
        let before = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        LocalIdentity::set_name_at(Some(&dir), Some("renamed")).expect("set");
        let after = LocalIdentity::load_or_create_at(Some(&dir)).expect("reload");
        assert_eq!(after.fingerprint, before.fingerprint);
        assert_eq!(after.public_key_b64(), before.public_key_b64());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resetting_the_key_changes_the_fingerprint_but_keeps_the_chosen_name() {
        // The user asked to reset an identity key, not to rename their computer.
        let dir = scratch("reset");
        let before = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        LocalIdentity::set_name_at(Some(&dir), Some("studio")).expect("set");
        let fresh_fp = LocalIdentity::reset_key_at(Some(&dir)).expect("reset");
        assert_ne!(fresh_fp, before.fingerprint);
        let after = LocalIdentity::load_or_create_at(Some(&dir)).expect("reload");
        assert_eq!(after.fingerprint, fresh_fp, "the returned fingerprint is the one on disk");
        assert_eq!(after.name, "studio");
        assert_ne!(after.public_key_b64(), before.public_key_b64());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_identity_file_written_before_the_override_existed_still_loads() {
        // `name_override` is `serde(default)`. Without it, upgrading AudioHub
        // would fail to parse identity.json, generate a new key, and silently
        // break every pairing this machine had.
        let dir = scratch("legacy");
        let path = dir.join("identity.json");
        let key = SigningKey::generate(&mut rand_core::OsRng);
        let legacy = serde_json::json!({
            "version": 1,
            "name": "old-host",
            "secret_b64": BASE64_STANDARD.encode(key.to_bytes()),
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).expect("json")).expect("write");
        let id = LocalIdentity::load_or_create_at(Some(&dir)).expect("legacy identity loads");
        assert_eq!(id.fingerprint, fingerprint_of(&key.verifying_key().to_bytes()));
        assert_eq!(LocalIdentity::name_source_at(Some(&dir)), NameSource::Hostname);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn loading_repairs_permissive_identity_and_directory_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("private-mode");
        LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
        let path = dir.join("identity.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("file mode");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("dir mode");

        LocalIdentity::load_or_create_at(Some(&dir)).expect("reload");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir)
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
