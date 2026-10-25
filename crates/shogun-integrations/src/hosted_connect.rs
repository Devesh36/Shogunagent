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
//!   runs never both spend a rotating refresh token (Slack, Linear, GitHub rotate; a replayed one
//!   is revoked by the vendor).
//! - **Connect**: the authorization-code + PKCE flow the core already speaks
//!   ([`crate::oauth`]: Google, Slack's user token, Linear's user actor), with a web redirect
//!   (`SHOGUN_CONNECT_REDIRECT_URI`, the gateway's callback) instead of the desktop loopback; and
//!   GitHub's device flow ([`crate::device_flow`], the steps the Mac app's flow uses), which needs
//!   no callback: the human types a code at github.com and presses "Check" on the gateway's page,
//!   and each check is one poll here. A pending flow (the PKCE verifier, or the device code) is
//!   kept sealed in the principal's own store ([`CredentialSlot::ConnectFlows`]) for ten minutes,
//!   keyed by a hash of the gateway's opaque state, so a sidecar restart does not lose it; the
//!   finish is single use and only the principal that started a flow can find it. The code is
//!   exchanged here and the tokenset goes straight into the principal's store; neither is
//!   returned to the gateway.
//! - **Status and disconnect** (`connections.list` / `connections.disconnect` on every face, and
//!   the gateway's REST): which connectors hold a usable grant, and removing one — revoked at the
//!   vendor where it has a revocation endpoint (Google, Slack), deleted from the store always.
//! - **Audit**: every connect, refresh, disconnect, revocation and read-sync fetch is reported to
//!   the [`AuditSink`] the core composes (a traceability row in the principal's own store: the
//!   action, provider, vendor host and outcome; never a token, a code or a body).
//! - **Read-sync queue** ([`crate::read_sync`]): the change notices the ingest hooks deliver for
//!   Gmail, Calendar and Notion, waiting in [`CredentialSlot::SyncQueue`] for the background tick.
//!
//! Every network call goes through the injected [`TokenExchange`] (the core's
//! `HttpTokenExchange`, the single egress; a fake in tests). Nothing here logs, formats or returns
//! a token, a code or a verifier beyond the one access token the vault hands out. Reads the
//! configured client secrets and the stored verifier (`.expose(`) to build the vendor's form:
//! allowlisted in `scripts/check-secret-exposure.py`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shogun_mcp::connector_vault::{
    ConnectPoll, ConnectStarted, Connected, ConnectorState, ConnectorVault, DeviceStep, Disconnected, IssuedToken, SyncQueued, VaultError,
};
use shogun_mcp::scope::Service;

use crate::device_flow::{self, DevicePoll};
use crate::oauth::{self, AuthConfig, Pkce, TokenExchange, TokenSet};
use crate::token::{is_transient_token_failure, GrantStore, DEFAULT_REFRESH_SKEW_MS};
use crate::token_store::{valid_principal, CredentialSlot, Modify, PrincipalGrants, Secret, TokenStore, TokenStoreError};

/// Gmail: drafts and sends (the router's `drafts.create`, `messages.send`), and the reads the
/// router's `messages.search` / `messages.get` and the read-sync make; the same scope the Mac
/// app's direct Gmail transport requests. No delete.
pub const GMAIL_SCOPES: &[&str] = &["https://www.googleapis.com/auth/gmail.modify"];
/// Google Calendar: list and insert events (the router's `events.list`, `events.create`).
pub const CALENDAR_SCOPES: &[&str] = &["https://www.googleapis.com/auth/calendar.events"];
/// Slack user token: post a message (the router's `messages.post`).
pub const SLACK_SCOPES: &[&str] = &["chat:write"];
/// Linear user actor: read, and create issues (the router's `issues.create`).
pub const LINEAR_SCOPES: &[&str] = &["read", "issues:create"];

/// How long a started connect flow waits for its callback (or its last device poll).
pub const FLOW_TTL_MS: i64 = 10 * 60_000;
/// Pending flows one principal keeps at once. A new start past it drops that principal's oldest
/// own flow (a human who pressed Continue again); another principal's flows are never touched.
pub const MAX_PENDING_FLOWS_PER_PRINCIPAL: usize = 8;

/// Whose OAuth endpoints a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Google,
    Slack,
    Linear,
    GitHub,
    /// Notion's REST API. The core has no hosted Notion OAuth flow (its Notion sign-in is the Mac
    /// app's loopback registration with Notion's MCP server, whose tokens do not open the REST
    /// API): a Notion grant is served while it is valid and cannot be connected or refreshed here.
    Notion,
}

impl Vendor {
    /// The host of the vendor's token endpoint (the destination of a refresh, a connect or a
    /// revocation in the audit row).
    pub fn token_host(self) -> &'static str {
        match self {
            Vendor::Google => "oauth2.googleapis.com",
            Vendor::Slack => "slack.com",
            Vendor::Linear => "api.linear.app",
            Vendor::GitHub => "github.com",
            Vendor::Notion => "api.notion.com",
        }
    }

    /// Where the vendor revokes a grant, when the core can call it with the token alone: Google's
    /// RFC 7009 endpoint, Slack's `auth.revoke`. Linear's and GitHub's revocation need the app's
    /// client secret or are not verified here, so a disconnect only deletes the stored grant
    /// (`vendor_revoke: "not_supported"`; the human can also remove the app at the vendor).
    pub fn revoke_endpoint(self) -> Option<&'static str> {
        match self {
            Vendor::Google => Some("https://oauth2.googleapis.com/revoke"),
            Vendor::Slack => Some("https://slack.com/api/auth.revoke"),
            Vendor::Linear | Vendor::GitHub | Vendor::Notion => None,
        }
    }
}

/// One router provider id, the connector service (and so the store slot) behind it.
#[derive(Debug)]
pub struct HostedProvider {
    /// The router's provider id (`apps/router/connectors/<id>`).
    pub id: &'static str,
    pub service: Service,
    pub vendor: Vendor,
    pub scopes: &'static [&'static str],
    /// Whether the hosted connect flow serves it (the core has a hosted flow for it).
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
    // The device flow (a GitHub App's user-to-server token); no scopes: the app's permissions.
    HostedProvider { id: "github", service: Service::GitHub, vendor: Vendor::GitHub, scopes: &[], connectable: true },
    // Read-sync only (Notion change notices): not a router provider, not connectable here.
    HostedProvider { id: "notion", service: Service::Notion, vendor: Vendor::Notion, scopes: &[], connectable: false },
];

