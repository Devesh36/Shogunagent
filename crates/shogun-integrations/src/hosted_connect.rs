//! Hosted connector credentials (ShogunAgent E50): the sidecar's [`ConnectorVault`] over a
//! principal's [`TokenStore`].
//!
//! The router's first-party connectors (Gmail, Google Calendar, Slack, Linear, GitHub) act as the
//! calling principal's own vendor account. The gateway (`apps/api`) asks the sidecar for that
//! account's access token per run, and runs the hosted connect flow through it; this module is the
//! sidecar's side of both (`docs/api/connector-tokens.md`):
//!
//! - **Token**: open the principal's grant for the slot from the store the sidecar already keeps
//!   (`shogun-api --multi`: the file-sealed store under each principal's own key, KMS-backed when
//!   the key provider is), refresh it when it expires within 60 seconds, write the refreshed
//!   tokenset back, and hand out the access token only. Refreshes of one principal's slot are
//!   serialized by a per-(principal, slot) lock, re-reading the store under it, so two concurrent
//!   runs never both spend a rotating refresh token (Slack, Linear rotate; a replayed one is
//!   revoked by the vendor).
//! - **Connect**: the authorization-code + PKCE flow the core already speaks
//!   ([`crate::oauth`]: Google, Slack's user token, Linear's user actor), with a web redirect
//!   (`SHOGUN_CONNECT_REDIRECT_URI`, the gateway's callback) instead of the desktop loopback. The
//!   PKCE verifier lives in this process's memory for ten minutes, keyed by a hash of the
//!   gateway's opaque state; the finish is single use and bound to the principal that started it.
//!   The code is exchanged here and the tokenset goes straight into the principal's store; neither
//!   is returned to the gateway. GitHub is served for tokens only: the core connects it with the
//!   device flow (`shogun_core::github_device`), which has no web callback.
//!
//! Every network call goes through the injected [`TokenExchange`] (the core's
//! `HttpTokenExchange`, the single egress; a fake in tests). Nothing here logs, formats or returns
//! a token, a code or a verifier beyond the one access token the vault hands out. Reads the
//! configured client secrets and the stored verifier (`.expose(`) to build the vendor's form:
//! allowlisted in `scripts/check-secret-exposure.py`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use sha2::{Digest, Sha256};
use shogun_mcp::connector_vault::{ConnectStarted, Connected, ConnectorVault, IssuedToken, VaultError};
use shogun_mcp::scope::Service;

use crate::oauth::{self, AuthConfig, Pkce, TokenExchange, TokenSet};
use crate::token::{is_transient_token_failure, GrantStore, DEFAULT_REFRESH_SKEW_MS};
use crate::token_store::{valid_principal, CredentialSlot, PrincipalGrants, Secret, TokenStore};

/// Gmail: drafts and sends (the router's `drafts.create`, `messages.send`); the same scope the Mac
/// app's direct Gmail transport requests. No delete.
pub const GMAIL_SCOPES: &[&str] = &["https://www.googleapis.com/auth/gmail.modify"];
/// Google Calendar: list and insert events (the router's `events.list`, `events.create`).
pub const CALENDAR_SCOPES: &[&str] = &["https://www.googleapis.com/auth/calendar.events"];
/// Slack user token: post a message (the router's `messages.post`).
pub const SLACK_SCOPES: &[&str] = &["chat:write"];
/// Linear user actor: read, and create issues (the router's `issues.create`).
pub const LINEAR_SCOPES: &[&str] = &["read", "issues:create"];

/// How long a started connect flow waits for its callback.
pub const FLOW_TTL_MS: i64 = 10 * 60_000;
/// Pending flows kept at once (all principals). A full table refuses new starts (503), never
/// evicts a live one.
pub const MAX_PENDING_FLOWS: usize = 4096;

/// Whose OAuth endpoints a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Google,
    Slack,
    Linear,
    GitHub,
}

/// One router provider id, the connector service (and so the store slot) behind it.
#[derive(Debug)]
pub struct HostedProvider {
    /// The router's provider id (`apps/router/connectors/<id>`).
    pub id: &'static str,
    pub service: Service,
    pub vendor: Vendor,
    pub scopes: &'static [&'static str],
    /// Whether the hosted connect flow serves it (the core has a web-capable flow for it).
    pub connectable: bool,
}

impl HostedProvider {
    /// The store slot account (`gmail-tokenset`, `gcal-tokenset`, …).
    pub fn slot(&self) -> String {
        CredentialSlot::Connector(self.service).account()
    }
}

