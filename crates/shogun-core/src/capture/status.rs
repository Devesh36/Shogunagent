//! Timestamped, content-free capture status shared with standalone API processes.
use std::path::Path;

pub const FILE_NAME: &str = "capture_status.json";
pub const MAX_AGE_MS: u64 = 4_000;

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub observed_at_ms: u64,
    pub paused_url: bool,
}

impl Status {
    pub fn fresh_pause(self, now_ms: u64) -> Option<bool> {
        let age = now_ms.checked_sub(self.observed_at_ms)?;
        (age <= MAX_AGE_MS).then_some(self.paused_url)
    }
}

pub fn write(path: &Path, now_ms: u64, paused_url: bool) -> std::io::Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&serde_json::to_vec(&Status {
        observed_at_ms: now_ms,
        paused_url,
    })?)?;
    std::fs::rename(temp, path)
}

/// Missing, corrupt, stale or future data is unknown, never a claim that capture is running.
pub fn read(path: &Path, now_ms: u64) -> Option<bool> {
    let status: Status = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    status.fresh_pause(now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_reads_observe_updates_and_reject_unavailable_status(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path =
            std::env::temp_dir().join(format!("shogun-capture-status-{}.json", std::process::id()));
        assert_eq!(read(&path, 10_000), None);
        for paused in [true, false] {
            write(&path, 10_000, paused)?;
            assert_eq!(read(&path, 10_000), Some(paused));
        }
        assert_eq!(read(&path, 9_999), None);
        assert_eq!(read(&path, 10_000 + MAX_AGE_MS + 1), None);
        std::fs::write(&path, "broken")?;
        assert_eq!(read(&path, 10_000), None);
        std::fs::remove_file(path)?;
        Ok(())
    }
}
