#![cfg(feature = "db")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use shogun_core::daemon::{BackupFailureClass, Clock, Db};
use shogun_memory::backup::{BackupCoordinator, BackupFilesystem, BackupPolicy, BackupSleeper};
use shogun_memory::event_log::NewEvent;
use shogun_memory::DbKey;

enum SyncBehavior {
    Block(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>),
    Fail,
}

struct BoundaryFilesystem {
    sync_behavior: SyncBehavior,
    reserve_block: Option<(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>)>,
}

impl BackupFilesystem for BoundaryFilesystem {
    fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)?;
        // Unix-only: mode bits do not exist on Windows dev builds.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
        if let Some((reached, release)) = &self.reserve_block {
            reached.send(()).map_err(|_| io::ErrorKind::BrokenPipe)?;
            release
                .lock()
                .map_err(|_| io::ErrorKind::Other)?
                .recv()
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        // Unix-only: mode bits do not exist on Windows dev builds.
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
        match &self.sync_behavior {
            SyncBehavior::Block(reached, release) => {
                reached.send(()).map_err(|_| io::ErrorKind::BrokenPipe)?;
                release
                    .lock()
                    .map_err(|_| io::ErrorKind::Other)?
                    .recv()
                    .map_err(|_| io::ErrorKind::BrokenPipe)?;
                let mut options = OpenOptions::new();
                options.read(true);
                // FlushFileBuffers needs a writable handle on Windows dev builds.
                #[cfg(windows)]
                options.write(true);
                options.open(path)?.sync_all()
            }
            SyncBehavior::Fail => Err(io::Error::other("injected")),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            // Windows refuses File::open on a directory; a directory handle needs
            // FILE_FLAG_BACKUP_SEMANTICS and a writable handle for FlushFileBuffers.
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .write(true)
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

struct NoopSleeper;

impl BackupSleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shogun_core_backup_{label}_{}", std::process::id()))
}

fn clock() -> Clock {
    Arc::new(|| 1_700_000_000_000)
}

fn event<'a>(content: &'a str, hash: &'a str) -> NewEvent<'a> {
    NewEvent {
        ts: 1,
        source: "capture",
        kind: "text",
        app_bundle_id: None,
        window_title: None,
        content,
        content_hash: hash,
        dwell_ms: 0,
        display_id: None,
        window_bounds: None,
    }
}

#[test]
fn writer_proceeds_before_verification_and_publication_finish() {
    let root = temp_root("release");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([41; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    let db = Db::new(source, clock());
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let coordinator = BackupCoordinator::with_boundaries(
        root.join("backups"),
        BackupPolicy::default(),
        Arc::new(BoundaryFilesystem {
            sync_behavior: SyncBehavior::Block(reached_tx, Mutex::new(release_rx)),
            reserve_block: None,
        }),
        Arc::new(NoopSleeper),
    )
    .unwrap();

    let backup_db = db.clone();
    let backup_key = key.clone();
    let backup = std::thread::spawn(move || {
        backup_db.create_verified_backup(&backup_key, &coordinator, 10, "release")
    });
    reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let writer_db = db.clone();
    let (writer_tx, writer_rx) = mpsc::channel();
    std::thread::spawn(move || {
        writer_tx.send(writer_db.capture(&event("after copy", "after-copy"))).unwrap();
    });
    assert!(writer_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_some());
    release_tx.send(()).unwrap();
    let status = backup.join().unwrap().unwrap();

    let snapshot = status.latest.unwrap().path;
    let reopened = shogun_memory::open_encrypted_existing_read_only(snapshot, &key).unwrap();
    let snapshot_rows: i64 = reopened
        .query_row("SELECT count(*) FROM event_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(snapshot_rows, 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn backup_failure_does_not_change_live_memory_health() {
    let root = temp_root("health");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([42; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    let db = Db::new(source, clock());
    let before = db.memory_health();
    let coordinator = BackupCoordinator::with_boundaries(
        root.join("backups"),
        BackupPolicy::default(),
        Arc::new(BoundaryFilesystem {
            sync_behavior: SyncBehavior::Fail,
            reserve_block: None,
        }),
        Arc::new(NoopSleeper),
    )
    .unwrap();

    let result = db.create_verified_backup(&key, &coordinator, 11, "health");
    assert_eq!(result.unwrap_err(), BackupFailureClass::Publish);
    assert_eq!(db.memory_health(), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn writer_proceeds_while_destination_prepare_is_blocked() {
    let root = temp_root("prepare");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([43; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    let db = Db::new(source, clock());
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let coordinator = BackupCoordinator::with_boundaries(
        root.join("backups"),
        BackupPolicy::default(),
        Arc::new(BoundaryFilesystem {
            sync_behavior: SyncBehavior::Fail,
            reserve_block: Some((reached_tx, Mutex::new(release_rx))),
        }),
        Arc::new(NoopSleeper),
    )
    .unwrap();

    let backup_db = db.clone();
    let backup_key = key.clone();
    let backup = std::thread::spawn(move || {
        backup_db.create_verified_backup(&backup_key, &coordinator, 12, "prepare")
    });
    reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let writer_db = db.clone();
    let (writer_tx, writer_rx) = mpsc::channel();
    std::thread::spawn(move || {
        writer_tx
            .send(writer_db.capture(&event("during prepare", "during-prepare")))
            .unwrap();
    });
    let writer = writer_rx.recv_timeout(Duration::from_millis(250));
    release_tx.send(()).unwrap();
    let _ = backup.join().unwrap();
    assert!(writer.unwrap().is_some());
    let _ = std::fs::remove_dir_all(&root);
}

// "Delete everything" (#647)

/// The real filesystem, with unlinks that can be made to fail.
struct RemoveFailFilesystem {
    fail_remove: std::sync::atomic::AtomicBool,
}

impl BackupFilesystem for RemoveFailFilesystem {
    fn ensure_private_directory(&self, path: &Path) -> io::Result<()> {
        BoundaryFilesystem {
            sync_behavior: SyncBehavior::Fail,
            reserve_block: None,
        }
        .ensure_private_directory(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn reserve_private_file(&self, path: &Path) -> io::Result<File> {
        BoundaryFilesystem {
            sync_behavior: SyncBehavior::Fail,
            reserve_block: None,
        }
        .reserve_private_file(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        std::fs::symlink_metadata(path)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.write(true);
        options.open(path)?.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        BoundaryFilesystem {
            sync_behavior: SyncBehavior::Fail,
            reserve_block: None,
        }
        .sync_directory(path)
    }

    fn entries(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.fail_remove.load(std::sync::atomic::Ordering::SeqCst) {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
        } else {
            std::fs::remove_file(path)
        }
    }
}

fn snapshot_rows(path: &Path, key: &DbKey) -> i64 {
    shogun_memory::open_encrypted_existing_read_only(path, key)
        .unwrap()
        .query_row("SELECT count(*) FROM event_log", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn delete_everything_also_removes_every_snapshot() {
    let root = temp_root("delete-all");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([44; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    let db = Db::new(source, clock());
    db.capture(&event("pre-delete secret", "pre-delete")).unwrap();
    let coordinator =
        BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
    for (at, nonce) in [(20, "first"), (21, "second")] {
        let status = db.create_verified_backup(&key, &coordinator, at, nonce).unwrap();
        assert_eq!(snapshot_rows(&status.latest.unwrap().path, &key), 1);
    }

    let outcome = db.delete_all_with_backups(&coordinator).unwrap();
    assert_eq!(outcome.report.events, 1);
    assert!(outcome.backups_removed >= shogun_memory::backup::BACKUP_KEEP);
    // No snapshot is left to hold a pre-delete row, and nothing of ours is left at all.
    assert!(coordinator.list_verified_snapshots(&key).unwrap().is_empty());
    assert!(
        std::fs::read_dir(root.join("backups")).unwrap().next().is_none(),
        "every snapshot, temp and side file is gone"
    );
    // The next snapshot is of the emptied database.
    let status = db.create_verified_backup(&key, &coordinator, 22, "after").unwrap();
    assert_eq!(snapshot_rows(&status.latest.unwrap().path, &key), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_snapshot_that_cannot_be_removed_is_reported_and_the_database_stays_deleted() {
    let root = temp_root("delete-all-fail");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([45; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    let db = Db::new(source, clock());
    db.capture(&event("pre-delete secret", "pre-delete")).unwrap();
    let filesystem = Arc::new(RemoveFailFilesystem {
        fail_remove: std::sync::atomic::AtomicBool::new(false),
    });
    let coordinator = BackupCoordinator::with_boundaries(
        root.join("backups"),
        BackupPolicy::default(),
        filesystem.clone(),
        Arc::new(NoopSleeper),
    )
    .unwrap();
    db.create_verified_backup(&key, &coordinator, 30, "kept").unwrap();

    filesystem
        .fail_remove
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = db.delete_all_with_backups(&coordinator).unwrap_err();
    let shogun_core::daemon::DeleteAllError::Backups { report, failure } = error else {
        panic!("the database delete must have committed: {error:?}");
    };
    assert_eq!(report.events, 1, "the rows were deleted before the backups were touched");
    assert!(failure.failed >= 1);
    assert!(!failure.directory_unreadable);
    // The failure is visible, not swallowed: the snapshot is still there to retry against.
    assert_eq!(coordinator.list_verified_snapshots(&key).unwrap().len(), 1);
    // The live database is consistent: empty, and still writable.
    assert_eq!(db.delete_all().unwrap().events, 0);
    assert!(db.capture(&event("after", "after")).is_some());

    // Retrying finishes the wipe (and deletes the row written since).
    filesystem
        .fail_remove
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let outcome = db.delete_all_with_backups(&coordinator).unwrap();
    assert_eq!(outcome.report.events, 1);
    assert!(coordinator.list_verified_snapshots(&key).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// Opt-in synthetic latency probe: outputs only elapsed times, never captured text.
#[test]
#[ignore = "64 MiB encrypted backup latency probe; run explicitly in release mode"]
fn capture_and_search_remain_available_during_backup_copy() {
    let root = temp_root("copy-latency");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([56; 32]);
    let source = shogun_memory::open_encrypted(root.join("memory.db"), &key).unwrap();
    source.execute_batch("CREATE TABLE backup_load(data BLOB); INSERT INTO backup_load VALUES (zeroblob(67108864));").unwrap();
    let db = Db::new(source, clock());
    db.capture(&event("synthetic searchable evidence", "seed"))
        .unwrap();
    let (prepared_tx, prepared_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    let (copied_tx, copied_rx) = mpsc::channel();
    let (publish_tx, publish_rx) = mpsc::channel();
    let coordinator = BackupCoordinator::with_boundaries(
        root.join("backups"),
        BackupPolicy::default(),
        Arc::new(BoundaryFilesystem {
            sync_behavior: SyncBehavior::Block(copied_tx, Mutex::new(publish_rx)),
            reserve_block: Some((prepared_tx, Mutex::new(start_rx))),
        }),
        Arc::new(NoopSleeper),
    )
    .unwrap();
    let backup_db = db.clone();
    let backup_key = key.clone();
    let backup = std::thread::spawn(move || {
        backup_db.create_verified_backup(&backup_key, &coordinator, 50, "latency")
    });
    prepared_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    start_tx.send(()).unwrap();
    // Destination preparation includes key setup. Wait for copied pages, so samples cannot
    // all finish before the online copy even starts.
    let copying = root.join("backups/.backup-50-latency.tmp");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&copying).map_or(0, |file| file.len()) < 1024 * 1024 {
        assert!(std::time::Instant::now() < deadline, "copy did not start");
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut capture = Vec::new();
    let mut search = Vec::new();
    for i in 0..100 {
        let started = std::time::Instant::now();
        assert!(db
            .capture(&event("synthetic latency sample", &format!("sample{i}")))
            .is_some());
        capture.push(started.elapsed());
        let started = std::time::Instant::now();
        assert!(!db.search("searchable", 5).is_empty());
        search.push(started.elapsed());
    }
    copied_rx.recv_timeout(Duration::from_secs(120)).unwrap();
    publish_tx.send(()).unwrap();
    let status = backup.join().unwrap().unwrap();
    assert_eq!(snapshot_rows(&status.latest.unwrap().path, &key), 1,
        "the pinned copy excludes writes committed after copying started");
    capture.sort_unstable();
    search.sort_unstable();
    eprintln!(
        "64MiB backup: capture p50={:?} p95={:?}; search p50={:?} p95={:?}",
        capture[49], capture[94], search[49], search[94]
    );
    // A blocked first operation must not disappear inside percentile reporting.
    assert!(*capture.last().unwrap() < Duration::from_millis(500));
    assert!(*search.last().unwrap() < Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(root);
}