/// The fixed provider table: the router's provider id → slot, as `docs/api/connector-tokens.md`.
pub const PROVIDERS: &[HostedProvider] = &[
    HostedProvider { id: "gmail", service: Service::Gmail, vendor: Vendor::Google, scopes: GMAIL_SCOPES, connectable: true },
    HostedProvider { id: "calendar", service: Service::GoogleCalendar, vendor: Vendor::Google, scopes: CALENDAR_SCOPES, connectable: true },
    HostedProvider { id: "slack", service: Service::Slack, vendor: Vendor::Slack, scopes: SLACK_SCOPES, connectable: true },
    HostedProvider { id: "linear", service: Service::Linear, vendor: Vendor::Linear, scopes: LINEAR_SCOPES, connectable: true },
    HostedProvider { id: "github", service: Service::GitHub, vendor: Vendor::GitHub, scopes: &[], connectable: false },
];

pub fn provider_by_id(id: &str) -> Option<&'static HostedProvider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

pub fn provider_by_slot(slot: &str) -> Option<&'static HostedProvider> {
    PROVIDERS.iter().find(|p| p.slot() == slot)
}

/// One OAuth client the operator registered with a vendor for the hosted callback.
#[derive(Clone)]
pub struct OAuthClient {
    client_id: String,
    client_secret: Option<Secret>,
}

impl OAuthClient {
    pub fn new(client_id: impl Into<String>, client_secret: Option<Secret>) -> Self {
        Self { client_id: client_id.into(), client_secret }
    }
}

/// The hosted OAuth clients and the gateway's callback URL (`SHOGUN_CONNECT_*`, `deploy/README.md`).
#[derive(Clone, Default)]
pub struct HostedClients {
    redirect_uri: Option<String>,
    google: Option<OAuthClient>,
    slack: Option<OAuthClient>,
    linear: Option<OAuthClient>,
}

/// The variables [`HostedClients::from_vars`] reads.
pub const REDIRECT_URI_VAR: &str = "SHOGUN_CONNECT_REDIRECT_URI";
pub const GOOGLE_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_GOOGLE_CLIENT_ID";
pub const GOOGLE_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_GOOGLE_CLIENT_SECRET";
pub const SLACK_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_SLACK_CLIENT_ID";
pub const SLACK_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_SLACK_CLIENT_SECRET";
pub const LINEAR_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_LINEAR_CLIENT_ID";
pub const LINEAR_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_LINEAR_CLIENT_SECRET";

/// Whether `uri` may be the hosted callback: `https://`, or plain http to a loopback host
/// (development). No fragment.
pub fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    url.fragment().is_none() && url.username().is_empty() && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
}

impl HostedClients {
    /// Read the clients from the environment. `Err` names the variable that is malformed (never its
    /// value). Unset variables leave that vendor unconfigured: its slots are still served while
    /// their tokens are valid, but they cannot be refreshed or connected.
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, &'static str> {
        let get = |name: &str| vars.get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let redirect_uri = match get(REDIRECT_URI_VAR) {
            Some(uri) if valid_redirect_uri(&uri) => Some(uri),
            Some(_) => return Err(REDIRECT_URI_VAR),
            None => None,
        };
        let client = |id_var: &'static str, secret_var: &'static str, valid: fn(&str) -> bool, secret_required: bool| -> Result<Option<OAuthClient>, &'static str> {
            let Some(id) = get(id_var) else {
                return if get(secret_var).is_some() { Err(id_var) } else { Ok(None) };
            };
            if !valid(&id) {
                return Err(id_var);
            }
            let secret = get(secret_var).map(Secret::new);
            if secret_required && secret.is_none() {
                return Err(secret_var);
            }
            Ok(Some(OAuthClient::new(id, secret)))
        };
        Ok(Self {
            redirect_uri,
            // A Google "Web application" client is confidential: the secret is required.
            google: client(GOOGLE_CLIENT_ID_VAR, GOOGLE_CLIENT_SECRET_VAR, oauth::valid_public_client_id, true)?,
            slack: client(SLACK_CLIENT_ID_VAR, SLACK_CLIENT_SECRET_VAR, oauth::valid_slack_client_id, false)?,
            linear: client(LINEAR_CLIENT_ID_VAR, LINEAR_CLIENT_SECRET_VAR, oauth::valid_public_client_id, false)?,
        })
    }

    #[must_use]
    pub fn with_redirect_uri(mut self, uri: impl Into<String>) -> Self {
        self.redirect_uri = Some(uri.into());
        self
    }

    #[must_use]
    pub fn with_client(mut self, vendor: Vendor, client: OAuthClient) -> Self {
        match vendor {
            Vendor::Google => self.google = Some(client),
            Vendor::Slack => self.slack = Some(client),
            Vendor::Linear => self.linear = Some(client),
            Vendor::GitHub => {}
        }
        self
    }

    /// The provider ids the connect flow can serve with this configuration.
    pub fn connectable(&self) -> Vec<&'static str> {
        PROVIDERS
            .iter()
            .filter(|p| p.connectable && self.redirect_uri.is_some() && self.client(p.vendor).is_some())
            .map(|p| p.id)
            .collect()
    }

    fn client(&self, vendor: Vendor) -> Option<&OAuthClient> {
        match vendor {
            Vendor::Google => self.google.as_ref(),
            Vendor::Slack => self.slack.as_ref(),
            Vendor::Linear => self.linear.as_ref(),
            // The core connects GitHub with the device flow only; no hosted client.
            Vendor::GitHub => None,
        }
    }

    /// The core's [`AuthConfig`] for `vendor`, or `None` when no client is configured. The redirect
    /// is the hosted callback (empty when unset: a refresh does not send it).
    fn auth_config(&self, vendor: Vendor) -> Option<AuthConfig> {
        let client = self.client(vendor)?;
        let redirect = self.redirect_uri.clone().unwrap_or_default();
        let secret = client.client_secret.as_ref().map(|s| s.expose().to_string());
        Some(match vendor {
            Vendor::Google => AuthConfig::google(client.client_id.clone(), secret, redirect),
            Vendor::Slack => match secret {
                Some(secret) => AuthConfig::slack(client.client_id.clone(), secret, redirect),
                None => AuthConfig::slack_public(client.client_id.clone(), redirect),
            },
            Vendor::Linear => {
                let mut cfg = AuthConfig::linear(client.client_id.clone(), redirect);
                cfg.client_secret = secret;
                cfg
            }
            Vendor::GitHub => return None,
        })
    }
}

