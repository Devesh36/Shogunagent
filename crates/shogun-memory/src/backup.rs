//! Verified encrypted SQLite snapshots.
//!
//! Copy and post-copy work are intentionally separate. The daemon owns one shared source
//! connection; runtime copies use a separate read-only connection instead of holding that mutex.
//! Validation, durability, publication, and rotation happen after that guard is released.
//!
//! ## How long a snapshot lives (#647)
//!
//! Two caps apply, whichever bites first:
//! - **count**: [`BACKUP_KEEP`] normal generation plus the latest upgrade rollback copy until
//!   its age cap, rotated only after a verified replacement publishes;
//! - **age**: no snapshot older than the configured maximum age (default
//!   [`DEFAULT_MAX_AGE_DAYS`] days), swept on every publish and by [`BackupCoordinator::prune_expired`],
//!   which the desktop runs at launch and on a timer, so the bound holds on nights no Dream Cycle
//!   runs and nothing new is published.
//!
//! The one exception to the age cap is crash safety: the **newest verified snapshot is never
//! pruned by age** when no newer verified one exists. A Mac that skipped a week of Dream Cycles
//! still has one restorable copy. As soon as a newer snapshot verifies, the old one goes.
//!
//! "Delete everything" goes further: [`BackupCoordinator::wipe_all`] unlinks every snapshot,
//! temp file and SQLite side file this module wrote, and reports any it could not remove.

use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::Connection;

use crate::DbKey;
use crate::{
    open_encrypted_existing_for_upgrade, open_encrypted_existing_read_only, schema_version,
};

pub const BACKUP_KEEP: usize = 1;
/// The default wall-clock cap on a snapshot's age (#647). The user-facing setting and its bounds
/// live with the settings file (`shogun_core::backup_retention`).
pub const DEFAULT_MAX_AGE_DAYS: u32 = 7;
pub const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupPolicy {
    pub pages_per_step: i32,
    pub max_busy_retries: u32,
    pub retry_delay: Duration,
    pub max_copy_duration: Duration,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            pages_per_step: 256,
            max_busy_retries: 20,
            retry_delay: Duration::from_millis(25),
            // Runtime backups use a separate read-only source connection, so this large-store
            // copy window does not hold the capture/search mutex.
            max_copy_duration: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BackupError {
    #[error("backup directory unavailable")]
    Directory,
    #[error("backup path permissions invalid")]
    Permission,
    #[error("backup source copy failed")]
    Copy,
    #[error("backup source copy deadline exceeded")]
    Deadline,
    #[error("backup verification failed")]
    Verification,
    #[error("backup publication failed")]
    Publish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRecord {
    pub created_at_ms: i64,
    pub schema_version: Option<u32>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupStatus {
    pub latest: Option<BackupRecord>,
    pub generations: usize,
    pub rotation_pending: bool,
}

/// What an existing pre-upgrade baseline means for this upgrade attempt.
#[derive(Debug)]
enum PriorBaseline {
    /// A verified baseline for this target from an uninterrupted failed attempt: reuse it.
    Reuse(BackupStatus),
    /// No retry baseline applies: take a fresh copy and rotate normally.
    Fresh,
    /// The baseline failed verification permanently and was set aside with an `R` name: take a
    /// fresh copy, publishing it without rotation.
    SetAside,
}

/// Which snapshot must precede migrations when an existing store opens at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupSnapshot {
    /// The store is at [`crate::LATEST_SCHEMA_VERSION`]: migrations have nothing to apply.
    NotNeeded,
    /// An older schema: an upgrade. Reuse or take the `M`-tagged pre-upgrade baseline.
    PreUpgrade,
    /// A newer schema (a downgraded app) or none readable: a plain verified snapshot. It is
    /// never tagged as an upgrade baseline, so no later launch can mistake it for one.
    Plain,
}

impl StartupSnapshot {
    pub fn for_schema(current_schema: Option<u32>) -> Self {
        match current_schema {
            Some(version) if version == crate::LATEST_SCHEMA_VERSION => Self::NotNeeded,
            Some(version) if version < crate::LATEST_SCHEMA_VERSION => Self::PreUpgrade,
            _ => Self::Plain,
        }
    }

    pub fn required(self) -> bool {
        self != Self::NotNeeded
    }
}

#[derive(Debug)]
pub struct PendingSnapshot {
    temp_path: PathBuf,
    final_path: PathBuf,
    created_at_ms: i64,
    /// The wipe epoch this snapshot was prepared in. A wipe in between means the copy may hold
    /// rows the user has since deleted, so it must never publish.
    epoch: u64,
}

#[derive(Debug)]
pub struct PreparedSnapshot {
    destination: Connection,
    temp_path: PathBuf,
    final_path: PathBuf,
    created_at_ms: i64,
    copy_started: Instant,
    epoch: u64,
}

/// What one age sweep did ([`BackupCoordinator::prune_expired`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneReport {
    /// Files unlinked: expired snapshots, and expired temp / side files.
    pub removed: usize,
    /// Expired files that could not be unlinked (retried on the next sweep), plus one if the
    /// directory could not be synced afterwards.
    pub failed: usize,
    /// Extra copies retained after an inconclusive verification, not failed unlinks.
    pub unverified_extra: usize,
    /// The newest snapshot is past the cap but was kept, because no newer one verifies.
    pub kept_expired_newest: bool,
    /// Published snapshots left in the directory.
    pub remaining: usize,
}

/// Why [`BackupCoordinator::wipe_all`] did not finish. The live database is not involved: a wipe
/// only ever unlinks files under the backup directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("backup snapshots were not all removed")]
pub struct WipeFailure {
    /// Files that were unlinked before the failure.
    pub removed: usize,
    /// Files that are still there.
    pub failed: usize,
    /// The directory could not be listed, so nothing is known to be removed.
    pub directory_unreadable: bool,
    /// Every unlink succeeded but the directory could not be synced, so a power loss could bring
    /// an entry back.
    pub not_durable: bool,
}

pub trait BackupFilesystem: Send + Sync {
    fn ensure_private_directory(&self, path: &Path) -> io::Result<()>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn reserve_private_file(&self, path: &Path) -> io::Result<File>;
    fn metadata(&self, path: &Path) -> io::Result<Metadata>;
    fn sync_file(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
    fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
}

pub trait BackupSleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

#[derive(Default)]
struct RealFilesystem;

impl BackupFilesystem for RealFilesystem {
    fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid backup directory",
            ));
        }
        // Unix-only: mode bits do not exist on Windows dev builds (the shipped target is macOS).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        // Unix-only: mode bits do not exist on Windows dev builds (the shipped target is macOS).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        std::fs::symlink_metadata(path)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true);
        // FlushFileBuffers needs a writable handle on Windows (dev builds only — the shipped
        // target is macOS).
        #[cfg(windows)]
        options.write(true);
        options.open(path)?.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            // Windows refuses File::open on a directory; a directory handle needs
            // FILE_FLAG_BACKUP_SEMANTICS (dev builds only — the shipped target is macOS).
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true) // FlushFileBuffers needs a writable handle
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(path)?
                .sync_all()
        }
        #[cfg(not(windows))]
        {
            File::open(path)?.sync_all()
        }
    }

    fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

#[derive(Default)]
struct ThreadSleeper;

impl BackupSleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Clone)]
pub struct BackupCoordinator {
    directory: PathBuf,
    policy: BackupPolicy,
    filesystem: Arc<dyn BackupFilesystem>,
    sleeper: Arc<dyn BackupSleeper>,
    failed_publications: Arc<Mutex<HashSet<PathBuf>>>,
    /// Shared by every clone: the settings change it at runtime.
    max_age_ms: Arc<AtomicI64>,
    /// Serializes publication against wipe and prune, so a wipe cannot list the directory while a
    /// pre-wipe copy is being renamed into place.
    publication: Arc<Mutex<()>>,
    /// Bumped by every wipe; a snapshot prepared before a wipe never publishes after it.
    wipe_epoch: Arc<AtomicU64>,
}

impl BackupCoordinator {
    pub fn production(
        directory: impl Into<PathBuf>,
        policy: BackupPolicy,
    ) -> Result<Self, BackupError> {
        Self::with_boundaries(
            directory,
            policy,
            Arc::new(RealFilesystem),
            Arc::new(ThreadSleeper),
        )
    }

