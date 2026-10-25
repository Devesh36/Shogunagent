//! Revoked tokens stop working at once, in processes that are already running (issue #400).
//!
//! The token list (verifiers, scopes, client links) lives in one Keychain item that only the app
//! writes. The standalone faces (`shogun-mcp` over stdio, `shogun-api` for REST and the CLI) used
//! to read it once at start-up, so a token revoked in Settings kept working until the client
//! restarted its server. [`LiveTokens`] re-reads the list when it may have changed:
//!
//! - **Revision marker.** After every change to the Keychain item (issue, revoke, "Remove from …"
//!   with revoke), the app replaces [`TOKEN_REVISION_FILE`] next to `memory_api.json`
//!   ([`bump_revision`]). The file holds a timestamp and nothing else: no token, no verifier, no
//!   id. Each authentication `stat`s it ([`revision_stamp`]) and re-reads the Keychain only when
//!   the stamp differs from the one the cached list was loaded under. The stamp is read *before*
//!   the Keychain, and the app writes the Keychain *before* the marker, so a cached list is never
//!   older than its stamp.
//! - **Safety net.** The list is re-read at least every [`MAX_AGE`] even when the marker did not
//!   move (a failed marker write, a Keychain change from an older build), so no path leaves a
//!   revoked token valid for longer than that.
//! - **Fail closed.** A re-read that fails serves [`TokenRegistry::unavailable`], which refuses
//!   every token and process trust too, and the next call tries again. A marker that cannot be
//!   read, or is deleted or forged, only causes a re-read: it can never keep an old list alive past
//!   [`MAX_AGE`] or widen what a token may do, because the list itself comes from the Keychain.
//!
//! The steady-state cost is one `stat` and an `Arc` clone per call (measured in the tests below).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::memory_api::TokenRegistry;

/// The marker file, next to `memory_api.json` in the app-data directory. Not a secret.
pub const TOKEN_REVISION_FILE: &str = "memory_api_tokens.rev";

/// The longest a running process serves a token list without re-reading it, marker or not.
pub const MAX_AGE: Duration = Duration::from_secs(60);

/// Where the marker is, given the Memory API settings path
/// ([`crate::memory_api_settings::resolve_settings_path`]).
pub fn revision_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name(TOKEN_REVISION_FILE)
}

/// What identifies one version of the marker. The app replaces the file by rename, so the inode
/// changes on every bump; size and modification time are compared too for filesystems where it
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevisionStamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

/// The marker's current stamp; `None` when it does not exist (no token change since install) or
/// cannot be read.
pub fn revision_stamp(path: &Path) -> Option<RevisionStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(RevisionStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
        inode,
    })
}

/// Tell every running face the token list changed. The app calls this after each write to the
/// Keychain item, never before it. The file holds a timestamp only, and is replaced atomically.
pub fn bump_revision(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "could not record the token change".to_string())?;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(TOKEN_REVISION_FILE);
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, format!("{nanos}\n"))
        .map_err(|_| "could not record the token change".to_string())?;
    std::fs::rename(&temp, path).map_err(|_| {
        let _ = std::fs::remove_file(&temp);
        "could not record the token change".to_string()
    })
}

type Loader = Box<dyn Fn() -> Result<TokenRegistry, String> + Send + Sync>;
type Stamper = Box<dyn Fn() -> Option<RevisionStamp> + Send + Sync>;

struct Cached {
    registry: Arc<TokenRegistry>,
    stamp: Option<RevisionStamp>,
    loaded_at: Instant,
    /// `false` after a failed load: served (it refuses everything) but retried on the next call.
    ok: bool,
}

/// The token list a running face authenticates against, re-read when the app changes it (see
/// the module docs). Cheap to share: every face holds one in an `Arc`.
pub struct LiveTokens {
    load: Loader,
    stamp: Stamper,
    max_age: Duration,
    cached: Mutex<Option<Cached>>,
    loads: std::sync::atomic::AtomicU64,
    /// Why the last read failed (the loader's own reason, e.g. a Keychain status), so a face can
    /// say why without reading the Keychain a second time (issue #744). Cleared by a good read.
    last_error: Mutex<Option<String>>,
}