pub fn provider_by_id(id: &str) -> Option<&'static HostedProvider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

pub fn provider_by_slot(slot: &str) -> Option<&'static HostedProvider> {
    PROVIDERS.iter().find(|p| p.slot() == slot)
}

pub fn provider_by_service(service: Service) -> Option<&'static HostedProvider> {
    PROVIDERS.iter().find(|p| p.service == service)
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
    github: Option<OAuthClient>,
}

/// The variables [`HostedClients::from_vars`] reads.
pub const REDIRECT_URI_VAR: &str = "SHOGUN_CONNECT_REDIRECT_URI";
pub const GOOGLE_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_GOOGLE_CLIENT_ID";
pub const GOOGLE_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_GOOGLE_CLIENT_SECRET";
pub const SLACK_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_SLACK_CLIENT_ID";
pub const SLACK_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_SLACK_CLIENT_SECRET";
pub const LINEAR_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_LINEAR_CLIENT_ID";
pub const LINEAR_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_LINEAR_CLIENT_SECRET";
/// The GitHub App's client id (device flow enabled). Public: the device flow needs no secret.
pub const GITHUB_CLIENT_ID_VAR: &str = "SHOGUN_CONNECT_GITHUB_CLIENT_ID";
/// Optional: sent with GitHub refreshes when the app requires it.
pub const GITHUB_CLIENT_SECRET_VAR: &str = "SHOGUN_CONNECT_GITHUB_CLIENT_SECRET";

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
            github: client(GITHUB_CLIENT_ID_VAR, GITHUB_CLIENT_SECRET_VAR, oauth::valid_public_client_id, false)?,
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
            Vendor::GitHub => self.github = Some(client),
            Vendor::Notion => {}
        }
        self
    }

    /// The provider ids the connect flow can serve with this configuration: a redirect flow needs
    /// the callback and the vendor's client, the device flow (GitHub) its client only.
    pub fn connectable(&self) -> Vec<&'static str> {
        PROVIDERS.iter().filter(|p| self.can_connect(p)).map(|p| p.id).collect()
    }

    fn can_connect(&self, provider: &HostedProvider) -> bool {
        provider.connectable && self.client(provider.vendor).is_some() && (provider.vendor == Vendor::GitHub || self.redirect_uri.is_some())
    }

    fn client(&self, vendor: Vendor) -> Option<&OAuthClient> {
        match vendor {
            Vendor::Google => self.google.as_ref(),
            Vendor::Slack => self.slack.as_ref(),
            Vendor::Linear => self.linear.as_ref(),
            Vendor::GitHub => self.github.as_ref(),
            // No hosted Notion flow in the core.
            Vendor::Notion => None,
        }
    }

    /// The core's [`AuthConfig`] for `vendor`, or `None` when no client is configured. The redirect
    /// is the hosted callback (empty when unset, and for GitHub's device flow: a refresh does not
    /// send it).
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
            Vendor::GitHub => AuthConfig {
                client_id: client.client_id.clone(),
                client_secret: secret,
                redirect_uri: String::new(),
                auth_endpoint: device_flow::DEVICE_ENDPOINT.to_string(),
                token_endpoint: oauth::GITHUB_TOKEN_ENDPOINT.to_string(),
            },
            Vendor::Notion => return None,
        })
    }
}

/// One audited connector action. Fixed words and a host: never a token, a code, a body or an
/// account identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditEvent {
    /// `connect_start` (the device flow's first vendor call), `connect`, `refresh`, `disconnect`,
    /// `revoke` or `sync` (one read-sync fetch).
    pub action: &'static str,
    /// The router provider id (`gmail`, `calendar`, …).
    pub provider: &'static str,
    /// `ok`, `rejected`, `unavailable`, `not_supported` or `failed`.
    pub outcome: &'static str,
    /// The vendor host the call went to (`oauth2.googleapis.com`, …), or `local` when it made none
    /// (a disconnect).
    pub destination: &'static str,
}

/// Where audit events go: the core writes each as a traceability row in the principal's store.
pub type AuditSink = Arc<dyn Fn(&str, &AuditEvent) + Send + Sync>;

/// A pending flow as it is sealed in the principal's store. No `Debug`: it holds the verifier or
/// the device code.
#[derive(Clone, Serialize, Deserialize)]
struct StoredFlow {
    /// Hex of [`state_key`].
    key: String,
    provider: String,
    /// The PKCE verifier (`pkce`) or the GitHub device code (`device`).
    secret: String,
    #[serde(default)]
    device: bool,
    #[serde(default)]
    user_code: Option<String>,
    #[serde(default)]
    interval_ms: i64,
    #[serde(default)]
    next_poll_ms: i64,
    expires_at_ms: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredFlows {
    #[serde(default)]
    flows: Vec<StoredFlow>,
}

type ExchangeFactory<X> = Box<dyn Fn() -> Result<X, String> + Send + Sync>;

/// One principal's slot account: the unit a read-modify-write is serialized on in this process.
type RowKey = (String, String);

/// The sidecar's [`ConnectorVault`]: the principals' grants in `tokens`, refreshed and connected
/// through the exchange `make_exchange` builds (once, on first use, off the async workers).
pub struct HostedConnectors<X> {
    tokens: Arc<dyn TokenStore>,
    clients: HostedClients,
    make_exchange: ExchangeFactory<X>,
    exchange: OnceLock<X>,
    rows: Mutex<HashMap<RowKey, Arc<Mutex<()>>>>,
    audit: Option<AuditSink>,
}

impl<X: TokenExchange + Send + Sync> HostedConnectors<X> {
    pub fn new(tokens: Arc<dyn TokenStore>, clients: HostedClients, make_exchange: impl Fn() -> Result<X, String> + Send + Sync + 'static) -> Self {
        Self {
            tokens,
            clients,
            make_exchange: Box::new(make_exchange),
            exchange: OnceLock::new(),
            rows: Mutex::new(HashMap::new()),
            audit: None,
        }
    }

    /// Report every connect, refresh, disconnect, revocation and read-sync fetch to `sink`.
    #[must_use]
    pub fn with_audit(mut self, sink: AuditSink) -> Self {
        self.audit = Some(sink);
        self
    }

    pub fn clients(&self) -> &HostedClients {
        &self.clients
    }

    /// The principals' store (the read-sync queue lives in it too).
    pub fn tokens(&self) -> &Arc<dyn TokenStore> {
        &self.tokens
    }