struct PendingFlow {
    principal: String,
    provider: &'static HostedProvider,
    verifier: Secret,
    expires_at_ms: i64,
}

type ExchangeFactory<X> = Box<dyn Fn() -> Result<X, String> + Send + Sync>;

/// One principal's slot: the unit a refresh is serialized on.
type RowKey = (String, Service);

/// The sidecar's [`ConnectorVault`]: the principals' grants in `tokens`, refreshed and connected
/// through the exchange `make_exchange` builds (once, on first use, off the async workers).
pub struct HostedConnectors<X> {
    tokens: Arc<dyn TokenStore>,
    clients: HostedClients,
    make_exchange: ExchangeFactory<X>,
    exchange: OnceLock<X>,
    rows: Mutex<HashMap<RowKey, Arc<Mutex<()>>>>,
    flows: Mutex<HashMap<[u8; 32], PendingFlow>>,
}

impl<X: TokenExchange + Send + Sync> HostedConnectors<X> {
    pub fn new(tokens: Arc<dyn TokenStore>, clients: HostedClients, make_exchange: impl Fn() -> Result<X, String> + Send + Sync + 'static) -> Self {
        Self {
            tokens,
            clients,
            make_exchange: Box::new(make_exchange),
            exchange: OnceLock::new(),
            rows: Mutex::new(HashMap::new()),
            flows: Mutex::new(HashMap::new()),
        }
    }

    pub fn clients(&self) -> &HostedClients {
        &self.clients
    }

    fn exchange(&self) -> Result<&X, VaultError> {
        if let Some(exchange) = self.exchange.get() {
            return Ok(exchange);
        }
        let made = (self.make_exchange)().map_err(|_| VaultError::Unavailable)?;
        // A racing first call may have set it; either value is the same client.
        let _ = self.exchange.set(made);
        self.exchange.get().ok_or(VaultError::Unavailable)
    }

    /// The lock of one principal's slot: every read-refresh-write of that grant holds it.
    fn row_lock(&self, principal: &str, service: Service) -> Arc<Mutex<()>> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        rows.entry((principal.to_string(), service)).or_default().clone()
    }

    /// Refresh `current` with the vendor and write it back. Called with the row lock held.
    fn refresh(&self, provider: &HostedProvider, grants: &PrincipalGrants<&dyn TokenStore>, current: &TokenSet, now_ms: i64) -> Result<TokenSet, VaultError> {
        let refresh = current.refresh_token.clone().ok_or(VaultError::NotConnected)?;
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let body = self
            .exchange()?
            .post_form(&cfg.token_endpoint, &oauth::refresh_form(&cfg, &refresh))
            .map_err(|reason| if is_transient_token_failure(&reason) { VaultError::Unavailable } else { VaultError::NotConnected })?;
        // A refresh response may omit the refresh token: the stored one stays valid.
        let mut fresh = oauth::parse_token_response_for(&cfg, &body, now_ms, Some(refresh)).map_err(|_| VaultError::NotConnected)?;
        if fresh.account_label.is_none() {
            fresh.account_label = current.account_label.clone();
        }
        // A failed write-back still answers with the fresh access token, which is valid now. When
        // the refresh token rotated, the consumed one is gone either way: the next refresh fails
        // and the human reconnects.
        let _ = grants.save(provider.service, &fresh);
        Ok(fresh)
    }
}

