//! `principal_id → <root>/<id>/memory.db`, with an LRU of open handles (E2-T1).
//!
//! One process serves many principals, so files are opened on demand and closed when the cap is
//! reached. A handle a request still holds is not closed under it: eviction drops the store's own
//! reference, and the connection closes when the last clone of the [`Db`] goes. The count of
//! handles alive anywhere ([`PrincipalStore::live_handles`]) is what the fd-leak test watches.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::daemon::{Clock, DaemonDb, Db};
use crate::db_backend::DbBackend;

use super::{KeyProvider, PrincipalId, TenancyError};

/// The file name inside a principal's directory, the same the desktop app uses.
pub const STORE_FILE: &str = "memory.db";

/// What a purge removed (E39). Both `false`: there was nothing to remove (an earlier purge, or a
/// principal that never had a store) — a purge is idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    /// The principal's directory (its store, write-ahead log and any other file in it) existed.
    pub store_removed: bool,
    /// A key file the key provider owns (a KMS-wrapped data key) existed and was removed.
    pub key_removed: bool,
}

fn remove_file_if_exists(path: &Path) -> Result<bool, TenancyError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(TenancyError::Io(e)),
    }
}

/// Whether the store may create and migrate a missing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPolicy {
    /// The sidecar owns the files: a missing one is created and migrated to the latest schema,
    /// an existing one is migrated forward on open.
    CreateIfMissing,
    /// Headless beside an app that owns the files (#800): never create, never migrate; a store
    /// at another schema version is refused as unavailable.
    ExistingOnly,
}

/// One principal's open store: the DB handle and the Memory API backend over it.
pub struct OpenPrincipal {
    principal: PrincipalId,
    path: PathBuf,
    db: Db,
    backend: Arc<DbBackend>,
    live: Arc<AtomicUsize>,
}

impl OpenPrincipal {
    pub fn principal(&self) -> &PrincipalId {
        &self.principal
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The Memory API backend over this principal's store.
    pub fn backend(&self) -> Arc<DbBackend> {
        self.backend.clone()
    }

    /// The same backend as the trait object the server takes.
    pub fn memory_backend(&self) -> Arc<dyn shogun_mcp::backend::MemoryBackend> {
        self.backend.clone()
    }
}

impl Drop for OpenPrincipal {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct Lru {
    /// Least recently used first.
    order: VecDeque<PrincipalId>,
    open: HashMap<PrincipalId, Arc<OpenPrincipal>>,
}

impl Lru {
    fn touch(&mut self, principal: &PrincipalId) {
        if let Some(at) = self.order.iter().position(|p| p == principal) {
            if let Some(id) = self.order.remove(at) {
                self.order.push_back(id);
            }
        }
    }
}

/// The per-principal store. Clone-free: share it in an `Arc`.
pub struct PrincipalStore {
    root: PathBuf,
    keys: Arc<dyn KeyProvider>,
    clock: Clock,
    max_open: usize,
    policy: OpenPolicy,
    lru: Mutex<Lru>,
    /// `OpenPrincipal` values alive anywhere: in the LRU or borrowed by a request.
    live: Arc<AtomicUsize>,
    /// The local embedding model every opened principal's `Db` gets (E24). One model for every
    /// principal: a server never mixes vector spaces across the people it serves either.
    embedder: Option<Arc<dyn shogun_memory::embed::Embedder>>,
}

impl PrincipalStore {
    /// A store under `root` that keeps at most `max_open` handles (at least one) and creates
    /// missing files ([`OpenPolicy::CreateIfMissing`]).
    pub fn new(root: impl Into<PathBuf>, keys: Arc<dyn KeyProvider>, clock: Clock, max_open: usize) -> Self {
        Self {
            root: root.into(),
            keys,
            clock,
            max_open: max_open.max(1),
            policy: OpenPolicy::CreateIfMissing,
            lru: Mutex::new(Lru::default()),
            live: Arc::new(AtomicUsize::new(0)),
            embedder: None,
        }
    }