    pub fn with_boundaries(
        directory: impl Into<PathBuf>,
        policy: BackupPolicy,
        filesystem: Arc<dyn BackupFilesystem>,
        sleeper: Arc<dyn BackupSleeper>,
    ) -> Result<Self, BackupError> {
        if policy.pages_per_step <= 0 {
            return Err(BackupError::Copy);
        }
        let directory = directory.into();
        filesystem
            .ensure_private_directory(&directory)
            .map_err(|_| BackupError::Directory)?;
        let directory = filesystem
            .canonicalize(&directory)
            .map_err(|_| BackupError::Directory)?;
        Ok(Self {
            directory,
            policy,
            filesystem,
            sleeper,
            failed_publications: Arc::new(Mutex::new(HashSet::new())),
            max_age_ms: Arc::new(AtomicI64::new(i64::from(DEFAULT_MAX_AGE_DAYS) * DAY_MS)),
            publication: Arc::new(Mutex::new(())),
            wipe_epoch: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Set the age cap in whole days. Applies to every clone of this coordinator, at the next
    /// publish or [`Self::prune_expired`].
    pub fn set_max_age_days(&self, days: u32) {
        self.max_age_ms
            .store(i64::from(days).saturating_mul(DAY_MS), Ordering::SeqCst);
    }

    /// The guard holds no data, so a poisoned lock carries no broken state: recover it rather
    /// than stop backups (or a user-requested wipe) for the rest of the process.
    fn lock_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.publication.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn max_age_ms(&self) -> i64 {
        self.max_age_ms.load(Ordering::SeqCst)
    }

    /// Reuse the original snapshot for this upgrade across partially applied migration retries.
    /// A baseline that fails verification permanently is set aside (renamed with `R`) for
    /// recovery, and a fresh verified copy gates the upgrade instead; that publish rotates
    /// nothing away. A transient verification failure aborts, so the next launch retries.
    pub fn create_pre_migration_snapshot(
        &self,
        source: &Connection,
        key: &DbKey,
        now_ms: i64,
        nonce: &str,
        target_schema: u32,
    ) -> Result<BackupStatus, BackupError> {
        if nonce.is_empty() || !nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(BackupError::Copy);
        }
        let rotate = {
            let _publication = self.lock_publication();
            match self.existing_pre_migration(key, target_schema, now_ms)? {
                PriorBaseline::Reuse(status) => return Ok(status),
                PriorBaseline::Fresh => true,
                PriorBaseline::SetAside => false,
            }
        };
        let pending =
            self.copy_to_closed_temp(source, key, now_ms, &format!("{nonce}M{target_schema}"))?;
        self.publish(key, pending, rotate)
    }

    /// The startup snapshot for an existing store at `current_schema`, before migrations run.
    /// `None` when the store is already current. See [`StartupSnapshot`].
    pub fn create_startup_snapshot(
        &self,
        source: &Connection,
        key: &DbKey,
        now_ms: i64,
        nonce: &str,
        current_schema: Option<u32>,
    ) -> Result<Option<BackupStatus>, BackupError> {
        match StartupSnapshot::for_schema(current_schema) {
            StartupSnapshot::NotNeeded => Ok(None),
            StartupSnapshot::PreUpgrade => self
                .create_pre_migration_snapshot(
                    source,
                    key,
                    now_ms,
                    nonce,
                    crate::LATEST_SCHEMA_VERSION,
                )
                .map(Some),
            StartupSnapshot::Plain => {
                let pending = self.copy_to_closed_temp(source, key, now_ms, nonce)?;
                self.verify_publish_rotate(key, pending).map(Some)
            }
        }
    }

    // Publication lock held. Check again before publishing in case another attempt won.
    fn existing_pre_migration(
        &self,
        key: &DbKey,
        target_schema: u32,
        now_ms: i64,
    ) -> Result<PriorBaseline, BackupError> {
        let records = self.scan_published()?;
        // Only the latest publication can belong to an uninterrupted failed upgrade.
        // A newer daily copy means an older build ran again and may contain new user data.
        // The latest `M` is also the one rotation protects, so a reused baseline is never the
        // copy the next rotation removes while it is within the age cap.
        let Some(baseline) = records
            .iter()
            .max_by_key(|record| classify_entry(&record.path).map_or(0, |entry| entry.sequence))
        else {
            return Ok(PriorBaseline::Fresh);
        };
        let entry = classify_entry(&baseline.path).ok_or(BackupError::Verification)?;
        if entry.migration_target != Some(target_schema) {
            return Ok(PriorBaseline::Fresh);
        }
        if entry.recovery {
            // The replacement for a set-aside baseline has not published yet.
            return Ok(PriorBaseline::SetAside);
        }
        // A rename whose directory sync failed is not yet a durable baseline.
        self.filesystem
            .sync_directory(&self.directory)
            .map_err(|_| BackupError::Publish)?;
        let schema = match verify_snapshot_classified(self.filesystem.as_ref(), &baseline.path, key)
        {
            Ok(schema) => schema,
            // I/O, locking or memory pressure: the bytes may be fine. Keep the baseline where it
            // is and block this upgrade attempt; the next launch verifies it again.
            Err(failure) if failure.transient => return Err(failure.error),
            Err(_) => {
                self.set_aside_baseline(&baseline.path, &entry, now_ms)?;
                eprintln!(
                    "[backup] pre-upgrade copy failed verification; kept aside for recovery, taking a fresh copy"
                );
                return Ok(PriorBaseline::SetAside);
            }
        };
        // A downgraded app can open a newer schema. Its tagged copy is not a failed-upgrade
        // baseline, and must not permanently block the next launch of that older app.
        if schema.is_some_and(|version| version >= target_schema) {
            return Ok(PriorBaseline::Fresh);
        }
        let mut baseline = baseline.clone();
        baseline.schema_version = schema;
        Ok(PriorBaseline::Reuse(BackupStatus {
            latest: Some(baseline),
            generations: records.len(),
            rotation_pending: self.rotation_pending(&records, now_ms),
        }))
    }

    /// Preserve uncertain bytes, including side files, through one retention window from
    /// detection. R survives restart and count rotation; explicit wipe includes it.
    fn set_aside_baseline(
        &self,
        path: &Path,
        entry: &EntryName,
        now_ms: i64,
    ) -> Result<(), BackupError> {
        let (_, nonce) = entry.stem.split_once('-').ok_or(BackupError::Publish)?;
        let recovery = self
            .directory
            .join(format!("backup-{now_ms}-{nonce}RG{}.db", entry.sequence));
        for suffix in ["-wal", "-shm", "-journal", ""] {
            let mut old = path.as_os_str().to_os_string();
            old.push(suffix);
            let mut new = recovery.as_os_str().to_os_string();
            new.push(suffix);
            let (old, new) = (PathBuf::from(old), PathBuf::from(new));
            match self.filesystem.metadata(&new) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(BackupError::Publish),
            }
            match self.filesystem.rename(&old, &new) {
                Ok(()) => {}
                Err(error) if !suffix.is_empty() && error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(BackupError::Publish),
            }
        }
        self.filesystem
            .sync_directory(&self.directory)
            .map_err(|_| BackupError::Publish)
    }

    /// One normal snapshot plus the latest rollback copy and unexpired recovery copies.
    pub fn rotation_pending(&self, records: &[BackupRecord], now_ms: i64) -> bool {
        let protected = protected_snapshots(records, now_ms.saturating_sub(self.max_age_ms()));
        records
            .iter()
            .filter(|record| !protected.contains(&record.path))
            .count()
            > BACKUP_KEEP
    }

    /// Copy one consistent source image into a closed, unpublished temporary database.
    ///
    /// Callers holding a shared-source mutex must release it immediately after this returns.
    pub fn copy_to_closed_temp(
        &self,
        source: &Connection,
        key: &DbKey,
        now_ms: i64,
        nonce: &str,
    ) -> Result<PendingSnapshot, BackupError> {
        let prepared = self.prepare_snapshot(key, now_ms, nonce)?;
        self.copy_prepared_to_closed(source, prepared)
    }

    /// Reserve, validate, open, key, and probe the destination before the source mutex is taken.
    pub fn prepare_snapshot(
        &self,
        key: &DbKey,
        now_ms: i64,
        nonce: &str,
    ) -> Result<PreparedSnapshot, BackupError> {
        if nonce.is_empty() || !nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(BackupError::Copy);
        }
        let stem = format!("backup-{now_ms}-{nonce}");
        let temp_path = self.directory.join(format!(".{stem}.tmp"));
        let final_path = self.directory.join(format!("{stem}.db"));
        let reserved = self
            .filesystem
            .reserve_private_file(&temp_path)
            .map_err(|_| BackupError::Copy)?;
        drop(reserved);
        if let Err(error) = validate_private_file(self.filesystem.as_ref(), &temp_path) {
            let _ = self.filesystem.remove_file(&temp_path);
            return Err(error.into());
        }

        let destination = open_encrypted_existing_for_upgrade(&temp_path, key).map_err(|_| {
            let _ = self.filesystem.remove_file(&temp_path);
            BackupError::Copy
        })?;
        Ok(PreparedSnapshot {
            destination,
            temp_path,
            final_path,
            created_at_ms: now_ms,
            copy_started: Instant::now(),
            epoch: self.wipe_epoch.load(Ordering::SeqCst),
        })
    }

    /// Run only SQLite's bounded online-copy window and the required destination close.
    pub fn copy_prepared_to_closed(
        &self,
        source: &Connection,
        prepared: PreparedSnapshot,
    ) -> Result<PendingSnapshot, BackupError> {
        let PreparedSnapshot {
            mut destination,
            temp_path,
            final_path,
            created_at_ms,
            copy_started,
            epoch,
        } = prepared;
        let copy = self.copy_into_prepared(source, &mut destination, copy_started);
        let closed = destination.close().map_err(|_| BackupError::Copy);
        let result = copy.and(closed);
        if result.is_err() {
            let _ = self.filesystem.remove_file(&temp_path);
        }
        result.map(|()| PendingSnapshot {
            temp_path,
            final_path,
            created_at_ms,
            epoch,
        })
    }