    /// Report one event (no-op without a sink).
    pub fn audit(&self, principal: &str, event: AuditEvent) {
        if let Some(sink) = &self.audit {
            sink(principal, &event);
        }
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

    /// The in-process lock of one principal's slot: every read-modify-write of it holds it (the
    /// file store adds its own cross-process lock inside [`TokenStore::modify`]).
    pub(crate) fn slot_lock(&self, principal: &str, slot: CredentialSlot) -> Arc<Mutex<()>> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        rows.entry((principal.to_string(), slot.account())).or_default().clone()
    }

    /// Run `f` over the principal's value of `slot` (a JSON document; a missing, unreadable or
    /// corrupt one reads as the default) and write back what it leaves; an empty document deletes
    /// the slot. Under the in-process lock and the store's own.
    pub(crate) fn modify_json<T, R>(&self, principal: &str, slot: CredentialSlot, f: impl FnOnce(&mut T) -> R) -> Result<R, VaultError>
    where
        T: Default + Serialize + for<'de> Deserialize<'de>,
    {
        let lock = self.slot_lock(principal, slot);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut f = Some(f);
        let mut out = None;
        let mut step = |current: Option<Secret>| -> Modify {
            let mut doc: T = current.and_then(|v| serde_json::from_str(v.expose()).ok()).unwrap_or_default();
            if let Some(f) = f.take() {
                out = Some(f(&mut doc));
            }
            match serde_json::to_string(&doc) {
                Ok(text) if text == "{}" || text == r#"{"flows":[]}"# || text == r#"{"jobs":[]}"# => Modify::Delete,
                Ok(text) => Modify::Put(Secret::new(text)),
                Err(_) => Modify::Keep,
            }
        };
        match self.tokens.modify(principal, slot, &mut step) {
            Ok(()) => {}
            // A value that no longer opens (sealed under a retired key, tampered): start over.
            Err(TokenStoreError::Unavailable) => {
                self.tokens.delete(principal, slot).map_err(|_| VaultError::Unavailable)?;
                self.tokens.modify(principal, slot, &mut step).map_err(|_| VaultError::Unavailable)?;
            }
            Err(_) => return Err(VaultError::Unavailable),
        }
        out.ok_or(VaultError::Unavailable)
    }

    /// Refresh `current` with the vendor and write it back. Called with the row lock held.
    fn refresh(&self, principal: &str, provider: &HostedProvider, grants: &PrincipalGrants<&dyn TokenStore>, current: &TokenSet, now_ms: i64) -> Result<TokenSet, VaultError> {
        let refresh = current.refresh_token.clone().ok_or(VaultError::NotConnected)?;
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let event = |outcome| AuditEvent { action: "refresh", provider: provider.id, outcome, destination: provider.vendor.token_host() };
        let body = self.exchange()?.post_form(&cfg.token_endpoint, &oauth::refresh_form(&cfg, &refresh)).map_err(|reason| {
            if is_transient_token_failure(&reason) {
                self.audit(principal, event("unavailable"));
                VaultError::Unavailable
            } else {
                self.audit(principal, event("rejected"));
                VaultError::NotConnected
            }
        })?;
        // A refresh response may omit the refresh token: the stored one stays valid.
        let mut fresh = oauth::parse_token_response_for(&cfg, &body, now_ms, Some(refresh)).map_err(|_| {
            self.audit(principal, event("rejected"));
            VaultError::NotConnected
        })?;
        if fresh.account_label.is_none() {
            fresh.account_label = current.account_label.clone();
        }
        self.audit(principal, event("ok"));
        // A failed write-back still answers with the fresh access token, which is valid now. When
        // the refresh token rotated, the consumed one is gone either way: the next refresh fails
        // and the human reconnects.
        let _ = grants.save(provider.service, &fresh);
        Ok(fresh)
    }

    /// Store a connected tokenset for `provider` under its row lock.
    fn store_grant(&self, principal: &str, provider: &HostedProvider, tokens: &TokenSet) -> Result<(), VaultError> {
        let lock = self.slot_lock(principal, CredentialSlot::Connector(provider.service));
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        PrincipalGrants::new(self.tokens.as_ref(), principal).save(provider.service, tokens).map_err(|_| VaultError::Unavailable)
    }

    /// Remove and answer the flow `state` names from the principal's pending flows.
    fn take_flow(&self, principal: &str, state: &str) -> Result<Option<StoredFlow>, VaultError> {
        let key = hex(&state_key(state));
        self.modify_json(principal, CredentialSlot::ConnectFlows, |doc: &mut StoredFlows| {
            let at = doc.flows.iter().position(|f| f.key == key)?;
            Some(doc.flows.remove(at))
        })
    }

    /// Add a flow, dropping expired ones and, past the cap, the principal's oldest.
    fn put_flow(&self, principal: &str, flow: StoredFlow, now_ms: i64) -> Result<bool, VaultError> {
        self.modify_json(principal, CredentialSlot::ConnectFlows, |doc: &mut StoredFlows| {
            doc.flows.retain(|f| f.expires_at_ms > now_ms);
            if doc.flows.iter().any(|f| f.key == flow.key) {
                return false;
            }
            while doc.flows.len() >= MAX_PENDING_FLOWS_PER_PRINCIPAL {
                doc.flows.remove(0);
            }
            doc.flows.push(flow);
            true
        })
    }

    fn start_device(&self, principal: &str, provider: &'static HostedProvider, state: &str, now_ms: i64) -> Result<ConnectStarted, VaultError> {
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let event = |outcome| AuditEvent { action: "connect_start", provider: provider.id, outcome, destination: provider.vendor.token_host() };
        let body = self.exchange()?.post_form(device_flow::DEVICE_ENDPOINT, &device_flow::start_form(&cfg.client_id)).map_err(|reason| {
            if is_transient_token_failure(&reason) {
                self.audit(principal, event("unavailable"));
                VaultError::Unavailable
            } else {
                self.audit(principal, event("rejected"));
                VaultError::ProviderRejected
            }
        })?;
        let grant = device_flow::parse_device(&body, now_ms).map_err(|_| {
            self.audit(principal, event("rejected"));
            VaultError::ProviderRejected
        })?;
        self.audit(principal, event("ok"));
        // The gateway's state lives ten minutes: the flow does not outlive it.
        let expires_at_ms = grant.expires_at_ms.min(now_ms.saturating_add(FLOW_TTL_MS));
        let flow = StoredFlow {
            key: hex(&state_key(state)),
            provider: provider.id.to_string(),
            secret: grant.device_code,
            device: true,
            user_code: Some(grant.user_code.clone()),
            interval_ms: grant.interval_ms,
            next_poll_ms: now_ms.saturating_add(grant.interval_ms),
            expires_at_ms,
        };
        if !self.put_flow(principal, flow, now_ms)? {
            return Err(VaultError::Invalid("state"));
        }
        Ok(ConnectStarted {
            authorize_url: device_flow::VERIFICATION_URI.to_string(),
            expires_at_ms,
            device: Some(DeviceStep { user_code: grant.user_code, verification_uri: device_flow::VERIFICATION_URI.to_string(), interval_ms: grant.interval_ms }),
        })
    }

