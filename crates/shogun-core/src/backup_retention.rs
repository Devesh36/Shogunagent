//! How long encrypted database snapshots are kept (issue #647).
//!
//! The snapshots in `<app-data>/backups/` are full encrypted copies of the memory database, so a
//! row deleted from the live database (a range delete, a per-item delete, an expired Visual
//! recall frame) lives on inside every snapshot taken while it existed. This setting bounds that:
//! snapshots expire after [`BackupRetention::days`] — default [`DEFAULT_DAYS`], at most
//! [`MAX_DAYS`]. An invalid upgrade copy gets one retention window from detection for recovery;
//! the newest verified snapshot also survives while no newer one exists (enforced in
//! `shogun_memory::backup`).
//!
//! The value lives in `backup.json` beside `memory.db`, the file the desktop and the Memory API
//! daemons (MCP / REST / CLI) all read and write, so every face sees the same setting
//! (invariant 6). Both directions apply directly (L1): shortening deletes local copies of data
//! the user already asked to keep less of, and lengthening only keeps a local, encrypted safety
//! copy longer, within a 30-day ceiling; nothing leaves the device either way.

use std::path::{Path, PathBuf};

/// The settings file, beside `memory.db`.
pub const SETTINGS_FILE: &str = "backup.json";
/// Shortest cap. A day keeps the nightly pre–Dream Cycle snapshot useful until the next one.
pub const MIN_DAYS: u32 = 1;
/// Longest cap. A backup is a crash-recovery aid, not an archive: a month is long enough to
/// notice a problem, and short enough that deleted data does not linger for a season.
pub const MAX_DAYS: u32 = 30;
/// The default (owner decision 2026-09-25), and the fallback for a missing or invalid file.
pub const DEFAULT_DAYS: u32 = 7;

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// The one sentence every face returns with the setting, so the rule is stated the same way.
pub const RULE: &str = "Normally one verified snapshot is kept, plus the latest pre-upgrade \
copy within retention_days. Cleanup runs at launch, normally every 30 minutes, after new \
snapshots and retention changes. Inconclusive verification retains extra young copies and \
backs off automatic checks up to daily. The newest verified snapshot is kept past retention \
while no newer one exists. Invalid upgrade copies are preserved for one retention window \
from detection while a fresh verified copy gates startup. Delete everything removes every snapshot.";

/// A validated retention period in whole days, [`MIN_DAYS`]..=[`MAX_DAYS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupRetention {
    days: u32,
}

impl Default for BackupRetention {
    fn default() -> Self {
        Self { days: DEFAULT_DAYS }
    }
}

impl BackupRetention {
    pub fn try_days(days: u32) -> Result<Self, String> {
        if (MIN_DAYS..=MAX_DAYS).contains(&days) {
            Ok(Self { days })
        } else {
            Err(invalid())
        }
    }

    pub fn days(self) -> u32 {
        self.days
    }

    pub fn max_age_ms(self) -> i64 {
        i64::from(self.days) * DAY_MS
    }
}

fn invalid() -> String {
    format!("expected backup retention days between {MIN_DAYS} and {MAX_DAYS}")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SettingsFile {
    retention_days: u32,
}

/// `backup.json` beside the database at `db_path`.
pub fn resolve_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|dir| dir.join(SETTINGS_FILE))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
}

/// The retention in force. A missing, unreadable, corrupt or out-of-range file is the default:
/// never longer than the user chose unless they chose more than seven days, and the newest
/// verified snapshot survives any value, so falling back costs no crash safety.
pub fn load(path: &Path) -> BackupRetention {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<SettingsFile>(&text).ok())
        .and_then(|file| BackupRetention::try_days(file.retention_days).ok())
        .unwrap_or_default()
}

/// Persist atomically (temp file, fsync, rename, directory fsync), so a reader in another process
/// never sees half a file and a power loss leaves either the old or the new value.
pub fn save(path: &Path, retention: BackupRetention) -> Result<(), String> {
    use std::io::Write;

    let dir = path
        .parent()
        .ok_or_else(|| "backup settings path has no directory".to_string())?;
    std::fs::create_dir_all(dir).map_err(|_| "backup settings save failed".to_string())?;
    let json = serde_json::to_string(&SettingsFile {
        retention_days: retention.days,
    })
    .map_err(|_| "backup settings save failed".to_string())?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    };
    write().map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        "backup settings save failed".to_string()
    })
}