    fn copy_into_prepared(
        &self,
        source: &Connection,
        destination: &mut Connection,
        started: Instant,
    ) -> Result<(), BackupError> {
        let mut busy_retries = 0;
        if started.elapsed() >= self.policy.max_copy_duration {
            return Err(BackupError::Deadline);
        }
        let backup = Backup::new(source, destination).map_err(|_| BackupError::Copy)?;
        loop {
            if started.elapsed() >= self.policy.max_copy_duration {
                return Err(BackupError::Deadline);
            }
            match backup
                .step(self.policy.pages_per_step)
                .map_err(|_| BackupError::Copy)?
            {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    busy_retries += 1;
                    if busy_retries > self.policy.max_busy_retries {
                        return Err(BackupError::Deadline);
                    }
                    self.sleeper.sleep(self.policy.retry_delay);
                }
                _ => return Err(BackupError::Copy),
            }
        }
        Ok(())
    }

    /// Verify, durably publish, then rotate an already-closed temporary snapshot.
    pub fn verify_publish_rotate(
        &self,
        key: &DbKey,
        pending: PendingSnapshot,
    ) -> Result<BackupStatus, BackupError> {
        self.publish(key, pending, true)
    }

    /// Verify and durably publish; rotate only when `rotate`. A pre-upgrade copy that replaces
    /// a set-aside baseline publishes without rotating, so no other verified copy goes with it.
    fn publish(
        &self,
        key: &DbKey,
        mut pending: PendingSnapshot,
        mut rotate: bool,
    ) -> Result<BackupStatus, BackupError> {
        let _publication = self.lock_publication();
        if pending.epoch != self.wipe_epoch.load(Ordering::SeqCst) {
            // "Delete everything" ran after this copy was taken: it may hold deleted rows.
            let _ = self.filesystem.remove_file(&pending.temp_path);
            return Err(BackupError::Publish);
        }
        if let Some(target) =
            classify_entry(&pending.final_path).and_then(|entry| entry.migration_target)
        {
            match self.existing_pre_migration(key, target, pending.created_at_ms)? {
                PriorBaseline::Reuse(status) => {
                    let _ = self.filesystem.remove_file(&pending.temp_path);
                    return Ok(status);
                }
                PriorBaseline::SetAside => rotate = false,
                PriorBaseline::Fresh => {}
            }
        }
        self.filesystem
            .sync_file(&pending.temp_path)
            .map_err(|_| BackupError::Publish)?;
        let schema = verify_snapshot(self.filesystem.as_ref(), &pending.temp_path, key)?;
        // Persist publication order in the filename: wall-clock timestamps can go backwards,
        // and a restarted cleanup must still recognize the latest replacement.
        let sequence = self
            .scan_published()?
            .iter()
            .filter_map(|record| classify_entry(&record.path))
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(BackupError::Publish)?;
        let stem = pending
            .final_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or(BackupError::Publish)?;
        pending.final_path = self.directory.join(format!("{stem}G{sequence}.db"));
        self.filesystem
            .rename(&pending.temp_path, &pending.final_path)
            .map_err(|_| BackupError::Publish)?;
        if self.filesystem.sync_directory(&self.directory).is_err() {
            self.failed_publications
                .lock()
                .map_err(|_| BackupError::Publish)?
                .insert(pending.final_path.clone());
            return Err(BackupError::Publish);
        }
        self.failed_publications
            .lock()
            .map_err(|_| BackupError::Publish)?
            .clear();

        let latest = BackupRecord {
            created_at_ms: pending.created_at_ms,
            schema_version: schema,
            path: pending.final_path,
        };
        // The replacement is already verified and durable. Rechecking old database contents
        // delays rotation by minutes and lets one damaged old copy block cleanup entirely.
        let Ok(mut records) = self.scan_published() else {
            return Ok(BackupStatus {
                latest: Some(latest),
                generations: self.count_candidate_finals(),
                rotation_pending: true,
            });
        };
        if !rotate {
            return Ok(BackupStatus {
                rotation_pending: self.rotation_pending(&records, pending.created_at_ms),
                latest: Some(latest),
                generations: records.len(),
            });
        }
        // Keep the freshly verified replacement even if the wall clock moved backwards.
        records.sort_by_key(|record| {
            (
                record.path != latest.path,
                std::cmp::Reverse(record.created_at_ms),
            )
        });
        let cutoff = pending.created_at_ms.saturating_sub(self.max_age_ms());
        let protected = protected_snapshots(&records, cutoff);
        let limit = BACKUP_KEEP
            + protected
                .iter()
                .filter(|path| **path != latest.path)
                .count();
        let mut rotation_pending = false;
        while records.len() > limit {
            let Some(index) = records.iter().rposition(|record| {
                record.path != latest.path && !protected.contains(&record.path)
            }) else {
                break;
            };
            let oldest = &records[index];
            if self.filesystem.remove_file(&oldest.path).is_err() {
                rotation_pending = true;
                break;
            }
            remove_side_files(self.filesystem.as_ref(), &oldest.path);
            records.remove(index);
        }
        // The age cap (#647). `records[0]` is the newest verified snapshot — normally the one just
        // published — and is never removed here, so this only ever drops older generations.
        let mut index = records.len();
        while !rotation_pending && index > 1 {
            index -= 1;
            if records[index].created_at_ms > cutoff {
                continue;
            }
            if remove_if_present(self.filesystem.as_ref(), &records[index].path).is_err() {
                rotation_pending = true;
                break;
            }
            remove_side_files(self.filesystem.as_ref(), &records[index].path);
            records.remove(index);
        }
        if !rotation_pending && self.filesystem.sync_directory(&self.directory).is_err() {
            rotation_pending = true;
        }
        Ok(BackupStatus {
            latest: Some(latest),
            generations: records.len(),
            rotation_pending,
        })
    }

    /// Enforce the generation and age caps, retaining a verified keeper (#647). Expired temp and
    /// side files go too. The desktop retries interrupted rotation at launch and on a timer,
    /// without needing another full-size snapshot first.
    ///
    /// Costs nothing when nothing has expired. Otherwise it verifies snapshots newest-first only
    /// until one passes. If none passes, the newest published file is kept anyway: an
    /// unverifiable result may be transient, and deleting the last copy is not reversible.
    pub fn prune_expired(&self, key: &DbKey, now_ms: i64) -> Result<PruneReport, BackupError> {
        let _publication = self.lock_publication();
        let excluded = self.retry_failed_publications()?;
        let entries = match self.filesystem.entries(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PruneReport::default())
            }
            Err(_) => return Err(BackupError::Directory),
        };
        let cutoff = now_ms.saturating_sub(self.max_age_ms());
        let mut published = Vec::new();
        let mut side = Vec::new();
        for path in entries {
            match classify_entry(&path) {
                Some(entry) if entry.kind == EntryKind::Published => published.push((entry, path)),
                Some(entry) => side.push((entry, path)),
                None => {}
            }
        }
        let expired_published = published.iter().any(|(entry, _)| entry.created_at_ms <= cutoff);
        let expired_side = side.iter().any(|(entry, _)| entry.created_at_ms <= cutoff);
        let mut protected: HashSet<_> = published
            .iter()
            .filter(|(entry, _)| {
                !entry.recovery && entry.migration_target.is_some() && entry.created_at_ms > cutoff
            })
            .max_by_key(|(entry, _)| entry.sequence)
            .map(|(entry, _)| entry.stem.clone())
            .into_iter()
            .collect();
        protected.extend(
            published
                .iter()
                .filter(|(entry, _)| entry.recovery && entry.created_at_ms > cutoff)
                .map(|(entry, _)| entry.stem.clone()),
        );
        let over_count = published.len() > BACKUP_KEEP + protected.len();
        let mut report = PruneReport {
            remaining: published.len(),
            ..PruneReport::default()
        };
        if !expired_published && !expired_side && !over_count {
            return Ok(report);
        }
        published
            .sort_by_key(|(entry, _)| std::cmp::Reverse((entry.sequence, entry.created_at_ms)));
        // Only an expired or excess snapshot needs the keeper decided, so only then verify.
        // A lone snapshot is its own keeper whatever verification says, so the steady state of
        // a Mac that has not run a Dream Cycle for weeks costs no integrity check per sweep.
        let mut keeper_verified = false;
        let mut unverified = HashSet::new();
        let keeper = if !expired_published && !over_count {
            None
        } else if published.len() == 1 {
            published.first().map(|(entry, _)| entry.clone())
        } else {
            let verified = published.iter().find(|(entry, path)| {
                if !excluded.contains(path)
                    && verify_snapshot(self.filesystem.as_ref(), path, key).is_ok()
                {
                    true
                } else {
                    unverified.insert(entry.stem.clone());
                    false
                }
            });
            keeper_verified = verified.is_some();
            verified
                .or_else(|| published.first())
                .map(|(entry, _)| entry.clone())
        };
        let is_keeper = |entry: &EntryName| keeper.as_ref().is_some_and(|k| k.stem == entry.stem);
        // A transient I/O or permission error must not destroy a newer recovery point just
        // because an older one verifies. Age expiry still follows the disclosed policy.
        report.unverified_extra = published
            .iter()
            .filter(|(entry, _)| {
                !is_keeper(entry)
                    && !protected.contains(&entry.stem)
                    && unverified.contains(&entry.stem)
                    && entry.created_at_ms > cutoff
            })
            .count();
        let count_pruned: HashSet<_> = if over_count && keeper_verified {
            published
                .iter()
                .filter(|(entry, _)| !is_keeper(entry) && !protected.contains(&entry.stem))
                .filter(|(entry, _)| !unverified.contains(&entry.stem))
                .skip(BACKUP_KEEP.saturating_sub(1))
                .map(|(entry, _)| entry.stem.clone())
                .collect()
        } else {
            HashSet::new()
        };
        for (entry, path) in published.iter().chain(side.iter()) {
            if entry.created_at_ms > cutoff && !count_pruned.contains(&entry.stem) {
                continue;
            }
            if is_keeper(entry) {
                report.kept_expired_newest |= entry.kind == EntryKind::Published;
                continue;
            }
            match remove_if_present(self.filesystem.as_ref(), path) {
                Ok(()) => {
                    report.removed += 1;
                    if entry.kind == EntryKind::Published {
                        report.remaining = report.remaining.saturating_sub(1);
                    }
                }
                Err(_) => report.failed += 1,
            }
        }
        if report.removed > 0 && self.filesystem.sync_directory(&self.directory).is_err() {
            report.failed += 1;
        }
        Ok(report)
    }

    /// Unlink every snapshot, temp copy and SQLite side file in the backup directory ("Delete
    /// everything", #647). Files this module did not name are left alone. A snapshot prepared
    /// before the wipe can no longer publish (see [`PendingSnapshot`]).
    ///
    /// Plain unlink, no overwrite: every snapshot is encrypted with the database key, so the
    /// freed blocks hold ciphertext only.
    pub fn wipe_all(&self) -> Result<usize, WipeFailure> {
        let _publication = self.lock_publication();
        self.wipe_epoch.fetch_add(1, Ordering::SeqCst);
        let entries = match self.filesystem.entries(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(_) => {
                return Err(WipeFailure {
                    removed: 0,
                    failed: 0,
                    directory_unreadable: true,
                    not_durable: false,
                })
            }
        };
        let mut removed = 0;
        let mut failed = 0;
        for path in entries.iter().filter(|path| classify_entry(path).is_some()) {
            match remove_if_present(self.filesystem.as_ref(), path) {
                Ok(()) => removed += 1,
                Err(_) => failed += 1,
            }
        }
        if let Ok(mut failed_publications) = self.failed_publications.lock() {
            failed_publications.clear();
        }
        let not_durable = removed > 0 && self.filesystem.sync_directory(&self.directory).is_err();
        if failed > 0 || not_durable {
            return Err(WipeFailure {
                removed,
                failed,
                directory_unreadable: false,
                not_durable,
            });
        }
        Ok(removed)
    }

    pub fn list_verified_snapshots(&self, key: &DbKey) -> Result<Vec<BackupRecord>, BackupError> {
        let mut records = self.scan_verified(key)?;
        records.sort_by_key(|record| {
            std::cmp::Reverse((
                classify_entry(&record.path).map_or(0, |entry| entry.sequence),
                record.created_at_ms,
            ))
        });
        Ok(records)
    }

    fn scan_verified(&self, key: &DbKey) -> Result<Vec<BackupRecord>, BackupError> {
        let excluded = self.retry_failed_publications()?;
        let mut records = Vec::new();
        for mut record in self.scan_published()? {
            if excluded.contains(&record.path)
                || classify_entry(&record.path).is_some_and(|entry| entry.recovery)
            {
                continue;
            }
            record.schema_version = verify_snapshot(self.filesystem.as_ref(), &record.path, key)?;
            records.push(record);
        }
        Ok(records)
    }

    fn scan_published(&self) -> Result<Vec<BackupRecord>, BackupError> {
        let entries = self
            .filesystem
            .entries(&self.directory)
            .map_err(|_| BackupError::Directory)?;
        Ok(entries
            .into_iter()
            .filter_map(|path| {
                published_timestamp(&path).map(|created_at_ms| BackupRecord {
                    created_at_ms,
                    schema_version: None,
                    path,
                })
            })
            .collect())
    }

    fn retry_failed_publications(&self) -> Result<HashSet<PathBuf>, BackupError> {
        let mut failed = self
            .failed_publications
            .lock()
            .map_err(|_| BackupError::Directory)?;
        if !failed.is_empty() && self.filesystem.sync_directory(&self.directory).is_ok() {
            failed.clear();
        }
        Ok(failed.clone())
    }

    fn count_candidate_finals(&self) -> usize {
        self.filesystem
            .entries(&self.directory)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|path| published_timestamp(path).is_some())
                    .count()
            })
            .unwrap_or(BACKUP_KEEP + 1)
    }
}