    /// The state of one provider's grant for `status`.
    fn grant_state(&self, principal: &str, provider: &HostedProvider, now_ms: i64) -> Result<(&'static str, Option<i64>), VaultError> {
        let grant = PrincipalGrants::new(self.tokens.as_ref(), principal).load(provider.service).map_err(|_| VaultError::Unavailable)?;
        Ok(match grant {
            None => ("disconnected", None),
            Some(t) if !t.is_expired(now_ms, 0) => ("connected", Some(t.expires_at_ms)),
            Some(t) if t.refresh_token.is_some() && self.clients.auth_config(provider.vendor).is_some() => ("connected", Some(t.expires_at_ms)),
            Some(t) => ("needs_reauth", Some(t.expires_at_ms)),
        })
    }

    /// Ask the vendor to revoke `token`. `revoked`, `failed` or `not_supported`.
    fn revoke(&self, principal: &str, provider: &HostedProvider, token: &str) -> &'static str {
        let Some(endpoint) = provider.vendor.revoke_endpoint() else {
            return "not_supported";
        };
        let outcome = match self.exchange().ok().map(|x| x.post_form(endpoint, &[("token".to_string(), token.to_string())])) {
            // Slack answers 200 with `ok: false` when it did not revoke.
            Some(Ok(body)) if provider.vendor == Vendor::Slack => {
                if serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v.get("ok").and_then(serde_json::Value::as_bool)) == Some(true) {
                    "revoked"
                } else {
                    "failed"
                }
            }
            Some(Ok(_)) => "revoked",
            _ => "failed",
        };
        self.audit(
            principal,
            AuditEvent { action: "revoke", provider: provider.id, outcome: if outcome == "revoked" { "ok" } else { "failed" }, destination: provider.vendor.token_host() },
        );
        outcome
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        let lock = self.slot_lock(principal, CredentialSlot::Connector(provider.service));
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
        match self.refresh(principal, provider, &grants, &current, now_ms) {
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
        if !self.clients.can_connect(provider) {
            return Err(VaultError::NotConfigured);
        }
        if provider.vendor == Vendor::GitHub {
            return self.start_device(principal, provider, state, now_ms);
        }
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let mut entropy = [0u8; 32];
        getrandom::fill(&mut entropy).map_err(|_| VaultError::Unavailable)?;
        let pkce = Pkce::from_entropy(&entropy);
        entropy.fill(0);
        let authorize_url = oauth::authorize_url(&cfg, provider.scopes, &pkce, state).map_err(|_| VaultError::NotConfigured)?;
        let expires_at_ms = now_ms.saturating_add(FLOW_TTL_MS);
        let flow = StoredFlow {
            key: hex(&state_key(state)),
            provider: provider.id.to_string(),
            secret: pkce.verifier,
            device: false,
            user_code: None,
            interval_ms: 0,
            next_poll_ms: 0,
            expires_at_ms,
        };
        if !self.put_flow(principal, flow, now_ms)? {
            return Err(VaultError::Invalid("state"));
        }
        Ok(ConnectStarted { authorize_url, expires_at_ms, device: None })
    }

    fn connect_finish(&self, principal: &str, state: &str, code: &str, now_ms: i64) -> Result<Connected, VaultError> {
        if !valid_state(state) {
            return Err(VaultError::Invalid("state"));
        }
        if !valid_code(code) {
            return Err(VaultError::Invalid("code"));
        }
        if !valid_principal(principal) {
            return Err(VaultError::FlowUnknown);
        }
        // Single use: the flow is gone from here on, whatever happens next. Only this principal's
        // own flows are searched, so another principal's state finds nothing.
        let flow = self.take_flow(principal, state)?.ok_or(VaultError::FlowUnknown)?;
        if flow.device || flow.expires_at_ms <= now_ms {
            return Err(VaultError::FlowUnknown);
        }
        let provider = provider_by_id(&flow.provider).ok_or(VaultError::FlowUnknown)?;
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let pkce = Pkce { verifier: flow.secret, challenge: String::new() };
        let event = |outcome| AuditEvent { action: "connect", provider: provider.id, outcome, destination: provider.vendor.token_host() };
        let body = self.exchange()?.post_form(&cfg.token_endpoint, &oauth::token_exchange_form(&cfg, code, &pkce)).map_err(|reason| {
            if is_transient_token_failure(&reason) {
                self.audit(principal, event("unavailable"));
                VaultError::Unavailable
            } else {
                self.audit(principal, event("rejected"));
                VaultError::ProviderRejected
            }
        })?;
        let rejected = || {
            self.audit(principal, event("rejected"));
            VaultError::ProviderRejected
        };
        if !scopes_granted(provider, &body) {
            return Err(rejected());
        }
        let tokens = oauth::parse_token_response_for(&cfg, &body, now_ms, None).map_err(|_| rejected())?;
        // Google answers a refresh token only with offline access; without one the grant would die
        // within the hour, so it is not stored.
        if provider.vendor == Vendor::Google && tokens.refresh_token.is_none() {
            return Err(rejected());
        }
        self.store_grant(principal, provider, &tokens)?;
        self.audit(principal, event("ok"));
        Ok(Connected { provider: provider.id, slot: provider.slot() })
    }

