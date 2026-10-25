//! Token lifecycle: store, load, and **auto-refresh** OAuth tokens so a first-layer MCP call always
//! has a valid access token (§6.9). This is the working half of "the connection actually stays
//! connected" — a background sync 50 minutes after connecting must not fail just because the access
//! token expired; [`TokenManager`] refreshes it transparently using the stored refresh token.
//!
//! Pure and Linux-testable over two seams: [`TokenStore`] (Keychain on macOS — [`crate::live`] —
//! or [`MemoryTokenStore`] in tests) and [`crate::oauth::TokenExchange`] (reqwest live, fake in
//! tests). Secrets live only in the store (Keychain in production); nothing here logs a token.

use serde::{Deserialize, Serialize};
use shogun_mcp::scope::Service;

use crate::oauth::{self, AuthConfig, TokenExchange, TokenSet};

/// Refresh this many ms before the access token's hard expiry, so an in-flight call never races the
/// boundary.
pub const DEFAULT_REFRESH_SKEW_MS: i64 = 60_000;

// A rotating refresh token belongs to one service. Serialize its refresh with credential edits,
// while an unrelated provider remains usable if this one's refresh stalls.
static CALENDAR_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static DRIVE_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SLACK_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static NOTION_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static GITHUB_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static LINEAR_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static GRANOLA_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static GMAIL_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static WEB_CREDENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub fn credential_guard(service: Service) -> std::sync::MutexGuard<'static, ()> {
    let lock = match service {
        Service::GoogleCalendar => &CALENDAR_CREDENTIAL,
        Service::GoogleDrive => &DRIVE_CREDENTIAL,
        Service::Slack => &SLACK_CREDENTIAL,
        Service::Notion => &NOTION_CREDENTIAL,
        Service::GitHub => &GITHUB_CREDENTIAL,
        Service::Linear => &LINEAR_CREDENTIAL,
        Service::Granola => &GRANOLA_CREDENTIAL,
        Service::Gmail => &GMAIL_CREDENTIAL,
        Service::Web => &WEB_CREDENTIAL,
    };
    // The mutex guards no data, so a panic while it was held leaves nothing inconsistent; a
    // poisoned lock must not disable the service until the app restarts.
    lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Content-free failure to read a stored grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLoadError {
    KeychainLocked,
    Unavailable,
}

impl TokenLoadError {
    /// Only errSecItemNotFound means the grant is absent. Never expose secret bytes in errors.
    pub fn from_keychain_status(status: i32) -> Option<Self> {
        match status {
            crate::keychain_fallback::ITEM_NOT_FOUND => None,
            -128 | -25291 | -25293 | -25308 | -25315 => Some(Self::KeychainLocked),
            _ => Some(Self::Unavailable),
        }
    }
}

/// Persists per-service grants in Keychain in production; memory only in tests.
pub trait TokenStore {
    fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError>;
    fn save(&self, service: Service, tokens: &TokenSet) -> Result<(), String>;
    fn delete(&self, service: Service) -> Result<(), String>;
}

/// Serializable mirror of [`TokenSet`] for whatever the store persists (a Keychain blob is a
/// string). JSON keeps it forward-compatible if fields are added.
#[derive(Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: i64,
    #[serde(default)]
    account_label: Option<String>,
}

/// Serialize a token set for storage.
pub fn serialize(tokens: &TokenSet) -> Result<String, String> {
    serde_json::to_string(&StoredToken {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at_ms: tokens.expires_at_ms,
        account_label: tokens.account_label.clone(),
    })
    .map_err(|_| "failed to serialize token".to_string())
}

/// Parse a stored token blob back into a [`TokenSet`].
pub fn deserialize(blob: &str) -> Result<TokenSet, String> {
    let s: StoredToken = serde_json::from_str(blob).map_err(|_| "failed to parse stored token".to_string())?;
    Ok(TokenSet {
        access_token: s.access_token,
        refresh_token: s.refresh_token,
        expires_at_ms: s.expires_at_ms,
        account_label: s.account_label,
    })
}

/// Why a valid access token could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// No stored token for the service — it is not connected.
    NotConnected,
    Store(TokenLoadError),
    /// The access token is expired and there is no refresh token — the user must reconnect.
    NeedsReauth,
    /// The refresh network call / store failed. Carries a short, content-free reason.
    Refresh(String),
}