/// Creation times of the published snapshots in `directory`, newest first, read from the file
/// names alone: no key, no verification, nothing opened. For content-free status reads on every
/// API face. A missing directory is an empty list.
pub fn published_snapshot_times(directory: &Path) -> io::Result<Vec<i64>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut times = Vec::new();
    for entry in entries {
        if let Some(timestamp) = published_timestamp(&entry?.path()) {
            times.push(timestamp);
        }
    }
    times.sort_unstable_by(|a, b| b.cmp(a));
    Ok(times)
}

fn protected_snapshots(records: &[BackupRecord], cutoff: i64) -> HashSet<PathBuf> {
    let mut protected: HashSet<_> = records
        .iter()
        .filter_map(|record| {
            let entry = classify_entry(&record.path)?;
            (!entry.recovery && entry.migration_target.is_some() && record.created_at_ms > cutoff)
                .then_some((entry.sequence, record.path.as_path()))
        })
        .max_by_key(|(sequence, _)| *sequence)
        .map(|(_, path)| path.to_path_buf())
        .into_iter()
        .collect();
    protected.extend(
        records
            .iter()
            .filter(|record| {
                record.created_at_ms > cutoff
                    && classify_entry(&record.path).is_some_and(|entry| entry.recovery)
            })
            .map(|record| record.path.clone()),
    );
    protected
}

fn published_timestamp(path: &Path) -> Option<i64> {
    classify_entry(path)
        .filter(|entry| entry.kind == EntryKind::Published)
        .map(|entry| entry.created_at_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// `backup-<ms>-<nonce>[G<sequence>].db`: a published snapshot.
    Published,
    /// A published filename followed by `-journal`, `-wal`, or `-shm`.
    PublishedSide,
    /// `.backup-<ms>-<nonce>.tmp[-<suffix>]`: an unpublished copy, or its side file.
    Temp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryName {
    kind: EntryKind,
    created_at_ms: i64,
    /// Persisted publication order; legacy filenames have no sequence.
    sequence: u64,
    /// Target schema of a pre-upgrade baseline; absent for normal snapshots.
    migration_target: Option<u32>,
    /// Unverifiable upgrade copy held for recovery; its timestamp starts at detection.
    recovery: bool,
    /// `<ms>-<nonce>`: ties a side file to its snapshot, including its pre-rename temp.
    stem: String,
}

/// Name every file this module can create in the backup directory; anything else is `None` and
/// is never touched.
fn classify_entry(path: &Path) -> Option<EntryName> {
    let name = path.file_name()?.to_str()?;
    let (kind, stem, suffix) = if let Some(rest) = name.strip_prefix(".backup-") {
        let (stem, suffix) = rest.split_once(".tmp")?;
        (EntryKind::Temp, stem, suffix)
    } else {
        let (stem, suffix) = name.strip_prefix("backup-")?.split_once(".db")?;
        let kind = if suffix.is_empty() {
            EntryKind::Published
        } else {
            EntryKind::PublishedSide
        };
        (kind, stem, suffix)
    };
    // No suffix, or a SQLite side-file suffix such as `-journal` / `-wal` / `-shm`.
    let side_suffix = suffix
        .strip_prefix('-')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase()));
    if !suffix.is_empty() && !side_suffix {
        return None;
    }
    let (timestamp, nonce) = stem.split_once('-')?;
    if nonce.is_empty() || !nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    // Production nonces are lowercase hex. An alphanumeric suffix preserves compatibility
    // with older builds' filename recognition (notably Delete everything).
    let (nonce, sequence) = nonce
        .rsplit_once('G')
        .filter(|(nonce, _)| !nonce.is_empty() && kind != EntryKind::Temp)
        .and_then(|(nonce, sequence)| sequence.parse::<u64>().ok().map(|n| (nonce, n)))
        .unwrap_or((nonce, 0));
    let migration_target = nonce
        .strip_suffix('R')
        .unwrap_or(nonce)
        .rsplit_once('M')
        .filter(|(nonce, _)| !nonce.is_empty())
        .and_then(|(_, target)| target.parse::<u32>().ok());
    let recovery = nonce.ends_with('R') && migration_target.is_some();
    Some(EntryName {
        kind,
        created_at_ms: timestamp.parse().ok()?,
        sequence,
        migration_target,
        recovery,
        stem: format!("{timestamp}-{nonce}"),
    })
}

/// Best-effort removal of the SQLite side files a rotated snapshot leaves behind: its own
/// `-wal` / `-shm` / `-journal`, and those of the temp copy it was renamed from. Anything missed
/// here is swept by [`BackupCoordinator::prune_expired`] once it ages past the cap.
fn remove_side_files(filesystem: &dyn BackupFilesystem, snapshot: &Path) {
    let (Some(dir), Some(name)) = (
        snapshot.parent(),
        snapshot.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    let Some(entry) = classify_entry(snapshot) else {
        return;
    };
    let stem = &entry.stem;
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = remove_if_present(filesystem, &dir.join(format!("{name}{suffix}")));
        let _ = remove_if_present(filesystem, &dir.join(format!(".backup-{stem}.tmp{suffix}")));
    }
}

/// Unlink `path`; already gone counts as removed (a concurrent rotation got there first).
fn remove_if_present(filesystem: &dyn BackupFilesystem, path: &Path) -> io::Result<()> {
    match filesystem.remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Why a published file failed verification. `transient` marks failures outside the file's own
/// bytes (I/O, locking, memory): they may pass on the next launch, so an upgrade must wait for
/// them rather than set the file aside. Everything else (bad mode or link count, a page that
/// does not decrypt, a failed integrity pragma) fails the same way on every retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerifyFailure {
    error: BackupError,
    transient: bool,
}

impl VerifyFailure {
    fn permanent(error: BackupError) -> Self {
        Self {
            error,
            transient: false,
        }
    }

    fn transient(error: BackupError) -> Self {
        Self {
            error,
            transient: true,
        }
    }

    fn sqlite(error: &rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        // SQLCipher reports an HMAC mismatch as a generic error and a bad first page as "not a
        // database"; both land in the permanent branch, as does any failed pragma row.
        let transient = matches!(
            error.sqlite_error_code(),
            Some(
                ErrorCode::SystemIoFailure
                    | ErrorCode::DatabaseBusy
                    | ErrorCode::DatabaseLocked
                    | ErrorCode::OutOfMemory
                    | ErrorCode::CannotOpen
                    | ErrorCode::OperationInterrupted
                    | ErrorCode::DiskFull
                    | ErrorCode::FileLockingProtocolFailed
            )
        );
        Self {
            error: BackupError::Verification,
            transient,
        }
    }
}

impl From<VerifyFailure> for BackupError {
    fn from(failure: VerifyFailure) -> Self {
        failure.error
    }
}

fn validate_private_file(
    filesystem: &dyn BackupFilesystem,
    path: &Path,
) -> Result<(), VerifyFailure> {
    let metadata = filesystem
        .metadata(path)
        .map_err(|_| VerifyFailure::transient(BackupError::Permission))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(VerifyFailure::permanent(BackupError::Permission));
    }
    // Unix-only: mode bits / link counts do not exist on Windows dev builds (the shipped
    // target is macOS).
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(VerifyFailure::permanent(BackupError::Permission));
        }
    }
    Ok(())
}

fn verify_snapshot(
    filesystem: &dyn BackupFilesystem,
    path: &Path,
    key: &DbKey,
) -> Result<Option<u32>, BackupError> {
    verify_snapshot_classified(filesystem, path, key).map_err(BackupError::from)
}

