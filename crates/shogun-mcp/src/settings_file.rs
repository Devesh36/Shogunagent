//! Cross-process read-modify-write for the small JSON files the desktop and the API daemons share
//! in app-data (`composio.json`, `connections_requests.json`, `connections_status.json`).
//!
//! Each writer takes an exclusive `flock` on a sibling `<file>.lock`, re-reads, and replaces the
//! file atomically (temp file + rename), so a reader in another process never sees half a file and
//! two writers never lose each other's change. `flock` locks belong to the open file description,
//! so two threads of one process exclude each other too.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const LOCK_RETRIES: usize = 200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Held while a writer reads, patches and replaces one file. Unlocked on drop.
pub(crate) struct FileLock {
    #[cfg_attr(not(unix), allow(dead_code))]
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

/// Take the exclusive lock for `path` (on `path` + `.lock`), retrying for about two seconds.
/// `busy` is the caller's error text; it never names the path.
pub(crate) fn lock(path: &Path, busy: &'static str) -> Result<FileLock, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|_| busy.to_string())?;
    }
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(std::path::PathBuf::from(lock_name))
        .map_err(|_| busy.to_string())?;
    #[cfg(unix)]
    for _ in 0..LOCK_RETRIES {
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0
        {
            return Ok(FileLock { file });
        }
        std::thread::sleep(LOCK_RETRY_DELAY);
    }
    #[cfg(not(unix))]
    return Ok(FileLock { file });
    #[cfg(unix)]
    Err(busy.to_string())
}

/// Replace `path` with `bytes` atomically, readable by the owner only. `durable` also flushes the
/// file to disk before the rename (settings the user changed); a status snapshot rewritten every
/// few seconds skips it.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], durable: bool) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("save failed: {e}"))?;
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp.{}.{}", std::process::id(), sequence));
    let tmp = std::path::PathBuf::from(tmp_name);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            if durable {
                file.sync_all()?;
            }
            Ok(())
        })
        .and_then(|_| std::fs::rename(&tmp, path));
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("save failed: {e}")
    })
}
