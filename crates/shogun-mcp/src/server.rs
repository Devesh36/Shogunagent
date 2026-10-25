//! The REST listener for the Memory API (§6.11, FR-API-01 / NFR-SEC-03). Feature `server`.
//!
//! A thin axum adapter over the pure routing layer ([`crate::rest`]): it extracts method / path /
//! Bearer token from the socket, calls [`rest::respond`], and writes the `(status, json)` back.
//! All the policy — auth, path→tool, method validation — lives in `rest` and is unit-tested there;
//! this module is only the bind + adapt.
//!
//! Binding is **localhost-only** (`127.0.0.1`, NFR-SEC-03: never a public interface, CORS not
//! enabled). The default port is 7464; if it is busy the listener falls back to an ephemeral port
//! (FR-API-01) whose value the caller reads from [`tokio::net::TcpListener::local_addr`].

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use shogun_agents::approval::ApprovalQueue;
use shogun_agents::entitlement::Entitlements;
use tokio::net::TcpListener;

use crate::backend::MemoryBackend;
use crate::live_tokens::LiveTokens;
use crate::memory_api::TokenRegistry;
use crate::memory_readiness::MemoryUnavailable;
use crate::rest::{self, Method, RestRequest, Routed};

#[path = "server_governance.rs"]
mod governance;

/// SHA-256 of an admin credential of at least 16 characters; `None` otherwise.
fn admin_digest(token: &str) -> Option<[u8; 32]> {
    use sha2::Digest as _;
    let token = token.trim();
    (token.chars().count() >= 16).then(|| sha2::Sha256::digest(token.as_bytes()).into())
}

/// The default Memory API port (FR-API-01).
pub const DEFAULT_PORT: u16 = 7464;

/// An injected millisecond clock (unix ms) — deterministic under test.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The plan entitlement provider (issue #97). A closure, not a snapshot: a trial can expire while
/// the server runs, so it is consulted on every request. The default (see [`AppState::new`]) is
/// trial-not-started — paid capabilities stay closed until a persisted clock or licence is wired in.
pub type EntitlementProvider = Arc<dyn Fn() -> Entitlements + Send + Sync>;

/// Live in-product SLO metrics source (NFR-SLO-00). The daemon implements it over its `SloRegistry`;
/// the server serves its JSON at `GET /v1/metrics` (`shogun metrics` / the Advanced UI).
pub trait MetricsSource: Send + Sync {
    /// The current SLO snapshot as JSON (the `{"metrics":[...]}` shape).
    fn snapshot_json(&self) -> String;
}

/// Shared server state. Clone is cheap (all `Arc`), as axum requires. The approval queue is the
/// **same one the Notch UI drains** — an API-requested L3 send and a human L3 are one flow
/// (invariant 6 / FR-API-04).
#[derive(Clone)]
pub struct AppState {
    tokens: Tokens,
    backend: Arc<dyn MemoryBackend>,
    approvals: Arc<Mutex<ApprovalQueue>>,
    clock: Clock,
    metrics: Option<Arc<dyn MetricsSource>>,
    entitlements: EntitlementProvider,
    surface: rest::ApprovalSurface,
    approval_store: Option<crate::approval_store::ApprovalStore>,
    desktop_running: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Why the plan locks the Memory API (issue #744), for the 403 `plan_required` body. Display
    /// only: the gate reads `entitlements`.
    plan_lock: Option<Arc<dyn Fn() -> Option<crate::plan_source::PlanLock> + Send + Sync>>,
    /// Whether the store may be served right now (#800). `None` = always (the desktop, tests).
    memory_gate: Option<Arc<dyn Fn() -> Option<MemoryUnavailable> + Send + Sync>>,
    /// The port this server bound, reported by `GET /v1/status` (FR-API-01, issue #776).
    port: Option<u16>,
    /// Per-client limit on model-starting calls (2026-10-20 cost audit), keyed by token id.
    llm_limiter: Arc<crate::rate_limit::LlmRateLimiter>,
    /// Multi-principal mode (E2-T2): the backend and the writer come from the request's bearer.
    /// `None` is the single-principal server: `backend`, written as the owner.
    principals: Option<Arc<dyn BackendResolver>>,
    /// The MCP face behind `POST /mcp` (E4-T1, Streamable HTTP, stateless). `None` answers 404
    /// there, as before the transport existed.
    mcp: Option<Arc<dyn crate::mcp_http::McpHandler>>,
    /// Remote approvals (E15): the approval base URL, the decision-token key and the ledger,
    /// shared with the MCP face.
    remote: crate::remote_approvals::RemoteApprovals,
    /// SHA-256 of the operator's admin credential (`SHOGUN_ADMIN_TOKEN`): the bearer that may
    /// decide approvals and change grants from the CLI. Never an agent's token; the raw value is
    /// not kept.
    admin_digest: Option<[u8; 32]>,
    /// Carries out an approved action once a human decided it (E15 follow-up). `None` in a build
    /// without the execution path: decided approvals stay `approved` (`execute_unavailable`).
    executor: Option<Arc<dyn crate::approval_exec::ApprovalExecutor>>,
}

/// Multi-principal mode (E2-T2): which store and which writer a bearer token names. `apps/api`
/// supplies the real agent keys in E3; the sidecar's `TokenPrincipalMap` stands in until then.
pub trait BackendResolver: Send + Sync {
    /// The backend for `token`, with the actor its writes record. `Err` carries the status and
    /// the JSON body to answer with (401 for a token no principal owns, 503 for a store that
    /// cannot be opened).
    fn resolve(&self, token: &str) -> Result<ResolvedBackend, (u16, String)>;