/// Whether a token-provider failure is a locked Keychain or a transient refresh fault (no network,
/// DNS, TLS, a timeout, a token endpoint that is down or rate limiting) rather than a problem with
/// the stored grant. A transient failure must not send the user back through OAuth: the stored
/// refresh token is still good and the next attempt can succeed. Everything else — no stored
/// grant ([`TokenError::NotConnected`]), no refresh token ([`TokenError::NeedsReauth`]), no OAuth
/// client, the token endpoint rejecting the grant (`invalid_grant` is an HTTP 400) or answering
/// 401/403 — is a credential problem. Unknown reasons count as credential problems, so a new
/// failure shape can never hide a revoked grant behind "try again later".
///
/// The markers are the content-free strings the live token exchange (`shogun-core`'s
/// `HttpTokenExchange`) produces, wrapped by [`ManagedTokenProvider`] as `Refresh("…")`.
pub fn is_transient_token_failure(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    if reason == "keychain_locked" {
        return true;
    }
    // The send itself failed: no HTTP status came back.
    if reason.contains("token exchange failed") || reason.contains("token response read failed") {
        return true;
    }
    // The endpoint answered, but with a status that says nothing about the grant.
    ["token endpoint http 408", "token endpoint http 429", "token endpoint http 5"]
        .iter()
        .any(|marker| reason.contains(marker))
}

/// Produces a valid access token for a service, refreshing on demand. Borrows the config, the token
/// exchange (network), and the store.
pub struct TokenManager<'a, X: TokenExchange, S: TokenStore> {
    cfg: &'a AuthConfig,
    exchange: &'a X,
    store: &'a S,
}

impl<'a, X: TokenExchange, S: TokenStore> TokenManager<'a, X, S> {
    pub fn new(cfg: &'a AuthConfig, exchange: &'a X, store: &'a S) -> Self {
        Self { cfg, exchange, store }
    }

    /// A currently-valid access token for `service`. If the stored one is within
    /// [`DEFAULT_REFRESH_SKEW_MS`] of expiry, refresh it (persisting the new one) before returning.
    pub fn valid_access_token(&self, service: Service, now_ms: i64) -> Result<String, TokenError> {
        let tokens = self.store.load(service).map_err(TokenError::Store)?.ok_or(TokenError::NotConnected)?;
        if !tokens.is_expired(now_ms, DEFAULT_REFRESH_SKEW_MS) {
            return Ok(tokens.access_token);
        }
        let refresh = tokens.refresh_token.clone().ok_or(TokenError::NeedsReauth)?;
        let form = oauth::refresh_form(self.cfg, &refresh);
        let body = self
            .exchange
            .post_form(&self.cfg.token_endpoint, &form)
            .map_err(TokenError::Refresh)?;
        // A refresh response often omits the refresh token — carry the existing one forward.
        let mut refreshed = oauth::parse_token_response_for(self.cfg, &body, now_ms, Some(refresh.clone()))
            .map_err(TokenError::Refresh)?;
        if refreshed.account_label.is_none() {
            refreshed.account_label = tokens.account_label;
        }
        if self.store.save(service, &refreshed).is_err() {
            // The stored grant is still good when the refresh token did not rotate (Google): use
            // the fresh access token now and refresh again next time. A rotated one consumed the
            // stored token, which must never be replayed, so that grant is gone.
            if refreshed.refresh_token.as_deref() == Some(refresh.as_str()) {
                return Ok(refreshed.access_token);
            }
            let _ = self.store.delete(service);
            return Err(TokenError::NeedsReauth);
        }
        Ok(refreshed.access_token)
    }
}

/// Wall-clock unix-ms — the default clock for [`ManagedTokenProvider`].
pub fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Picks the [`AuthConfig`] (OAuth client + endpoints) used to refresh a service's token. Vendors
/// differ — Google services share one config, Slack has its own token endpoint — so refresh must
/// never cross vendors. `None` = no client registered for that service (its refresh fails cleanly).
pub type ConfigSelector = Box<dyn Fn(Service) -> Option<AuthConfig> + Send + Sync>;

/// A [`crate::rpc::TokenProvider`] that always returns a *valid* access token, refreshing via
/// [`TokenManager`] when the stored one is near expiry. This is what the live MCP client
/// (`shogun-core::mcp_http::HttpMcpRpc`) uses, so a first-layer call an hour after connecting still
/// succeeds. Pure (no network of its own) — the refresh network call goes through the injected
/// [`TokenExchange`].
pub struct ManagedTokenProvider<X: TokenExchange, S: TokenStore> {
    config_for: ConfigSelector,
    exchange: X,
    store: S,
    clock: fn() -> i64,
}