    fn connect_poll(&self, principal: &str, state: &str, now_ms: i64) -> Result<ConnectPoll, VaultError> {
        if !valid_state(state) {
            return Err(VaultError::Invalid("state"));
        }
        if !valid_principal(principal) {
            return Err(VaultError::FlowUnknown);
        }
        let key = hex(&state_key(state));
        // Claim this poll under the lock: a second "Check" inside the interval is answered pending
        // without a vendor call, so two clicks never poll GitHub twice.
        let claimed = self.modify_json(principal, CredentialSlot::ConnectFlows, |doc: &mut StoredFlows| {
            doc.flows.retain(|f| f.expires_at_ms > now_ms);
            let flow = doc.flows.iter_mut().find(|f| f.key == key && f.device)?;
            let due = now_ms >= flow.next_poll_ms;
            if due {
                flow.next_poll_ms = now_ms.saturating_add(flow.interval_ms);
            }
            Some((flow.clone(), due))
        })?;
        let (flow, due) = claimed.ok_or(VaultError::FlowUnknown)?;
        let provider = provider_by_id(&flow.provider).ok_or(VaultError::FlowUnknown)?;
        let pending = |flow: &StoredFlow| ConnectPoll::Pending {
            device: DeviceStep {
                user_code: flow.user_code.clone().unwrap_or_default(),
                verification_uri: device_flow::VERIFICATION_URI.to_string(),
                interval_ms: flow.interval_ms,
            },
            expires_at_ms: flow.expires_at_ms,
        };
        if !due {
            return Ok(pending(&flow));
        }
        let cfg = self.clients.auth_config(provider.vendor).ok_or(VaultError::NotConfigured)?;
        let event = |outcome| AuditEvent { action: "connect", provider: provider.id, outcome, destination: provider.vendor.token_host() };
        // A poll that did not get through is the human's to retry: the flow stays.
        let body = self.exchange()?.post_form(device_flow::TOKEN_ENDPOINT, &device_flow::poll_form(&cfg.client_id, &flow.secret)).map_err(|_| VaultError::Unavailable)?;
        match device_flow::parse_poll(&body, now_ms) {
            Ok(DevicePoll::Pending) => Ok(pending(&flow)),
            Ok(DevicePoll::SlowDown { interval_s }) => {
                let interval = flow.interval_ms.saturating_add(device_flow::SLOW_DOWN_STEP_MS).max(interval_s.unwrap_or(0).saturating_mul(1_000));
                let slowed = self.modify_json(principal, CredentialSlot::ConnectFlows, |doc: &mut StoredFlows| {
                    let flow = doc.flows.iter_mut().find(|f| f.key == key)?;
                    flow.interval_ms = interval;
                    flow.next_poll_ms = now_ms.saturating_add(interval);
                    Some(flow.clone())
                })?;
                Ok(pending(&slowed.unwrap_or(flow)))
            }
            Ok(DevicePoll::Tokens(tokens)) => {
                // Single use from here on.
                let _ = self.take_flow(principal, state)?;
                self.store_grant(principal, provider, &tokens)?;
                self.audit(principal, event("ok"));
                Ok(ConnectPoll::Connected(Connected { provider: provider.id, slot: provider.slot() }))
            }
            Err(_) => {
                let _ = self.take_flow(principal, state)?;
                self.audit(principal, event("rejected"));
                Err(VaultError::ProviderRejected)
            }
        }
    }

    fn status(&self, principal: &str, now_ms: i64) -> Result<Vec<ConnectorState>, VaultError> {
        if !valid_principal(principal) {
            return Err(VaultError::Invalid("principal"));
        }
        shogun_mcp::connections_api::listed_services()
            .map(|service| {
                let Some(provider) = provider_by_service(service) else {
                    return Ok(ConnectorState { provider: None, service, state: "coming_soon", connectable: false, expires_at_ms: None });
                };
                let (state, expires_at_ms) = self.grant_state(principal, provider, now_ms)?;
                Ok(ConnectorState { provider: Some(provider.id), service, state, connectable: self.clients.can_connect(provider), expires_at_ms })
            })
            .collect()
    }

    fn disconnect(&self, principal: &str, service: Service, _now_ms: i64) -> Result<Disconnected, VaultError> {
        if !valid_principal(principal) {
            return Err(VaultError::Invalid("principal"));
        }
        let provider = provider_by_service(service);
        let provider_id = provider.map_or(service.source_str(), |p| p.id);
        let slot = CredentialSlot::Connector(service);
        let stored = {
            let lock = self.slot_lock(principal, slot);
            let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
            // A grant that no longer opens is deleted all the same; it just cannot be revoked.
            let stored = PrincipalGrants::new(self.tokens.as_ref(), principal).load(service);
            self.tokens.delete(principal, slot).map_err(|_| VaultError::Unavailable)?;
            stored
        };
        // A flow still pending for this provider would connect it again: drop it too.
        let _ = self.modify_json(principal, CredentialSlot::ConnectFlows, |doc: &mut StoredFlows| doc.flows.retain(|f| f.provider != provider_id));
        let (removed, vendor_revoke) = match (stored, provider) {
            (Ok(None), _) => (false, "nothing_stored"),
            (Ok(Some(tokens)), Some(provider)) => {
                let token = tokens.refresh_token.as_deref().unwrap_or(&tokens.access_token);
                (true, self.revoke(principal, provider, token))
            }
            (Ok(Some(_)), None) => (true, "not_supported"),
            (Err(_), _) => (true, "failed"),
        };
        if let Some(provider) = provider {
            self.audit(principal, AuditEvent { action: "disconnect", provider: provider.id, outcome: "ok", destination: "local" });
        }
        Ok(Disconnected { provider: provider_id, slot: slot.account(), removed, vendor_revoke })
    }

