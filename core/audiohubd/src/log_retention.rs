//! Retain a bounded tail of the App-managed stderr file without replacing its
//! inode: the inherited append handle must remain usable after the UI exits.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const RETAIN_BYTES: u64 = 2 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MARKER: &[u8] = b"[audiohubd] Older log entries removed by the size limit.\n";

pub(crate) struct LogRetention {
    path: PathBuf,
    last_check: Option<Instant>,
    error_reported: bool,
}

impl LogRetention {
    pub(crate) fn for_app(config_dir: &Path) -> Option<Self> {
        (std::env::var_os("AUDIOHUB_MANAGED_LOG").as_deref() == Some(std::ffi::OsStr::new("1")))
            .then(|| Self { path: config_dir.join("daemon.log"), last_check: None, error_reported: false })
    }

    pub(crate) fn maintain(&mut self) {
        if self.last_check.is_some_and(|at| at.elapsed() < CHECK_INTERVAL) {
            return;
        }
        self.last_check = Some(Instant::now());
        // Only the off-thread logger takes this lock. It also serializes the
        // occasional direct Rust stderr writer during in-place compaction.
        let stderr = std::io::stderr();
        let _guard = stderr.lock();
        let active = retain_tail(&self.path, FILE_LIMIT, RETAIN_BYTES);
        let backup = retain_tail(&self.path.with_extension("log.1"), FILE_LIMIT, RETAIN_BYTES);
        if let Some(error) = active.err().or_else(|| backup.err()) {
            if !self.error_reported {
                let line = format!("[audiohubd] Log size maintenance failed ({:?}); will retry.\n", error.kind());
                let _ = std::io::stderr().write_all(line.as_bytes());
                self.error_reported = true;
            }
        } else {
            self.error_reported = false;
        }
    }
}

fn retain_tail(path: &Path, limit: u64, retain: u64) -> io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() <= limit {
        return Ok(false);
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len <= limit {
        return Ok(false);
    }
    let payload_budget = retain.saturating_sub(MARKER.len() as u64);
    file.seek(SeekFrom::Start(len.saturating_sub(payload_budget)))?;
    let mut tail = Vec::with_capacity(payload_budget as usize);
    Read::by_ref(&mut file).take(payload_budget).read_to_end(&mut tail)?;
    // Discard the partial oldest line. A single oversized line is discarded
    // entirely instead of retaining an arbitrary UTF-8 or secret fragment.
    let begin = tail.iter().position(|byte| *byte == b'\n').map_or(tail.len(), |i| i + 1);
    // With limit >= 2 * retain, rewriting the head cannot destroy the old
    // tail. Shorten only after the replacement bytes were written successfully.
    file.rewind()?;
    file.write_all(MARKER)?;
    file.write_all(&tail[begin..])?;
    file.set_len((MARKER.len() + tail.len() - begin) as u64)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("audiohub-log-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn inherited_append_handle_survives_repeated_compaction() {
        let dir = test_dir("retention");
        let path = dir.join("daemon.log");
        let mut inherited = OpenOptions::new().create_new(true).append(true).open(&path).unwrap();
        for index in 0..8 {
            inherited.write_all(&b"older log line\n".repeat(100)).unwrap();
            let recent = format!("recent-{index}\n");
            inherited.write_all(recent.as_bytes()).unwrap();
            assert!(retain_tail(&path, 1024, 512).unwrap());
            let kept = std::fs::read(&path).unwrap();
            assert!(kept.len() <= 512);
            assert!(kept.ends_with(recent.as_bytes()));
            inherited.write_all(b"still writable\n").unwrap();
            assert!(std::fs::read(&path).unwrap().ends_with(b"still writable\n"));
        }
        drop(inherited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_small_and_oversized_single_line_are_bounded() {
        let dir = test_dir("shapes");
        let path = dir.join("daemon.log");
        assert!(!retain_tail(&path, 1024, 512).unwrap());
        std::fs::write(&path, b"small\n").unwrap();
        assert!(!retain_tail(&path, 1024, 512).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"small\n");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        assert!(retain_tail(&path, 1024, 512).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), MARKER);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
