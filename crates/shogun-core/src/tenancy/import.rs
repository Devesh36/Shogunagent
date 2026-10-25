//! Import a ShogunAI single-user `memory.db` as a principal's file (E2-T6). The schema is the
//! same lineage (V1–V91 frozen), so the import is a copy — encrypting a plaintext store on the way
//! — followed by the pending migrations (V92+) through the product upgrade path, then the
//! principal's own `principal_meta` rows.

use std::path::{Path, PathBuf};

use super::store::STORE_FILE;
use super::{KeyProvider, PrincipalId, TenancyError};

/// What an import did. Paths only; never a row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImportReport {
    pub principal: String,
    pub path: PathBuf,
    /// The source's schema version before the pending migrations ran (`None`: no history table).
    pub schema_before: Option<u32>,
    pub schema_after: u32,
    /// The source was a plaintext SQLite file and was encrypted with the principal's key.
    pub encrypted_from_plaintext: bool,
}

/// Copy the store at `from` to `<root>/<principal>/memory.db`, encrypting it with the principal's
/// key if it is plaintext (an encrypted source must already use that key — for a ShogunAI store,
/// the app's Keychain key handed over as `SHOGUN_DB_KEY`), apply the pending migrations, and
/// record the principal id and the import in `principal_meta`. Refuses to overwrite an existing
/// principal file and to copy a store whose write-ahead log is still live.
pub fn import_single_user_store(
    from: &Path,
    root: &Path,
    principal: &PrincipalId,
    keys: &dyn KeyProvider,
    now_ms: i64,
) -> Result<ImportReport, TenancyError> {
    if !from.is_file() {
        return Err(TenancyError::SourceMissing);
    }
    let wal = PathBuf::from(format!("{}-wal", from.display()));
    if wal.metadata().is_ok_and(|meta| meta.len() > 0) {
        return Err(TenancyError::SourceInUse);
    }
    let dir = root.join(principal.as_str());
    let dest = dir.join(STORE_FILE);
    if dest.exists() {
        return Err(TenancyError::AlreadyExists);
    }
    let key = keys.key_for(principal)?;
    std::fs::create_dir_all(&dir)?;

    let plaintext = shogun_memory::is_plaintext_db(from);
    let copied = if plaintext {
        shogun_memory::encrypt_existing(from, &dest, &key).map_err(|error| TenancyError::Open(error.to_string()))
    } else {
        std::fs::copy(from, &dest).map(|_| ()).map_err(TenancyError::Io)
    };
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&dest);
        return Err(error);
    }

    let migrated = (|| -> Result<Option<u32>, TenancyError> {
        // The desktop's own existing-store upgrade path: inspect, then migrate and check.
        let mut conn = shogun_memory::open_encrypted_existing_for_upgrade(&dest, &key)
            .map_err(|error| TenancyError::Open(error.to_string()))?;
        let before = shogun_memory::schema_version(&conn).ok().flatten();
        shogun_memory::migrate_and_check_existing(&mut conn).map_err(|error| TenancyError::Open(error.to_string()))?;
        conn.execute(
            "INSERT INTO principal_meta (key, value) VALUES ('principal_id', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [principal.as_str()],
        )
        .map_err(|error| TenancyError::Open(error.to_string()))?;
        conn.execute(
            "INSERT INTO principal_meta (key, value) VALUES ('import_source', 'shogunai_single_user'),
                                                         ('imported_at_ms', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [now_ms.to_string()],
        )
        .map_err(|error| TenancyError::Open(error.to_string()))?;
        Ok(before)
    })();
    let schema_before = match migrated {
        Ok(before) => before,
        Err(error) => {
            // Leave nothing half-imported: the caller can fix the key or the source and retry.
            let _ = std::fs::remove_file(&dest);
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", dest.display()));
            }
            return Err(error);
        }
    };
    Ok(ImportReport {
        principal: principal.to_string(),
        path: dest,
        schema_before,
        schema_after: shogun_memory::LATEST_SCHEMA_VERSION,
        encrypted_from_plaintext: plaintext,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{EnvKeyProvider, PrincipalStore};
    use shogun_mcp::backend::MemoryBackend;
    use shogun_memory::DbKey;
    use std::sync::Arc;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("shogun_import_{label}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key() -> DbKey {
        DbKey::new([0x5a; 32])
    }

    /// A pre-tenancy (V91) single-user store with one event and one commitment, as the test
    /// helpers of the memory crate shape them.
    fn single_user_store(path: &Path) {
        let conn = shogun_memory::open_plaintext_at_version(path, 91).unwrap();
        conn.execute_batch(
            "INSERT INTO event_log (id, ts, source, kind, content, content_hash, last_seen_at)
             VALUES (7, 1760000000000, 'user', 'note', 'Send the forecast to Aiko by Friday', 'h7', 1760000000000);
             INSERT INTO commitments (id, direction, description, status, confidence, created_at, updated_at, last_evidence_at)
             VALUES (3, 'mine', 'Send the forecast to Aiko', 'open', 0.9, 1760000000000, 1760000000000, 1760000000000);
             INSERT INTO state_provenance (state_table, state_id, event_id, weight) VALUES ('commitments', 3, 7, 1.0);",
        )
        .unwrap();
        // Checkpoint so the main file alone carries the rows, as a closed app leaves it.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }

    #[test]
    fn imports_a_plaintext_single_user_store_and_applies_v92_onward() {
        let root = TempRoot::new("plaintext");
        let source = root.0.join("source.db");
        single_user_store(&source);
        let principal = PrincipalId::parse("alice").unwrap();
        let keys = EnvKeyProvider::with_default(key());

        let report = import_single_user_store(&source, &root.0.join("principals"), &principal, &keys, 1_760_000_100_000).unwrap();
        assert_eq!(report.schema_before, Some(91));
        assert_eq!(report.schema_after, shogun_memory::LATEST_SCHEMA_VERSION);
        assert!(report.encrypted_from_plaintext);
        assert_eq!(report.path, root.0.join("principals").join("alice").join("memory.db"));
        assert!(!shogun_memory::is_plaintext_db(&report.path));
        // The source is untouched.
        assert!(shogun_memory::is_plaintext_db(&source));

        // The principal store serves it (the backend answers `state.commitments.list` from it).
        let store = PrincipalStore::new(root.0.join("principals"), Arc::new(keys), Arc::new(|| 1_760_000_200_000), 2);
        let handle = store.open(&principal).unwrap();
        let listed = handle.backend().read(
            shogun_mcp::memory_api::Tool::StateCommitmentsList,
            &shogun_mcp::backend::ReadParams::default(),
        );
        assert_eq!(listed.len(), 1);
        assert!(listed[0].label.contains("Send the forecast to Aiko"), "{}", listed[0].label);
        drop(handle);
        store.close_all();

        // The same row, read straight from the file: the pre-V93 marks and the principal's meta.
        let conn = shogun_memory::open_encrypted_existing_read_only(&report.path, &key()).unwrap();
        let commitments = shogun_memory::state::list_commitments(&conn).unwrap();
        assert_eq!(commitments.len(), 1);
        assert_eq!(commitments[0].id, 3);
        assert_eq!(commitments[0].description, "Send the forecast to Aiko");
        let (actor, namespace): (Option<String>, String) = conn
            .query_row("SELECT actor_last, namespace FROM commitments WHERE id = 3", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(actor, None, "a pre-V93 row reads as the owner's (NULL)");
        assert_eq!(namespace, "world");
        let meta: String = conn
            .query_row("SELECT value FROM principal_meta WHERE key = 'principal_id'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(meta, "alice");
        drop(conn);

        // A second import of the same principal is refused, the file untouched.
        assert!(matches!(
            import_single_user_store(&source, &root.0.join("principals"), &principal, &EnvKeyProvider::with_default(key()), 0),
            Err(TenancyError::AlreadyExists)
        ));
    }

    #[test]
    fn imports_an_encrypted_store_by_copy_and_refuses_a_missing_source() {
        let root = TempRoot::new("encrypted");
        let source = root.0.join("encrypted.db");
        {
            let conn = shogun_memory::open_encrypted(&source, &key()).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        let principal = PrincipalId::parse("bob").unwrap();
        let keys = EnvKeyProvider::with_default(key());
        let report = import_single_user_store(&source, &root.0.join("principals"), &principal, &keys, 0).unwrap();
        assert_eq!(report.schema_before, Some(shogun_memory::LATEST_SCHEMA_VERSION));
        assert!(!report.encrypted_from_plaintext);

        assert!(matches!(
            import_single_user_store(&root.0.join("nope.db"), &root.0, &PrincipalId::parse("c").unwrap(), &keys, 0),
            Err(TenancyError::SourceMissing)
        ));
        // A wrong key leaves nothing behind.
        let wrong = EnvKeyProvider::with_default(DbKey::new([0x01; 32]));
        let carol = PrincipalId::parse("carol").unwrap();
        assert!(matches!(
            import_single_user_store(&source, &root.0.join("principals"), &carol, &wrong, 0),
            Err(TenancyError::Open(_))
        ));
        assert!(!root.0.join("principals").join("carol").join("memory.db").exists());
    }
}