    fn enqueue_sync(&self, principal: &str, provider: &str, detail: &serde_json::Value, now_ms: i64) -> Result<SyncQueued, VaultError> {
        if !valid_principal(principal) {
            return Err(VaultError::Invalid("principal"));
        }
        crate::read_sync::enqueue(self, principal, provider, detail, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::MemorySecretStore;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: i64 = 1_800_000_000_000;
    const REDIRECT: &str = "https://api.example.test/v1/connectors/callback";

    /// One form the fake vendor received, with its endpoint.
    type Sent = (String, Vec<(String, String)>);

    /// A vendor token endpoint: rotating refresh tokens (`r0` → `r1` → …), a replayed one is
    /// `invalid_grant`; the code `good-code` exchanges once; GitHub's device endpoints answer from
    /// `device_body` / `polls`; the revocation endpoints answer `revoke_body`.
    #[derive(Default)]
    struct FakeVendor {
        calls: AtomicUsize,
        generation: Mutex<usize>,
        forms: Mutex<Vec<Sent>>,
        code_body: Mutex<Option<String>>,
        device_body: Mutex<Option<String>>,
        polls: Mutex<VecDeque<String>>,
        revoke_body: Mutex<Option<String>>,
        down: std::sync::atomic::AtomicBool,
    }

    impl TokenExchange for Arc<FakeVendor> {
        fn post_form(&self, endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.forms.lock().unwrap().push((endpoint.to_string(), form.to_vec()));
            if self.down.load(Ordering::SeqCst) {
                return Err("token exchange failed: connect".into());
            }
            let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
            if endpoint == device_flow::DEVICE_ENDPOINT {
                return self.device_body.lock().unwrap().clone().ok_or_else(|| "token endpoint http 400 Bad Request".to_string());
            }
            if endpoint.ends_with("/revoke") || endpoint.ends_with("auth.revoke") {
                return self.revoke_body.lock().unwrap().clone().ok_or_else(|| "token endpoint http 400 Bad Request".to_string());
            }
            if get("grant_type").as_deref() == Some("urn:ietf:params:oauth:grant-type:device_code") {
                return self.polls.lock().unwrap().pop_front().ok_or_else(|| "token exchange failed: connect".to_string());
            }
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
            .with_client(Vendor::GitHub, OAuthClient::new("Iv1.github-app", None))
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
                ("notion", "notion-tokenset".to_string()),
            ]
        );
        assert!(provider_by_slot("batch-relay-bearer").is_none());
        assert_eq!(clients().connectable(), vec!["gmail", "calendar", "slack", "linear", "github"]);
        assert!(HostedClients::default().connectable().is_empty());
        // GitHub's device flow needs no callback; the redirect flows do.
        let github_only = HostedClients::default().with_client(Vendor::GitHub, OAuthClient::new("Iv1.x", None));
        assert_eq!(github_only.connectable(), vec!["github"]);
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
        let form = vendor.forms.lock().unwrap()[0].1.clone();
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
        vendor.down.store(false, Ordering::SeqCst);
        // No refresh token, expired → not connected; Notion has no hosted client → not configured.
        save(&store, "prn_a", Service::Slack, &grant("x", None, NOW - 1));
        assert_eq!(vault.access_token("prn_a", "slack-tokenset", NOW).err(), Some(VaultError::NotConnected));
        save(&store, "prn_a", Service::Notion, &grant("ntn_expired", Some("nr"), NOW - 1));
        assert_eq!(vault.access_token("prn_a", "notion-tokenset", NOW).err(), Some(VaultError::NotConfigured));
        save(&store, "prn_a", Service::Notion, &grant("ntn_valid_token", None, NOW + 3_600_000));
        assert!(vault.access_token("prn_a", "notion-tokenset", NOW).is_ok());
        // GitHub refreshes with the hosted app's client now; a refused one is not connected.
        save(&store, "prn_a", Service::GitHub, &grant("ghu_expired", Some("ghr"), NOW - 1));
        assert_eq!(vault.access_token("prn_a", "github-tokenset", NOW).err(), Some(VaultError::NotConnected));
        let (endpoint, form) = vendor.forms.lock().unwrap().last().unwrap().clone();
        assert_eq!(endpoint, oauth::GITHUB_TOKEN_ENDPOINT);
        assert!(form.contains(&("client_id".into(), "Iv1.github-app".into())));
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
        assert!(started.device.is_none());
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
        // Another principal cannot finish it: its own store holds no such flow, and no vendor call.
        assert_eq!(vault.connect_finish("prn_b", STATE, "good-code", NOW).err(), Some(VaultError::FlowUnknown));
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 0);
        assert!(load(&store, "prn_b", Service::Gmail).is_none());

        let done = vault.connect_finish("prn_a", STATE, "good-code", NOW + 1_000).unwrap();
        assert_eq!((done.provider, done.slot.as_str()), ("gmail", "gmail-tokenset"));
        let stored = load(&store, "prn_a", Service::Gmail).unwrap();
        assert_eq!(stored.access_token, "ya29.connected-token");
        assert_eq!(stored.refresh_token.as_deref(), Some("1//refresh"));
        // The code went with the PKCE verifier the start generated.
        let form = vendor.forms.lock().unwrap().last().unwrap().1.clone();
        let verifier = form.iter().find(|(k, _)| k == "code_verifier").unwrap().1.clone();
        assert!(verifier.len() >= 43);
        assert!(form.contains(&("code".into(), "good-code".into())));
        assert!(form.contains(&("redirect_uri".into(), REDIRECT.into())));
        // Single use.
        assert_eq!(vault.connect_finish("prn_a", STATE, "good-code", NOW + 2_000).err(), Some(VaultError::FlowUnknown));
        // And now the token route serves it.
        assert_eq!(vault.access_token("prn_a", "gmail-tokenset", NOW + 2_000).unwrap().access_token, Secret::new("ya29.connected-token"));
        // Nothing of the flow is left in the store.
        assert_eq!(store.get("prn_a", CredentialSlot::ConnectFlows).unwrap(), None);
    }

