use anyhow::{anyhow, bail, Context, Result};
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

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    version: u32,
    name: String,
    secret_b64: String,
}

impl LocalIdentity {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_at(None)
    }

    /// `dir`, when given, overrides AUDIOHUB_CONFIG_DIR / platform default
    /// (lets one process host several isolated daemon instances).
    pub fn load_or_create_at(dir: Option<&Path>) -> Result<Self> {
        let base = dir.map(Path::to_path_buf).unwrap_or_else(Self::config_dir);
        let path = base.join("identity.json");
        if path.exists() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let file: IdentityFile = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            if file.version != 1 {
                bail!("unsupported identity.json version {}", file.version);
            }
            // The name is read FRESH, not taken from the file: it is what peers
            // put on the virtual devices they publish for this machine, so a
            // Mac renamed after AudioHub first ran must announce the new name.
            // The file's copy is only the fallback. `AUDIOHUB_NAME` overrides
            // both, which is what lets several test daemons on one host be
            // told apart.
            let mut name = std::env::var("AUDIOHUB_NAME").unwrap_or_default();
            if name.trim().is_empty() {
                name = local_hostname();
            }
            if name.trim().is_empty() {
                name = file.name.clone();
            }
            return Self::from_parts(&name, &file.secret_b64);
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
        };
        write_atomic(&path, serde_json::to_string_pretty(&file)?.as_bytes(), true)?;
        Ok(Self::from_key(name, signing_key))
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

#[cfg_attr(not(unix), allow(unused_variables))]
fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