fn issue(tokens: TokenSet) -> IssuedToken {
    IssuedToken { access_token: Secret::new(tokens.access_token), expires_at_ms: tokens.expires_at_ms }
}

/// The gateway's state: opaque, URL-safe, 16–1024 characters.
fn valid_state(state: &str) -> bool {
    (16..=1024).contains(&state.len()) && state.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

fn valid_code(code: &str) -> bool {
    (1..=4096).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_graphic())
}

fn state_key(state: &str) -> [u8; 32] {
    Sha256::digest(format!("shogun/connect-state/v1\n{state}").as_bytes()).into()
}

/// Whether the token response granted every scope `provider` needs. An omitted scope field means
/// the request was granted as asked (RFC 6749 §5.1).
fn scopes_granted(provider: &HostedProvider, body: &str) -> bool {
    if provider.vendor == Vendor::Google {
        return oauth::verify_granted_scopes(body, provider.scopes).is_ok();
    }
    let Ok(response) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let granted = match provider.vendor {
        Vendor::Slack => response.pointer("/authed_user/scope").or_else(|| response.get("scope")),
        _ => response.get("scope"),
    };
    let names: Vec<String> = match granted {
        None => return true,
        Some(serde_json::Value::String(s)) => s.split(|c: char| c == ',' || c.is_ascii_whitespace()).map(str::to_string).collect(),
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(_) => return false,
    };
    provider.scopes.iter().all(|required| names.iter().any(|n| n == required))
}

impl<X: TokenExchange + Send + Sync> ConnectorVault for HostedConnectors<X> {
    fn access_token(&self, principal: &str, slot: &str, now_ms: i64) -> Result<IssuedToken, VaultError> {
        let provider = provider_by_slot(slot).ok_or(VaultError::UnknownSlot)?;
        if !valid_principal(principal) {
            return Err(VaultError::NotConnected);
        }
        let lock = self.row_lock(principal, provider.service);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // Read under the lock: a refresh that just finished on another request is seen here.
        let grants = PrincipalGrants::new(self.tokens.as_ref(), principal);
        let current = grants
            .load(provider.service)
            .map_err(|_| VaultError::Unavailable)?
            .ok_or(VaultError::NotConnected)?;
        if !current.is_expired(now_ms, DEFAULT_REFRESH_SKEW_MS) {
            return Ok(issue(current));
        }
        match self.refresh(provider, &grants, &current, now_ms) {
            Ok(fresh) => Ok(issue(fresh)),
            // Whatever stopped the refresh, a token that has not expired yet is still usable.
            Err(_) if now_ms < current.expires_at_ms => Ok(issue(current)),
            Err(e) => Err(e),
        }
    }

    fn connect_start(&self, principal: &str, provider: &str, state: &str, now_ms: i64) -> Result<ConnectStarted, VaultError> {
        let provider = provider_by_id(provider).ok_or(VaultError::UnknownSlot)?;
        if !valid_principal(principal) {
            return Err(VaultError::Invalid("principal"));
        }
        if !valid_state(state) {
            return Err(VaultError::Invalid("state"));
        }
        if !provider.connectable || self.clients.redirect_uri.is_none() {
            return Err(VaultError::NotConfigured);
        }
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let mut entropy = [0u8; 32];
        getrandom::fill(&mut entropy).map_err(|_| VaultError::Unavailable)?;
        let pkce = Pkce::from_entropy(&entropy);
        entropy.fill(0);
        let authorize_url = oauth::authorize_url(&cfg, provider.scopes, &pkce, state).map_err(|_| VaultError::NotConfigured)?;
        let expires_at_ms = now_ms.saturating_add(FLOW_TTL_MS);
        let mut flows = self.flows.lock().unwrap_or_else(PoisonError::into_inner);
        flows.retain(|_, flow| flow.expires_at_ms > now_ms);
        let key = state_key(state);
        if flows.contains_key(&key) {
            return Err(VaultError::Invalid("state"));
        }
        if flows.len() >= MAX_PENDING_FLOWS {
            return Err(VaultError::Unavailable);
        }
        flows.insert(key, PendingFlow { principal: principal.to_string(), provider, verifier: Secret::new(pkce.verifier), expires_at_ms });
        Ok(ConnectStarted { authorize_url, expires_at_ms })
    }