impl<X: TokenExchange, S: TokenStore> ManagedTokenProvider<X, S> {
    /// One config for every service (fine while only Google services are released — they share
    /// endpoints and one OAuth client). Uses the wall clock.
    pub fn new(cfg: AuthConfig, exchange: X, store: S) -> Self {
        Self::with_config_selector(Box::new(move |_| Some(cfg.clone())), exchange, store)
    }

    /// Per-service config selection — required once services from more than one vendor are
    /// connectable (e.g. Google + Slack), so a Slack refresh never posts to Google's endpoint.
    pub fn with_config_selector(config_for: ConfigSelector, exchange: X, store: S) -> Self {
        Self { config_for, exchange, store, clock: system_now_ms }
    }

    /// Override the clock (tests).
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }
}

impl<X: TokenExchange, S: TokenStore> crate::rpc::TokenProvider for ManagedTokenProvider<X, S> {
    fn access_token(&self, service: Service) -> Result<String, String> {
        let _credential_operation = credential_guard(service);
        // PATs and API keys have no OAuth client or refresh exchange. Valid stored credentials
        // can also survive a temporarily unavailable client configuration without crossing vendors.
        let now = (self.clock)();
        if let Some(tokens) = self.store.load(service).map_err(|_| "keychain_locked".to_string())? {
            if !tokens.is_expired(now, DEFAULT_REFRESH_SKEW_MS) {
                return Ok(tokens.access_token);
            }
        }
        let cfg = (self.config_for)(service)
            .ok_or_else(|| format!("no oauth client configured for {}", service.source_str()))?;
        let mgr = TokenManager::new(&cfg, &self.exchange, &self.store);
        mgr.valid_access_token(service, now)
            .map_err(|e| match e {
                TokenError::Store(_) => "keychain_locked".to_string(),
                _ => format!("{e:?}"),
            })
    }
}

