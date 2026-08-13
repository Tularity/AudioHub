//! Stable public identity for AudioHub's AirPlay 2 receiver.
//!
//! The on-disk file contains an Ed25519 signing seed and therefore must be
//! treated as a secret even though only its public half is advertised.  The
//! format and persistence code are AudioHub-owned; no third-party receiver
//! identity implementation is linked or copied.

use audiohub_security::{secure_private_directory, secure_private_file};
use ed25519_dalek::SigningKey;
use rand::{rngs::OsRng, RngCore};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const FILE_MAGIC: &str = "audiohub-airplay2-identity-v1";

#[derive(Clone)]
pub(crate) struct ReceiverIdentity {
    signing_key: SigningKey,
    public_identifier: Uuid,
}

impl ReceiverIdentity {
    pub(crate) fn load_or_create(path: &Path) -> io::Result<Self> {
        secure_parent(path)?;
        match secure_private_file(path) {
            Ok(()) => Self::parse(&fs::read_to_string(path)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate();
                persist_new_identity(path, identity.serialized().as_bytes())?;
                // A second daemon may have won the create/rename race. Read
                // the installed file so every process observes one identity.
                secure_private_file(path)?;
                let installed = fs::read_to_string(path)?;
                Self::parse(&installed)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn generate() -> Self {
        Self::generate_inner()
    }

    #[cfg(not(test))]
    fn generate() -> Self {
        Self::generate_inner()
    }

    fn generate_inner() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
            public_identifier: Uuid::new_v4(),
        }
    }

    pub(crate) fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub(crate) fn public_key_hex(&self) -> String {
        hex::encode(self.public_key())
    }

    pub(crate) fn public_identifier(&self) -> String {
        self.public_identifier.hyphenated().to_string()
    }

    fn serialized(&self) -> String {
        format!(
            "{FILE_MAGIC}\nseed={}\npi={}\n",
            hex::encode(self.signing_key.to_bytes()),
            self.public_identifier.hyphenated()
        )
    }

    fn parse(contents: &str) -> io::Result<Self> {
        let mut lines = contents.lines();
        if lines.next() != Some(FILE_MAGIC) {
            return Err(invalid_identity());
        }
        let seed_hex = lines
            .next()
            .and_then(|line| line.strip_prefix("seed="))
            .ok_or_else(invalid_identity)?;
        let pi = lines
            .next()
            .and_then(|line| line.strip_prefix("pi="))
            .ok_or_else(invalid_identity)?;
        if lines.any(|line| !line.is_empty()) {
            return Err(invalid_identity());
        }
        let seed: [u8; 32] = hex::decode(seed_hex)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(invalid_identity)?;
        let public_identifier = Uuid::parse_str(pi).map_err(|_| invalid_identity())?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&seed),
            public_identifier,
        })
    }
}

impl fmt::Debug for ReceiverIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverIdentity")
            .field("public_key", &self.public_key_hex())
            .field("public_identifier", &self.public_identifier())
            .finish_non_exhaustive()
    }
}

fn invalid_identity() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "AirPlay 2 identity file is malformed",
    )
}

fn persist_new_identity(path: &Path, contents: &[u8]) -> io::Result<()> {
    secure_parent(path)?;
    let temp = unique_temp_path(path);
    let write_result = write_private_new(&temp, contents).and_then(|file| {
        file.sync_all()?;
        // A same-directory hard link is an atomic, no-replace installation on
        // both Unix and NTFS. Unlike `rename`, it can never replace an
        // identity installed by a concurrently starting daemon.
        match fs::hard_link(&temp, path) {
            Ok(()) => sync_parent_directory(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    });
    let _ = fs::remove_file(&temp);
    write_result
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let mut random = [0u8; 8];
    OsRng.fill_bytes(&mut random);
    let suffix = format!(".{}.{}.tmp", std::process::id(), u64::from_le_bytes(random));
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "airplay2-identity".into());
    name.push(suffix);
    path.with_file_name(name)
}

fn secure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        secure_private_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_private_new(path: &Path, contents: &[u8]) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    secure_private_file(path)?;
    file.write_all(contents)?;
    Ok(file)
}

#[cfg(not(unix))]
fn write_private_new(path: &Path, contents: &[u8]) -> io::Result<fs::File> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    secure_private_file(path)?;
    file.write_all(contents)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_identity(label: &str) -> PathBuf {
        let mut random = [0u8; 8];
        OsRng.fill_bytes(&mut random);
        std::env::temp_dir().join(format!(
            "audiohub-{label}-{}-{}",
            std::process::id(),
            u64::from_le_bytes(random)
        ))
    }

    #[test]
    fn private_material_never_appears_in_debug() {
        let identity = ReceiverIdentity::generate();
        let seed = hex::encode(identity.signing_key.to_bytes());
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&seed));
        assert!(debug.contains(&identity.public_key_hex()));
    }

    #[test]
    fn persisted_identity_is_stable() {
        let dir = temp_identity("ap2-identity");
        let path = dir.join("identity");
        let first = ReceiverIdentity::load_or_create(&path).unwrap();
        let second = ReceiverIdentity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.public_identifier(), second.public_identifier());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_creators_observe_one_identity() {
        let dir = temp_identity("ap2-identity-race");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let identity = ReceiverIdentity::load_or_create(&path).unwrap();
                    (identity.public_key(), identity.public_identifier())
                })
            })
            .collect();
        let identities: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_or_trailing_fields_fail_closed() {
        assert!(ReceiverIdentity::parse("garbage").is_err());
        let identity = ReceiverIdentity::generate();
        let mut contents = identity.serialized();
        contents.push_str("unexpected=value\n");
        assert!(ReceiverIdentity::parse(&contents).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn identity_seed_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_identity("ap2-mode");
        let path = dir.join("identity");
        ReceiverIdentity::load_or_create(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ReceiverIdentity::load_or_create(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