impl LiveTokens {
    /// A list read by `load` (the Keychain item in the binaries), re-read when the marker at
    /// `marker` changes or after [`MAX_AGE`].
    pub fn watching(
        marker: PathBuf,
        load: impl Fn() -> Result<TokenRegistry, String> + Send + Sync + 'static,
    ) -> Self {
        Self::new(load, move || revision_stamp(&marker))
    }

    /// The general form: `stamp` says which version of the list is current (`None` = unknown).
    pub fn new(
        load: impl Fn() -> Result<TokenRegistry, String> + Send + Sync + 'static,
        stamp: impl Fn() -> Option<RevisionStamp> + Send + Sync + 'static,
    ) -> Self {
        Self {
            load: Box::new(load),
            stamp: Box::new(stamp),
            max_age: MAX_AGE,
            cached: Mutex::new(None),
            loads: std::sync::atomic::AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    /// Replace [`MAX_AGE`] (tests; `Duration::ZERO` re-reads on every call).
    #[must_use]
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The list to authenticate this call against. Re-reads it when the marker moved, the cached
    /// copy is older than the maximum age, or the last read failed; a failed read serves a list
    /// that refuses everything.
    pub fn current(&self) -> Arc<TokenRegistry> {
        // Stamp first, then the list (see the module docs for why the order matters).
        let stamp = (self.stamp)();
        let Ok(mut cached) = self.cached.lock() else {
            // A poisoned lock means a loader panicked mid-read: refuse rather than guess.
            return Arc::new(TokenRegistry::unavailable());
        };
        if let Some(current) = cached.as_ref() {
            if current.ok && current.stamp == stamp && current.loaded_at.elapsed() < self.max_age {
                return current.registry.clone();
            }
        }
        self.loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (registry, ok, error) = match (self.load)() {
            Ok(registry) => (Arc::new(registry), true, None),
            Err(error) => (Arc::new(TokenRegistry::unavailable()), false, Some(error)),
        };
        if let Ok(mut last) = self.last_error.lock() {
            *last = error;
        }
        *cached = Some(Cached {
            registry: registry.clone(),
            stamp,
            loaded_at: Instant::now(),
            ok,
        });
        registry
    }

    /// Why the most recent read failed, or `None` after a good one. Never secret material: the
    /// loaders report a status, not what they read.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|last| last.clone())
    }

    /// How many times the list has been read (tests and the overhead measurement).
    pub fn loads(&self) -> u64 {
        self.loads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_api::{AuthResult, TokenScope};
    use crate::memory_api_settings::{issue_into, revoke_from, TokenBlob};

    /// A stand-in for the Keychain item: the serialized blob the app writes, shared with the
    /// "running process" under test.
    #[derive(Clone, Default)]
    struct Store(Arc<Mutex<Option<Vec<u8>>>>);

    impl Store {
        fn write(&self, blob: &TokenBlob) {
            *self.0.lock().unwrap() =
                Some(crate::memory_api_settings::serialize_token_blob(blob).unwrap());
        }
        fn blob(&self) -> TokenBlob {
            match self.0.lock().unwrap().as_ref() {
                Some(bytes) => crate::memory_api_settings::parse_token_blob_with_migration(bytes).unwrap().0,
                None => TokenBlob::default(),
            }
        }
        /// Exactly what the binaries' `load_token_registry` does with the Keychain bytes.
        fn load(&self) -> Result<TokenRegistry, String> {
            let mut tokens = TokenRegistry::new();
            if let Some(bytes) = self.0.lock().unwrap().as_ref() {
                let (blob, _) = crate::memory_api_settings::parse_token_blob_with_migration(bytes)?;
                tokens.register_blob(&blob)?;
            }
            Ok(tokens)
        }
    }

    fn temp_marker(tag: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir()
            .join(format!("shogun-live-tokens-{tag}-{}-{stamp}", std::process::id()))
            .join(TOKEN_REVISION_FILE)
    }

    fn live(store: &Store, marker: &Path) -> LiveTokens {
        let store = store.clone();
        LiveTokens::watching(marker.to_path_buf(), move || store.load())
    }

    /// Settings' Revoke as the app does it: Keychain first, then the marker.
    fn app_revoke(store: &Store, marker: &Path, id: &str) {
        let mut blob = store.blob();
        revoke_from(&mut blob, id).unwrap();
        store.write(&blob);
        bump_revision(marker).unwrap();
    }

    #[test]
    fn a_revoked_token_is_refused_on_the_next_call_and_others_keep_working() {
        let marker = temp_marker("revoke");
        let store = Store::default();
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "Claude Code", 1, "shogun_a", TokenScope::Read).unwrap();
        issue_into(&mut blob, "b".into(), "Cursor", 2, "shogun_b", TokenScope::Full).unwrap();
        store.write(&blob);
        bump_revision(&marker).unwrap();
        let tokens = live(&store, &marker);
        assert_eq!(tokens.current().authenticate(Some("shogun_a")), AuthResult::Granted(TokenScope::Read));
        assert_eq!(tokens.current().authenticate(Some("shogun_b")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(tokens.loads(), 1, "an unchanged marker serves the cached list");

        app_revoke(&store, &marker, "a");
        // Same process, next call: refused, without a restart.
        assert_eq!(tokens.current().authenticate(Some("shogun_a")), AuthResult::DeniedInvalidToken);
        // The unrelated token is unaffected, with its scope.
        assert_eq!(tokens.current().authenticate(Some("shogun_b")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(tokens.loads(), 2);
        std::fs::remove_dir_all(marker.parent().unwrap()).ok();
    }

    #[test]
    fn a_legacy_record_still_works_through_a_live_list() {
        // A record written before scopes and client links (no `scope`, no `client`).
        let marker = temp_marker("legacy");
        let store = Store::default();
        let raw = format!(
            r#"{{"tokens":[{{"id":"old","name":"client","created_at":1,"verifier":"{}"}}]}}"#,
            crate::memory_api_settings::token_verifier("legacy-token")
        );
        *store.0.lock().unwrap() = Some(raw.into_bytes());
        let tokens = live(&store, &marker);
        assert_eq!(tokens.current().authenticate(Some("legacy-token")), AuthResult::Granted(TokenScope::Full));
        assert_eq!(tokens.current().listing()[0].client, None);
    }

    #[test]
    fn a_newly_issued_token_works_at_once_too() {
        let marker = temp_marker("issue");
        let store = Store::default();
        let tokens = live(&store, &marker);
        assert_eq!(tokens.current().authenticate(Some("shogun_new")), AuthResult::DeniedInvalidToken);
        let mut blob = store.blob();
        issue_into(&mut blob, "n".into(), "Codex CLI", 3, "shogun_new", TokenScope::Read).unwrap();
        store.write(&blob);
        bump_revision(&marker).unwrap();
        assert_eq!(tokens.current().authenticate(Some("shogun_new")), AuthResult::Granted(TokenScope::Read));
        std::fs::remove_dir_all(marker.parent().unwrap()).ok();
    }

    #[test]
    fn without_a_marker_bump_the_list_is_still_re_read_after_the_maximum_age() {
        let marker = temp_marker("age");
        let store = Store::default();
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "x", 1, "shogun_a", TokenScope::Read).unwrap();
        store.write(&blob);
        let store2 = store.clone();
        let tokens = LiveTokens::watching(marker.clone(), move || store2.load())
            .with_max_age(Duration::ZERO);
        assert!(matches!(tokens.current().authenticate(Some("shogun_a")), AuthResult::Granted(_)));
        // Revoked in the Keychain but the marker write "failed": the age bound still applies.
        let mut blob = store.blob();
        revoke_from(&mut blob, "a").unwrap();
        store.write(&blob);
        assert_eq!(tokens.current().authenticate(Some("shogun_a")), AuthResult::DeniedInvalidToken);
    }

    #[test]
    fn a_failed_read_keeps_its_reason_until_a_good_read() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = fail.clone();
        let live = LiveTokens::new(
            move || {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    Err("Keychain status -128".into())
                } else {
                    Ok(TokenRegistry::new())
                }
            },
            || None,
        );
        assert!(live.current().is_unavailable());
        assert_eq!(live.last_error().as_deref(), Some("Keychain status -128"));
        assert_eq!(live.loads(), 1, "the reason comes from the one read, not a second one");
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!live.current().is_unavailable());
        assert_eq!(live.last_error(), None);
    }

    #[test]
    fn a_failed_read_refuses_everything_including_process_trust_and_is_retried() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail2 = fail.clone();
        let flip = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let flip2 = flip.clone();
        let tokens = LiveTokens::new(
            move || {
                if fail2.load(std::sync::atomic::Ordering::SeqCst) {
                    Err("keychain unavailable".into())
                } else {
                    let mut r = TokenRegistry::new();
                    r.issue_scoped("t", TokenScope::Read);
                    Ok(r)
                }
            },
            // A stamp that changes on demand, like a bumped marker.
            move || {
                Some(RevisionStamp {
                    len: flip2.load(std::sync::atomic::Ordering::SeqCst),
                    modified: None,
                    inode: 0,
                })
            },
        );
        assert!(matches!(tokens.current().authenticate(Some("t")), AuthResult::Granted(_)));
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        flip.store(1, std::sync::atomic::Ordering::SeqCst);
        let registry = tokens.current();
        assert_eq!(registry.authenticate(Some("t")), AuthResult::DeniedInvalidToken);
        // An unreadable list is not "no token issued yet": a stdio process is refused too.
        assert_eq!(registry.authenticate_process(None), None);
        assert_eq!(registry.authenticate_process(Some("t")), None);
        let loads = tokens.loads();
        // Recovered: the failed read is retried on the next call even with the same stamp.
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(tokens.current().authenticate(Some("t")), AuthResult::Granted(_)));
        assert_eq!(tokens.loads(), loads + 1);
    }

    #[test]
    fn the_marker_holds_no_token_material() {
        let marker = temp_marker("content");
        bump_revision(&marker).unwrap();
        let first = revision_stamp(&marker).unwrap();
        let text = std::fs::read_to_string(&marker).unwrap();
        assert!(text.trim().bytes().all(|b| b.is_ascii_digit()), "{text}");
        bump_revision(&marker).unwrap();
        assert_ne!(revision_stamp(&marker).unwrap(), first, "every bump is a new stamp");
        assert_eq!(revision_path(&marker.with_file_name("memory_api.json")), marker);
        std::fs::remove_dir_all(marker.parent().unwrap()).ok();
    }

    /// The per-call overhead of the live check in the steady state (marker unchanged): one `stat`
    /// plus a lock and an `Arc` clone. Asserted against a generous bound so a loaded CI machine
    /// does not flake; the measured p50/p95 go in the PR (run with `--nocapture`).
    #[test]
    fn steady_state_overhead_is_a_stat_per_call() {
        let marker = temp_marker("overhead");
        let store = Store::default();
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "x", 1, "shogun_a", TokenScope::Read).unwrap();
        store.write(&blob);
        bump_revision(&marker).unwrap();
        let tokens = live(&store, &marker);
        let _ = tokens.current();
        let mut samples: Vec<Duration> = (0..2_000)
            .map(|_| {
                let start = Instant::now();
                let registry = tokens.current();
                let elapsed = start.elapsed();
                assert!(!registry.is_empty());
                elapsed
            })
            .collect();
        samples.sort();
        let p50 = samples[samples.len() / 2];
        let p95 = samples[samples.len() * 95 / 100];
        assert_eq!(tokens.loads(), 1, "no re-read while the marker is unchanged");
        assert!(p95 < Duration::from_millis(5), "p50 {p50:?} p95 {p95:?}");
        std::fs::remove_dir_all(marker.parent().unwrap()).ok();
    }
}