/// Parse a set request: a bare integer, or `{"days": N}` / `{"retention_days": N}`.
pub fn parse_body(body: &str) -> Result<BackupRetention, String> {
    if let Ok(days) = body.trim().parse::<u32>() {
        return BackupRetention::try_days(days);
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| invalid())?;
    let days = value
        .get("days")
        .or_else(|| value.get("retention_days"))
        .ok_or_else(invalid)?
        .as_u64()
        .and_then(|days| u32::try_from(days).ok())
        .ok_or_else(invalid)?;
    BackupRetention::try_days(days)
}

/// The content-free status every face returns: the setting, its bounds, the rule, and how many
/// snapshots exist and how old they are (from file names only; `None` = the directory could not
/// be read).
pub fn status_json(retention: BackupRetention, snapshot_times: Option<&[i64]>) -> serde_json::Value {
    let snapshots = snapshot_times.map(|times| {
        serde_json::json!({
            "count": times.len(),
            "newest_ms": times.iter().max(),
            "oldest_ms": times.iter().min(),
        })
    });
    serde_json::json!({
        "retention_days": retention.days(),
        "min_days": MIN_DAYS,
        "max_days": MAX_DAYS,
        "default_days": DEFAULT_DAYS,
        "snapshots": snapshots,
        "rule": RULE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shogun_backup_retention_{label}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bounds_are_one_to_thirty_days_default_seven() {
        assert_eq!(BackupRetention::default().days(), 7);
        assert_eq!(BackupRetention::try_days(1).unwrap().days(), 1);
        assert_eq!(BackupRetention::try_days(30).unwrap().days(), 30);
        assert!(BackupRetention::try_days(0).is_err());
        assert!(BackupRetention::try_days(31).is_err());
        assert!(BackupRetention::try_days(u32::MAX).is_err());
        assert_eq!(BackupRetention::try_days(2).unwrap().max_age_ms(), 2 * DAY_MS);
    }

    #[test]
    fn parse_accepts_bare_and_object_bodies_and_rejects_out_of_range() {
        assert_eq!(parse_body("14").unwrap().days(), 14);
        assert_eq!(parse_body(r#"{"days":3}"#).unwrap().days(), 3);
        assert_eq!(parse_body(r#"{"retention_days":30}"#).unwrap().days(), 30);
        for bad in ["0", "31", "-1", "7.5", r#"{"days":0}"#, r#"{"days":"7"}"#, r#"{}"#, "x"] {
            assert!(parse_body(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn save_then_load_round_trips_and_bad_files_fall_back_to_default() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(SETTINGS_FILE);
        assert_eq!(load(&path).days(), DEFAULT_DAYS, "missing file");
        save(&path, BackupRetention::try_days(3).unwrap()).unwrap();
        assert_eq!(load(&path).days(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner-only");
        }
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path).days(), DEFAULT_DAYS, "corrupt file");
        std::fs::write(&path, r#"{"retention_days":400}"#).unwrap();
        assert_eq!(load(&path).days(), DEFAULT_DAYS, "out of range");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_is_beside_the_database() {
        assert_eq!(
            resolve_path(Path::new("/data/memory.db")),
            PathBuf::from("/data/backup.json")
        );
    }

    #[test]
    fn status_reports_setting_bounds_and_snapshot_ages() {
        let status = status_json(BackupRetention::try_days(5).unwrap(), Some(&[30, 10, 20]));
        assert_eq!(status["retention_days"], 5);
        assert_eq!(status["min_days"], 1);
        assert_eq!(status["max_days"], 30);
        assert_eq!(status["default_days"], 7);
        assert_eq!(status["snapshots"]["count"], 3);
        assert_eq!(status["snapshots"]["newest_ms"], 30);
        assert_eq!(status["snapshots"]["oldest_ms"], 10);
        assert!(status_json(BackupRetention::default(), None)["snapshots"].is_null());
    }

    #[cfg(feature = "db")]
    #[test]
    fn default_matches_the_coordinator_default() {
        assert_eq!(DEFAULT_DAYS, shogun_memory::backup::DEFAULT_MAX_AGE_DAYS);
    }
}