/// An in-memory token store for tests and dev (never used in production — production is the Keychain).
#[derive(Default)]
pub struct MemoryTokenStore {
    tokens: std::sync::Mutex<Vec<(Service, TokenSet)>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError> {
        let g = self.tokens.lock().map_err(|_| TokenLoadError::Unavailable)?;
        Ok(g.iter().find(|(s, _)| *s == service).map(|(_, t)| t.clone()))
    }
    fn save(&self, service: Service, tokens: &TokenSet) -> Result<(), String> {
        let mut g = self.tokens.lock().map_err(|_| "store poisoned".to_string())?;
        g.retain(|(s, _)| *s != service);
        g.push((service, tokens.clone()));
        Ok(())
    }
    fn delete(&self, service: Service) -> Result<(), String> {
        let mut g = self.tokens.lock().map_err(|_| "store poisoned".to_string())?;
        g.retain(|(s, _)| *s != service);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_stored_tokens_default_to_no_identity() {
        let tokens = deserialize(r#"{"access_token":"secret","refresh_token":null,"expires_at_ms":123}"#).expect("test");
        assert_eq!(tokens.account_label, None);
        let mut labeled = tokens;
        labeled.account_label = Some("you@example.com".into());
        assert_eq!(deserialize(&serialize(&labeled).expect("test")).expect("test"), labeled);
    }

    use std::cell::RefCell;

    #[test]
    fn keychain_status_never_turns_denial_into_missing() {
        assert_eq!(TokenLoadError::from_keychain_status(-25300), None);
        for status in [-128, -25291, -25293, -25308, -25315] {
            assert_eq!(TokenLoadError::from_keychain_status(status), Some(TokenLoadError::KeychainLocked));
        }
        assert_eq!(TokenLoadError::from_keychain_status(-50), Some(TokenLoadError::Unavailable));
    }

    #[test]
    fn refresh_network_faults_are_transient_and_grant_failures_are_not() {
        let wrap = |e: TokenError| format!("{e:?}");
        for transient in [
            "keychain_locked".to_string(),
            wrap(TokenError::Refresh("token exchange failed: error sending request for url".into())),
            wrap(TokenError::Refresh("token response read failed".into())),
            wrap(TokenError::Refresh("token endpoint http 503 Service Unavailable".into())),
            wrap(TokenError::Refresh("token endpoint http 500 Internal Server Error".into())),
            wrap(TokenError::Refresh("token endpoint http 429 Too Many Requests".into())),
        ] {
            assert!(is_transient_token_failure(&transient), "{transient}");
        }
        for credential in [
            wrap(TokenError::NotConnected),
            wrap(TokenError::NeedsReauth),
            wrap(TokenError::Refresh("token endpoint http 400 Bad Request".into())),
            wrap(TokenError::Refresh("token endpoint http 401 Unauthorized".into())),
            wrap(TokenError::Refresh("Slack OAuth unauthorized".into())),
            wrap(TokenError::Refresh("token response had no access token".into())),
            "no oauth client configured for gmail".to_string(),
            "something new".to_string(),
        ] {
            assert!(!is_transient_token_failure(&credential), "{credential}");
        }
    }

    #[test]
    fn one_service_refresh_does_not_block_another_services_credentials() {
        let calendar = credential_guard(Service::GoogleCalendar);
        let (tx, rx) = std::sync::mpsc::channel();
        let drive = std::thread::spawn(move || {
            let _guard = credential_guard(Service::GoogleDrive);
            tx.send(()).unwrap();
        });
        let independent = rx.recv_timeout(std::time::Duration::from_millis(250)).is_ok();
        drop(calendar);
        drive.join().unwrap();
        assert!(independent, "unrelated credentials waited for the Calendar refresh");
    }

    fn cfg() -> AuthConfig {
        AuthConfig::google("cid", Some("secret".into()), "http://127.0.0.1:0/callback")
    }

    #[test]
    fn locked_store_is_transient_for_every_provider_on_either_read() {
        use crate::rpc::TokenProvider;
        struct LockedStore {
            reads_before_lock: std::cell::Cell<u32>,
            error: TokenLoadError,
        }
        impl TokenStore for LockedStore {
            fn load(&self, _: Service) -> Result<Option<TokenSet>, TokenLoadError> {
                let remaining = self.reads_before_lock.get();
                if remaining == 0 {
                    return Err(self.error);
                }
                self.reads_before_lock.set(remaining - 1);
                Ok(Some(token("expired", Some("refresh"), 0)))
            }
            fn save(&self, _: Service, _: &TokenSet) -> Result<(), String> {
                panic!("a locked read must not write credentials")
            }
            fn delete(&self, _: Service) -> Result<(), String> {
                panic!("a locked read must not delete credentials")
            }
        }
        for &service in shogun_mcp::scope::ALL_SERVICES {
            for error in [TokenLoadError::KeychainLocked, TokenLoadError::Unavailable] {
                for reads_before_lock in [0, 1] {
                    let store = LockedStore { reads_before_lock: std::cell::Cell::new(reads_before_lock), error };
                    let exchange = CountingExchange { calls: RefCell::new(0), reply: Err("must not exchange".into()) };
                    let provider = ManagedTokenProvider::new(cfg(), exchange, store);
                    let reason = provider.access_token(service).expect_err("locked store");
                    assert_eq!(reason, "keychain_locked");
                    assert!(is_transient_token_failure(&reason));
                    assert!(!crate::runtime::is_auth_failure(&reason));
                    assert_eq!(*provider.exchange.calls.borrow(), 0);
                }
            }
        }
    }

    /// Records how many times a refresh was attempted and returns a canned refresh response.
    struct CountingExchange {
        calls: RefCell<u32>,
        reply: Result<String, String>,
    }
    impl TokenExchange for CountingExchange {
        fn post_form(&self, _e: &str, _f: &[(String, String)]) -> Result<String, String> {
            *self.calls.borrow_mut() += 1;
            self.reply.clone()
        }
    }

    fn token(access: &str, refresh: Option<&str>, expires_at_ms: i64) -> TokenSet {
        TokenSet { access_token: access.into(), refresh_token: refresh.map(str::to_string), expires_at_ms, account_label: None }
    }

    #[test]
    fn serialize_round_trips() {
        let t = token("at", Some("rt"), 12345);
        let blob = serialize(&t).unwrap();
        assert_eq!(deserialize(&blob).unwrap(), t);
    }

    #[test]
    fn bearer_credentials_need_no_oauth_client_but_expired_grants_do() {
        use crate::rpc::TokenProvider;
        let store = MemoryTokenStore::new();
        store.save(Service::GitHub, &token("test-pat", None, i64::MAX)).unwrap();
        store.save(Service::Linear, &token("expired", None, 0)).unwrap();
        let exchange = CountingExchange { calls: RefCell::new(0), reply: Err("must not exchange".into()) };
        let provider = ManagedTokenProvider::with_config_selector(Box::new(|_| None), exchange, store);
        assert_eq!(provider.access_token(Service::GitHub).unwrap(), "test-pat");
        assert!(provider.access_token(Service::Linear).is_err());
        assert_eq!(*provider.exchange.calls.borrow(), 0);
    }

    #[test]
    fn valid_token_is_returned_without_refresh() {
        let store = MemoryTokenStore::new();
        store.save(Service::Gmail, &token("live-at", Some("rt"), 1_000_000)).unwrap();
        let ex = CountingExchange { calls: RefCell::new(0), reply: Ok(String::new()) };
        let cfg = cfg();
        let mgr = TokenManager::new(&cfg, &ex, &store);
        // now well before expiry (minus skew) → no refresh
        assert_eq!(mgr.valid_access_token(Service::Gmail, 100_000).unwrap(), "live-at");
        assert_eq!(*ex.calls.borrow(), 0, "must not refresh a still-valid token");
    }

    #[test]
    fn expired_token_is_refreshed_and_persisted() {
        let store = MemoryTokenStore::new();
        let mut original = token("old-at", Some("rt-1"), 1_000);
        original.account_label = Some("you@example.com".into());
        store.save(Service::Gmail, &original).expect("test");
        let ex = CountingExchange {
            calls: RefCell::new(0),
            reply: Ok(r#"{"access_token":"new-at","expires_in":3600}"#.to_string()),
        };
        let cfg = cfg();
        let mgr = TokenManager::new(&cfg, &ex, &store);
        // now is past expiry → refresh happens
        let at = mgr.valid_access_token(Service::Gmail, 2_000).unwrap();
        assert_eq!(at, "new-at");
        assert_eq!(*ex.calls.borrow(), 1);
        // the refreshed token was persisted, carrying the prior refresh token forward
        let stored = store.load(Service::Gmail).expect("test").expect("test");
        assert_eq!(stored.access_token, "new-at");
        assert_eq!(stored.account_label.as_deref(), Some("you@example.com"));
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(stored.expires_at_ms, 2_000 + 3_600_000);
    }

    #[test]
    fn expired_without_refresh_token_needs_reauth() {
        let store = MemoryTokenStore::new();
        store.save(Service::Gmail, &token("old-at", None, 1_000)).unwrap();
        let ex = CountingExchange { calls: RefCell::new(0), reply: Ok(String::new()) };
        let cfg = cfg();
        let mgr = TokenManager::new(&cfg, &ex, &store);
        assert_eq!(mgr.valid_access_token(Service::Gmail, 2_000), Err(TokenError::NeedsReauth));
        assert_eq!(*ex.calls.borrow(), 0);
    }

    #[test]
    fn unconnected_service_errors() {
        let store = MemoryTokenStore::new();
        let ex = CountingExchange { calls: RefCell::new(0), reply: Ok(String::new()) };
        let cfg = cfg();
        let mgr = TokenManager::new(&cfg, &ex, &store);
        assert_eq!(mgr.valid_access_token(Service::Gmail, 0), Err(TokenError::NotConnected));
    }

    fn clock_2000() -> i64 {
        2_000
    }

    /// Loads and deletes like [`MemoryTokenStore`], but every save fails (a locked Keychain).
    struct SaveFails(MemoryTokenStore);
    impl TokenStore for SaveFails {
        fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError> { self.0.load(service) }
        fn save(&self, _: Service, _: &TokenSet) -> Result<(), String> { Err("keychain busy".into()) }
        fn delete(&self, service: Service) -> Result<(), String> { self.0.delete(service) }
    }

    #[test]
    fn a_failed_save_keeps_a_non_rotating_grant_and_drops_a_rotated_one() {
        let cfg = cfg();
        for (reply, kept) in [
            (r#"{"access_token":"fresh","expires_in":3600}"#, true),
            (r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":3600}"#, false),
        ] {
            let store = SaveFails(MemoryTokenStore::new());
            store.0.save(Service::GoogleCalendar, &token("old", Some("r"), 1_000)).unwrap();
            let ex = CountingExchange { calls: RefCell::new(0), reply: Ok(reply.into()) };
            let result = TokenManager::new(&cfg, &ex, &store).valid_access_token(Service::GoogleCalendar, 2_000);
            assert_eq!(result.is_ok(), kept, "{reply}");
            assert_eq!(store.load(Service::GoogleCalendar).expect("test").is_some(), kept, "{reply}");
        }
    }

    #[test]
    fn managed_provider_serves_a_valid_token_via_the_provider_trait() {
        use crate::rpc::TokenProvider;
        let store = MemoryTokenStore::new();
        // stored token is expired at now=2000 → the provider refreshes transparently
        store.save(Service::Gmail, &token("old", Some("rt"), 1_000)).unwrap();
        let ex = CountingExchange {
            calls: RefCell::new(0),
            reply: Ok(r#"{"access_token":"fresh","expires_in":3600}"#.to_string()),
        };
        let provider = ManagedTokenProvider::new(cfg(), ex, store).with_clock(clock_2000);
        assert_eq!(provider.access_token(Service::Gmail).unwrap(), "fresh");
        // an unconnected service surfaces an error string
        assert!(provider.access_token(Service::GoogleCalendar).is_err());
    }

    #[test]
    fn config_selector_scopes_refresh_to_the_right_vendor() {
        use crate::rpc::TokenProvider;
        let store = MemoryTokenStore::new();
        store.save(Service::Gmail, &token("g", Some("r"), 9_999_999)).unwrap();
        store.save(Service::Slack, &token("s", Some("r"), 1_000)).unwrap();
        let ex = CountingExchange { calls: RefCell::new(0), reply: Ok(String::new()) };
        // Only Google services have a config; Slack has none registered.
        let selector: super::ConfigSelector = Box::new(|svc| match svc {
            Service::Gmail | Service::GoogleCalendar | Service::GoogleDrive => {
                Some(AuthConfig::google("cid", None, "http://127.0.0.1:0/cb"))
            }
            _ => None,
        });
        let provider =
            ManagedTokenProvider::with_config_selector(selector, ex, store).with_clock(clock_2000);
        // Gmail's token is valid → served.
        assert_eq!(provider.access_token(Service::Gmail).unwrap(), "g");
        // Slack has an expired stored token but no oauth client → a clean, explicit error (not a
        // cross-vendor refresh against Google's endpoint).
        let err = provider.access_token(Service::Slack).unwrap_err();
        assert!(err.contains("no oauth client"), "got: {err}");
    }

    #[test]
    fn concurrent_providers_refresh_a_rotating_grant_once() {
        use crate::rpc::TokenProvider;
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        #[derive(Clone)]
        struct SharedStore(Arc<MemoryTokenStore>);
        impl TokenStore for SharedStore {
            fn load(&self, service: Service) -> Result<Option<TokenSet>, TokenLoadError> { self.0.load(service) }
            fn save(&self, service: Service, tokens: &TokenSet) -> Result<(),String> { self.0.save(service,tokens) }
            fn delete(&self, service: Service) -> Result<(),String> { self.0.delete(service) }
        }
        struct Exchange(Arc<AtomicUsize>);
        impl TokenExchange for Exchange {
            fn post_form(&self, _: &str, form: &[(String,String)]) -> Result<String,String> {
                assert!(form.iter().any(|(key,value)| key == "refresh_token" && value == "rotate-once"));
                assert_eq!(self.0.fetch_add(1,Ordering::SeqCst),0,"refresh token was replayed");
                Ok(r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":3600}"#.into())
            }
        }
        let store = SharedStore(Arc::new(MemoryTokenStore::new()));
        store.save(Service::Notion,&token("expired",Some("rotate-once"),1_000)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = [0,1].map(|_| ManagedTokenProvider::new(cfg(),Exchange(calls.clone()),store.clone()).with_clock(clock_2000));
        std::thread::scope(|scope| {
            let handles = providers.iter().map(|provider| scope.spawn(move || provider.access_token(Service::Notion))).collect::<Vec<_>>();
            for handle in handles { assert_eq!(handle.join().unwrap().unwrap(),"fresh"); }
        });
        assert_eq!(calls.load(Ordering::SeqCst),1);
        assert_eq!(store.load(Service::Notion).expect("test").expect("test").refresh_token.as_deref(),Some("rotated"));
    }

    #[test]
    fn delete_removes_only_that_service() {
        let store = MemoryTokenStore::new();
        store.save(Service::Gmail, &token("g", Some("r"), 9)).unwrap();
        store.save(Service::GoogleCalendar, &token("c", Some("r"), 9)).unwrap();
        store.delete(Service::Gmail).unwrap();
        assert!(store.load(Service::Gmail).expect("test").is_none());
        assert!(store.load(Service::GoogleCalendar).expect("test").is_some());
    }
}