    /// Attach the local embedding model to every principal this store opens from now on (E24).
    #[must_use]
    pub fn with_embedder(mut self, embedder: Option<Arc<dyn shogun_memory::embed::Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: OpenPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_open(&self) -> usize {
        self.max_open
    }

    /// `<root>/<principal>/memory.db`.
    pub fn path_for(&self, principal: &PrincipalId) -> PathBuf {
        self.root.join(principal.as_str()).join(STORE_FILE)
    }

    /// Whether a store file exists for `principal`.
    pub fn exists(&self, principal: &PrincipalId) -> bool {
        self.path_for(principal).is_file()
    }

    /// The handle for `principal`, opening the file when it is not already open and evicting the
    /// least recently used handle past the cap. Opens are serialised through the store's lock so
    /// two requests for one principal never open the file twice.
    pub fn open(&self, principal: &PrincipalId) -> Result<Arc<OpenPrincipal>, TenancyError> {
        let mut lru = self.lru.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = lru.open.get(principal).cloned() {
            lru.touch(principal);
            return Ok(handle);
        }
        let handle = Arc::new(self.open_fresh(principal)?);
        while lru.open.len() >= self.max_open {
            match lru.order.pop_front() {
                Some(oldest) => {
                    lru.open.remove(&oldest);
                }
                None => break,
            }
        }
        lru.open.insert(principal.clone(), handle.clone());
        lru.order.push_back(principal.clone());
        Ok(handle)
    }

    fn open_fresh(&self, principal: &PrincipalId) -> Result<OpenPrincipal, TenancyError> {
        let path = self.path_for(principal);
        let key = self.keys.key_for(principal)?;
        let db = match self.policy {
            OpenPolicy::CreateIfMissing => {
                if path.is_file() && shogun_memory::is_plaintext_db(&path) {
                    return Err(TenancyError::Open(
                        "the store is not encrypted; import it with `shogun principal import`".to_string(),
                    ));
                }
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                Db::open_encrypted(&path, &key, self.clock.clone())
                    .map_err(|error| TenancyError::Open(error.to_string()))?
            }
            OpenPolicy::ExistingOnly => {
                if !path.is_file() {
                    return Err(TenancyError::Unavailable("no store for this principal".to_string()));
                }
                let store = DaemonDb::open_with_key(&path, &key, self.clock.clone()).map_err(TenancyError::Open)?;
                if let Some(why) = store.unavailable() {
                    return Err(TenancyError::Unavailable(why.line()));
                }
                store.db().clone()
            }
        };
        let db = match &self.embedder {
            Some(embedder) => db.with_embedder(embedder.clone()),
            None => db,
        };
        self.live.fetch_add(1, Ordering::AcqRel);
        Ok(OpenPrincipal {
            principal: principal.clone(),
            path,
            backend: Arc::new(DbBackend::new(db.clone())),
            db,
            live: self.live.clone(),
        })
    }

    /// Drop the store's handle for `principal`. `true` when one was open. A request still holding
    /// the handle keeps the connection until it finishes.
    pub fn close(&self, principal: &PrincipalId) -> bool {
        let mut lru = self.lru.lock().unwrap_or_else(PoisonError::into_inner);
        lru.order.retain(|p| p != principal);
        lru.open.remove(principal).is_some()
    }

    /// Remove `principal`'s store for good (E39, after a SCIM delete's grace period): close its
    /// handle, then — holding the store's lock so no request can reopen it meanwhile — remove the
    /// key file first (a KMS-wrapped data key: the store is unreadable from here on, even if
    /// removing the rest fails), then the database and its write-ahead files, then the directory.
    /// A request still holding the store refuses the purge with [`TenancyError::Busy`] (retry);
    /// a principal with nothing on disk answers an empty report (idempotent). Never reads a row.
    pub fn purge(&self, principal: &PrincipalId) -> Result<PurgeReport, TenancyError> {
        let mut lru = self.lru.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = lru.open.get(principal) {
            // One reference is the LRU's own; one `DbBackend` reference is the handle's own.
            if Arc::strong_count(handle) > 1 || Arc::strong_count(&handle.backend) > 1 {
                return Err(TenancyError::Busy);
            }
        }
        lru.order.retain(|p| p != principal);
        drop(lru.open.remove(principal));
        self.keys.forget(principal);
        let dir = self.root.join(principal.as_str());
        let key_removed = match self.keys.key_file(principal) {
            Some(path) => remove_file_if_exists(&path)?,
            None => false,
        };
        let mut store_removed = false;
        for suffix in ["", "-wal", "-shm", "-journal"] {
            store_removed |= remove_file_if_exists(&dir.join(format!("{STORE_FILE}{suffix}")))?;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => store_removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(TenancyError::Io(e)),
        }
        // Make the removal durable before answering (best effort: not every filesystem syncs a
        // directory).
        if let Ok(root) = std::fs::File::open(&self.root) {
            let _ = root.sync_all();
        }
        drop(lru);
        Ok(PurgeReport { store_removed, key_removed })
    }

    /// Drop every handle the store holds.
    pub fn close_all(&self) {
        let mut lru = self.lru.lock().unwrap_or_else(PoisonError::into_inner);
        lru.order.clear();
        lru.open.clear();
    }

    /// Handles the LRU currently holds (≤ `max_open`).
    pub fn open_count(&self) -> usize {
        self.lru.lock().unwrap_or_else(PoisonError::into_inner).open.len()
    }

    /// Handles alive anywhere: the LRU's plus any a caller still holds. Zero once everything is
    /// closed and dropped — the fd-leak check.
    pub fn live_handles(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// The principals the LRU holds, least recently used first.
    pub fn open_principals(&self) -> Vec<PrincipalId> {
        self.lru.lock().unwrap_or_else(PoisonError::into_inner).order.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::EnvKeyProvider;
    use shogun_memory::DbKey;

    fn clock() -> Clock {
        Arc::new(|| 1_700_000_000_000)
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("shogun_tenancy_{label}_{}", std::process::id()));
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

    fn store(root: &Path, max_open: usize) -> PrincipalStore {
        PrincipalStore::new(
            root,
            Arc::new(EnvKeyProvider::with_default(DbKey::new([0x5a; 32]))),
            clock(),
            max_open,
        )
    }

    #[test]
    fn creates_the_file_under_the_principal_directory_and_reuses_the_handle() {
        let root = TempRoot::new("layout");
        let store = store(&root.0, 4);
        let alice = PrincipalId::parse("alice").unwrap();
        assert!(!store.exists(&alice));
        let first = store.open(&alice).unwrap();
        assert_eq!(first.path(), root.0.join("alice").join("memory.db"));
        assert!(store.exists(&alice));
        assert!(!shogun_memory::is_plaintext_db(first.path()), "the file must be encrypted");
        let again = store.open(&alice).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(store.open_count(), 1);
        assert_eq!(store.live_handles(), 1);
        drop((first, again));
        assert_eq!(store.live_handles(), 1, "the LRU still holds it");
        assert!(store.close(&alice));
        assert_eq!(store.live_handles(), 0);
        assert!(!store.close(&alice));
    }

    #[test]
    fn a_thousand_principals_open_and_close_within_the_cap_without_leaking_handles() {
        let root = TempRoot::new("thousand");
        let store = store(&root.0, 8);
        // One migrated file, copied 999 times: the test is about open/close, not about running
        // 97 migrations a thousand times. Every copy carries the same key.
        let template = PrincipalId::parse("p0000").unwrap();
        let template_path = store.open(&template).unwrap().path().to_path_buf();
        store.close(&template);
        assert_eq!(store.live_handles(), 0);
        let ids: Vec<PrincipalId> = (0..1000).map(|i| PrincipalId::parse(&format!("p{i:04}")).unwrap()).collect();
        for id in ids.iter().skip(1) {
            let dest = store.path_for(id);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(&template_path, &dest).unwrap();
        }

        let mut peak_open = 0;
        let mut peak_live = 0;
        for (i, id) in ids.iter().enumerate() {
            let handle = store.open(id).unwrap();
            assert_eq!(handle.principal(), id);
            // The handle works: a trivial read on this principal's own connection.
            assert_eq!(handle.db().people().len(), 0);
            peak_open = peak_open.max(store.open_count());
            peak_live = peak_live.max(store.live_handles());
            assert!(store.open_count() <= 8, "open_count {} after {i}", store.open_count());
            assert!(store.live_handles() <= 8, "live {} after {i}", store.live_handles());
            drop(handle);
        }
        assert_eq!(peak_open, 8);
        assert_eq!(peak_live, 8);
        // The last eight are the ones still open, most recent last.
        let open = store.open_principals();
        assert_eq!(open.len(), 8);
        assert_eq!(open.last(), ids.last());
        assert_eq!(open.first(), ids.get(992));
        store.close_all();
        assert_eq!(store.open_count(), 0);
        assert_eq!(store.live_handles(), 0, "every handle closed: no fd leak");
    }

    #[test]
    fn a_borrowed_handle_outlives_its_eviction() {
        let root = TempRoot::new("borrowed");
        let store = store(&root.0, 1);
        let a = PrincipalId::parse("a").unwrap();
        let b = PrincipalId::parse("b").unwrap();
        let held = store.open(&a).unwrap();
        let _other = store.open(&b).unwrap();
        assert_eq!(store.open_count(), 1, "a was evicted for b");
        assert_eq!(store.live_handles(), 2, "but the borrowed handle is still alive");
        assert_eq!(held.db().people().len(), 0);
        drop(held);
        assert_eq!(store.live_handles(), 1);
    }

    #[test]
    fn existing_only_never_creates_a_file() {
        let root = TempRoot::new("existing_only");
        let store = store(&root.0, 2).with_policy(OpenPolicy::ExistingOnly);
        let ghost = PrincipalId::parse("ghost").unwrap();
        assert!(matches!(store.open(&ghost), Err(TenancyError::Unavailable(_))));
        assert!(!store.exists(&ghost));

        // A file the creating policy made is served by the existing-only policy.
        let creator = PrincipalStore::new(
            &root.0,
            Arc::new(EnvKeyProvider::with_default(DbKey::new([0x5a; 32]))),
            clock(),
            2,
        );
        let real = PrincipalId::parse("real").unwrap();
        creator.open(&real).unwrap();
        creator.close_all();
        assert!(store.open(&real).is_ok());
    }

    #[test]
    fn a_purge_closes_the_handle_removes_the_files_and_is_idempotent() {
        let root = TempRoot::new("purge");
        let store = store(&root.0, 4);
        let alice = PrincipalId::parse("alice").unwrap();
        let bob = PrincipalId::parse("bob").unwrap();
        store.open(&bob).unwrap();
        let held = store.open(&alice).unwrap();
        assert_eq!(held.db().people().len(), 0);
        // A request in flight holds the backend: the purge waits for it.
        let in_flight = held.memory_backend();
        drop(held);
        assert!(matches!(store.purge(&alice), Err(TenancyError::Busy)));
        assert!(store.exists(&alice), "nothing was removed while busy");
        drop(in_flight);
        let report = store.purge(&alice).unwrap();
        assert_eq!(report, PurgeReport { store_removed: true, key_removed: false });
        assert!(!store.exists(&alice));
        assert!(!root.0.join("alice").exists(), "the whole directory is gone");
        assert_eq!(store.open_principals(), vec![bob.clone()], "only alice's handle was closed");
        assert_eq!(store.live_handles(), 1);
        // Again: nothing left, still a success.
        assert_eq!(store.purge(&alice).unwrap(), PurgeReport { store_removed: false, key_removed: false });
        assert!(store.exists(&bob), "another principal's store is untouched");
    }

    #[test]
    fn a_wrong_key_or_missing_key_is_an_error_not_a_panic() {
        let root = TempRoot::new("keys");
        let alice = PrincipalId::parse("alice").unwrap();
        store(&root.0, 2).open(&alice).unwrap();
        let wrong = PrincipalStore::new(
            &root.0,
            Arc::new(EnvKeyProvider::with_default(DbKey::new([0x11; 32]))),
            clock(),
            2,
        );
        assert!(matches!(wrong.open(&alice), Err(TenancyError::Open(_))));
        let none = PrincipalStore::new(&root.0, Arc::new(EnvKeyProvider::default()), clock(), 2);
        assert!(matches!(none.open(&alice), Err(TenancyError::NoKey)));
    }
}