    fn connect_finish(&self, principal: &str, state: &str, code: &str, now_ms: i64) -> Result<Connected, VaultError> {
        if !valid_state(state) {
            return Err(VaultError::Invalid("state"));
        }
        if !valid_code(code) {
            return Err(VaultError::Invalid("code"));
        }
        // Single use: the flow is gone from here on, whatever happens next.
        let flow = self
            .flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&state_key(state))
            .ok_or(VaultError::FlowUnknown)?;
        if flow.principal != principal || flow.expires_at_ms <= now_ms {
            return Err(VaultError::FlowUnknown);
        }
        let provider = flow.provider;
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let pkce = Pkce { verifier: flow.verifier.expose().to_string(), challenge: String::new() };
        let body = self
            .exchange()?
            .post_form(&cfg.token_endpoint, &oauth::token_exchange_form(&cfg, code, &pkce))
            .map_err(|reason| if is_transient_token_failure(&reason) { VaultError::Unavailable } else { VaultError::ProviderRejected })?;
        if !scopes_granted(provider, &body) {
            return Err(VaultError::ProviderRejected);
        }
        let tokens = oauth::parse_token_response_for(&cfg, &body, now_ms, None).map_err(|_| VaultError::ProviderRejected)?;
        // Google answers a refresh token only with offline access; without one the grant would die
        // within the hour, so it is not stored.
        if provider.vendor == Vendor::Google && tokens.refresh_token.is_none() {
            return Err(VaultError::ProviderRejected);
        }
        let lock = self.row_lock(principal, provider.service);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        PrincipalGrants::new(self.tokens.as_ref(), principal)
            .save(provider.service, &tokens)
            .map_err(|_| VaultError::Unavailable)?;
        Ok(Connected { provider: provider.id, slot: provider.slot() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::MemorySecretStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: i64 = 1_800_000_000_000;
    const REDIRECT: &str = "https://api.example.test/v1/connectors/callback";

    /// A vendor token endpoint: rotating refresh tokens (`r0` → `r1` → …), a replayed one is
    /// `invalid_grant`; the code `good-code` exchanges once.
    #[derive(Default)]
    struct FakeVendor {
        calls: AtomicUsize,
        generation: Mutex<usize>,
        forms: Mutex<Vec<Vec<(String, String)>>>,
        code_body: Mutex<Option<String>>,
        down: std::sync::atomic::AtomicBool,
    }

    impl TokenExchange for Arc<FakeVendor> {
        fn post_form(&self, _endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.forms.lock().unwrap().push(form.to_vec());
            if self.down.load(Ordering::SeqCst) {
                return Err("token exchange failed: connect".into());
            }
            let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
            if get("grant_type").as_deref() == Some("authorization_code") {
                return self.code_body.lock().unwrap().clone().ok_or_else(|| "token endpoint http 400 Bad Request".to_string());
            }
            // Widen the race window: a second refresh must wait for the lock, not overlap this.
            std::thread::sleep(std::time::Duration::from_millis(30));
            let mut generation = self.generation.lock().unwrap();
            if get("refresh_token") != Some(format!("r{generation}")) {
                return Err("token endpoint http 400 Bad Request".into());
            }
            *generation += 1;
            Ok(format!(r#"{{"access_token":"access-{n}-0123456789","token_type":"Bearer","expires_in":3600,"scope":"read","refresh_token":"r{n}"}}"#, n = *generation))
        }
    }

    fn clients() -> HostedClients {
        HostedClients::default()
            .with_redirect_uri(REDIRECT)
            .with_client(Vendor::Google, OAuthClient::new("123-abc.apps.googleusercontent.com", Some(Secret::new("google-web-secret"))))
            .with_client(Vendor::Slack, OAuthClient::new("1234.5678", Some(Secret::new("slack-secret"))))
            .with_client(Vendor::Linear, OAuthClient::new("linear-client", None))
    }

    fn vault(store: &Arc<MemorySecretStore>, vendor: &Arc<FakeVendor>) -> HostedConnectors<Arc<FakeVendor>> {
        let vendor = vendor.clone();
        HostedConnectors::new(store.clone(), clients(), move || Ok(vendor.clone()))
    }

    fn grant(access: &str, refresh: Option<&str>, expires_at_ms: i64) -> TokenSet {
        TokenSet { access_token: access.into(), refresh_token: refresh.map(str::to_string), expires_at_ms, account_label: Some("someone@example.test".into()) }
    }

    fn save(store: &Arc<MemorySecretStore>, principal: &str, service: Service, tokens: &TokenSet) {
        PrincipalGrants::new(store.as_ref(), principal).save(service, tokens).unwrap();
    }

    fn load(store: &Arc<MemorySecretStore>, principal: &str, service: Service) -> Option<TokenSet> {
        PrincipalGrants::new(store.as_ref(), principal).load(service).unwrap()
    }

    #[test]
    fn the_table_matches_the_router_slots() {
        let slots: Vec<(&str, String)> = PROVIDERS.iter().map(|p| (p.id, p.slot())).collect();
        assert_eq!(
            slots,
            vec![
                ("gmail", "gmail-tokenset".to_string()),
                ("calendar", "gcal-tokenset".to_string()),
                ("slack", "slack-tokenset".to_string()),
                ("linear", "linear-tokenset".to_string()),
                ("github", "github-tokenset".to_string()),
            ]
        );
        assert!(provider_by_slot("batch-relay-bearer").is_none());
        assert_eq!(clients().connectable(), vec!["gmail", "calendar", "slack", "linear"]);
        assert!(HostedClients::default().connectable().is_empty());
    }

    #[test]
    fn a_valid_token_is_served_without_a_vendor_call_and_only_to_its_principal() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        save(&store, "prn_a", Service::Gmail, &grant("ya29.alice-access", Some("r0"), NOW + 3_600_000));
        let issued = vault.access_token("prn_a", "gmail-tokenset", NOW).unwrap();
        assert_eq!(issued.access_token, Secret::new("ya29.alice-access"));
        assert_eq!(issued.expires_at_ms, NOW + 3_600_000);
        assert_eq!(vault.access_token("prn_b", "gmail-tokenset", NOW).err(), Some(VaultError::NotConnected));
        assert_eq!(vault.access_token("prn_a", "slack-tokenset", NOW).err(), Some(VaultError::NotConnected));
        assert_eq!(vault.access_token("prn_a", "batch-relay-bearer", NOW).err(), Some(VaultError::UnknownSlot));
        assert_eq!(vault.access_token("../x", "gmail-tokenset", NOW).err(), Some(VaultError::NotConnected));
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_expiring_token_is_refreshed_and_written_back() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        save(&store, "prn_a", Service::GoogleCalendar, &grant("stale", Some("r0"), NOW + 59_000));
        let issued = vault.access_token("prn_a", "gcal-tokenset", NOW).unwrap();
        assert_eq!(issued.access_token, Secret::new("access-1-0123456789"));
        assert_eq!(issued.expires_at_ms, NOW + 3_600_000);
        let stored = load(&store, "prn_a", Service::GoogleCalendar).unwrap();
        assert_eq!(stored.access_token, "access-1-0123456789");
        assert_eq!(stored.refresh_token.as_deref(), Some("r1"), "the rotated refresh token is stored");
        assert_eq!(stored.account_label.as_deref(), Some("someone@example.test"));
        // The Google client's secret goes to the vendor's form, nowhere else.
        let form = vendor.forms.lock().unwrap()[0].clone();
        assert!(form.contains(&("client_secret".into(), "google-web-secret".into())));
        assert!(form.contains(&("refresh_token".into(), "r0".into())));
    }

    #[test]
    fn concurrent_runs_spend_a_rotating_refresh_token_once() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = Arc::new(vault(&store, &vendor));
        save(&store, "prn_a", Service::Linear, &grant("stale", Some("r0"), NOW - 1));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let vault = vault.clone();
                std::thread::spawn(move || vault.access_token("prn_a", "linear-tokenset", NOW).map(|t| t.access_token))
            })
            .collect();
        let tokens: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for token in &tokens {
            assert_eq!(token.as_ref().ok(), Some(&Secret::new("access-1-0123456789")), "every run gets the one refreshed token");
        }
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 1, "one refresh, the others read it under the lock");
        // Another principal's slot is not behind the same lock.
        save(&store, "prn_b", Service::Linear, &grant("b-valid-token", Some("rb"), NOW + 3_600_000));
        assert!(vault.access_token("prn_b", "linear-tokenset", NOW).is_ok());
    }

    #[test]
    fn revoked_and_unavailable_refreshes() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        // invalid_grant: still valid → the current token; expired → not connected.
        save(&store, "prn_a", Service::Gmail, &grant("still-valid-token", Some("revoked"), NOW + 10_000));
        assert_eq!(vault.access_token("prn_a", "gmail-tokenset", NOW).unwrap().access_token, Secret::new("still-valid-token"));
        save(&store, "prn_a", Service::Gmail, &grant("expired-token", Some("revoked"), NOW - 1));
        assert_eq!(vault.access_token("prn_a", "gmail-tokenset", NOW).err(), Some(VaultError::NotConnected));
        // The vendor is down: expired → unavailable (retry), the grant is kept.
        vendor.down.store(true, Ordering::SeqCst);
        assert_eq!(vault.access_token("prn_a", "gmail-tokenset", NOW).err(), Some(VaultError::Unavailable));
        assert!(load(&store, "prn_a", Service::Gmail).is_some());
        // No refresh token, expired → not connected; GitHub has no hosted client → not configured.
        save(&store, "prn_a", Service::Slack, &grant("x", None, NOW - 1));
        assert_eq!(vault.access_token("prn_a", "slack-tokenset", NOW).err(), Some(VaultError::NotConnected));
        save(&store, "prn_a", Service::GitHub, &grant("ghu_expired", Some("ghr"), NOW - 1));
        assert_eq!(vault.access_token("prn_a", "github-tokenset", NOW).err(), Some(VaultError::NotConfigured));
        save(&store, "prn_a", Service::GitHub, &grant("ghu_valid_token", Some("ghr"), NOW + 3_600_000));
        assert!(vault.access_token("prn_a", "github-tokenset", NOW).is_ok());
    }

    fn query(url: &str) -> HashMap<String, String> {
        url::Url::parse(url).unwrap().query_pairs().into_owned().collect()
    }

    const STATE: &str = "gateway-opaque-state-0123456789";

    #[test]
    fn the_connect_flow_stores_the_tokenset_for_the_principal_that_started_it() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        let started = vault.connect_start("prn_a", "gmail", STATE, NOW).unwrap();
        assert_eq!(started.expires_at_ms, NOW + FLOW_TTL_MS);
        let q = query(&started.authorize_url);
        assert!(started.authorize_url.starts_with(oauth::GOOGLE_AUTH_ENDPOINT));
        assert_eq!(q["redirect_uri"], REDIRECT);
        assert_eq!(q["state"], STATE);
        assert_eq!(q["scope"], GMAIL_SCOPES.join(" "));
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["access_type"], "offline");
        assert!(!started.authorize_url.contains("google-web-secret"));

        *vendor.code_body.lock().unwrap() = Some(format!(
            r#"{{"access_token":"ya29.connected-token","expires_in":3599,"refresh_token":"1//refresh","scope":"{}","token_type":"Bearer"}}"#,
            GMAIL_SCOPES[0]
        ));
        // Another principal cannot finish it, and that attempt burns the flow.
        assert_eq!(vault.connect_finish("prn_b", STATE, "good-code", NOW).err(), Some(VaultError::FlowUnknown));
        assert_eq!(vault.connect_finish("prn_a", STATE, "good-code", NOW).err(), Some(VaultError::FlowUnknown));
        assert!(load(&store, "prn_b", Service::Gmail).is_none());

        let state = "gateway-opaque-state-second-0001";
        vault.connect_start("prn_a", "gmail", state, NOW).unwrap();
        let done = vault.connect_finish("prn_a", state, "good-code", NOW + 1_000).unwrap();
        assert_eq!((done.provider, done.slot.as_str()), ("gmail", "gmail-tokenset"));
        let stored = load(&store, "prn_a", Service::Gmail).unwrap();
        assert_eq!(stored.access_token, "ya29.connected-token");
        assert_eq!(stored.refresh_token.as_deref(), Some("1//refresh"));
        // The code went with the PKCE verifier the start generated.
        let form = vendor.forms.lock().unwrap().last().unwrap().clone();
        let verifier = form.iter().find(|(k, _)| k == "code_verifier").unwrap().1.clone();
        assert!(verifier.len() >= 43);
        assert!(form.contains(&("code".into(), "good-code".into())));
        assert!(form.contains(&("redirect_uri".into(), REDIRECT.into())));
        // Single use.
        assert_eq!(vault.connect_finish("prn_a", state, "good-code", NOW + 2_000).err(), Some(VaultError::FlowUnknown));
        // And now the token route serves it.
        assert_eq!(vault.access_token("prn_a", "gmail-tokenset", NOW + 2_000).unwrap().access_token, Secret::new("ya29.connected-token"));
    }

    #[test]
    fn a_flow_refuses_what_the_core_would_not_store() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        let run = |state: &str, body: &str| {
            *vendor.code_body.lock().unwrap() = Some(body.to_string());
            vault.connect_start("prn_a", "calendar", state, NOW).unwrap();
            vault.connect_finish("prn_a", state, "good-code", NOW)
        };
        // A scope the human unticked, and a Google grant without offline access.
        assert_eq!(run("state-scopes-not-granted-01", r#"{"access_token":"a-token-123","expires_in":3600,"refresh_token":"r","scope":"openid"}"#).err(), Some(VaultError::ProviderRejected));
        assert_eq!(run("state-no-refresh-token-0001", r#"{"access_token":"a-token-123","expires_in":3600}"#).err(), Some(VaultError::ProviderRejected));
        // The vendor refused the code.
        *vendor.code_body.lock().unwrap() = None;
        vault.connect_start("prn_a", "calendar", "state-code-refused-000001", NOW).unwrap();
        assert_eq!(vault.connect_finish("prn_a", "state-code-refused-000001", "bad", NOW).err(), Some(VaultError::ProviderRejected));
        // An expired flow.
        vault.connect_start("prn_a", "calendar", "state-expired-flow-000001", NOW).unwrap();
        assert_eq!(vault.connect_finish("prn_a", "state-expired-flow-000001", "c", NOW + FLOW_TTL_MS).err(), Some(VaultError::FlowUnknown));
        assert!(load(&store, "prn_a", Service::GoogleCalendar).is_none());

        // Inputs and providers the flow does not serve.
        assert_eq!(vault.connect_start("prn_a", "github", STATE, NOW).err(), Some(VaultError::NotConfigured));
        assert_eq!(vault.connect_start("prn_a", "notion", STATE, NOW).err(), Some(VaultError::UnknownSlot));
        assert_eq!(vault.connect_start("prn_a", "gmail", "short", NOW).err(), Some(VaultError::Invalid("state")));
        assert_eq!(vault.connect_start("prn_a", "gmail", "has spaces in the state!!", NOW).err(), Some(VaultError::Invalid("state")));
        assert_eq!(vault.connect_finish("prn_a", STATE, "", NOW).err(), Some(VaultError::Invalid("code")));
        let bare = HostedConnectors::new(store.clone(), HostedClients::default(), || Ok(Arc::new(FakeVendor::default())));
        assert_eq!(bare.connect_start("prn_a", "gmail", STATE, NOW).err(), Some(VaultError::NotConfigured));
        // A state already pending is not started twice.
        vault.connect_start("prn_a", "slack", "state-pending-twice-00001", NOW).unwrap();
        assert_eq!(vault.connect_start("prn_a", "slack", "state-pending-twice-00001", NOW).err(), Some(VaultError::Invalid("state")));
    }

    #[test]
    fn slack_and_linear_flows_use_their_own_endpoints_and_scopes() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let vault = vault(&store, &vendor);
        let slack = vault.connect_start("prn_a", "slack", "state-slack-flow-0000001", NOW).unwrap();
        assert!(slack.authorize_url.starts_with(oauth::SLACK_AUTH_ENDPOINT));
        assert_eq!(query(&slack.authorize_url)["scope"], "chat:write");
        *vendor.code_body.lock().unwrap() =
            Some(r#"{"ok":true,"authed_user":{"access_token":"xoxp-user-token-1","token_type":"user","scope":"chat:write"}}"#.into());
        vault.connect_finish("prn_a", "state-slack-flow-0000001", "good-code", NOW).unwrap();
        let stored = load(&store, "prn_a", Service::Slack).unwrap();
        assert_eq!(stored.access_token, "xoxp-user-token-1");
        assert_eq!(stored.expires_at_ms, i64::MAX, "a non-rotating Slack user token does not expire");

        let linear = vault.connect_start("prn_a", "linear", "state-linear-flow-000001", NOW).unwrap();
        assert!(linear.authorize_url.starts_with(oauth::LINEAR_AUTH_ENDPOINT));
        assert_eq!(query(&linear.authorize_url)["scope"], "read,issues:create");
        *vendor.code_body.lock().unwrap() = Some(r#"{"access_token":"lin_oauth_token","token_type":"Bearer","expires_in":86399,"scope":"read issues:create","refresh_token":"lin_refresh"}"#.into());
        vault.connect_finish("prn_a", "state-linear-flow-000001", "good-code", NOW).unwrap();
        assert_eq!(load(&store, "prn_a", Service::Linear).unwrap().refresh_token.as_deref(), Some("lin_refresh"));
    }

    #[test]
    fn clients_read_from_the_environment_name_the_bad_variable() {
        let vars = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        let ok = HostedClients::from_vars(&vars(&[
            (REDIRECT_URI_VAR, REDIRECT),
            (GOOGLE_CLIENT_ID_VAR, "123-abc.apps.googleusercontent.com"),
            (GOOGLE_CLIENT_SECRET_VAR, "s"),
            (LINEAR_CLIENT_ID_VAR, "linear-client"),
        ]))
        .unwrap();
        assert_eq!(ok.connectable(), vec!["gmail", "calendar", "linear"]);
        assert!(HostedClients::from_vars(&HashMap::new()).unwrap().connectable().is_empty());
        for (pairs, bad) in [
            (vec![(REDIRECT_URI_VAR, "http://api.example.test/cb")], REDIRECT_URI_VAR),
            (vec![(GOOGLE_CLIENT_ID_VAR, "123-abc.apps.googleusercontent.com")], GOOGLE_CLIENT_SECRET_VAR),
            (vec![(GOOGLE_CLIENT_SECRET_VAR, "s")], GOOGLE_CLIENT_ID_VAR),
            (vec![(SLACK_CLIENT_ID_VAR, "not-a-slack-id")], SLACK_CLIENT_ID_VAR),
        ] {
            assert_eq!(HostedClients::from_vars(&vars(&pairs)).err(), Some(bad));
        }
        assert!(valid_redirect_uri("http://127.0.0.1:8787/v1/connectors/callback"));
        assert!(!valid_redirect_uri("https://api.example.test/cb#frag"));
    }
}