    /// The backend of `principal` with no caller: for the operator's admin routes and for
    /// recording an approval decision in the principal's audit log. Default: unknown.
    fn backend_for_principal(&self, _principal: &str) -> Result<Arc<dyn MemoryBackend>, (u16, String)> {
        Err((404, r#"{"error":"principal_not_found"}"#.to_string()))
    }

    /// Team (E18): `token`'s principal reading `target`'s world state. Only when both are on one
    /// team and `target` granted the caller `world_read`; the backend answers world rows only
    /// (the server pins the read scope). Default: refused.
    fn cross_read(&self, _token: &str, _target: &str) -> Result<ResolvedBackend, (u16, String)> {
        Err((403, crate::server::CROSS_READ_REFUSED.to_string()))
    }

    /// The teams this sidecar knows, as JSON (`GET /v1/teams`). Default: none (single principal).
    fn teams(&self) -> Option<serde_json::Value> {
        None
    }

    /// Define or replace a team (`POST /v1/teams`, admin). Default: not available.
    fn set_team(&self, _tenant: &str, _team: &str, _members: &[String]) -> Result<serde_json::Value, (u16, String)> {
        Err((404, r#"{"error":"teams_unavailable","message":"Teams exist only in multi-principal mode."}"#.to_string()))
    }
}

/// One principal's backend and the actor (`agent:<id>`) the request's writes carry.
pub struct ResolvedBackend {
    pub backend: Arc<dyn MemoryBackend>,
    pub actor: String,
    /// The principal id (multi-principal mode).
    pub principal: Option<String>,
}

/// A request's store, call scope and principal (multi-principal mode).
type ResolvedCall = (Arc<dyn MemoryBackend>, shogun_memory::actor::CallScope, Option<String>);

/// The 403 body of a cross-principal read without a team grant: the same whether the principal
/// exists or not, so it is no oracle on principal ids.
pub const CROSS_READ_REFUSED: &str = r#"{"error":"cross_principal_forbidden","message":"No read grant from that principal. A teammate grants world read access with acl.grant (kind principal).","next_action":null}"#;

impl AppState {
    /// Build state from the token registry, the data backend, the shared approval queue, and a
    /// clock. The daemon injects a DB-backed backend + its real approval queue; tests inject stubs.
    /// The plan gate defaults to trial-not-started (paid capabilities blocked — the pre-onboarding
    /// default); the composition root attaches the real provider via [`AppState::with_entitlements`].
    pub fn new(
        tokens: Arc<TokenRegistry>,
        backend: Arc<dyn MemoryBackend>,
        approvals: Arc<Mutex<ApprovalQueue>>,
        clock: Clock,
    ) -> Self {
        Self {
            tokens: Tokens::Fixed(tokens),
            backend,
            approvals,
            clock,
            metrics: None,
            entitlements: Arc::new(Entitlements::trial_not_started),
            // Headless by default — see `McpServer::new`. The composition root that owns a
            // confirm UI opts in with `with_approval_surface`.
            surface: rest::ApprovalSurface::Absent,
            approval_store: None,
            desktop_running: Arc::new(|| false),
            plan_lock: None,
            port: None,
            memory_gate: None,
            llm_limiter: Arc::new(crate::rate_limit::LlmRateLimiter::in_memory()),
            principals: None,
            mcp: None,
            remote: crate::remote_approvals::RemoteApprovals::from_env(),
            admin_digest: std::env::var("SHOGUN_ADMIN_TOKEN").ok().and_then(|t| admin_digest(&t)),
            executor: None,
        }
    }

    /// Remote approvals (E15): base URL, decision-token key and ledger. Overrides the
    /// environment (`SHOGUN_APPROVAL_BASE_URL`, `SHOGUN_APPROVAL_SIGNING_KEY`); share the same value
    /// with the MCP face so both record into one ledger.
    #[must_use]
    pub fn with_remote_approvals(mut self, remote: crate::remote_approvals::RemoteApprovals) -> Self {
        self.remote = remote;
        self
    }

    /// The operator's admin credential (overrides `SHOGUN_ADMIN_TOKEN`). `None` disables the
    /// admin routes' bearer path (a signed approval token still decides).
    #[must_use]
    pub fn with_admin_token(mut self, token: Option<&str>) -> Self {
        self.admin_digest = token.and_then(admin_digest);
        self
    }

    /// Carry out approved actions with `executor` (E15 follow-up): a human's approval on the decide
    /// route runs the action once and records `sent` / `send_failed`. Without one, a decided
    /// approval stays `approved` with `next_action: "execute_unavailable"`.
    #[must_use]
    pub fn with_approval_executor(mut self, executor: Arc<dyn crate::approval_exec::ApprovalExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Serve many principals from one process (E2-T2): every memory request resolves its backend
    /// and writer from the bearer through `resolver`; the backend given to [`AppState::new`] is
    /// never read. Auth stays with the token registry, which must know the same tokens.
    #[must_use]
    pub fn with_backend_resolver(mut self, resolver: Arc<dyn BackendResolver>) -> Self {
        self.principals = Some(resolver);
        self
    }

    /// The backend and call scope for one request. Single-principal: the fixed backend, written as
    /// the owner. Multi-principal: the resolver's answer, or the status and body to refuse with.
    /// `cross` names another principal to read (Team, E18): its world rows only, through the
    /// resolver's grant check; the caller's writer stays the call's actor.
    fn backend_for(
        &self,
        token: Option<&str>,
        include_scratch: bool,
        cross: Option<&str>,
    ) -> Result<ResolvedCall, (u16, String)> {
        use shogun_memory::actor::{CallScope, Namespace, ReadScope, WriteContext};
        let read = ReadScope { include_scratch };
        let Some(resolver) = &self.principals else {
            if cross.is_some() {
                return Err((403, CROSS_READ_REFUSED.to_string()));
            }
            return Ok((self.backend.clone(), CallScope { write: WriteContext::owner(), read }, None));
        };
        let token = token.ok_or_else(|| (401, r#"{"error":"unauthorized"}"#.to_string()))?;
        let resolved = resolver.resolve(token)?;
        if let Some(target) = cross.filter(|target| Some(*target) != resolved.principal.as_deref()) {
            let other = resolver.cross_read(token, target)?;
            // A teammate is a human of another principal: not restricted by this store's scratch
            // ACL, so the read scope is pinned to the world — never scratch, whatever was asked.
            let actor = shogun_memory::actor::Actor::human(resolved.principal.as_deref().unwrap_or("team"))
                .map_err(|_| (500, r#"{"error":"invalid_actor"}"#.to_string()))?;
            return Ok((
                other.backend,
                CallScope { write: WriteContext::world(actor), read: ReadScope::WORLD_ONLY },
                other.principal,
            ));
        }
        let write = WriteContext::new(&resolved.actor, Namespace::World)
            .map_err(|_| (500, r#"{"error":"invalid_actor"}"#.to_string()))?;
        Ok((resolved.backend, CallScope { write, read }, resolved.principal))
    }

    /// Serve the MCP face at `POST /mcp` (E4-T1): the handler runs every request under the
    /// request's own bearer ([`crate::mcp::McpServer::handle_line_as`]), so it shares the
    /// token list, the backend and the approval queue with the REST face.
    #[must_use]
    pub fn with_mcp(mut self, mcp: Arc<dyn crate::mcp_http::McpHandler>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Count model-starting calls against `limiter` (2026-10-20 cost audit). The standalone
    /// binary passes one shared through a file with the stdio faces.
    #[must_use]
    pub fn with_llm_rate_limiter(mut self, limiter: Arc<crate::rate_limit::LlmRateLimiter>) -> Self {
        self.llm_limiter = limiter;
        self
    }

    /// Answer 503 `{"error":<code>,…}` on every memory endpoint while `gate` names a reason (#800):
    /// a headless face whose store the app has not upgraded yet. Consulted per request, after
    /// auth and the plan, so the app upgrading the store needs no restart.
    #[must_use]
    pub fn with_memory_gate(
        mut self,
        gate: impl Fn() -> Option<MemoryUnavailable> + Send + Sync + 'static,
    ) -> Self {
        self.memory_gate = Some(Arc::new(gate));
        self
    }

    /// Report the port this server bound in `GET /v1/status`. It differs from the configured one
    /// when that was taken and the server fell back to an ephemeral port (FR-API-01, #776).
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Name the cause in 403 `plan_required` bodies (issue #744); consulted only on that path.
    #[must_use]
    pub fn with_plan_lock(
        mut self,
        lock: impl Fn() -> Option<crate::plan_source::PlanLock> + Send + Sync + 'static,
    ) -> Self {
        self.plan_lock = Some(Arc::new(lock));
        self
    }

    /// Authenticate every request against a live token list (issue #400): a token revoked in
    /// Settings is refused on its next request, a new one accepted, with no restart. Replaces
    /// the fixed registry given to [`AppState::new`].
    #[must_use]
    pub fn with_live_tokens(mut self, tokens: Arc<LiveTokens>) -> Self {
        self.tokens = Tokens::Live(tokens);
        self
    }

    /// Declare that this process runs a confirm UI, so L3 sends may be enqueued for it.
    #[must_use]
    pub fn with_approval_surface(mut self, surface: rest::ApprovalSurface) -> Self {
        self.surface = surface;
        self
    }

    /// Use the durable queue shared with the desktop, sealed under the memory DB key this
    /// process opened the store with (audit M2). Without a key, new L3 requests are refused
    /// rather than written in the clear.
    #[must_use]
    pub fn with_approval_store(mut self, store: crate::approval_store::ApprovalStore) -> Self {
        self.approval_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_desktop_running_check(
        mut self,
        check: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        self.desktop_running = Arc::new(check);
        self
    }

    /// Attach the live SLO metrics source served at `GET /v1/metrics`. Without it, the endpoint
    /// returns an empty (all-unmeasured) snapshot.
    pub fn with_metrics(mut self, metrics: Arc<dyn MetricsSource>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Attach the plan entitlement provider (issue #97) — consulted on every request, so trial
    /// expiry takes effect without a restart.
    pub fn with_entitlements(mut self, entitlements: EntitlementProvider) -> Self {
        self.entitlements = entitlements;
        self
    }
}

/// Where a request's token list comes from: fixed at start-up (tests, dev) or live.
#[derive(Clone)]
enum Tokens {
    Fixed(Arc<TokenRegistry>),
    Live(Arc<LiveTokens>),
}

impl Tokens {
    fn current(&self) -> Arc<TokenRegistry> {
        match self {
            Tokens::Fixed(tokens) => tokens.clone(),
            Tokens::Live(tokens) => tokens.current(),
        }
    }
}

/// Build the router: a single catch-all handler that defers to the pure routing layer.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route(crate::mcp_http::PATH, any(handle_mcp))
        .fallback(any(handle))
        .with_state(state)
}

/// `POST /mcp` (E4-T1): the Streamable HTTP transport. The adapter reads the method, the body,
/// the bearer and the `Mcp-*` / `Accept` headers, hands them to the pure transport
/// ([`crate::mcp_http::handle`]) and writes its reply and headers back. The same loopback rule
/// as REST (NFR-SEC-03).
async fn handle_mcp(State(state): State<AppState>, req: Request) -> Response {
    if !host_is_loopback(
        req.headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok()),
    ) {
        return render(403, r#"{"error":"forbidden_host"}"#.to_string());
    }
    // Multi-principal mode (E2-T2): every request resolves its own principal from its bearer
    // through the same resolver REST uses, so the MCP face needs no handler of its own.
    let mcp: Arc<dyn crate::mcp_http::McpHandler> = match (&state.principals, state.mcp.clone()) {
        (Some(_), _) => Arc::new(PrincipalMcp { state: state.clone() }),
        (None, Some(mcp)) => mcp,
        (None, None) => return render(404, r#"{"error":"not_found"}"#.to_string()),
    };
    let method = req.method().as_str().to_string();
    let headers = req.headers().clone();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mcp_method = header(crate::mcp_http::HEADER_METHOD);
    let mcp_name = header(crate::mcp_http::HEADER_NAME);
    let accept = header("accept");
    let bearer = rest::bearer(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()));
    let body = match axum::body::to_bytes(req.into_body(), crate::mcp_http::MAX_BODY_BYTES).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => return render(413, r#"{"error":"payload_too_large"}"#.to_string()),
    };
    let reply = crate::mcp_http::handle(
        mcp.as_ref(),
        &crate::mcp_http::HttpRequest {
            method: &method,
            body: &body,
            mcp_method: mcp_method.as_deref(),
            mcp_name: mcp_name.as_deref(),
            accept: accept.as_deref(),
            bearer: bearer.as_deref(),
        },
    );
    let mut response = Response::builder()
        .status(reply.status)
        .header(CONTENT_TYPE, reply.content_type)
        .header(crate::mcp_http::HEADER_METHOD, reply.mcp_method.as_str())
        .header(crate::mcp_http::HEADER_PROTOCOL_VERSION, crate::mcp::STATELESS_PROTOCOL_VERSION);
    if let Some(name) = &reply.mcp_name {
        response = response.header(crate::mcp_http::HEADER_NAME, name.as_str());
    }
    if let Some(quantity) = reply.mcp_quantity {
        response = response.header(crate::mcp_http::HEADER_QUANTITY, quantity.to_string());
    }
    if let Some(allow) = reply.allow {
        response = response.header(axum::http::header::ALLOW, allow);
    }
    if let Some(challenge) = reply.www_authenticate {
        response = response.header(axum::http::header::WWW_AUTHENTICATE, challenge);
    }
    // E15: a call that queued an approval names its decision token in a header, so the gateway
    // can notify Slack and webhooks without reading the body (invariant 2).
    if let Some(token) = approval_token_in(&reply.body) {
        response = response.header(crate::remote_approvals::TOKEN_HEADER, token);
    }
    response
        .body(Body::from(reply.body))
        .unwrap_or_else(|_| render(500, r#"{"error":"internal"}"#.to_string()))
}

/// `POST /mcp` in multi-principal mode (E2-T2 × E4-T1): one MCP server per request, over the store
/// of the principal the request's bearer names ([`BackendResolver::resolve`], the resolver REST
/// uses), writing as that bearer's actor, with the server's one approval queue, remote-approval
/// ledger, plan gate and rate limiter. Same header contract and MRTR shapes as the
/// single-principal face; approvals it queues are recorded under the principal, and another
/// principal's approval, store or audit log is never reachable from it. A request without a bearer
/// the resolver knows is refused before any store is opened.
struct PrincipalMcp {
    state: AppState,
}

impl crate::mcp_http::McpHandler for PrincipalMcp {
    fn handle_line_as(&self, line: &str, token: Option<&str>) -> Option<String> {
        use shogun_memory::actor::{CallScope, Namespace, ReadScope, WriteContext};
        let id = serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v.get("id").cloned());
        let refuse = |code: i64, message: &str| {
            id.clone().map(|id| {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
            })
        };
        let state = &self.state;
        let Some(resolver) = &state.principals else {
            return refuse(-32000, "internal: no principal resolver");
        };
        let resolved = match token.map(|token| resolver.resolve(token)) {
            Some(Ok(resolved)) => resolved,
            Some(Err((503, _))) => return refuse(-32000, "memory_unavailable: the principal's store cannot be opened right now; retry."),
            _ => {
                return refuse(
                    crate::mcp::TOKEN_INVALID_CODE,
                    "unauthorized: present a principal's bearer token (Authorization: Bearer <token>).",
                )
            }
        };
        let Ok(write) = WriteContext::new(&resolved.actor, Namespace::World) else {
            return refuse(-32000, "internal: invalid actor");
        };
        let clock = state.clock.clone();
        let entitlements = state.entitlements.clone();
        let mut server = crate::mcp::McpServer::new(resolved.backend, state.approvals.clone(), move || clock(), move || entitlements())
            .with_remote_approvals(state.remote.clone())
            .with_principal(resolved.principal)
            .with_approval_execution(state.executor.is_some())
            .with_llm_rate_limiter(state.llm_limiter.clone());
        if let Some(token_id) = token.and_then(|token| state.tokens.current().token_id(token)) {
            server = server.with_rate_client(crate::rate_limit::ClientKey::token(&token_id));
        }
        shogun_memory::actor::with_call_scope(CallScope { write, read: ReadScope { include_scratch: false } }, || {
            server.handle_line_as(line, token)
        })
    }
}

/// The one handler. Adapts an axum request into a [`RestRequest`], calls [`rest::respond`], and
/// renders the JSON response.
/// Is this request addressed to us as localhost?
///
/// The listener is loopback-only and every tool endpoint needs a Bearer token, so memory data was
/// never exposed. But `/v1/status` and `/v1/metrics` are deliberately open, and a page on
/// `attacker.com` whose DNS rebinds to 127.0.0.1 is SAME-ORIGIN with this server — CORS never
/// applies, and the live SLO snapshot (device activity timing) becomes readable by an arbitrary
/// website. A Host check is the standard answer: a rebound page still carries its own hostname.
fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    let name = name.trim_start_matches('[').trim_end_matches(']');
    // "0.0.0.0" is deliberately NOT accepted: no legitimate local caller of a loopback-bound
    // listener sends it as Host, but it is exactly the host a hostile page uses for the
    // "0.0.0.0-day" class of localhost-service attacks.
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

/// Does the query string opt into low-confidence read results (`?include_low`, FR-API-06)?
///
/// The VALUE matters: `?include_low=false` / `=0` must stay opted OUT — the MCP face parses the
/// same flag as a real JSON bool, and the three faces must agree. Bare `?include_low` (or a bare
/// `=`) opts in. Matching is case- and spelling-tolerant on the truthy side because REST clients
/// build this by hand (Python's `str(True)` gives `include_low=True`); a silently-ignored opt-in
/// returns 200 with rows missing and no signal to the caller.
fn include_low_opt_in(raw_query: &str) -> bool {
    query_flag(raw_query, "include_low")
}

fn query_flag(raw_query: &str, name: &str) -> bool {
    let prefix = format!("{name}=");
    raw_query.split('&').any(|kv| {
        kv == name
            || kv.strip_prefix(&prefix).is_some_and(|v| {
                matches!(
                    v.to_ascii_lowercase().as_str(),
                    "" | "1" | "true" | "yes" | "on"
                )
            })
    })
}

fn query_value(raw_query: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&prefix))
        .map(percent_decode)
        .filter(|s| !s.is_empty())
}

async fn handle(State(state): State<AppState>, req: Request) -> Response {
    if !host_is_loopback(
        req.headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok()),
    ) {
        return render(403, r#"{"error":"forbidden_host"}"#.to_string());
    }
    let method = match *req.method() {
        axum::http::Method::GET => Some(Method::Get),
        axum::http::Method::POST => Some(Method::Post),
        _ => None,
    };
    let path = req.uri().path().to_string();
    let token = rest::bearer(
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );
    // E15: a hosted surface's signed decision token; E18: the request id every audit record
    // carries (the caller's `X-Request-Id`, or a fresh one), echoed on the response.
    let approval_token = req
        .headers()
        .get(crate::remote_approvals::TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_id = crate::governance_api::request_id(
        req.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
    );
    // Parse the query string: `?include_low` (FR-API-06 opt-in), `?q=<search>`, visual-recall window.
    let raw_query = req.uri().query().unwrap_or("");
    // E26: the operator's hash-chained audit export, streamed (no agent bearer reaches it).
    if path == "/v1/audit/export" {
        return match method {
            Some(Method::Get) => governance::audit_export(&state, token.as_deref(), raw_query, &request_id),
            _ => render_with(405, r#"{"error":"method_not_allowed"}"#.to_string(), &request_id, None),
        };
    }
    let include_low = include_low_opt_in(raw_query);
    // E2-T5: `?include_scratch` widens reads to the agents' scratch namespaces for this call.
    let include_scratch = query_flag(raw_query, "include_scratch");
    // E18 Team: `?principal=<other>` reads a teammate's world state, when it granted it.
    let cross = query_value(raw_query, "principal");
    let for_generation = query_flag(raw_query, "for_generation");
    // `?url=` (the web reads, issue #555) rides the same slot as `?q=`: one string per read.
    let query = raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix("q=").or_else(|| kv.strip_prefix("url=")))
        .map(percent_decode);
    let search_depth = raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix("depth="))
        .map(percent_decode);
    let search_source_kind = raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix("source_kind="))
        .map(percent_decode);
    let search_mode = query_value(raw_query, "mode");
    let raw_from_ms = raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix("from_ms="));
    let raw_to_ms = raw_query
        .split('&')
        .find_map(|kv| kv.strip_prefix("to_ms="));
    let search_bound_invalid = raw_from_ms.is_some_and(|value| value.parse::<i64>().is_err())
        || raw_to_ms.is_some_and(|value| value.parse::<i64>().is_err());
    let from_ms = raw_from_ms.and_then(|value| value.parse::<i64>().ok());
    let to_ms = raw_to_ms.and_then(|value| value.parse::<i64>().ok());
    let cursor_ts = query_value(raw_query, "cursor_ts").and_then(|v| v.parse::<i64>().ok());
    let cursor_id = query_value(raw_query, "cursor_id").and_then(|v| v.parse::<i64>().ok());
    let cursor_direction = query_value(raw_query, "cursor_direction");
    let page_size = query_value(raw_query, "page_size").and_then(|v| v.parse::<usize>().ok());
    let app_bundle_id = query_value(raw_query, "app_bundle_id");
    let person_id = query_value(raw_query, "person_id");
    let project_id = query_value(raw_query, "project_id");
    // Mail triage list controls (FR-TG-30). Values stay strings; the backend parses them against
    // its own enums so an unknown bucket / attention is refused with a reason, not dropped.
    let mail = crate::backend::MailListQuery {
        bucket: query_value(raw_query, "bucket"),
        attention: query_value(raw_query, "attention"),
        label: query_value(raw_query, "label"),
        since_ms: query_value(raw_query, "since_ms").and_then(|v| v.parse::<i64>().ok()),
        limit: query_value(raw_query, "limit").and_then(|v| v.parse::<usize>().ok()),
        include_handled: query_flag(raw_query, "include_handled"),
        include_low,
    };

    // Read the request body (POST writes / actions). Bounded to 256 KiB — an over-limit body is
    // refused, never silently treated as empty (a 202 for a note whose text was dropped is data
    // loss the caller cannot see). A mid-read disconnect lands here too, harmlessly: that caller
    // is gone. The two memory-import bodies get the import envelope instead, so a package the
    // 256 KiB package cap accepts still fits once escaped and with its ids selected (#391).
    let body_cap = match path.as_str() {
        "/v1/memory/import" | "/v1/memory/import/preview" => {
            shogun_memory::memory_import::MAX_ENVELOPE_BYTES
        }
        _ => 256 * 1024,
    };
    let body = match axum::body::to_bytes(req.into_body(), body_cap).await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|s| !s.is_empty()),
        Err(_) => return render(413, r#"{"error":"payload_too_large"}"#.to_string()),
    };

    // The operator's and the hosted surfaces' routes (E15 decide, E18 grants and teams) answer
    // before the agent routing: none of them is a tool an agent's bearer can reach.
    if let Some(method) = method {
        let incoming = governance::Incoming {
            method,
            path: &path,
            bearer: token.as_deref(),
            approval_token: approval_token.as_deref(),
            body: body.as_deref(),
            request_id: &request_id,
        };
        if let Some((status, body)) = governance::admin_route(&state, &incoming) {
            return render_with(status, body, &request_id, None);
        }
    }

    let (status, resp_body, approval_header) = match method {
        None => (405, r#"{"error":"method_not_allowed"}"#.to_string(), None),
        Some(method) => {
            let rreq = RestRequest {
                method,
                path,
                token,
                include_low,
                query,
                search_depth,
                search_source_kind,
                search_mode,
                search_bound_invalid,
                body,
                from_ms,
                to_ms,
                cursor_ts,
                cursor_id,
                cursor_direction,
                page_size,
                for_generation,
                app_bundle_id,
                person_id,
                project_id,
                mail,
            };
            // Resolve the plan once per request (issue #97) — the provider re-reads its source, so
            // a trial expiring while the server runs locks the next request.
            let ent = (state.entitlements)();
            // One token list per request (issue #400): re-read if the app changed it.
            let tokens = state.tokens.current();
            let routed = rest::route(&rreq, &tokens, &ent);
            // Store readiness (#800): the tool behind each route, classified by the predicate MCP
            // uses too. Approval polling, status and metrics never touch the memory store.
            let tool = match &routed {
                Routed::Read { tool, .. } | Routed::Write { tool, .. } => Some(*tool),
                Routed::Action => Some(crate::memory_api::Tool::ActionsExecute),
                Routed::ApprovalStatus { .. } => Some(crate::memory_api::Tool::ActionsStatus),
                _ => None,
            };
            if tool.is_some_and(crate::memory_readiness::tool_needs_store) {
                if let Some(why) = state.memory_gate.as_ref().and_then(|gate| gate()) {
                    return render(503, why.rest_body());
                }
            }
            // Per-client limit on model-starting calls (2026-10-20 cost audit). `route` has
            // already answered 401/403 for anything unauthenticated, unentitled or out of scope.
            if let Some(tool) = tool {
                if let Some(body) = rate_limited(&state, &tokens, &rreq, tool) {
                    return render(429, body);
                }
            }
            // The store this request reads and writes, and the writer it records (E2-T2 / E2-T4).
            // Only a route that touches memory needs one; status, metrics and approval polling
            // answer without it.
            // A cross-principal read (E18 Team) reaches world-state reads only: no write, no
            // scratch file, no other principal's approvals, audit log or grants.
            if cross.is_some() && !governance::cross_allowed(&routed) {
                return render_with(403, governance::CROSS_READ_ONLY.to_string(), &request_id, None);
            }
            let (backend, scope, principal) = match state.backend_for(rreq.token.as_deref(), include_scratch, cross.as_deref()) {
                Ok(resolved) => resolved,
                Err((status, body)) if tool.is_some() => return render_with(status, body, &request_id, None),
                Err(_) => (state.backend.clone(), shogun_memory::actor::CallScope::default(), None),
            };
            let write_info = governance::write_info(&routed);
            let actor = scope.write.actor.as_str().to_string();
            let answered = shogun_memory::actor::with_call_scope(scope, || match routed {
                // E15: the approval summaries, from the queue and the ledger (never the body).
                Routed::Read { tool: tool @ (crate::memory_api::Tool::ApprovalsList | crate::memory_api::Tool::ApprovalsGet), id } => {
                    governance::approvals_read(&state, backend.as_ref(), tool, id, principal.as_deref())
                }
                // actions.execute needs the shared approval queue (L3 sends enqueue there).
                // A local action is answered here, before the queue (issue #779): a search runs,
                // the screen-side kinds are refused. Sends take the approval queue.
                Routed::Action => match rest::slack_proposal(
                    rreq.body.as_deref(), backend.as_ref(),
                    crate::forms_api::FormsAuth { token: rreq.token.clone(), admitted_by_token: true, client: None }, "api",
                ).or_else(|| rest::local_answer(rreq.body.as_deref(), backend.as_ref())) {
                    Some(answer) => answer,
                    None => with_approvals(&state, |queue, surface| {
                    rest::act(
                        rreq.body.as_deref(),
                        (state.clock)(),
                        queue,
                        shogun_agents::approval::ApprovalOrigin::Api,
                        surface,
                    )
                }),
                },
                Routed::Write {
                    tool: crate::memory_api::Tool::DictionaryAutoAddSet, ..
                } if crate::dictionary_autoadd_api::needs_approval(rreq.body.as_deref()) => {
                    with_approvals(&state, |queue, surface| {
                        crate::dictionary_autoadd_api::request_enable(
                            rreq.body.as_deref(), (state.clock)(), queue,
                            shogun_agents::approval::ApprovalOrigin::Api, surface,
                        )
                    })
                }
                // Issue #654: enabling writing-style sharing is an L3 request, queued for the
                // user like a send. Disable / clear fall through to the backend (L1, direct).
                Routed::Write {
                    tool: crate::memory_api::Tool::WritingVoiceSet,
                    ..
                } if crate::writing_voice_api::needs_approval(rreq.body.as_deref()) => {
                    with_approvals(&state, |queue, surface| {
                        crate::writing_voice_api::request_enable(
                            rreq.body.as_deref(),
                            (state.clock)(),
                            queue,
                            shogun_agents::approval::ApprovalOrigin::Api,
                            surface,
                        )
                    })
                }
                // Issue #897: `{"paused": false}` while capture is paused ends the user's pause, so
                // it is an L3 request queued for the user. A pause falls through to the backend
                // (L1, direct), which refuses on its own anything that starts capture sooner.
                Routed::Write { tool: crate::memory_api::Tool::CaptureSetPaused, .. } => {
                    match crate::capture_pause_api::unpause_request(
                        backend.as_ref(),
                        rreq.body.as_deref(),
                    ) {
                        Some(pause) => with_approvals(&state, |queue, surface| {
                            crate::capture_pause_api::request_unpause(
                                pause,
                                (state.clock)(),
                                queue,
                                shogun_agents::approval::ApprovalOrigin::Api,
                                surface,
                            )
                        }),
                        None => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
                    }
                }
                // Issue #657: a capture_exclusions.set that drops an exclusion resumes capture, so
                // it is an L3 request queued for the user. One that only adds exclusions falls
                // through to the backend (L1, direct), which refuses any removal on its own.
                Routed::Write {
                    tool: crate::memory_api::Tool::CaptureExclusionsSet,
                    ..
                } => match crate::capture_exclusions_api::loosening(
                    backend.as_ref(),
                    rreq.body.as_deref(),
                ) {
                    Some(change) => with_approvals(&state, |queue, surface| {
                        crate::capture_exclusions_api::request_change(
                            change,
                            (state.clock)(),
                            queue,
                            shogun_agents::approval::ApprovalOrigin::Api,
                            surface,
                        )
                    }),
                    None => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
                },
                // Issue #110: selecting a subscription CLI is the opt-in to a consumer plan's data
                // handling, so it is an L3 request queued for the user. Switching to an API key or
                // withdrawing the opt-in falls through to the backend (L1, direct), which refuses
                // a subscription selection on its own.
                Routed::Write {
                    tool: crate::memory_api::Tool::AgentSourceSet,
                    ..
                } => match crate::agent_source_api::loosening(
                    backend.as_ref(),
                    rreq.body.as_deref(),
                ) {
                    Some(request) => with_approvals(&state, |queue, surface| {
                        crate::agent_source_api::request_subscription(
                            &request,
                            (state.clock)(),
                            queue,
                            shogun_agents::approval::ApprovalOrigin::Api,
                            surface,
                        )
                    }),
                    None => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
                },
                // Issue #782: turning Visual recall on, or keeping saved screens longer, starts or
                // lengthens keeping images of the screen, so it is an L3 request queued for the
                // user. Turning it off or shortening falls through to the backend (L1, direct),
                // which refuses any widening on its own.
                Routed::Write { tool, .. }
                    if crate::visual_recall_settings_api::is_settings_write(tool) =>
                {
                    match crate::visual_recall_settings_api::loosening(
                        backend.as_ref(),
                        tool,
                        rreq.body.as_deref(),
                    ) {
                        Some(change) => with_approvals(&state, |queue, surface| {
                            crate::visual_recall_settings_api::request_change(
                                change,
                                (state.clock)(),
                                queue,
                                shogun_agents::approval::ApprovalOrigin::Api,
                                surface,
                            )
                        }),
                        None => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
                    }
                }
                // Issue #779: a Gmail archive / unarchive the thread's label has no standing
                // approval for waits for the user's one tap. One it has, or one that changes
                // nothing on Gmail, falls through to the backend, which refuses anything else.
                Routed::Write { tool, .. } if crate::mail_archive_api::is_archive_write(tool) => {
                    let raw = rreq.body.as_deref().unwrap_or("");
                    let body = rest::mail_write_body(tool, &rreq.path, raw);
                    match crate::mail_archive_api::needs_approval(
                        backend.as_ref(),
                        tool,
                        Some(body.as_deref().unwrap_or(raw)),
                    ) {
                        Some(request) => with_approvals(&state, |queue, surface| {
                            crate::mail_archive_api::request(
                                request,
                                (state.clock)(),
                                queue,
                                shogun_agents::approval::ApprovalOrigin::Api,
                                surface,
                            )
                        }),
                        None => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
                    }
                }
                // The queue's answer, else the decision the caller's store persisted (V103);
                // multi-principal, only for the caller's own approvals.
                Routed::ApprovalStatus { id } => {
                    governance::approval_status(&state, backend.as_ref(), id, principal.as_deref())
                }
                // Discovery answers with the port actually bound, when the server knows it.
                Routed::Status => (200, rest::status_body(state.port)),
                // metrics come from the injected live source (empty snapshot if none).
                Routed::Metrics => (
                    200,
                    state
                        .metrics
                        .as_ref()
                        .map(|m| m.snapshot_json())
                        .unwrap_or_else(|| r#"{"metrics":[]}"#.to_string()),
                ),
                // Issue #744: same 403 and `error`, plus the cause. `route` has already answered
                // 401 for a missing or wrong token, so the plan is never disclosed before auth.
                Routed::PlanLocked => (
                    403,
                    rest::plan_locked_body(state.plan_lock.as_ref().and_then(|lock| lock())),
                ),
                // reads/writes/status/errors go through the backend renderer.
                _ => rest::respond_with(&rreq, &tokens, &ent, backend.as_ref()),
            });
            governance::after_call(
                &state,
                backend.as_ref(),
                &governance::CallFacts {
                    actor: &actor,
                    principal: principal.as_deref(),
                    write: write_info,
                    cross: cross.as_deref(),
                    request_id: &request_id,
                },
                answered,
            )
        }
    };

    render_with(status, resp_body, &request_id, approval_header.as_deref())
}

/// The 429 body when `tool` starts a model call and the caller's token is over its per-client
/// limit ([`crate::rate_limit`]); `None` to go ahead.
fn rate_limited(
    state: &AppState,
    tokens: &TokenRegistry,
    rreq: &RestRequest,
    tool: crate::memory_api::Tool,
) -> Option<String> {
    use crate::rate_limit::{triggers_llm, ClientKey, RATE_LIMITED, RATE_LIMITED_MESSAGE};
    if !triggers_llm(tool, rreq.search_mode.as_deref()) {
        return None;
    }
    let key = rreq
        .token
        .as_deref()
        .and_then(|token| tokens.token_id(token))
        .map(|id| ClientKey::token(&id))
        .unwrap_or_else(ClientKey::local);
    let now = u64::try_from((state.clock)()).unwrap_or(0);
    let retry_ms = state.llm_limiter.check(&key, now).err()?;
    Some(
        serde_json::json!({
            "error": RATE_LIMITED,
            "message": RATE_LIMITED_MESSAGE,
            "retry_after_ms": retry_ms,
        })
        .to_string(),
    )
}

/// Run an L3 `request` against the one approval queue this server may write: the durable store
/// shared with the desktop (only while the desktop is live to confirm), or the in-process queue
/// with the declared surface. `actions.execute` sends and a `writing_voice.set` enable both go
/// through here, so they refuse and answer alike.
fn with_approvals(
    state: &AppState,
    request: impl FnOnce(&mut ApprovalQueue, rest::ApprovalSurface) -> (u16, String),
) -> (u16, String) {
    if let Some(store) = &state.approval_store {
        if !(state.desktop_running)() && !state.remote.enabled() {
            return (503, r#"{"error":"desktop_unavailable"}"#.to_string());
        }
        return match store.with_queue(|queue| request(queue, rest::ApprovalSurface::Present)) {
            Ok(result) => result,
            Err(why) => (500, approval_store_error(&why)),
        };
    }
    match state.approvals.lock() {
        Ok(mut queue) => request(&mut queue, if state.remote.enabled() { rest::ApprovalSurface::Present } else { state.surface }),
        Err(_) => (500, r#"{"error":"internal"}"#.to_string()),
    }
}

/// The 500 body for a failed store pass. A missing sealing key (audit M2) is named so the
/// caller can tell "start the app / fix the key" from a corrupt file; the reason text is the
/// store's fixed message, never row content.
fn approval_store_error(why: &str) -> String {
    if why.contains("sealing key") {
        return serde_json::json!({ "error": "approval_store", "detail": why }).to_string();
    }
    r#"{"error":"approval_store"}"#.to_string()
}

/// The decision token in an `approval_required` MCP result (`result.approval.url` ends in
/// `/approve/<token>`), if any.
fn approval_token_in(body: &str) -> Option<String> {
    if !body.contains("/approve/") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let url = value["result"]["approval"]["url"].as_str()?;
    url.rsplit_once("/approve/").map(|(_, token)| token.to_string()).filter(|t| !t.is_empty())
}

/// [`render`] with the request id (E18) and, when this call queued an approval, its decision
/// token (E15) — the gateway reads the header to notify Slack and webhooks without reading the
/// body (invariant 2).
fn render_with(status: u16, body: String, request_id: &str, approval_token: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .header("x-request-id", request_id);
    if let Some(token) = approval_token {
        builder = builder.header(crate::remote_approvals::TOKEN_HEADER, token);
    }
    builder.body(Body::from(body)).unwrap_or_default()
}

/// The one place a JSON response is built, so every exit from `handle` is shaped the same.
fn render(status: u16, body: String) -> Response {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

/// Minimal `application/x-www-form-urlencoded` value decode: `+` → space, `%XX` → byte. Unknown
/// escapes pass through. Enough for a search query param (no dep).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bind the localhost listener on `port`, falling back to an ephemeral port if it is busy
/// (FR-API-01). Never binds a non-loopback interface (NFR-SEC-03).
pub async fn bind_local(port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            TcpListener::bind(("127.0.0.1", 0)).await
        }
        Err(e) => Err(e),
    }
}

/// Bind the localhost listener on exactly `port` (`0` = an ephemeral port), with no fallback
/// (threat model R3): the hosted sidecar answers on the port its gateway dials or not at all, so a
/// process that took that port first never receives the gateway's bearers and bodies while this
/// server sits elsewhere. `AddrInUse` is returned to the caller, which refuses to start. Never
/// binds a non-loopback interface (NFR-SEC-03).
pub async fn bind_exact(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}

/// Serve on an already-bound listener until the process ends. The caller reads the actual port from
/// the listener before handing it over (so it can report it via `shogun api status`).
pub async fn serve_on(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, build_router(state)).await
}

/// Convenience: bind the default (or fallback) port and serve. Returns only on error.
pub async fn serve(state: AppState, port: u16) -> std::io::Result<()> {
    let listener = bind_local(port).await?;
    serve_on(listener, state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn raw_get(addr: std::net::SocketAddr, path: &str, auth: Option<&str>) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth_line = auth
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth_line}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// GET with an arbitrary `Host:` header — what a DNS-rebound page's request looks like.
    async fn raw_get_with_host(addr: std::net::SocketAddr, path: &str, host: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn raw_post(
        addr: std::net::SocketAddr,
        path: &str,
        auth: Option<&str>,
        body: &str,
    ) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth_line = auth
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\n{auth_line}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// R3: the exact bind refuses a taken port instead of moving; the desktop's bind still moves.
    #[tokio::test]
    async fn the_exact_bind_refuses_a_taken_port_and_never_moves() {
        let taken = bind_exact(0).await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let refused = bind_exact(port).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
        let moved = bind_local(port).await.unwrap();
        assert_ne!(moved.local_addr().unwrap().port(), port, "the desktop fallback is unchanged");
        assert!(moved.local_addr().unwrap().ip().is_loopback());
    }

    async fn spawn_server(tokens: TokenRegistry) -> std::net::SocketAddr {
        spawn_server_with(tokens, Arc::new(crate::backend::StubBackend)).await
    }

    async fn spawn_server_with(
        tokens: TokenRegistry,
        backend: Arc<dyn crate::backend::MemoryBackend>,
    ) -> std::net::SocketAddr {
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        // The tests stand in for a process that HAS a confirm UI (the desktop app hosting this
        // face); the headless default is exercised by `send_is_refused_without_an_approval_surface`.
        let state = AppState::new(Arc::new(tokens), backend, approvals, Arc::new(|| 0))
            .with_entitlements(Arc::new(|| shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        addr
    }

    async fn raw_request(addr: std::net::SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
        response
            .split("\r\n\r\n")
            .next()?
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
            })
    }

    /// E4-T1: `POST /mcp` is the Streamable HTTP transport over the same listener: stateless,
    /// every reply carries `Mcp-Method` / `Mcp-Name`, a GET is 405, and a request without the
    /// metering headers is refused before the handler runs.
    #[tokio::test]
    async fn mcp_over_http_is_stateless_and_carries_the_metering_headers() {
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let mcp = crate::mcp::McpServer::new(
            crate::backend::StubBackend,
            approvals.clone(),
            || 0,
            || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
        );
        let state = AppState::new(Arc::new(tokens), Arc::new(crate::backend::StubBackend), approvals, Arc::new(|| 0))
            .with_entitlements(Arc::new(|| shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)))
            .with_mcp(Arc::new(mcp));
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let list = raw_request(
            addr,
            "POST",
            "/mcp",
            &[("Content-Type", "application/json"), ("Mcp-Method", "tools/list"), ("Authorization", "Bearer t")],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .await;
        assert!(list.starts_with("HTTP/1.1 200"), "{list}");
        assert_eq!(header(&list, "mcp-method"), Some("tools/list"));
        assert_eq!(header(&list, "mcp-protocol-version"), Some(crate::mcp::STATELESS_PROTOCOL_VERSION));
        assert!(header(&list, "mcp-session-id").is_none(), "stateless: no session header");
        let body = list.split("\r\n\r\n").nth(1).unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["result"]["cacheScope"], crate::mcp::CACHE_SCOPE);
        assert!(value["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "memory.ingest"));

        let call = raw_request(
            addr,
            "POST",
            "/mcp",
            &[("Content-Type", "application/json"), ("Mcp-Method", "tools/call"), ("Mcp-Name", "memory.ingest"), ("Authorization", "Bearer t")],
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory.ingest","arguments":{"events":[{"content":"a"},{"content":"b"},{"content":"c"}],"source":"s","actor":"a","idempotency_key":"k"}}}"#,
        )
        .await;
        assert!(call.starts_with("HTTP/1.1 200"), "{call}");
        assert_eq!(header(&call, "mcp-name"), Some("memory.ingest"));
        // The stub backend has no ingest, so the call is refused and settles nothing (E36a: the
        // header carries what the core took; `mcp_http` tests cover a taken count).
        assert_eq!(header(&call, "mcp-quantity"), Some("0"), "{call}");

        let unmetered = raw_request(
            addr,
            "POST",
            "/mcp",
            &[("Content-Type", "application/json"), ("Authorization", "Bearer t")],
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        )
        .await;
        assert!(unmetered.starts_with("HTTP/1.1 400"), "{unmetered}");
        assert!(unmetered.contains("missing_header"));

        let get = raw_request(addr, "GET", "/mcp", &[("Authorization", "Bearer t")], "").await;
        assert!(get.starts_with("HTTP/1.1 405"), "{get}");
        assert_eq!(header(&get, "allow"), Some("POST"));

        // Without the MCP face wired, the path is simply not there.
        let bare = spawn_server(TokenRegistry::new()).await;
        let missing = raw_request(bare, "POST", "/mcp", &[("Mcp-Method", "ping")], r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await;
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    }

    /// 2026-10-20 cost audit: a token's model-starting calls are limited (429 with a retry hint),
    /// per token; another token and calls that start no model are unaffected.
    #[tokio::test]
    async fn model_starting_calls_are_rate_limited_per_token() {
        let mut blob = crate::memory_api_settings::TokenBlob::default();
        crate::memory_api_settings::issue_into(&mut blob, "a".into(), "A", 1, "alpha", crate::memory_api::TokenScope::Full).unwrap();
        crate::memory_api_settings::issue_into(&mut blob, "b".into(), "B", 2, "beta", crate::memory_api::TokenScope::Full).unwrap();
        let mut tokens = TokenRegistry::new();
        tokens.register_blob(&blob).unwrap();
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(
            Arc::new(tokens),
            Arc::new(crate::backend::StubBackend),
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|| 1_000),
        )
        .with_entitlements(Arc::new(|| shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)))
        .with_llm_rate_limiter(Arc::new(
            crate::rate_limit::LlmRateLimiter::in_memory().with_limit(2, 60_000),
        ));
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        let body = r#"{"text":"hello","target":"ja"}"#;
        for _ in 0..2 {
            let resp = raw_post(addr, "/v1/translate", Some("alpha"), body).await;
            assert!(!resp.starts_with("HTTP/1.1 429"), "{resp}");
        }
        let resp = raw_post(addr, "/v1/translate", Some("alpha"), body).await;
        assert!(resp.starts_with("HTTP/1.1 429"), "{resp}");
        assert!(resp.contains("rate_limited") && resp.contains("retry_after_ms"), "{resp}");
        // Another token has its own window; a call that starts no model is never limited.
        let resp = raw_post(addr, "/v1/translate", Some("beta"), body).await;
        assert!(!resp.starts_with("HTTP/1.1 429"), "{resp}");
        let resp = raw_post(addr, "/v1/memory/notes", Some("alpha"), r#"{"text":"x"}"#).await;
        assert!(!resp.starts_with("HTTP/1.1 429"), "{resp}");
        // No token is a 401 before any counting.
        let resp = raw_post(addr, "/v1/translate", None, body).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
    }

    #[tokio::test]
    async fn read_token_is_refused_writes_over_the_socket_and_nothing_is_queued() {
        // Issue #400: the L3-queueing writes (`writing_voice.set` enable, `agent.source.set`) have
        // their own branches in `handle`; the scope refusal must come first, so a read token can
        // neither write nor put a card in front of the user.
        let mut tokens = TokenRegistry::new();
        tokens.issue_scoped("reader", crate::memory_api::TokenScope::Read);
        let addr = spawn_server(tokens).await;
        for (path, body) in [
            ("/v1/memory/notes", r#"{"text":"x"}"#),
            ("/v1/writing_voice", r#"{"action":"enable","consent":true}"#),
            ("/v1/agent/source", r#"{"source":"claude-code","subscription_consent":true}"#),
            ("/v1/actions/execute", r#"{"kind":"send_email","to":"a@b.com","subject":"s","body":"b"}"#),
        ] {
            let resp = raw_post(addr, path, Some("reader"), body).await;
            assert!(resp.starts_with("HTTP/1.1 403"), "{path}: {resp}");
            assert!(resp.contains(r#""error":"scope_insufficient""#), "{path}: {resp}");
            assert!(!resp.contains("approval_id"), "{path}: {resp}");
        }
        let resp = raw_get(addr, "/v1/api/tokens", Some("reader")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(r#""caller_scope":"read""#), "{resp}");
        assert!(!resp.contains("reader\""), "{resp}");
        // Issue #400: the generated access list and the token-free mount setup, same as MCP.
        assert!(resp.contains(r#""access":[{"group":"core""#), "{resp}");
        assert!(resp.contains(r#""token_env":"SHOGUN_API_TOKEN""#), "{resp}");
    }

    #[tokio::test]
    async fn status_endpoint_serves_over_the_socket() {
        let addr = spawn_server(TokenRegistry::new()).await;
        let resp = raw_get(addr, "/v1/status", None).await;
        assert!(resp.contains("200"), "status line: {resp}");
        assert!(resp.contains("shogun-memory-api"));
        assert!(resp.contains("application/json"));
    }

    #[tokio::test]
    async fn read_without_token_is_401_over_the_socket() {
        let addr = spawn_server(TokenRegistry::new()).await;
        let resp = raw_get(addr, "/v1/memory/search", None).await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("unauthorized"));
    }

    #[tokio::test]
    async fn read_with_valid_token_is_200_over_the_socket() {
        let mut tokens = TokenRegistry::new();
        tokens.issue("secret-token");
        let addr = spawn_server(tokens).await;
        let resp = raw_get(addr, "/v1/state/people", Some("secret-token")).await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
        assert!(resp.contains("state.people.list"));
    }

    #[tokio::test]
    async fn a_token_revoked_while_the_server_runs_is_401_on_its_next_request() {
        // Issue #400: the running REST face (and so the CLI) re-reads the token list when the app
        // bumps the revision marker; no restart. An unrelated token keeps working.
        use crate::live_tokens::{bump_revision, LiveTokens, TOKEN_REVISION_FILE};
        use crate::memory_api_settings::{issue_into, revoke_from, TokenBlob};
        let dir = std::env::temp_dir().join(format!(
            "shogun-rest-live-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let marker = dir.join(TOKEN_REVISION_FILE);
        let mut blob = TokenBlob::default();
        issue_into(&mut blob, "a".into(), "Cursor", 1, "shogun_revoked", crate::memory_api::TokenScope::Read).unwrap();
        issue_into(&mut blob, "b".into(), "Codex CLI", 2, "shogun_kept", crate::memory_api::TokenScope::Read).unwrap();
        let keychain = Arc::new(Mutex::new(blob));
        let source = keychain.clone();
        let live = Arc::new(LiveTokens::watching(marker.clone(), move || {
            let mut tokens = TokenRegistry::new();
            for record in &source.lock().unwrap().tokens {
                tokens.issue_record(record)?;
            }
            Ok(tokens)
        }));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(
            Arc::new(TokenRegistry::new()),
            Arc::new(crate::backend::StubBackend),
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|| 0),
        )
        .with_entitlements(Arc::new(|| {
            shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
        }))
        .with_live_tokens(live);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        let resp = raw_get(addr, "/v1/state/people", Some("shogun_revoked")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        // Settings' Revoke: Keychain first, then the marker.
        revoke_from(&mut keychain.lock().unwrap(), "a").unwrap();
        bump_revision(&marker).unwrap();
        let resp = raw_get(addr, "/v1/state/people", Some("shogun_revoked")).await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        let resp = raw_get(addr, "/v1/state/people", Some("shogun_kept")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn locked_plan_is_403_over_the_socket() {
        // Issue #97: a Standard-plan provider turns every tool endpoint into 403 plan_required,
        // valid token included; /v1/status stays open.
        use shogun_agents::entitlement::{entitlements, Plan};
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let state = AppState::new(
            Arc::new(tokens),
            Arc::new(crate::backend::StubBackend),
            approvals,
            Arc::new(|| 0),
        )
        .with_entitlements(Arc::new(|| entitlements(Plan::Standard, 0)));
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        let resp = raw_get(addr, "/v1/state/people", Some("t")).await;
        assert!(resp.contains("403"), "expected 403, got: {resp}");
        assert!(resp.contains("plan_required"));
        let resp = raw_get(addr, "/v1/status", None).await;
        assert!(resp.contains("200"), "status must stay open: {resp}");
    }

    #[tokio::test]
    async fn a_locked_plan_names_its_cause_only_after_auth() {
        // Issue #744: the 403 body carries the reason; a caller without a valid token still gets
        // a bare 401 and learns nothing about the plan.
        use shogun_agents::entitlement::{entitlements, Plan};
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(
            Arc::new(tokens),
            Arc::new(crate::backend::StubBackend),
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|| 0),
        )
        .with_entitlements(Arc::new(|| entitlements(Plan::Standard, 0)))
        .with_plan_lock(|| Some(crate::plan_source::PlanLock::StandardPlan));
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        let resp = raw_get(addr, "/v1/state/people", Some("t")).await;
        assert!(resp.contains("403") && resp.contains("plan_required"), "{resp}");
        assert!(resp.contains(r#""reason":"standard_plan""#), "{resp}");
        assert!(resp.contains(crate::plan_source::PlanLock::StandardPlan.fix()), "{resp}");
        for token in [None, Some("wrong")] {
            let resp = raw_get(addr, "/v1/state/people", token).await;
            assert!(resp.contains("401") && !resp.contains("standard_plan"), "{resp}");
        }
    }

    #[tokio::test]
    async fn a_memory_gate_is_503_on_memory_endpoints_until_it_opens() {
        // #800: a store the app has not upgraded is refused with a stable code, status stays
        // open, and the next request after the upgrade is served without a restart.
        use shogun_agents::entitlement::{entitlements, Plan};
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let open = Arc::new(AtomicBool::new(false));
        let gate = open.clone();
        let state = AppState::new(
            Arc::new(tokens),
            Arc::new(crate::backend::StubBackend),
            Arc::new(Mutex::new(ApprovalQueue::new())),
            Arc::new(|| 0),
        )
        .with_entitlements(Arc::new(|| entitlements(Plan::Pro, 0)))
        .with_memory_gate(move || {
            (!gate.load(Ordering::SeqCst)).then_some(MemoryUnavailable::NewerThanApp)
        });
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });
        let resp = raw_get(addr, "/v1/state/people", Some("t")).await;
        assert!(resp.contains("503") && resp.contains(r#""error":"memory_newer_than_app""#), "{resp}");
        assert!(resp.contains(crate::memory_readiness::NEWER_THAN_APP_MESSAGE), "{resp}");
        let resp = raw_get(addr, "/v1/state/people", None).await;
        assert!(resp.contains("401"), "auth still answers first: {resp}");
        let resp = raw_get(addr, "/v1/status", None).await;
        assert!(resp.contains("200"), "status must stay open: {resp}");
        let resp = raw_get(addr, "/v1/actions/status/1", Some("t")).await;
        assert!(!resp.contains("memory_newer_than_app"), "approval polling stays open: {resp}");
        open.store(true, Ordering::SeqCst);
        let resp = raw_get(addr, "/v1/state/people", Some("t")).await;
        assert!(resp.contains("200"), "served after the upgrade: {resp}");
    }

    #[test]
    fn the_plan_locked_body_keeps_its_error_and_adds_the_cause() {
        use crate::plan_source::PlanLock;
        assert_eq!(rest::plan_locked_body(None), r#"{"error":"plan_required"}"#);
        let v: serde_json::Value =
            serde_json::from_str(&rest::plan_locked_body(Some(PlanLock::TrialEnded))).unwrap();
        assert_eq!(v["error"], "plan_required");
        assert_eq!(v["reason"], "trial_ended");
        assert_eq!(v["message"], PlanLock::TrialEnded.fix());
    }

    #[tokio::test]
    async fn unknown_path_is_404_over_the_socket() {
        let addr = spawn_server(TokenRegistry::new()).await;
        let resp = raw_get(addr, "/v1/bogus", Some("t")).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
    }

    #[tokio::test]
    async fn backend_data_flows_through_the_socket() {
        use crate::backend::{MemoryBackend, ReadItem};
        use crate::memory_api::Tool;

        struct Fake;
        impl MemoryBackend for Fake {
            fn read(&self, _tool: Tool, _params: &crate::backend::ReadParams) -> Vec<ReadItem> {
                vec![ReadItem::new("ship the report", 0.9)]
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let addr = spawn_server_with(tokens, Arc::new(Fake)).await;
        let resp = raw_get(addr, "/v1/state/commitments", Some("t")).await;
        assert!(resp.contains("200"), "got: {resp}");
        assert!(
            resp.contains("ship the report"),
            "real backend data missing: {resp}"
        );
    }

    #[tokio::test]
    async fn post_note_reaches_the_backend_write_over_the_socket() {
        use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use crate::memory_api::Tool;
        use std::sync::Mutex;

        // a backend that records the note body it was asked to write
        struct Recorder {
            last: Mutex<Option<String>>,
        }
        impl MemoryBackend for Recorder {
            fn read(&self, _t: Tool, _p: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn write(&self, tool: Tool, body: &str) -> WriteResult {
                if tool == Tool::MemoryAppendNote {
                    *self.last.lock().unwrap() = Some(body.to_string());
                    Ok(Some(7))
                } else {
                    Ok(None)
                }
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(Recorder {
            last: Mutex::new(None),
        });
        let addr = spawn_server_with(tokens, backend.clone()).await;

        let resp = raw_post(addr, "/v1/memory/notes", Some("t"), "buy milk").await;
        assert!(resp.contains("202"), "got: {resp}");
        assert!(resp.contains("\"note_id\":7"), "got: {resp}");
        assert_eq!(backend.last.lock().unwrap().as_deref(), Some("buy milk"));

        // no token → 401, and the backend is not touched
        let resp = raw_post(addr, "/v1/memory/notes", None, "sneaky").await;
        assert!(resp.contains("401"), "got: {resp}");
    }

    #[tokio::test]
    async fn an_over_limit_body_is_refused_not_written_as_empty() {
        use crate::backend::{MemoryBackend, ReadItem, ReadParams, WriteResult};
        use crate::memory_api::Tool;
        use std::sync::Mutex;

        struct Recorder {
            wrote: Mutex<bool>,
        }
        impl MemoryBackend for Recorder {
            fn read(&self, _t: Tool, _p: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
                *self.wrote.lock().unwrap() = true;
                Ok(None)
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(Recorder {
            wrote: Mutex::new(false),
        });
        let addr = spawn_server_with(tokens, backend.clone()).await;

        // 256 KiB + 1: over the body cap. A failed read must not become "no body": that answered
        // 202 for a note whose text was silently dropped.
        let huge = "x".repeat(256 * 1024 + 1);
        let resp = raw_post(addr, "/v1/memory/notes", Some("t"), &huge).await;
        assert!(resp.contains("413"), "got: {}", &resp[..resp.len().min(200)]);
        assert!(resp.contains("payload_too_large"));
        assert!(!*backend.wrote.lock().unwrap(), "nothing may reach the backend");
    }

    /// #391: the memory-import bodies take the import envelope, not the 256 KiB body cap, and
    /// are still refused past it.
    #[tokio::test]
    async fn memory_import_bodies_get_the_import_envelope() {
        use crate::backend::{MemoryBackend, ReadItem, ReadParams};
        use crate::memory_api::Tool;
        use crate::memory_import_api::MemoryImportOperation as Op;
        use shogun_memory::memory_import::MAX_ENVELOPE_BYTES;
        use std::sync::Mutex;

        struct Recorder {
            got: Mutex<Vec<usize>>,
        }
        impl MemoryBackend for Recorder {
            fn read(&self, _t: Tool, _p: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn memory_import(&self, operation: Op) -> Result<String, String> {
                if let Op::Preview { body } = operation {
                    self.got.lock().unwrap().push(body.len());
                }
                Ok(r#"{"items":[]}"#.to_string())
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(Recorder {
            got: Mutex::new(Vec::new()),
        });
        let addr = spawn_server_with(tokens, backend.clone()).await;

        let big = "x".repeat(300 * 1024);
        let resp = raw_post(addr, "/v1/memory/import/preview", Some("t"), &big).await;
        assert!(resp.contains("200"), "got: {}", &resp[..resp.len().min(200)]);
        let over = "x".repeat(MAX_ENVELOPE_BYTES + 1);
        let resp = raw_post(addr, "/v1/memory/import/preview", Some("t"), &over).await;
        assert!(resp.contains("payload_too_large"), "got: {}", &resp[..resp.len().min(200)]);
        assert_eq!(*backend.got.lock().unwrap(), [big.len()]);
    }

    #[tokio::test]
    async fn a_rebound_hostname_cannot_read_the_open_endpoints() {
        // /v1/metrics has no token by design, and the listener is loopback-only — but a page on
        // attacker.com whose DNS answers 127.0.0.1 is same-origin with this server, so CORS never
        // applies. The Host header is what still distinguishes it.
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let addr = spawn_server(tokens).await;

        let rebound = raw_get_with_host(addr, "/v1/metrics", "attacker.example").await;
        assert!(rebound.contains("403"), "got: {rebound}");
        assert!(rebound.contains("forbidden_host"), "got: {rebound}");

        // The real local caller is unaffected, including with an explicit port.
        let ok =
            raw_get_with_host(addr, "/v1/metrics", &format!("127.0.0.1:{}", addr.port())).await;
        assert!(ok.contains("200"), "got: {ok}");
        let ok_name = raw_get_with_host(addr, "/v1/status", "localhost").await;
        assert!(ok_name.contains("200"), "got: {ok_name}");
    }

    #[test]
    fn include_low_opt_in_is_truthy_value_matching() {
        // Opted in: bare flag, bare `=`, and the truthy spellings hand-built clients emit.
        for q in [
            "include_low",
            "include_low=",
            "include_low=1",
            "include_low=true",
            "include_low=True",
            "include_low=TRUE",
            "include_low=yes",
            "include_low=on",
            "q=ship&include_low=True",
        ] {
            assert!(include_low_opt_in(q), "expected opt-in for {q:?}");
        }
        // Opted out: the explicit falses, an unrecognised value, and a prefix collision.
        for q in [
            "",
            "q=ship",
            "include_low=false",
            "include_low=False",
            "include_low=0",
            "include_low=maybe",
            "include_lower=true",
            "xinclude_low=true",
        ] {
            assert!(!include_low_opt_in(q), "expected opt-out for {q:?}");
        }
    }

    #[test]
    fn query_value_percent_decodes_and_skips_empty() {
        assert_eq!(
            query_value("person_id=alice%40example.com", "person_id").as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(query_value("person_id=", "person_id"), None);
        assert_eq!(query_value("q=ship", "person_id"), None);
        assert!(query_flag("for_generation", "for_generation"));
        assert!(query_flag("for_generation=true", "for_generation"));
        assert!(!query_flag("for_generation=false", "for_generation"));
    }

    #[test]
    fn host_header_matching_is_name_only() {
        assert!(host_is_loopback(Some("127.0.0.1")));
        assert!(host_is_loopback(Some("127.0.0.1:7464")));
        assert!(host_is_loopback(Some("localhost:7464")));
        assert!(host_is_loopback(Some("[::1]:7464")));
        assert!(!host_is_loopback(Some("attacker.example")));
        assert!(!host_is_loopback(Some("localhost.attacker.example")));
        // 0.0.0.0 is the "0.0.0.0-day" bypass host, never a legitimate local caller.
        assert!(!host_is_loopback(Some("0.0.0.0")));
        assert!(!host_is_loopback(Some("0.0.0.0:7464")));
        // A missing Host is HTTP/1.1-invalid and is not something a real local caller sends.
        assert!(!host_is_loopback(None));
    }

    /// Issue #654 over the socket: `{"action":"enable","consent":true}` is queued for the user
    /// (202 pending, same envelope as a send) and never reaches the backend; disable and clear
    /// are direct L1 writes.
    #[tokio::test]
    async fn writing_voice_enable_is_pending_l3_and_disable_clear_are_direct() {
        use crate::backend::{ReadItem, ReadParams, WriteResult};
        use crate::memory_api::Tool;

        #[derive(Default)]
        struct Recorder {
            writes: Mutex<Vec<String>>,
        }
        impl MemoryBackend for Recorder {
            fn read(&self, _t: Tool, _p: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn write(&self, tool: Tool, body: &str) -> WriteResult {
                assert_eq!(tool, Tool::WritingVoiceSet);
                self.writes.lock().unwrap().push(body.to_string());
                Ok(None)
            }
        }
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(Recorder::default());
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_post(addr, "/v1/writing_voice", Some("t"), r#"{"action":"enable","consent":true}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L3""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        assert!(backend.writes.lock().unwrap().is_empty(), "the setting must not change");
        let id = {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 1);
            let id = q.pending_ids()[0];
            assert_eq!(q.action(id), Some(&shogun_agents::permission::SendAction::ShareWritingStyle));
            id.0
        };
        let resp = raw_get(addr, &format!("/v1/actions/status/{id}"), Some("t")).await;
        assert!(resp.contains(r#""status":"pending""#), "got: {resp}");

        // Consent missing: refused, nothing queued, backend untouched.
        let resp = raw_post(addr, "/v1/writing_voice", Some("t"), r#"{"action":"enable"}"#).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        assert!(resp.contains("consent_required"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        for action in ["disable", "clear"] {
            let body = format!(r#"{{"action":"{action}"}}"#);
            let resp = raw_post(addr, "/v1/writing_voice", Some("t"), &body).await;
            assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
            assert!(resp.contains(r#""level":"L1""#), "{action} is direct L1: {resp}");
            assert!(!resp.contains("pending"), "got: {resp}");
        }
        assert_eq!(backend.writes.lock().unwrap().len(), 2);
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        // No token: 401 before anything is queued.
        let resp = raw_post(addr, "/v1/writing_voice", None, r#"{"action":"enable","consent":true}"#).await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);
    }

    /// Issue #657 over the socket: a `capture_exclusions.set` that drops an exclusion is queued
    /// for the user (202 pending, same envelope as a send) and the lists stay as they are; one
    /// that only adds is written directly and answers with the saved lists.
    #[tokio::test]
    async fn capture_exclusions_removal_is_pending_l3_and_additions_are_direct() {
        use crate::capture_exclusions_api::test_support::FakeExclusions;

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FakeExclusions::with(&["com.example.Secret"], &["private.example"]));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        // Removes the website and adds an app: the whole request waits.
        let body = r#"{"apps":["com.example.Secret","com.example.New"],"websites":[]}"#;
        let resp = raw_post(addr, "/v1/capture_exclusions", Some("t"), body).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L3""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 0, "the lists must not change");
        assert_eq!(*backend.websites.lock().unwrap(), vec!["private.example".to_string()]);
        let id = {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 1);
            let id = q.pending_ids()[0];
            assert!(matches!(
                q.action(id),
                Some(shogun_agents::permission::SendAction::ResumeCapture { change })
                    if change.resumed_websites == vec!["private.example".to_string()]
            ));
            id.0
        };
        let resp = raw_get(addr, &format!("/v1/actions/status/{id}"), Some("t")).await;
        assert!(resp.contains(r#""status":"pending""#), "got: {resp}");

        // Additions only: direct, answered with the saved lists, nothing queued.
        let body = r#"{"apps":["com.example.Secret","com.example.New"],"websites":["private.example"]}"#;
        let resp = raw_post(addr, "/v1/capture_exclusions", Some("t"), body).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains("com.example.New"), "got: {resp}");
        assert!(!resp.contains("pending"), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 1);
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        // No token: 401 before anything is queued.
        let resp = raw_post(addr, "/v1/capture_exclusions", None, r#"{"apps":[],"websites":[]}"#).await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);
    }

    /// Issue #779 over the socket: an API archive the label has no standing approval for is
    /// queued for the user (202 pending with an approval id, level L2) and nothing is recorded;
    /// one under a standing approval is recorded directly.
    #[tokio::test]
    async fn a_gmail_archive_waits_for_the_user_unless_a_standing_approval_covers_it() {
        use crate::mail_archive_api::test_support::{FakeMail, Thread};
        use shogun_agents::permission::SendAction;

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FakeMail::with(&[
            ("gmail:a", Thread { archived: false, requested: false, standing: false }),
            ("gmail:s", Thread { archived: false, requested: false, standing: true }),
        ]));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_post(addr, "/v1/mail/triage/gmail:a/archive", Some("t"), "").await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""approval_id":"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L2""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 0, "nothing recorded until the user confirms");
        let id = {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 1);
            let id = q.pending_ids()[0];
            assert_eq!(
                q.action(id),
                Some(&SendAction::ArchiveMailThread { thread_key: "gmail:a".into(), unarchive: false })
            );
            id.0
        };
        let resp = raw_get(addr, &format!("/v1/actions/status/{id}"), Some("t")).await;
        assert!(resp.contains(r#""status":"pending""#), "got: {resp}");

        // Under the standing approval: recorded directly, nothing queued.
        let resp = raw_post(addr, "/v1/mail/triage/gmail:s/archive", Some("t"), "").await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""accepted":true"#), "got: {resp}");
        assert!(!resp.contains("approval_id"), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 1);
        assert!(backend.thread("gmail:s").is_some_and(|t| t.requested));
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        // Unknown thread: the backend's 404, not a queued request.
        let resp = raw_post(addr, "/v1/mail/triage/gmail:nope/archive", Some("t"), "").await;
        assert!(resp.starts_with("HTTP/1.1 404"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        // No token: 401 before anything is queued.
        let resp = raw_post(addr, "/v1/mail/triage/gmail:a/archive", None, "").await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);
    }

    /// Issue #897 over the socket (the CLI's `shogun capture resume`): ending the user's pause is
    /// queued for the user (202 pending, same envelope as a send) and capture stays paused;
    /// pausing is written directly at L1; ending a timed pause early is refused with its reason.
    #[tokio::test]
    async fn resuming_a_paused_capture_is_pending_l3_and_pausing_is_direct() {
        use crate::capture_pause_api::test_support::FakePause;
        use shogun_agents::permission::SendAction;

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FakePause::with(Some(Some(60_000))));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_post(addr, "/v1/capture/paused", Some("t"), r#"{"paused":false}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L3""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 0, "nothing changes until the user confirms");
        {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 1);
            assert!(matches!(q.action(q.pending_ids()[0]), Some(SendAction::UnpauseCapture { .. })));
        }

        let resp =
            raw_post(addr, "/v1/capture/paused", Some("t"), r#"{"paused":true,"resume_at_ms":30000}"#).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        assert!(resp.contains("already paused for longer"), "got: {resp}");

        let resp = raw_post(addr, "/v1/capture/paused", Some("t"), r#"{"paused":true}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 1);
        assert_eq!(*backend.pause.lock().unwrap(), Some(None));
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);
    }

    /// Issue #782 over the socket: turning Visual recall on, or keeping saved screens longer, is
    /// queued for the user (202 pending, same envelope as a send) and the settings stay as they
    /// are; turning it off or shortening is written directly at L1.
    #[tokio::test]
    async fn visual_recall_widening_is_pending_l3_and_narrowing_is_direct() {
        use crate::visual_recall_settings_api::test_support::FakeVisualRecall;
        use shogun_agents::permission::{SendAction, VisualRecallWidening};

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FakeVisualRecall::with(false, 3));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_post(addr, "/v1/visual_recall/enabled", Some("t"), r#"{"enabled":true}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L3""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        let resp = raw_post(addr, "/v1/visual_recall/retention", Some("t"), r#"{"days":3650}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 0, "nothing changes until the user confirms");
        assert!(!*backend.enabled.lock().unwrap());
        assert_eq!(*backend.days.lock().unwrap(), 3);
        let id = {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 2);
            let ids = q.pending_ids();
            assert_eq!(
                q.action(ids[0]),
                Some(&SendAction::WidenVisualRecall {
                    change: VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 3 },
                })
            );
            assert_eq!(
                q.action(ids[1]),
                Some(&SendAction::WidenVisualRecall {
                    change: VisualRecallWidening { enable: false, retention_days: Some(3650), previous_retention_days: 3 },
                })
            );
            ids[0].0
        };
        let resp = raw_get(addr, &format!("/v1/actions/status/{id}"), Some("t")).await;
        assert!(resp.contains(r#""status":"pending""#), "got: {resp}");

        // Shortening and turning off: direct L1, nothing queued.
        let resp = raw_post(addr, "/v1/visual_recall/retention", Some("t"), r#"{"days":1}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(!resp.contains("pending"), "got: {resp}");
        assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
        let resp = raw_post(addr, "/v1/visual_recall/enabled", Some("t"), r#"{"enabled":false}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(!resp.contains("pending"), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 2);
        assert_eq!(*backend.days.lock().unwrap(), 1);
        assert_eq!(approvals.lock().unwrap().pending_len(), 2);

        // No token: 401 before anything is queued.
        let resp = raw_post(addr, "/v1/visual_recall/enabled", None, r#"{"enabled":true}"#).await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 2);
    }

    /// Issue #394 over the socket: list, answer (origin `api`, whatever the body claims), and
    /// the refusals with their statuses. An L3 send pending under the same number as the question
    /// stays pending after the answer — the answer route never reaches the approval queue.
    #[tokio::test]
    async fn clarify_questions_and_answer_over_the_socket() {
        use crate::backend::{MemoryBackend, ReadItem, ReadParams};
        use crate::clarify_api::{test_store::MemClarify, ClarifyOperation};
        use crate::memory_api::Tool;
        use shogun_agents::clarify::{Question, RunKind, RunRef};

        struct Clarify(MemClarify);
        impl MemoryBackend for Clarify {
            fn read(&self, _t: Tool, _p: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn clarify(&self, operation: ClarifyOperation) -> Result<String, String> {
                Ok(self.0.apply(operation))
            }
        }
        let store = MemClarify { now_ms: 60_000, ..MemClarify::default() };
        let asked = store.ask(
            RunRef { kind: RunKind::SkillRun, id: 7 },
            Question::new("Which Sam?", &["Sam Lee", "Sam Ortiz"], true, 50_000, None).unwrap(),
        );
        let late = store.ask(
            RunRef { kind: RunKind::Notch, id: 3 },
            Question::new("Which folder?", &[], true, 0, Some(1)).unwrap(),
        );
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let addr = spawn_server_with(tokens, Arc::new(Clarify(store))).await;

        let send = raw_post(
            addr,
            "/v1/actions/execute",
            Some("t"),
            r#"{"kind":"send_email","to":"a@b.com","subject":"s","body":"b"}"#,
        )
        .await;
        assert!(send.contains("\"approval_id\":1"), "same number as the question: {send}");

        let resp = raw_get(addr, "/v1/actions/questions", Some("t")).await;
        assert!(resp.contains("200"), "got: {resp}");
        assert!(resp.contains("\"count\":1") && resp.contains("awaiting_input"), "got: {resp}");

        let body = format!(r#"{{"id":{asked},"answer":"Sam Park","origin":"ui"}}"#);
        let resp = raw_post(addr, "/v1/actions/answer", Some("t"), &body).await;
        assert!(resp.contains("200"), "got: {resp}");
        assert!(resp.contains("\"status\":\"answered\""), "got: {resp}");
        assert!(resp.contains("\"answered_via\":\"api\""), "REST/CLI answer as api: {resp}");
        assert!(resp.contains("\"kind\":\"skill_run\",\"id\":7") || resp.contains("\"id\":7,\"kind\":\"skill_run\""), "got: {resp}");

        // The L3 send is still pending: answering is not approving.
        let resp = raw_get(addr, "/v1/actions/status/1", Some("t")).await;
        assert!(resp.contains("\"status\":\"pending\""), "got: {resp}");

        let resp = raw_post(addr, "/v1/actions/answer", Some("t"), &body).await;
        assert!(resp.contains("409") && resp.contains("already_answered"), "got: {resp}");
        let resp = raw_post(addr, "/v1/actions/answer", Some("t"), r#"{"id":99,"answer":"x"}"#).await;
        assert!(resp.contains("404") && resp.contains("unknown_question"), "got: {resp}");
        let late_body = format!(r#"{{"id":{late},"answer":"Docs"}}"#);
        let resp = raw_post(addr, "/v1/actions/answer", Some("t"), &late_body).await;
        assert!(resp.contains("409") && resp.contains("expired"), "got: {resp}");
        let resp = raw_post(addr, "/v1/actions/answer", Some("t"), r#"{"id":1}"#).await;
        assert!(resp.contains("400") && resp.contains("missing_answer"), "got: {resp}");
        let resp = raw_get(addr, &format!("/v1/actions/questions/{late}"), Some("t")).await;
        assert!(resp.contains("200") && resp.contains("\"status\":\"expired\""), "got: {resp}");
        let resp = raw_get(addr, "/v1/actions/questions/99", Some("t")).await;
        assert!(resp.contains("404"), "got: {resp}");
        // GET on the answer route and no token are refused before any store call.
        let resp = raw_get(addr, "/v1/actions/answer", Some("t")).await;
        assert!(resp.contains("405"), "got: {resp}");
        let resp = raw_post(addr, "/v1/actions/answer", None, &body).await;
        assert!(resp.contains("401"), "got: {resp}");
    }

    /// Issue #110 over the socket: selecting a subscription CLI is queued for the user (202
    /// pending, same envelope as a send) and llm.json is untouched; switching to an API key or
    /// withdrawing the opt-in is a direct L1 write; granting consent on its own is a 400.
    #[tokio::test]
    async fn agent_source_subscription_is_pending_l3_and_api_key_switches_are_direct() {
        use crate::agent_source_api::test_support::FakeAgentSource;

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FakeAgentSource::with("anthropic", false));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_get(addr, "/v1/agent/source", Some("t")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
        assert!(resp.contains(r#""selected":"anthropic""#), "got: {resp}");

        let body = r#"{"source":"claude-code","subscription_consent":true}"#;
        let resp = raw_post(addr, "/v1/agent/source", Some("t"), body).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""pending":true"#), "got: {resp}");
        assert!(resp.contains(r#""level":"L3""#), "got: {resp}");
        assert!(resp.contains(r#""origin":"api""#), "got: {resp}");
        assert_eq!(*backend.writes.lock().unwrap(), 0, "the source must not change");
        let id = {
            let q = approvals.lock().unwrap();
            assert_eq!(q.pending_len(), 1);
            let id = q.pending_ids()[0];
            assert_eq!(
                q.action(id),
                Some(&shogun_agents::permission::SendAction::UseSubscriptionAgent { delegate: "claude-code".into() })
            );
            id.0
        };
        let resp = raw_get(addr, &format!("/v1/actions/status/{id}"), Some("t")).await;
        assert!(resp.contains(r#""status":"pending""#), "got: {resp}");

        // Refused, nothing queued, nothing written: no attestation, or consent on its own.
        for body in [r#"{"source":"claude-code"}"#, r#"{"subscription_consent":true}"#] {
            let resp = raw_post(addr, "/v1/agent/source", Some("t"), body).await;
            assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        }
        assert_eq!(*backend.writes.lock().unwrap(), 0);

        // An API key provider and a withdrawal: direct L1.
        for body in [r#"{"source":"openai"}"#, r#"{"subscription_consent":false}"#] {
            let resp = raw_post(addr, "/v1/agent/source", Some("t"), body).await;
            assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
            assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
            assert!(!resp.contains("pending"), "got: {resp}");
        }
        assert_eq!(*backend.writes.lock().unwrap(), 2);
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);

        // No token: 401 before anything is queued.
        let resp = raw_post(addr, "/v1/agent/source", None, r#"{"source":"codex","subscription_consent":true}"#).await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(approvals.lock().unwrap().pending_len(), 1);
    }

    /// Issue #82 over the socket: the Connections screen reads, disconnecting and revoking the
    /// Mail consent are direct L1 writes, and granting consent or turning draft-only mode off is a
    /// 400 that writes nothing and queues nothing.
    #[tokio::test]
    async fn connections_narrowing_is_direct_and_loosening_is_refused() {
        use crate::connections_api::test_support::FileConnections;
        use crate::scope::Service;

        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let backend = Arc::new(FileConnections::new("socket"));
        let approvals = Arc::new(Mutex::new(ApprovalQueue::new()));
        let listener = bind_local(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new(Arc::new(tokens), backend.clone(), approvals.clone(), Arc::new(|| 5))
            .with_entitlements(Arc::new(|| {
                shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0)
            }))
            .with_approval_surface(rest::ApprovalSurface::Present);
        tokio::spawn(async move {
            let _ = serve_on(listener, state).await;
        });

        let resp = raw_get(addr, "/v1/connections", Some("t")).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
        assert!(resp.contains(r#""service":"gcal""#), "got: {resp}");
        assert!(resp.contains(r#""consent":true"#), "got: {resp}");
        assert!(!resp.contains("user-123"), "no account id: {resp}");

        // Refused, nothing written: granting consent back, turning draft-only mode off.
        for body in [r#"{"draft_stop":false}"#, r#"{"service":"gcal"}"#] {
            let resp = raw_post(addr, "/v1/connections/composio", Some("t"), body).await;
            assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        }
        assert!(backend.policy().draft_stop);

        let resp = raw_post(addr, "/v1/connections/disconnect", Some("t"), r#"{"service":"gdrive"}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
        assert!(!resp.contains("pending"), "got: {resp}");
        let resp = raw_post(addr, "/v1/connections/composio", Some("t"), r#"{"consent":false}"#).await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
        assert!(!backend.policy().consent_acknowledged);
        assert_eq!(backend.pending(), vec![Service::GoogleDrive]);

        let resp = raw_post(addr, "/v1/connections/composio", Some("t"), r#"{"consent":true}"#).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        assert!(resp.contains("consent_only_in_app"), "got: {resp}");
        assert!(!backend.policy().consent_acknowledged);
        let resp = raw_post(addr, "/v1/connections/disconnect", Some("t"), r#"{"service":"web"}"#).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");

        let resp = raw_post(
            addr,
            "/v1/connections/gmail",
            Some("t"),
            r#"{"draft_stop":true}"#,
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 202"), "got: {resp}");
        assert!(resp.contains(r#""level":"L1""#), "got: {resp}");
        for body in [
            r#"{"draft_stop":false}"#,
            r#"{}"#,
            r#"{"draft_stop":true,"consent":true}"#,
        ] {
            let resp = raw_post(addr, "/v1/connections/gmail", Some("t"), body).await;
            assert!(resp.starts_with("HTTP/1.1 400"), "got: {resp}");
        }
        let resp = raw_post(
            addr,
            "/v1/connections/gmail",
            None,
            r#"{"draft_stop":true}"#,
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "got: {resp}");

        // No token: 401 before anything is read or written.
        let resp = raw_post(addr, "/v1/connections/disconnect", None, r#"{"service":"slack"}"#).await;
        assert!(resp.contains("401"), "got: {resp}");
        let resp = raw_get(addr, "/v1/connections", None).await;
        assert!(resp.contains("401"), "got: {resp}");
        assert_eq!(backend.pending(), vec![Service::GoogleDrive]);
        assert_eq!(approvals.lock().unwrap().pending_len(), 0);
    }

    #[tokio::test]
    async fn actions_execute_local_and_send_over_the_socket() {
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let addr = spawn_server(tokens).await;

        // A local action never reaches the queue (issue #779): a screen-side kind is refused as
        // not available over the API, and nothing is reported executed.
        let resp = raw_post(
            addr,
            "/v1/actions/execute",
            Some("t"),
            r#"{"kind":"open_app","bundle_id":"com.example.App"}"#,
        )
        .await;
        assert!(resp.contains("501"), "got: {resp}");
        assert!(resp.contains("local_action_unavailable"), "got: {resp}");
        assert!(!resp.contains("\"executed\""), "got: {resp}");

        // an external send is pending L3 approval (202 + approval id) — not executed
        let resp = raw_post(
            addr,
            "/v1/actions/execute",
            Some("t"),
            r#"{"kind":"send_email","to":"a@b.com","subject":"s","body":"b"}"#,
        )
        .await;
        assert!(resp.contains("202"), "got: {resp}");
        assert!(resp.contains("\"pending\":true"));
        assert!(resp.contains("\"approval_id\":"));

        // no token → 401
        let resp = raw_post(
            addr,
            "/v1/actions/execute",
            None,
            r#"{"kind":"local_search","query":"x"}"#,
        )
        .await;
        assert!(resp.contains("401"), "got: {resp}");
    }
}