fn verify_snapshot_classified(
    filesystem: &dyn BackupFilesystem,
    path: &Path,
    key: &DbKey,
) -> Result<Option<u32>, VerifyFailure> {
    validate_private_file(filesystem, path)?;
    let connection = open_encrypted_existing_read_only(path, key).map_err(|error| match error {
        crate::MemoryError::Sqlite(error) => VerifyFailure::sqlite(&error),
        _ => VerifyFailure::permanent(BackupError::Verification),
    })?;
    // Vendored SQLCipher computes the integrity check's file offset in 32 bits, so it
    // rereads page 1 at 4 GiB and falsely reports every later page as a bad HMAC.
    // SQLite's quick_check below still reads and validates the live pages of large files.
    let file_len = filesystem
        .metadata(path)
        .map_err(|_| VerifyFailure::transient(BackupError::Verification))?
        .len();
    if sqlcipher_integrity_check_safe(file_len) {
        require_all_ok(&connection, "PRAGMA cipher_integrity_check", false)?;
    } else {
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if !REPORTED.swap(true, Ordering::Relaxed) {
            eprintln!("[backup] whole-file cipher check unavailable above 4 GiB; checking active pages only");
        }
    }
    require_all_ok(&connection, "PRAGMA quick_check", true)?;
    let mut foreign_keys = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| VerifyFailure::sqlite(&error))?;
    if foreign_keys
        .query([])
        .map_err(|error| VerifyFailure::sqlite(&error))?
        .next()
        .map_err(|error| VerifyFailure::sqlite(&error))?
        .is_some()
    {
        return Err(VerifyFailure::permanent(BackupError::Verification));
    }
    drop(foreign_keys);
    let version = schema_version(&connection).map_err(|error| VerifyFailure::sqlite(&error))?;
    connection
        .close()
        .map_err(|(_, error)| VerifyFailure::sqlite(&error))?;
    Ok(version)
}

fn sqlcipher_integrity_check_safe(file_len: u64) -> bool {
    file_len <= (1_u64 << 32)
}