    #[test]
    fn a_pending_flow_survives_a_restart_sealed_in_the_principals_store() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let started = vault(&store, &vendor).connect_start("prn_a", "calendar", STATE, NOW).unwrap();
        let verifier_in_url = query(&started.authorize_url)["code_challenge"].clone();
        // The sidecar restarts: a new vault over the same store finishes the flow.
        let restarted = vault(&store, &vendor);
        *vendor.code_body.lock().unwrap() = Some(format!(
            r#"{{"access_token":"ya29.after-restart","expires_in":3599,"refresh_token":"1//r","scope":"{}","token_type":"Bearer"}}"#,
            CALENDAR_SCOPES[0]
        ));
        restarted.connect_finish("prn_a", STATE, "good-code", NOW + 60_000).unwrap();
        assert_eq!(load(&store, "prn_a", Service::GoogleCalendar).unwrap().access_token, "ya29.after-restart");
        let verifier = vendor.forms.lock().unwrap().last().unwrap().1.iter().find(|(k, _)| k == "code_verifier").unwrap().1.clone();
        use base64::Engine as _;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, verifier_in_url, "the stored verifier matches the challenge sent");
        // Expiry holds across the restart too.
        restarted.connect_start("prn_a", "calendar", "state-expires-across-restart", NOW).unwrap();
        assert_eq!(vault(&store, &vendor).connect_finish("prn_a", "state-expires-across-restart", "c", NOW + FLOW_TTL_MS).err(), Some(VaultError::FlowUnknown));
        // One principal's pending flows are capped, oldest first, and never another's.
        vault(&store, &vendor).connect_start("prn_b", "slack", "state-of-principal-b-0001", NOW).unwrap();
        for i in 0..MAX_PENDING_FLOWS_PER_PRINCIPAL + 2 {
            vault(&store, &vendor).connect_start("prn_a", "slack", &format!("state-capped-flow-{i:04}"), NOW).unwrap();
        }
        let flows: StoredFlows = serde_json::from_str(store.get("prn_a", CredentialSlot::ConnectFlows).unwrap().unwrap().expose()).unwrap();
        assert_eq!(flows.flows.len(), MAX_PENDING_FLOWS_PER_PRINCIPAL);
        assert!(store.get("prn_b", CredentialSlot::ConnectFlows).unwrap().is_some());
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
        assert_eq!(vault.connect_start("prn_a", "notion", STATE, NOW).err(), Some(VaultError::NotConfigured));
        assert_eq!(vault.connect_start("prn_a", "jira", STATE, NOW).err(), Some(VaultError::UnknownSlot));
        assert_eq!(vault.connect_start("prn_a", "gmail", "short", NOW).err(), Some(VaultError::Invalid("state")));
        assert_eq!(vault.connect_start("prn_a", "gmail", "has spaces in the state!!", NOW).err(), Some(VaultError::Invalid("state")));
        assert_eq!(vault.connect_finish("prn_a", STATE, "", NOW).err(), Some(VaultError::Invalid("code")));
        let bare = HostedConnectors::new(store.clone(), HostedClients::default(), || Ok(Arc::new(FakeVendor::default())));
        assert_eq!(bare.connect_start("prn_a", "gmail", STATE, NOW).err(), Some(VaultError::NotConfigured));
        assert_eq!(bare.connect_start("prn_a", "github", STATE, NOW).err(), Some(VaultError::NotConfigured));
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

    const DEVICE: &str = "device_code=secret-device-code&user_code=WDJB-MJHT&verification_uri=https%3A%2F%2Fgithub.com%2Flogin%2Fdevice&expires_in=899&interval=5";

    #[test]
    fn github_connects_with_the_device_flow_one_poll_per_check() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let events: Arc<Mutex<Vec<(String, AuditEvent)>>> = Arc::default();
        let sink = events.clone();
        let vault = vault(&store, &vendor).with_audit(Arc::new(move |p: &str, e: &AuditEvent| sink.lock().unwrap().push((p.to_string(), *e))));
        *vendor.device_body.lock().unwrap() = Some(DEVICE.into());
        let started = vault.connect_start("prn_a", "github", STATE, NOW).unwrap();
        assert_eq!(started.authorize_url, device_flow::VERIFICATION_URI);
        assert_eq!(started.expires_at_ms, NOW + FLOW_TTL_MS, "capped to the gateway state's life");
        let device = started.device.unwrap();
        assert_eq!((device.user_code.as_str(), device.interval_ms), ("WDJB-MJHT", 5_000));
        let (endpoint, form) = vendor.forms.lock().unwrap()[0].clone();
        assert_eq!(endpoint, device_flow::DEVICE_ENDPOINT);
        assert!(form.contains(&("client_id".into(), "Iv1.github-app".into())));
        // The device code is sealed in the store, never in an answer.
        assert!(!format!("{device:?}").contains("secret-device-code"));

        // A check inside the interval answers pending without asking GitHub.
        assert!(matches!(vault.connect_poll("prn_a", STATE, NOW + 1_000).unwrap(), ConnectPoll::Pending { .. }));
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 1);
        // Another principal cannot poll it.
        assert_eq!(vault.connect_poll("prn_b", STATE, NOW + 6_000).err(), Some(VaultError::FlowUnknown));
        vendor.polls.lock().unwrap().extend(["error=authorization_pending".to_string(), "error=slow_down&interval=10".to_string()]);
        assert!(matches!(vault.connect_poll("prn_a", STATE, NOW + 6_000).unwrap(), ConnectPoll::Pending { .. }));
        let slowed = vault.connect_poll("prn_a", STATE, NOW + 11_000).unwrap();
        assert!(matches!(slowed, ConnectPoll::Pending { ref device, .. } if device.interval_ms == 10_000));
        assert!(matches!(vault.connect_poll("prn_a", STATE, NOW + 15_000).unwrap(), ConnectPoll::Pending { .. }), "inside the slowed interval");
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 3);
        let poll_form = vendor.forms.lock().unwrap()[1].1.clone();
        assert!(poll_form.contains(&("device_code".into(), "secret-device-code".into())));

        vendor.polls.lock().unwrap().push_back("access_token=ghu_connected_1&refresh_token=ghr_1&expires_in=28800&token_type=bearer".into());
        let done = vault.connect_poll("prn_a", STATE, NOW + 21_000).unwrap();
        assert_eq!(done, ConnectPoll::Connected(Connected { provider: "github", slot: "github-tokenset".into() }));
        assert_eq!(load(&store, "prn_a", Service::GitHub).unwrap().access_token, "ghu_connected_1");
        assert_eq!(vault.connect_poll("prn_a", STATE, NOW + 30_000).err(), Some(VaultError::FlowUnknown), "single use");
        // A device flow is never finished with a code, and a denied one ends.
        vault.connect_start("prn_a", "github", "state-github-denied-0001", NOW).unwrap();
        assert_eq!(vault.connect_finish("prn_a", "state-github-denied-0001", "code", NOW).err(), Some(VaultError::FlowUnknown));
        vault.connect_start("prn_a", "github", "state-github-denied-0002", NOW).unwrap();
        vendor.polls.lock().unwrap().push_back("error=access_denied".into());
        assert_eq!(vault.connect_poll("prn_a", "state-github-denied-0002", NOW + 6_000).err(), Some(VaultError::ProviderRejected));
        assert_eq!(vault.connect_poll("prn_a", "state-github-denied-0002", NOW + 12_000).err(), Some(VaultError::FlowUnknown));

        let events = events.lock().unwrap();
        let words: Vec<(&str, &str)> = events.iter().map(|(_, e)| (e.action, e.outcome)).collect();
        assert!(words.contains(&("connect_start", "ok")) && words.contains(&("connect", "ok")) && words.contains(&("connect", "rejected")));
        assert!(events.iter().all(|(p, e)| p == "prn_a" && e.provider == "github" && e.destination == "github.com"));
    }

    #[test]
    fn status_and_disconnect_narrow_and_revoke_where_the_vendor_can() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let events: Arc<Mutex<Vec<AuditEvent>>> = Arc::default();
        let sink = events.clone();
        let vault = vault(&store, &vendor).with_audit(Arc::new(move |_: &str, e: &AuditEvent| sink.lock().unwrap().push(*e)));
        save(&store, "prn_a", Service::Gmail, &grant("ya29.live-access", Some("1//google-refresh"), NOW + 3_600_000));
        save(&store, "prn_a", Service::Slack, &grant("xoxp-expired", None, NOW - 1));
        save(&store, "prn_a", Service::Linear, &grant("lin-expired", Some("lr"), NOW - 1));
        let states = vault.status("prn_a", NOW).unwrap();
        let of = |s: Service| states.iter().find(|r| r.service == s).unwrap().clone();
        assert_eq!(of(Service::Gmail).state, "connected");
        assert_eq!(of(Service::Gmail).expires_at_ms, Some(NOW + 3_600_000));
        assert_eq!(of(Service::Slack).state, "needs_reauth");
        assert_eq!(of(Service::Linear).state, "connected", "refreshable");
        assert_eq!(of(Service::GoogleCalendar).state, "disconnected");
        assert!(of(Service::GitHub).connectable);
        assert_eq!((of(Service::GoogleDrive).state, of(Service::GoogleDrive).provider), ("coming_soon", None));
        assert_eq!((of(Service::Notion).state, of(Service::Notion).connectable), ("disconnected", false));
        assert!(vault.status("prn_b", NOW).unwrap().iter().all(|r| r.state != "connected"));
        assert_eq!(vendor.calls.load(Ordering::SeqCst), 0, "status is a read: no vendor call");

        // Google: revoked with the refresh token, deleted.
        *vendor.revoke_body.lock().unwrap() = Some("{}".into());
        let gone = vault.disconnect("prn_a", Service::Gmail, NOW).unwrap();
        assert_eq!((gone.provider, gone.removed, gone.vendor_revoke), ("gmail", true, "revoked"));
        let (endpoint, form) = vendor.forms.lock().unwrap().last().unwrap().clone();
        assert_eq!(endpoint, "https://oauth2.googleapis.com/revoke");
        assert_eq!(form, vec![("token".to_string(), "1//google-refresh".to_string())]);
        assert!(load(&store, "prn_a", Service::Gmail).is_none());
        // Slack answers ok:false: deleted all the same.
        *vendor.revoke_body.lock().unwrap() = Some(r#"{"ok":false,"error":"invalid_auth"}"#.into());
        assert_eq!(vault.disconnect("prn_a", Service::Slack, NOW).unwrap().vendor_revoke, "failed");
        assert!(load(&store, "prn_a", Service::Slack).is_none());
        // Linear: no revocation the core can call; deleted.
        assert_eq!(vault.disconnect("prn_a", Service::Linear, NOW).unwrap().vendor_revoke, "not_supported");
        assert!(load(&store, "prn_a", Service::Linear).is_none());
        // Nothing stored: idempotent. Another principal's grant is untouched.
        save(&store, "prn_b", Service::Gmail, &grant("ya29.bob", Some("rb"), NOW + 3_600_000));
        let again = vault.disconnect("prn_a", Service::Gmail, NOW).unwrap();
        assert_eq!((again.removed, again.vendor_revoke), (false, "nothing_stored"));
        assert!(load(&store, "prn_b", Service::Gmail).is_some());
        // A pending flow for the provider goes with it.
        vault.connect_start("prn_a", "calendar", STATE, NOW).unwrap();
        vault.disconnect("prn_a", Service::GoogleCalendar, NOW).unwrap();
        assert_eq!(vault.connect_finish("prn_a", STATE, "good-code", NOW).err(), Some(VaultError::FlowUnknown));

        let events = events.lock().unwrap();
        assert!(events.contains(&AuditEvent { action: "revoke", provider: "gmail", outcome: "ok", destination: "oauth2.googleapis.com" }));
        assert!(events.contains(&AuditEvent { action: "revoke", provider: "slack", outcome: "failed", destination: "slack.com" }));
        assert!(events.contains(&AuditEvent { action: "disconnect", provider: "linear", outcome: "ok", destination: "local" }));
    }

    #[test]
    fn refreshes_are_audited_without_values() {
        let store = Arc::new(MemorySecretStore::new());
        let vendor = Arc::new(FakeVendor::default());
        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = events.clone();
        let vault = vault(&store, &vendor).with_audit(Arc::new(move |p: &str, e: &AuditEvent| sink.lock().unwrap().push(format!("{p} {e:?}"))));
        save(&store, "prn_a", Service::GoogleCalendar, &grant("stale", Some("r0"), NOW - 1));
        vault.access_token("prn_a", "gcal-tokenset", NOW).unwrap();
        save(&store, "prn_a", Service::GoogleCalendar, &grant("stale", Some("spent"), NOW - 1));
        assert!(vault.access_token("prn_a", "gcal-tokenset", NOW).is_err());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("\"refresh\"") && events[0].contains("\"ok\""));
        assert!(events[1].contains("\"rejected\""));
        for line in events.iter() {
            for value in ["access-1", "r0", "r1", "spent", "stale", "google-web-secret"] {
                assert!(!line.contains(value), "{line}");
            }
        }
    }

    #[test]
    fn clients_read_from_the_environment_name_the_bad_variable() {
        let vars = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        let ok = HostedClients::from_vars(&vars(&[
            (REDIRECT_URI_VAR, REDIRECT),
            (GOOGLE_CLIENT_ID_VAR, "123-abc.apps.googleusercontent.com"),
            (GOOGLE_CLIENT_SECRET_VAR, "s"),
            (LINEAR_CLIENT_ID_VAR, "linear-client"),
            (GITHUB_CLIENT_ID_VAR, "Iv1.8a61f9b3a7aba766"),
        ]))
        .unwrap();
        assert_eq!(ok.connectable(), vec!["gmail", "calendar", "linear", "github"]);
        assert!(HostedClients::from_vars(&HashMap::new()).unwrap().connectable().is_empty());
        for (pairs, bad) in [
            (vec![(REDIRECT_URI_VAR, "http://api.example.test/cb")], REDIRECT_URI_VAR),
            (vec![(GOOGLE_CLIENT_ID_VAR, "123-abc.apps.googleusercontent.com")], GOOGLE_CLIENT_SECRET_VAR),
            (vec![(GOOGLE_CLIENT_SECRET_VAR, "s")], GOOGLE_CLIENT_ID_VAR),
            (vec![(SLACK_CLIENT_ID_VAR, "not-a-slack-id")], SLACK_CLIENT_ID_VAR),
            (vec![(GITHUB_CLIENT_SECRET_VAR, "s")], GITHUB_CLIENT_ID_VAR),
        ] {
            assert_eq!(HostedClients::from_vars(&vars(&pairs)).err(), Some(bad));
        }
        assert!(valid_redirect_uri("http://127.0.0.1:8787/v1/connectors/callback"));
        assert!(!valid_redirect_uri("https://api.example.test/cb#frag"));
    }
}
