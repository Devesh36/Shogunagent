#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use shogun_memory::backup::{BackupCoordinator, BackupError, BackupPolicy};
use shogun_memory::{
    open_encrypted, open_encrypted_existing_for_upgrade, open_encrypted_existing_read_only, DbKey,
};

fn temp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "shogun_backup_{label}_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

#[test]
fn encrypted_snapshot_round_trips_but_wrong_key_cannot_read_it() {
    let root = temp_dir("roundtrip");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let live = root.join("memory.db");
    let key = DbKey::new([7; 32]);
    let source = open_encrypted(&live, &key).unwrap();
    source
        .execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at) \
             VALUES (1, 'capture', 'text', 'backup secret', 'backup-hash', 1)",
            [],
        )
        .unwrap();

    let coordinator =
        BackupCoordinator::production(root.join("backups"), BackupPolicy::default()).unwrap();
    let pending = coordinator
        .copy_to_closed_temp(&source, &key, 1_700_000_000_000, "roundtrip")
        .unwrap();
    let status = coordinator.verify_publish_rotate(&key, pending).unwrap();
    let snapshot = status.latest.unwrap().path;

    assert!(!shogun_memory::is_plaintext_db(&snapshot));
    assert!(open_encrypted_existing_read_only(&snapshot, &DbKey::new([8; 32])).is_err());
    let reopened = open_encrypted_existing_read_only(&snapshot, &key).unwrap();
    let count: i64 = reopened
        .query_row("SELECT count(*) FROM event_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn expired_copy_publishes_nothing() {
    let root = temp_dir("deadline");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([9; 32]);
    let source = open_encrypted(root.join("memory.db"), &key).unwrap();
    let coordinator = BackupCoordinator::production(
        root.join("backups"),
        BackupPolicy {
            max_copy_duration: Duration::ZERO,
            ..BackupPolicy::default()
        },
    )
    .unwrap();

    let error = coordinator
        .copy_to_closed_temp(&source, &key, 1_700_000_000_001, "deadline")
        .unwrap_err();
    assert_eq!(error, BackupError::Deadline);
    assert!(coordinator
        .list_verified_snapshots(&key)
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn copy_deadline_includes_time_between_prepare_and_source_lock() {
    let root = temp_dir("prelock-deadline");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let key = DbKey::new([10; 32]);
    let source = open_encrypted(root.join("memory.db"), &key).unwrap();
    let coordinator = BackupCoordinator::production(
        root.join("backups"),
        BackupPolicy {
            max_copy_duration: Duration::from_millis(1),
            ..BackupPolicy::default()
        },
    )
    .unwrap();
    let prepared = coordinator
        .prepare_snapshot(&key, 1_700_000_000_002, "prelock")
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        coordinator.copy_prepared_to_closed(&source, prepared).unwrap_err(),
        BackupError::Deadline
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn existing_open_modes_never_create_missing_files() {
    let root = temp_dir("no-create");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let missing = root.join("missing.db");
    let key = DbKey::new([1; 32]);

    assert!(open_encrypted_existing_read_only(&missing, &key).is_err());
    assert!(open_encrypted_existing_for_upgrade(&missing, &key).is_err());
    assert!(!missing.exists());
    let _ = std::fs::remove_dir_all(&root);
}