fn require_all_ok(
    connection: &Connection,
    pragma: &str,
    require_row: bool,
) -> Result<(), VerifyFailure> {
    let mut statement = connection
        .prepare(pragma)
        .map_err(|error| VerifyFailure::sqlite(&error))?;
    let mut rows = statement
        .query([])
        .map_err(|error| VerifyFailure::sqlite(&error))?;
    let mut saw_row = false;
    while let Some(row) = rows.next().map_err(|error| VerifyFailure::sqlite(&error))? {
        saw_row = true;
        let status: String = row.get(0).map_err(|error| VerifyFailure::sqlite(&error))?;
        if status != "ok" {
            return Err(VerifyFailure::permanent(BackupError::Verification));
        }
    }
    if require_row && !saw_row {
        return Err(VerifyFailure::permanent(BackupError::Verification));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn sqlcipher_integrity_check_stops_before_its_four_gib_offset_wrap() {
        assert!(sqlcipher_integrity_check_safe(1_u64 << 32));
        assert!(!sqlcipher_integrity_check_safe((1_u64 << 32) + 1));
    }

    struct DeleteFailFilesystem {
        real: RealFilesystem,
        fail_delete: AtomicBool,
    }

    struct FailOnceDirectorySyncFilesystem {
        real: RealFilesystem,
        sync_calls: AtomicUsize,
    }

    impl BackupFilesystem for FailOnceDirectorySyncFilesystem {
        fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
            self.real.ensure_private_directory(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.real.canonicalize(path)
        }

        fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
            self.real.reserve_private_file(path)
        }

        fn metadata(&self, path: &Path) -> io::Result<Metadata> {
            self.real.metadata(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.real.sync_file(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.real.rename(from, to)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            if self.sync_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(io::Error::other("injected first sync failure"))
            } else {
                self.real.sync_directory(path)
            }
        }

        fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            self.real.entries(path)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.real.remove_file(path)
        }
    }

    impl DeleteFailFilesystem {
        fn new() -> Self {
            Self {
                real: RealFilesystem,
                fail_delete: AtomicBool::new(false),
            }
        }
    }

    impl BackupFilesystem for DeleteFailFilesystem {
        fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
            self.real.ensure_private_directory(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.real.canonicalize(path)
        }

        fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
            self.real.reserve_private_file(path)
        }

        fn metadata(&self, path: &Path) -> io::Result<Metadata> {
            self.real.metadata(path)
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.real.sync_file(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.real.rename(from, to)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            self.real.sync_directory(path)
        }

        fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            self.real.entries(path)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
            } else {
                self.real.remove_file(path)
            }
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shogun_backup_unit_{label}_{}", std::process::id()))
    }

    fn snapshot(
        coordinator: &BackupCoordinator,
        source: &Connection,
        key: &DbKey,
        timestamp: i64,
    ) -> Result<BackupStatus, BackupError> {
        let nonce = format!("n{timestamp}");
        let pending = coordinator.copy_to_closed_temp(source, key, timestamp, &nonce)?;
        coordinator.verify_publish_rotate(key, pending)
    }

    // Model generations left by an older version or an interrupted rotation.
    fn plant_snapshot(coordinator: &BackupCoordinator, source: &Connection, key: &DbKey, at: i64) {
        let pending = coordinator
            .copy_to_closed_temp(source, key, at, &format!("n{at}"))
            .unwrap();
        verify_snapshot(coordinator.filesystem.as_ref(), &pending.temp_path, key).unwrap();
        std::fs::rename(pending.temp_path, pending.final_path).unwrap();
    }

    /// A fresh root with an encrypted source DB and a coordinator over `filesystem`.
    fn fixture(
        label: &str,
        seed: u8,
        filesystem: Arc<dyn BackupFilesystem>,
    ) -> (PathBuf, DbKey, Connection, BackupCoordinator) {
        let root = temp_root(label);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = DbKey::new([seed; 32]);
        let source = crate::open_encrypted(root.join("memory.db"), &key).unwrap();
        let coordinator = BackupCoordinator::with_boundaries(
            root.join("backups"),
            BackupPolicy::default(),
            filesystem,
            Arc::new(ThreadSleeper),
        )
        .unwrap();
        (root, key, source, coordinator)
    }

    fn published_times(coordinator: &BackupCoordinator) -> Vec<i64> {
        published_snapshot_times(coordinator.directory()).unwrap()
    }

    /// The creation time of every file this module named — snapshots, temps and the SQLite
    /// `-wal` / `-shm` / `-journal` side files that WAL-mode snapshots leave beside them.
    fn times_on_disk(coordinator: &BackupCoordinator) -> std::collections::BTreeSet<i64> {
        std::fs::read_dir(coordinator.directory())
            .unwrap()
            .filter_map(|entry| classify_entry(&entry.unwrap().path()))
            .map(|entry| entry.created_at_ms)
            .collect()
    }

    /// A private (0600, one link) file the scanner will treat as a snapshot but that is not a
    /// database: verification fails on it.
    fn plant_private(path: &Path, bytes: &[u8]) {
        let file = RealFilesystem.reserve_private_file(path).unwrap();
        drop(file);
        std::fs::write(path, bytes).unwrap();
    }

    const D: i64 = DAY_MS;

    #[test]
    fn prune_removes_snapshots_past_the_age_cap() {
        let (root, key, source, coordinator) = fixture("prune-age", 34, Arc::new(RealFilesystem));
        coordinator.set_max_age_days(30);
        for timestamp in [D, 3 * D, 9 * D] {
            plant_snapshot(&coordinator, &source, &key, timestamp);
        }
        coordinator.set_max_age_days(7);
        // Cutoff = 10d - 7d = 3d: the 1d and 3d snapshots are at or past the cap.
        let report = coordinator.prune_expired(&key, 10 * D).unwrap();
        assert!(report.removed >= 2, "two snapshots and their side files");
        assert_eq!(report.failed, 0);
        assert!(!report.kept_expired_newest);
        assert_eq!(report.remaining, 1);
        assert_eq!(published_times(&coordinator), vec![9 * D]);
        assert_eq!(times_on_disk(&coordinator), [9 * D].into());
        // Idempotent: nothing more to do.
        assert_eq!(coordinator.prune_expired(&key, 10 * D).unwrap().removed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publishing_applies_the_age_cap_without_a_separate_sweep() {
        let (root, key, source, coordinator) = fixture("publish-age", 35, Arc::new(RealFilesystem));
        coordinator.set_max_age_days(7);
        snapshot(&coordinator, &source, &key, D).unwrap();
        let status = snapshot(&coordinator, &source, &key, 9 * D).unwrap();
        assert_eq!(status.generations, 1);
        assert!(!status.rotation_pending);
        assert_eq!(published_times(&coordinator), vec![9 * D]);
        assert_eq!(times_on_disk(&coordinator), [9 * D].into());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_newest_verified_snapshot_survives_the_cap_until_a_newer_one_exists() {
        let (root, key, source, coordinator) = fixture("keeper", 36, Arc::new(RealFilesystem));
        coordinator.set_max_age_days(30);
        plant_snapshot(&coordinator, &source, &key, D);
        plant_snapshot(&coordinator, &source, &key, 2 * D);
        coordinator.set_max_age_days(7);
        // Weeks without a Dream Cycle: both are past the cap. Only the newest survives.
        let report = coordinator.prune_expired(&key, 40 * D).unwrap();
        assert!(report.removed >= 1);
        assert!(report.kept_expired_newest);
        assert_eq!(report.remaining, 1);
        assert_eq!(published_times(&coordinator), vec![2 * D]);
        // Alone and expired, it is still kept, however often the sweep runs.
        let again = coordinator.prune_expired(&key, 400 * D).unwrap();
        assert_eq!(again.removed, 0);
        assert!(again.kept_expired_newest);
        assert_eq!(published_times(&coordinator), vec![2 * D]);
        assert_eq!(times_on_disk(&coordinator), [2 * D].into(), "the keeper's side files stay");
        // A newer verified snapshot takes over, and the old keeper goes on that publish.
        snapshot(&coordinator, &source, &key, 41 * D).unwrap();
        assert_eq!(published_times(&coordinator), vec![41 * D]);
        assert_eq!(times_on_disk(&coordinator), [41 * D].into(), "side files rotate with it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unverifiable_newer_file_does_not_displace_the_verified_keeper() {
        let (root, key, source, coordinator) = fixture("corrupt-newer", 37, Arc::new(RealFilesystem));
        coordinator.set_max_age_days(30);
        plant_snapshot(&coordinator, &source, &key, D);
        plant_snapshot(&coordinator, &source, &key, 2 * D);
        coordinator.set_max_age_days(7);
        // A corrupt snapshot inside the window is not a valid replacement.
        plant_private(
            &coordinator
                .directory()
                .join(format!("backup-{}-bad.db", 19 * D)),
            b"x",
        );
        let report = coordinator.prune_expired(&key, 20 * D).unwrap();
        assert!(report.removed >= 1, "the 1d snapshot");
        assert!(report.kept_expired_newest);
        assert_eq!(published_times(&coordinator), vec![19 * D, 2 * D]);
        assert_eq!(report.unverified_extra, 1);
        assert_eq!(report.failed, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_takes_expired_temp_and_side_files_but_not_foreign_files() {
        let (root, key, source, coordinator) = fixture("prune-side", 38, Arc::new(RealFilesystem));
        snapshot(&coordinator, &source, &key, 20 * D).unwrap();
        let dir = coordinator.directory().to_path_buf();
        std::fs::write(dir.join(format!(".backup-{D}-crashed.tmp")), b"x").unwrap();
        std::fs::write(dir.join(format!(".backup-{D}-crashed.tmp-journal")), b"x").unwrap();
        std::fs::write(dir.join(format!("backup-{D}-gone.db-wal")), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"not ours").unwrap();
        let report = coordinator.prune_expired(&key, 20 * D).unwrap();
        assert_eq!(report.removed, 3);
        assert!(dir.join("notes.txt").exists());
        assert_eq!(published_times(&coordinator), vec![20 * D]);
        assert_eq!(times_on_disk(&coordinator), [20 * D].into());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_removes_every_snapshot_temp_and_side_file() {
        let (root, key, source, coordinator) = fixture("wipe", 39, Arc::new(RealFilesystem));
        for timestamp in 1..=3 {
            plant_snapshot(&coordinator, &source, &key, timestamp);
        }
        let dir = coordinator.directory().to_path_buf();
        std::fs::write(dir.join(".backup-9-inflight.tmp"), b"x").unwrap();
        std::fs::write(dir.join("backup-3-n3.db-journal"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"not ours").unwrap();
                let removed = coordinator.wipe_all().unwrap();
        assert!(removed >= 5, "3 snapshots, a temp, a journal, and the WAL side files: {removed}");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec!["notes.txt".to_string()]);
        assert!(coordinator.list_verified_snapshots(&key).unwrap().is_empty());
        // Nothing left: a second wipe is a clean no-op.
        assert_eq!(coordinator.wipe_all(), Ok(0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_failure_is_reported_with_what_is_left() {
        let filesystem = Arc::new(DeleteFailFilesystem::new());
        let (root, key, source, coordinator) = fixture("wipe-fail", 40, filesystem.clone());
        plant_snapshot(&coordinator, &source, &key, 1);
        plant_snapshot(&coordinator, &source, &key, 2);
        filesystem.fail_delete.store(true, Ordering::SeqCst);
        let failure = coordinator.wipe_all().unwrap_err();
        assert_eq!(failure.removed, 0);
        assert!(failure.failed >= 2);
        assert!(!failure.directory_unreadable);
        assert_eq!(published_times(&coordinator).len(), 2);
        // Retrying once the filesystem cooperates finishes the job.
        filesystem.fail_delete.store(false, Ordering::SeqCst);
        assert_eq!(coordinator.wipe_all(), Ok(failure.failed));
        assert!(published_times(&coordinator).is_empty());
        assert!(times_on_disk(&coordinator).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_copy_taken_before_a_wipe_never_publishes_after_it() {
        let (root, key, source, coordinator) = fixture("wipe-race", 41, Arc::new(RealFilesystem));
        let pending = coordinator.copy_to_closed_temp(&source, &key, 5, "race").unwrap();
        assert!(coordinator.wipe_all().unwrap() >= 1, "the unpublished temp copy");
        assert_eq!(
            coordinator.verify_publish_rotate(&key, pending).unwrap_err(),
            BackupError::Publish
        );
        assert!(times_on_disk(&coordinator).is_empty());
        // Snapshots taken after the wipe publish normally.
        snapshot(&coordinator, &source, &key, 6).unwrap();
        assert_eq!(published_times(&coordinator), vec![6]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_names_are_classified_strictly() {
        let kind = |name: &str| classify_entry(Path::new(name)).map(|entry| entry.kind);
        assert_eq!(kind("backup-12-ab1.db"), Some(EntryKind::Published));
        assert_eq!(kind("backup-12-g12.db"), Some(EntryKind::Published));
        assert_eq!(kind("backup-12-ab1G2.db"), Some(EntryKind::Published));
        assert_eq!(kind("backup-12-ab1G2.db-wal"), Some(EntryKind::PublishedSide));
        assert_eq!(
            kind("backup-12-ab1.db-journal"),
            Some(EntryKind::PublishedSide)
        );
        assert_eq!(kind(".backup-12-ab1.tmp"), Some(EntryKind::Temp));
        assert_eq!(kind(".backup-12-ab1.tmp-wal"), Some(EntryKind::Temp));
        for foreign in [
            "memory.db",
            "backup-12-.db",
            "backup-x-ab1.db",
            "backup-12-a.b.db",
            "backup-12-ab1G+2.db",
            "backup-12-ab1.db-",
            "backup-12-ab1.db.bak",
            ".backup-12-ab1.tmpx",
            "notes.txt",
        ] {
            assert_eq!(kind(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn pre_upgrade_copy_survives_partial_retry_restart_and_daily_rotation() {
        let (root, key, source, coordinator) =
            fixture("migration-retry", 52, Arc::new(RealFilesystem));
        let target = crate::LATEST_SCHEMA_VERSION + 2;
        let original = coordinator
            .create_pre_migration_snapshot(&source, &key, D, "first", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        // An ungrouped upgrade committed a step, then failed.
        source
            .execute(
                "UPDATE refinery_schema_history SET version=?1 WHERE version=?2",
                [target - 1, crate::LATEST_SCHEMA_VERSION],
            )
            .unwrap();
        drop(coordinator);
        let restarted =
            BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
        let reused = restarted
            .create_pre_migration_snapshot(&source, &key, 2 * D, "retry", target)
            .unwrap();
        assert_eq!(reused.latest.unwrap().path, original);
        assert_eq!(reused.generations, 1);
        assert_eq!(
            verify_snapshot(&RealFilesystem, &original, &key).unwrap(),
            Some(crate::LATEST_SCHEMA_VERSION)
        );
        source
            .execute(
                "UPDATE refinery_schema_history SET version=?1 WHERE version=?2",
                [target, target - 1],
            )
            .unwrap();
        let daily = snapshot(&restarted, &source, &key, 3 * D).unwrap();
        assert_eq!(
            daily.generations, 2,
            "one current copy plus the upgrade rollback copy"
        );
        assert!(!daily.rotation_pending);
        assert_eq!(restarted.prune_expired(&key, 4 * D).unwrap().remaining, 2);
        assert!(original.exists());
        // The configured age cap still expires the protected copy once a replacement exists.
        assert_eq!(restarted.prune_expired(&key, 8 * D).unwrap().remaining, 1);
        assert!(!original.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn downgrade_relaunch_and_reupgrade_use_current_evidence() {
        let (root, key, source, coordinator) =
            fixture("version-switch", 53, Arc::new(RealFilesystem));
        let older = crate::LATEST_SCHEMA_VERSION - 1;
        for (day, nonce) in [(1, "down"), (2, "again"), (400, "late")] {
            coordinator
                .create_pre_migration_snapshot(&source, &key, day * D, nonce, older)
                .unwrap();
        }
        let target = crate::LATEST_SCHEMA_VERSION + 1;
        let old = coordinator
            .create_pre_migration_snapshot(&source, &key, 401 * D, "upgrade", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        // The older app runs after a failed upgrade, stores new data, and publishes a daily copy.
        source.execute_batch("CREATE TABLE intervening_work(value TEXT); INSERT INTO intervening_work VALUES ('keep me');").unwrap();
        snapshot(&coordinator, &source, &key, 402 * D).unwrap();
        let fresh = coordinator
            .create_pre_migration_snapshot(&source, &key, 403 * D, "reupgrade", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        assert_ne!(old, fresh);
        let copy = crate::open_encrypted_existing_read_only(&fresh, &key).unwrap();
        assert_eq!(
            copy.query_row("SELECT value FROM intervening_work", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "keep me"
        );
        drop(copy);
        let restarted =
            BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
        assert_eq!(
            restarted
                .create_pre_migration_snapshot(&source, &key, 404 * D, "retry", target)
                .unwrap()
                .latest
                .unwrap()
                .path,
            fresh
        );
        snapshot(&restarted, &source, &key, 405 * D).unwrap();
        assert!(fresh.exists());
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_upgrade_baseline_is_preserved_while_a_fresh_copy_gates_startup() {
        for damage in ["corrupt", "permissions", "hardlink"] {
            if damage == "permissions" && !cfg!(unix) {
                continue;
            }
            let (root, key, source, coordinator) = fixture(&format!("invalid-baseline-{damage}"), 54, Arc::new(RealFilesystem));
            let target = crate::LATEST_SCHEMA_VERSION + 1;
            let original = coordinator
                .create_pre_migration_snapshot(&source, &key, D, "first", target)
                .unwrap()
                .latest
                .unwrap()
                .path;
            match damage {
                "corrupt" => std::fs::write(&original, b"uncertain recovery bytes").unwrap(),
                "permissions" => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o644))
                            .unwrap();
                    }
                }
                _ => std::fs::hard_link(&original, root.join("external-copy.db")).unwrap(),
            }
            let bytes = std::fs::read(&original).unwrap();
            if damage == "permissions" {
                for suffix in ["-wal", "-shm", "-journal"] {
                    std::fs::write(format!("{}{suffix}", original.display()), b"recovery side bytes").unwrap();
                }
            }
            // Force the replacement reservation to fail after the damaged original is preserved.
            let collision = root.join(format!("backups/.backup-{}-blockedM{target}.tmp", 400 * D));
            std::fs::write(&collision, b"occupied").unwrap();
            assert!(coordinator
                .create_pre_migration_snapshot(&source, &key, 400 * D, "blocked", target)
                .is_err());
            let recovery = coordinator
                .scan_published()
                .unwrap()
                .into_iter()
                .find(|record| classify_entry(&record.path).unwrap().recovery)
                .unwrap()
                .path;
            assert_eq!(std::fs::read(&recovery).unwrap(), bytes);
            let restarted =
                BackupCoordinator::production(root.join("backups"), BackupPolicy::default())
                    .unwrap();
            let fresh = restarted
                .create_pre_migration_snapshot(&source, &key, 400 * D, "fresh", target)
                .unwrap()
                .latest
                .unwrap()
                .path;
            assert_eq!(
                verify_snapshot(&RealFilesystem, &fresh, &key).unwrap(),
                Some(crate::LATEST_SCHEMA_VERSION)
            );
            assert_eq!(
                restarted
                    .create_pre_migration_snapshot(&source, &key, 401 * D, "retry", target)
                    .unwrap()
                    .latest
                    .unwrap()
                    .path,
                fresh
            );
            let daily = snapshot(&restarted, &source, &key, 402 * D).unwrap();
            assert_eq!(daily.generations, 3);
            assert!(!daily.rotation_pending);
            assert_eq!(restarted.list_verified_snapshots(&key).unwrap().len(), 2);
            restarted.prune_expired(&key, 403 * D).unwrap();
            assert_eq!(std::fs::read(&recovery).unwrap(), bytes);
            if damage == "permissions" {
                for suffix in ["-wal", "-shm", "-journal"] {
                    assert_eq!(std::fs::read(format!("{}{suffix}", recovery.display())).unwrap(), b"recovery side bytes");
                }
            }
            if damage == "corrupt" {
                restarted.prune_expired(&key, 408 * D).unwrap();
                assert!(!recovery.exists(), "recovery grace remains bounded");
            }
            restarted.wipe_all().unwrap();
            assert!(!fresh.exists());
            assert!(!recovery.exists());
            assert!(published_times(&restarted).is_empty());
            assert!(times_on_disk(&restarted).is_empty());
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn corrupt_old_backup_does_not_block_rotation_after_a_verified_publish() {
        let (root, key, source, coordinator) =
            fixture("rotate-corrupt-old", 45, Arc::new(RealFilesystem));
        let oldest = snapshot(&coordinator, &source, &key, 1)
            .unwrap()
            .latest
            .unwrap()
            .path;
        std::fs::write(&oldest, b"corrupt old snapshot").unwrap();
        let status = snapshot(&coordinator, &source, &key, BACKUP_KEEP as i64 + 1).unwrap();
        assert!(
            !status.rotation_pending,
            "a verified replacement must allow cleanup"
        );
        assert_eq!(published_times(&coordinator).len(), BACKUP_KEEP);
        assert!(!oldest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_retries_count_rotation_before_backups_age_out() {
        let filesystem = Arc::new(DeleteFailFilesystem::new());
        let (root, key, source, coordinator) = fixture("sweep-count", 46, filesystem.clone());
        filesystem.fail_delete.store(true, Ordering::SeqCst);
        for timestamp in 1..=BACKUP_KEEP as i64 + 1 {
            snapshot(&coordinator, &source, &key, timestamp).unwrap();
        }
        filesystem.fail_delete.store(false, Ordering::SeqCst);
        let report = coordinator
            .prune_expired(&key, BACKUP_KEEP as i64 + 2)
            .unwrap();
        assert_eq!(report.remaining, BACKUP_KEEP);
        assert_eq!(published_times(&coordinator).len(), BACKUP_KEEP);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replacement_survives_rotation_when_the_clock_moves_backwards() {
        let (root, key, source, coordinator) = fixture("rotate-clock", 48, Arc::new(RealFilesystem));
        snapshot(&coordinator, &source, &key, 100).unwrap();
        let status = snapshot(&coordinator, &source, &key, 1).unwrap();
        assert!(status.latest.unwrap().path.exists());
        assert_eq!(published_times(&coordinator), vec![1]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restarted_sweep_keeps_replacement_after_clock_rollback_and_failed_rotation() {
        let filesystem = Arc::new(DeleteFailFilesystem::new());
        let (root, key, source, coordinator) = fixture("clock-retry", 49, filesystem.clone());
        // A legacy backup can have a later wall-clock time than both new publications.
        plant_snapshot(&coordinator, &source, &key, 200);
        filesystem.fail_delete.store(true, Ordering::SeqCst);
        snapshot(&coordinator, &source, &key, 100).unwrap();
        let status = snapshot(&coordinator, &source, &key, 1).unwrap();
        assert!(status.rotation_pending);
        let replacement = status.latest.unwrap().path;
        drop(coordinator);

        let restarted =
            BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
        assert_eq!(restarted.prune_expired(&key, 2).unwrap().remaining, 1);
        assert!(replacement.exists(), "cleanup must retain the fresher data");
        assert_eq!(published_times(&restarted), vec![1]);
        // New generations must also be included in Delete everything.
        restarted.wipe_all().unwrap();
        assert!(!replacement.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn count_sweep_keeps_nonexpired_copies_when_none_verify() {
        let (root, key, _source, coordinator) =
            fixture("sweep-no-keeper", 47, Arc::new(RealFilesystem));
        for timestamp in 1..=BACKUP_KEEP as i64 + 1 {
            plant_private(
                &coordinator
                    .directory()
                    .join(format!("backup-{timestamp}-bad.db")),
                b"x",
            );
        }
        let report = coordinator
            .prune_expired(&key, BACKUP_KEEP as i64 + 2)
            .unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.remaining, BACKUP_KEEP + 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.unverified_extra, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_failure_keeps_extra_verified_generation_and_marks_rotation_pending() {
        let root = temp_root("rotation");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = DbKey::new([31; 32]);
        let source = crate::open_encrypted(root.join("memory.db"), &key).unwrap();
        let filesystem = Arc::new(DeleteFailFilesystem::new());
        let coordinator = BackupCoordinator::with_boundaries(
            root.join("backups"),
            BackupPolicy::default(),
            filesystem.clone(),
            Arc::new(ThreadSleeper),
        )
        .unwrap();
        for timestamp in 1..=3 {
            assert!(!snapshot(&coordinator, &source, &key, timestamp)
                .unwrap()
                .rotation_pending);
        }

        filesystem.fail_delete.store(true, Ordering::SeqCst);
        let status = snapshot(&coordinator, &source, &key, 4).unwrap();
        assert!(status.rotation_pending);
        assert_eq!(status.generations, BACKUP_KEEP + 1);
        assert_eq!(
            coordinator.list_verified_snapshots(&key).unwrap().len(),
            BACKUP_KEEP + 1
        );

        filesystem.fail_delete.store(false, Ordering::SeqCst);
        let status = snapshot(&coordinator, &source, &key, 5).unwrap();
        assert!(!status.rotation_pending);
        assert_eq!(status.generations, BACKUP_KEEP);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_new_temp_never_removes_prior_verified_generations() {
        let root = temp_root("corrupt");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = DbKey::new([32; 32]);
        let source = crate::open_encrypted(root.join("memory.db"), &key).unwrap();
        let coordinator =
            BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
        for timestamp in 1..=3 {
            snapshot(&coordinator, &source, &key, timestamp).unwrap();
        }
        let pending = coordinator
            .copy_to_closed_temp(&source, &key, 4, "corrupt")
            .unwrap();
        std::fs::write(&pending.temp_path, b"corrupt").unwrap();
        assert_eq!(
            coordinator.verify_publish_rotate(&key, pending).unwrap_err(),
            BackupError::Verification
        );
        assert_eq!(
            coordinator.list_verified_snapshots(&key).unwrap().len(),
            BACKUP_KEEP
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_publish_sync_is_retried_before_snapshot_becomes_eligible() {
        let root = temp_root("publish-sync");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = DbKey::new([33; 32]);
        let source = crate::open_encrypted(root.join("memory.db"), &key).unwrap();
        let filesystem = Arc::new(FailOnceDirectorySyncFilesystem {
            real: RealFilesystem,
            sync_calls: AtomicUsize::new(0),
        });
        let coordinator = BackupCoordinator::with_boundaries(
            root.join("backups"),
            BackupPolicy::default(),
            filesystem.clone(),
            Arc::new(ThreadSleeper),
        )
        .unwrap();
        let pending = coordinator
            .copy_to_closed_temp(&source, &key, 6, "publishsync")
            .unwrap();
        assert_eq!(
            coordinator.verify_publish_rotate(&key, pending).unwrap_err(),
            BackupError::Publish
        );
        assert_eq!(filesystem.sync_calls.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.list_verified_snapshots(&key).unwrap().len(), 1);
        assert_eq!(filesystem.sync_calls.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    struct MetadataFailFilesystem {
        real: RealFilesystem,
        fail_published: AtomicBool,
    }

    impl BackupFilesystem for MetadataFailFilesystem {
        fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
            self.real.ensure_private_directory(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.real.canonicalize(path)
        }

        fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
            self.real.reserve_private_file(path)
        }

        fn metadata(&self, path: &Path) -> io::Result<Metadata> {
            let published = classify_entry(path)
                .is_some_and(|entry| entry.kind == EntryKind::Published);
            if published && self.fail_published.load(Ordering::SeqCst) {
                Err(io::Error::other("injected transient read failure"))
            } else {
                self.real.metadata(path)
            }
        }

        fn sync_file(&self, path: &Path) -> io::Result<()> {
            self.real.sync_file(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.real.rename(from, to)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            self.real.sync_directory(path)
        }

        fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            self.real.entries(path)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.real.remove_file(path)
        }
    }

    fn set_schema(source: &Connection, from: u32, to: u32) {
        source
            .execute(
                "UPDATE refinery_schema_history SET version=?1 WHERE version=?2",
                [to, from],
            )
            .unwrap();
    }

    fn migration_target(path: &Path) -> Option<u32> {
        classify_entry(path).and_then(|entry| entry.migration_target)
    }

    #[test]
    fn startup_snapshot_tags_only_upgrades() {
        let latest = crate::LATEST_SCHEMA_VERSION;
        assert_eq!(StartupSnapshot::for_schema(Some(latest)), StartupSnapshot::NotNeeded);
        assert_eq!(StartupSnapshot::for_schema(Some(latest - 1)), StartupSnapshot::PreUpgrade);
        assert_eq!(StartupSnapshot::for_schema(Some(latest + 1)), StartupSnapshot::Plain);
        assert_eq!(StartupSnapshot::for_schema(None), StartupSnapshot::Plain);
        assert!(!StartupSnapshot::NotNeeded.required());
        assert!(StartupSnapshot::Plain.required());
        assert!(StartupSnapshot::PreUpgrade.required());
    }

    #[test]
    fn downgraded_store_keeps_launching_with_plain_snapshots() {
        let (root, key, source, coordinator) =
            fixture("downgrade-launches", 61, Arc::new(RealFilesystem));
        let latest = crate::LATEST_SCHEMA_VERSION;
        assert!(coordinator
            .create_startup_snapshot(&source, &key, D, "current", Some(latest))
            .unwrap()
            .is_none());
        assert!(published_times(&coordinator).is_empty());
        // A newer build migrated this store; the user reinstalled this (older) build.
        set_schema(&source, latest, latest + 1);
        let mut previous = None;
        for (day, nonce) in [(2, "first"), (3, "second"), (4, "third")] {
            let status = coordinator
                .create_startup_snapshot(&source, &key, day * D, nonce, Some(latest + 1))
                .unwrap()
                .unwrap();
            let path = status.latest.unwrap().path;
            assert_eq!(migration_target(&path), None, "never an upgrade baseline");
            assert_ne!(Some(path.clone()), previous, "each launch takes a fresh copy");
            assert_eq!(status.generations, 1);
            previous = Some(path);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tagged_copy_at_or_above_target_is_replaced_not_an_error() {
        let (root, key, source, coordinator) =
            fixture("downgrade-heal", 62, Arc::new(RealFilesystem));
        let latest = crate::LATEST_SCHEMA_VERSION;
        set_schema(&source, latest, latest + 1);
        // What a build left on disk after a downgrade launch.
        let bad = coordinator
            .create_pre_migration_snapshot(&source, &key, D, "left", latest)
            .unwrap()
            .latest
            .unwrap()
            .path;
        assert_eq!(verify_snapshot(&RealFilesystem, &bad, &key).unwrap(), Some(latest + 1));
        for (day, nonce) in [(2, "second"), (400, "late")] {
            let fresh = coordinator
                .create_pre_migration_snapshot(&source, &key, day * D, nonce, latest)
                .unwrap()
                .latest
                .unwrap()
                .path;
            assert_ne!(fresh, bad);
        }
        let status = coordinator
            .create_startup_snapshot(&source, &key, 401 * D, "plain", Some(latest + 1))
            .unwrap()
            .unwrap();
        assert_eq!(migration_target(&status.latest.unwrap().path), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_baseline_is_replaced_and_the_fresh_one_is_the_protected_one() {
        let (root, key, source, coordinator) =
            fixture("stale-baseline", 63, Arc::new(RealFilesystem));
        let latest = crate::LATEST_SCHEMA_VERSION;
        let target = latest + 1;
        let stale = coordinator
            .create_pre_migration_snapshot(&source, &key, D, "first", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        // The upgrade failed; the previous build ran for days and published a daily copy.
        source
            .execute_batch(
                "CREATE TABLE interim_work(value TEXT); INSERT INTO interim_work VALUES ('days');",
            )
            .unwrap();
        snapshot(&coordinator, &source, &key, 2 * D).unwrap();
        let fresh = coordinator
            .create_pre_migration_snapshot(&source, &key, 3 * D, "again", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        assert_ne!(fresh, stale);
        let copy = crate::open_encrypted_existing_read_only(&fresh, &key).unwrap();
        let value: String = copy
            .query_row("SELECT value FROM interim_work", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "days");
        drop(copy);
        // Reuse and rotation protection pick the same baseline.
        let reused = coordinator
            .create_pre_migration_snapshot(&source, &key, 3 * D + 1, "retry", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        assert_eq!(reused, fresh);
        let records = coordinator.scan_published().unwrap();
        let cutoff = 3 * D - coordinator.max_age_ms();
        assert!(protected_snapshots(&records, cutoff).contains(&fresh));
        // The upgrade succeeds; the next daily rotation keeps the pre-upgrade copy.
        set_schema(&source, latest, target);
        let daily = snapshot(&coordinator, &source, &key, 4 * D).unwrap();
        assert_eq!(daily.generations, 2);
        assert!(fresh.exists());
        assert!(!stale.exists());
        assert_eq!(coordinator.prune_expired(&key, 5 * D).unwrap().remaining, 2);
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permanently_bad_baseline_is_set_aside_without_rotating_good_copies() {
        let target = crate::LATEST_SCHEMA_VERSION + 1;
        for damage in ["truncated", "mode"] {
            if damage == "mode" && !cfg!(unix) {
                continue;
            }
            let filesystem = Arc::new(DeleteFailFilesystem::new());
            let (root, key, source, coordinator) =
                fixture(&format!("bad-baseline-{damage}"), 64, filesystem.clone());
            let older = snapshot(&coordinator, &source, &key, D).unwrap().latest.unwrap().path;
            // An earlier rotation could not delete the older verified copy.
            filesystem.fail_delete.store(true, Ordering::SeqCst);
            let baseline = coordinator
                .create_pre_migration_snapshot(&source, &key, 2 * D, "first", target)
                .unwrap();
            assert!(baseline.rotation_pending);
            let baseline = baseline.latest.unwrap().path;
            filesystem.fail_delete.store(false, Ordering::SeqCst);
            match damage {
                "truncated" => {
                    let len = std::fs::metadata(&baseline).unwrap().len();
                    OpenOptions::new()
                        .write(true)
                        .open(&baseline)
                        .unwrap()
                        .set_len(len / 2)
                        .unwrap();
                }
                _ => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &baseline,
                            std::fs::Permissions::from_mode(0o644),
                        )
                        .unwrap();
                    }
                }
            }
            let bytes = std::fs::read(&baseline).unwrap();
            let status = coordinator
                .create_pre_migration_snapshot(&source, &key, 3 * D, "fresh", target)
                .unwrap();
            let fresh = status.latest.unwrap().path;
            assert_eq!(
                verify_snapshot(&RealFilesystem, &fresh, &key).unwrap(),
                Some(crate::LATEST_SCHEMA_VERSION)
            );
            assert!(older.exists(), "the recovery publish rotates nothing away");
            assert_eq!(status.generations, 3);
            let recovery = coordinator
                .scan_published()
                .unwrap()
                .into_iter()
                .find(|record| classify_entry(&record.path).unwrap().recovery)
                .unwrap()
                .path;
            assert_eq!(std::fs::read(&recovery).unwrap(), bytes, "bad bytes kept");
            // The good copies and the set-aside file all survive the launch sweep.
            let report = coordinator.prune_expired(&key, 3 * D + 1).unwrap();
            assert_eq!(report.remaining, 3);
            assert!(older.exists() && fresh.exists() && recovery.exists());
            let verified = coordinator.list_verified_snapshots(&key).unwrap();
            assert_eq!(verified.len(), 2);
            // Retries reuse the fresh baseline; the next daily keeps it as the rollback copy.
            let reused = coordinator
                .create_pre_migration_snapshot(&source, &key, 3 * D + 2, "retry", target)
                .unwrap()
                .latest
                .unwrap()
                .path;
            assert_eq!(reused, fresh);
            set_schema(&source, crate::LATEST_SCHEMA_VERSION, target);
            snapshot(&coordinator, &source, &key, 4 * D).unwrap();
            assert!(fresh.exists());
            assert!(recovery.exists());
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn transient_baseline_read_failure_aborts_and_keeps_the_baseline() {
        let filesystem = Arc::new(MetadataFailFilesystem {
            real: RealFilesystem,
            fail_published: AtomicBool::new(false),
        });
        let (root, key, source, coordinator) =
            fixture("transient-baseline", 65, filesystem.clone());
        let target = crate::LATEST_SCHEMA_VERSION + 1;
        let baseline = coordinator
            .create_pre_migration_snapshot(&source, &key, D, "first", target)
            .unwrap()
            .latest
            .unwrap()
            .path;
        filesystem.fail_published.store(true, Ordering::SeqCst);
        assert_eq!(
            coordinator
                .create_pre_migration_snapshot(&source, &key, 2 * D, "retry", target)
                .unwrap_err(),
            BackupError::Permission
        );
        assert!(baseline.exists(), "not renamed or removed");
        assert!(coordinator
            .scan_published()
            .unwrap()
            .iter()
            .all(|record| !classify_entry(&record.path).unwrap().recovery));
        assert_eq!(published_times(&coordinator), vec![D]);
        // The next launch, with the I/O problem gone, reuses the same baseline.
        filesystem.fail_published.store(false, Ordering::SeqCst);
        assert_eq!(
            coordinator
                .create_pre_migration_snapshot(&source, &key, 3 * D, "later", target)
                .unwrap()
                .latest
                .unwrap()
                .path,
            baseline
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
