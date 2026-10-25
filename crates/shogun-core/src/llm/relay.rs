//! The Batch-relay client (docs/batch-relay-design.md).
//!
//! The shipping Batch lane never holds an Anthropic key: the device holds only a **license
//! token** (FR-BIL-08 — signed, device-bound and ~24h-lived, so it is cached in `billing.json`
//! rather than the Keychain; CLAUDE.md invariant 7 の 2026-08-13 例外) and talks to the
//! Select-operated relay
//! (`api.shogunaios.com`), which verifies the token, enforces the plan's daily chunk cap, and
//! delegates to the Anthropic Batch API with the operator's server-side key. Two consequences
//! are encoded here:
//!
//! - **The device never names a model** (§4.4): a client that can pick the model can pick an
//!   expensive one, so the request carries only a [`ModelClass`] intent and the relay chooses.
//! - **The route is distinguishable** (§3.3): every submitted chunk records
//!   [`Route::BatchRelay`] — not `BatchApi` — so the traceability screen can show "via operator
//!   server", the same way Composio sends show "via third party".
//!
//! Same lifecycle as [`AnthropicBatchClient`](super::anthropic::AnthropicBatchClient) — submit /
//! poll / results, unified by the [`BatchLane`] trait — so the Dream Cycle scheduler is oblivious
//! to which lane is wired in. The relay wire shape differs (see §4.2/§4.3): create returns
//! `{batch_id, accepted}`, and status/results share one `GET /v1/batch/{id}` endpoint.
//!
//! Everything except the socket write is a pure function, exhaustively tested on Linux with the
//! same [`MockTransport`](super::transport::MockTransport) the Anthropic client uses.

use serde_json::{json, Value};

use super::anthropic::{
    shared_batch_system, BatchHandle, BatchItem, BatchLane, BatchResult, BatchStatus,
};
use super::retry::{classify_shared_throttle, SharedThrottle};
use super::traceability::{RoundTrip, Route, TraceKind, TraceRecord, TraceabilitySink};
use super::transport::{HttpRequest, HttpResponse, HttpTransport, Method};
use super::usage::{Usage, UsageHook, UsageOutcome, UsageRecord};
use super::{LicenseBearer, LlmError, Secret};

/// The production relay host: a subdomain of the domain SHOGUN already owns and serves the
/// licensing control plane from. Overridable per-config for a staging relay; never overridable
/// to plain HTTP ([`HttpRequest::new`] rejects non-HTTPS).
///
/// **It must stay a hostname SHOGUN controls.** This constant is where a licence bearer token
/// and Dream Cycle chunk content are sent, so pointing it at a host we do not operate hands both
/// to whoever does — which is exactly what the previous value, `relay.shogun.app`, did: an
/// unregistered domain parked by a registrar (issue #334). Changing this to a host outside our
/// control is a data-egress change, not a config tweak.
pub const DEFAULT_RELAY_BASE_URL: &str = "https://api.shogunaios.com";

/// Accept a relay override only when it is a loopback URL (`http` or `https`, no credentials).
/// Anything else returns `None`, so a stray env var or file can never redirect the licence
/// bearer or chunk content off this Mac.
pub fn loopback_relay_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw.trim()).ok()?;
    let host_ok = match u.host()? {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(a) => a.is_loopback(),
        url::Host::Ipv6(a) => a.is_loopback(),
    };
    let ok = host_ok && matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none();
    ok.then(|| u.as_str().trim_end_matches('/').to_string())
}

/// Dev override for the relay base URL: env `SHOGUN_RELAY_BASE_URL`, else
/// `relay_override.json` (`{"base_url": "http://127.0.0.1:8787"}`) in the root app-data dir
/// (GUI apps do not inherit shell env). Loopback only; a non-loopback value is ignored.
///
/// Compiled in only with the non-default `local-relay` feature; shipping builds never read
/// the env var or the file.
#[cfg(feature = "local-relay")]
pub fn relay_override_url() -> Option<String> {
    let from_file = || {
        let path = dirs::data_dir()?.join("com.syogun.shogunai").join("relay_override.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        v.get("base_url")?.as_str().map(str::to_string)
    };
    let raw = std::env::var("SHOGUN_RELAY_BASE_URL").ok().or_else(from_file)?;
    let checked = loopback_relay_url(&raw);
    if checked.is_none() {
        eprintln!("relay override ignored: not a loopback URL");
    }
    checked
}

#[cfg(not(feature = "local-relay"))]
pub fn relay_override_url() -> Option<String> {
    None
}

/// The *intent* the device sends instead of a model id (§4.4). The relay maps each class to a
/// concrete model server-side, so model choice — and therefore unit cost — is an operator
/// decision, not a client capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelClass {
    /// Per-event labelling (Dream Cycle consolidation / indexing).
    Classify,
    /// Abstractive summarisation (Compression).
    Summarize,
    /// Morning Brief generation.
    Brief,
    /// Grounded About You profile generation.
    AboutProfile,
    /// Grounded daily activity summary generation.
    DailyActivitySummary,
}

impl ModelClass {
    /// The wire string in the `model_class` field.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelClass::Classify => "classify",
            ModelClass::Summarize => "summarize",
            ModelClass::Brief => "brief",
            ModelClass::AboutProfile => "about_profile",
            ModelClass::DailyActivitySummary => "daily_activity_summary",
        }
    }
}

/// Relay connection settings. Deliberately has **no model and no max_tokens field** — those are
/// the relay's decisions (§4.4). Compare [`super::anthropic::AnthropicConfig`], which must carry
/// both because it talks to Anthropic directly.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Base URL, no trailing slash (e.g. `https://api.shogunaios.com`).
    pub base_url: String,
    /// The intent sent as `model_class`.
    pub model_class: ModelClass,
}

impl RelayConfig {
    /// Config for the default relay host.
    pub fn new(model_class: ModelClass) -> Self {
        let base_url = relay_override_url().unwrap_or_else(|| DEFAULT_RELAY_BASE_URL.to_string());
        Self { base_url, model_class }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        // Normalise: no trailing slash so URL joins are simple.
        while self.base_url.ends_with('/') {
            self.base_url.pop();
        }
        self
    }

    /// The destination host recorded in traceability (scheme stripped, no path).
    fn destination(&self) -> String {
        self.base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    }

    /// Standard headers, carrying the license token as `Authorization: Bearer …` (§4.1 — the
    /// FR-BIL-08 license JWT, never an Anthropic key). `expose()` here is the single egress
    /// point, mirroring the `x-api-key` builder in `anthropic.rs`; the transport's `Debug`
    /// redacts the `authorization` header, so the token cannot leak through a captured request.
    fn headers(&self, token: &Secret) -> Vec<(String, String)> {
        vec![
            ("authorization".to_string(), format!("Bearer {}", token.expose())),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    }
}

/// Build the `POST /v1/batch` create request (§4.2). The body carries `purpose` (traceability
/// vocabulary, taken from the items), `model_class` (the intent — never a model id), and the
/// items. There is deliberately no way to put a `model` key — or a `cache_control` — in this
/// body: both change unit cost, so both are relay decisions (§4.4).
///
/// Two item shapes (#667):
/// - no shared instruction → items are `{custom_id, chunk}`, each chunk a self-contained prompt
///   (the original contract, still used by every lane but Dream consolidation);
/// - a shared instruction → it rides **once** as top-level `system`, and items are
///   `{custom_id, data}` where `data` is only the per-item user half. The field is named
///   `data`, not `chunk`, on purpose: a relay that predates `system` rejects the body with 400
///   ("each item needs a chunk") before reserving or calling upstream, instead of silently
///   forwarding instruction-less data and billing a night of unparseable results.
pub fn build_relay_submit_request(
    cfg: &RelayConfig,
    token: &Secret,
    items: &[BatchItem],
    client_request_id: Option<&str>,
) -> Result<HttpRequest, LlmError> {
    let purpose = items.first().map(|it| it.purpose.as_str()).unwrap_or("batch");
    let system = shared_batch_system(items)?;
    let item_key = if system.is_some() { "data" } else { "chunk" };
    let wire_items: Vec<Value> = items
        .iter()
        .map(|it| {
            let mut item = serde_json::Map::new();
            item.insert("custom_id".into(), json!(it.custom_id));
            item.insert(item_key.into(), json!(it.chunk));
            Value::Object(item)
        })
        .collect();
    let mut body = json!({
        "purpose": purpose,
        "model_class": cfg.model_class.as_str(),
        "items": wire_items,
    });
    if let Some(system) = system {
        body["system"] = json!(system);
    }
    // Every relay caller must persist its attempt before egress. A payload hash alone
    // would replay an expired job forever; a random fallback would duplicate unknown work.
    let id = client_request_id.ok_or_else(|| LlmError::Provider("relay submission requires a durable attempt identity".into()))?;
    body["client_request_id"] = json!(id);
    Ok(HttpRequest::new(
        Method::Post,
        format!("{}/v1/batch", cfg.base_url),
        cfg.headers(token),
        Some(body.to_string()),
    )?)
}

/// Build the `POST /v1/review` request: one synchronous model call for one item. The relay picks
/// the model and the output shape from `purpose`, like `/v1/batch`.
pub fn build_relay_review_request(cfg: &RelayConfig, token: &Secret, item: &BatchItem) -> Result<HttpRequest, LlmError> {
    let body = json!({
        "purpose": item.purpose,
        "system": item.system.as_deref().unwrap_or(""),
        "user": item.chunk,
    });
    let mut req = HttpRequest::new(Method::Post, format!("{}/v1/review", cfg.base_url), cfg.headers(token), Some(body.to_string()))?;
    req.timeout = Some(REVIEW_TIMEOUT);
    Ok(req)
}

/// The relay's synchronous review answer: the model's JSON (as text, for the shared result
/// parser) and the provider-reported usage.
pub fn parse_relay_review(body: &str) -> Result<(String, Usage), LlmError> {
    let v: Value = serde_json::from_str(body).map_err(|e| LlmError::Parse(format!("relay review: {e}")))?;
    let json = v.get("json").filter(|j| j.is_object()).ok_or_else(|| LlmError::Parse("relay review: no json".into()))?;
    let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
    let usage = Usage { input_tokens: n("input_tokens"), output_tokens: n("output_tokens"), cache_read_tokens: n("cache_read_input_tokens"), cache_write_tokens: 0, estimated: false };
    Ok((json.to_string(), usage))
}

/// Per-call ceiling: the relay makes up to two 90 s model calls (one retry on an empty answer).
const REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(200);
/// How long one review call may wait out the relay's per-minute rate limit in total.
const REVIEW_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// A failed review is always retryable (the window stays pending); the dream driver keys on this.
pub const REVIEW_FAILED: &str = "review failed";
/// The relay said the model refused this content or could not answer it usably (HTTP 422
/// `upstream_refused` / `upstream_unusable`). The only relay answer that is about the content, so
/// the only one the dream driver may count toward abandoning a window; every other non-success is
/// an outage.
pub const REVIEW_REFUSED: &str = "review refused";

/// Build the `GET /v1/batch/{id}` request (§4.3). One endpoint serves both status and, once
/// ended, the results.
pub fn build_relay_status_request(
    cfg: &RelayConfig,
    token: &Secret,
    batch_id: &str,
) -> Result<HttpRequest, LlmError> {
    Ok(HttpRequest::new(
        Method::Get,
        format!("{}/v1/batch/{}", cfg.base_url, batch_id),
        cfg.headers(token),
        None,
    )?)
}

/// Parse the `202 Accepted` create response (`{"batch_id": "rb_…", "accepted": n}`) into a
/// handle. A freshly accepted batch is in progress by definition.
pub fn parse_relay_submit(body: &str) -> Result<BatchHandle, LlmError> {
    let v: Value = serde_json::from_str(body).map_err(|e| LlmError::Parse(e.to_string()))?;
    let id = v
        .get("batch_id")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Parse("relay create response missing batch_id".into()))?;
    Ok(BatchHandle { id: id.to_string(), status: BatchStatus::InProgress })
}

/// Parse the status half of a `GET /v1/batch/{id}` response. The relay echoes no id, so the
/// caller supplies the one it asked about.
pub fn parse_relay_status(batch_id: &str, body: &str) -> Result<BatchHandle, LlmError> {
    let v: Value = serde_json::from_str(body).map_err(|e| LlmError::Parse(e.to_string()))?;
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Parse("relay status response missing status".into()))?;
    let status = match status {
        "in_progress" => BatchStatus::InProgress,
        "ended" => BatchStatus::Ended,
        other => BatchStatus::Other(other.to_string()),
    };
    Ok(BatchHandle { id: batch_id.to_string(), status })
}

/// Parse the results half of an ended `GET /v1/batch/{id}` response:
/// `{"status":"ended","results":[{"custom_id":"…","text":"…"}]}`. Results are keyed by
/// `custom_id` and may arrive in any order — the same contract `parse_batch_classification`
/// already reads (§4.3). A result carrying `error` instead of `text` surfaces as an error entry,
/// not a silent drop.
pub fn parse_relay_results(body: &str) -> Result<Vec<BatchResult>, LlmError> {
    let v: Value = serde_json::from_str(body).map_err(|e| LlmError::Parse(e.to_string()))?;
    if v.get("status").and_then(Value::as_str) != Some("ended") {
        return Err(LlmError::Provider("relay batch has not ended; no results yet".into()));
    }
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Parse("relay ended response missing results".into()))?;
    Ok(results
        .iter()
        .map(|r| {
            let custom_id = r.get("custom_id").and_then(Value::as_str).unwrap_or("").to_string();
            let text = r.get("text").and_then(Value::as_str).map(str::to_string);
            let error = r.get("error").and_then(Value::as_str).map(str::to_string);
            match (text, error) {
                (Some(text), _) => BatchResult { custom_id, text: Some(text), error: None },
                (None, Some(err)) => BatchResult { custom_id, text: None, error: Some(err) },
                (None, None) => BatchResult {
                    custom_id,
                    text: None,
                    error: Some("missing text and error".into()),
                },
            }
        })
        .collect())
}

/// The provider-reported usage the relay passes through on each succeeded result (counts only:
/// `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`),
/// summed over the batch. `None` when no result carries one — a relay that predates the
/// passthrough — and the caller falls back to the byte-length estimate.
pub fn parse_relay_results_usage(body: &str) -> Option<Usage> {
    let v: Value = serde_json::from_str(body).ok()?;
    let mut acc = Usage::default();
    let mut seen = false;
    for r in v.get("results")?.as_array()? {
        if let Some(u) = super::usage::anthropic_usage(r) {
            acc.input_tokens += u.input_tokens;
            acc.output_tokens += u.output_tokens;
            acc.cache_read_tokens += u.cache_read_tokens;
            acc.cache_write_tokens += u.cache_write_tokens;
            seen = true;
        }
    }
    seen.then_some(acc)
}

/// In-flight `submission unknown` and `metering unavailable` are the only create failures
/// the client retries, and only with the same body (#343). An orphan claim that stays
/// unknown is still returned after this budget; Dream must not mint a new id for it.
const CREATE_RETRY_LIMIT: u32 = 2;

/// The longest in-line wait a create retry will honour from `Retry-After`. The create path holds
/// a lease and a runtime thread; a longer interval is the poll loop's job, not an inline sleep.
const CREATE_RETRY_AFTER_CAP: std::time::Duration = std::time::Duration::from_secs(30);

fn create_retry_delay(
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    if status != 503 {
        return None;
    }
    let fallback = if body.contains("submission unknown") {
        // The relay's documented interval for an in-flight claim.
        std::time::Duration::from_secs(1)
    } else if body.contains("metering unavailable") {
        std::time::Duration::from_millis(200)
    } else {
        return None;
    };
    // FR-SVC-02: the server's own `Retry-After` wins when it sent one.
    Some(retry_after.map_or(fallback, |wait| wait.min(CREATE_RETRY_AFTER_CAP)))
}

/// A failed relay call, mapped per §4.5. 401/403 → `Unauthorized` (re-verify the license),
/// 429 → `RateLimited` (daily cap reached — tonight runs the local lane), 402 →
/// `QuotaExhausted` (plan does not cover the Batch lane — not a bug, not the user's key), and
/// anything else stays a retryable `Provider` failure.
fn relay_status_error(step: &str, status: u16, body: &str) -> LlmError {
    if status == 402 {
        return LlmError::QuotaExhausted(
            "the current plan does not include the Batch lane (HTTP 402)".into(),
        );
    }
    crate::llm::status_error(&format!("relay {step}"), status, body)
}

/// How a [`RelayBatchClient`] gets a new bearer after the relay refused the one it presented
/// (HTTP 401: an expired or unknown token — issue #777). The desktop re-verifies the licence (or
/// re-asks for the trial's relay token) and answers with the new bearer, or `None` when it has
/// nothing newer than the token that was refused. A future, not a blocking call: the refresh is
/// itself a network round trip and must not stall the runtime the Batch lane runs on.
pub type BearerRefresh = std::sync::Arc<dyn Fn() -> BearerFuture + Send + Sync>;

/// What a [`BearerRefresh`] returns.
pub type BearerFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<LicenseBearer>> + Send>>;

/// Batch-lane client that goes through the Select-operated relay. Constructed with a
/// [`LicenseBearer`] — the FR-BIL-08 licence token, never an Anthropic key: the type makes
/// wiring a raw `sk-ant-` credential into this path (and so sending the operator key to a
/// non-Anthropic host) a compile error, the same way invariant 5 separates the lanes. Same
/// submit / poll / results lifecycle as the direct client, via [`BatchLane`].
pub struct RelayBatchClient<T: HttpTransport, S: TraceabilitySink> {
    transport: T,
    sink: S,
    /// The bearer presented on every call. Behind a lock because a 401 can swap in a refreshed
    /// one mid-run (issue #777, [`RelayBatchClient::with_bearer_refresh`]).
    token: std::sync::Mutex<LicenseBearer>,
    /// Asked for a fresh bearer when the relay answers 401 — at most once per client.
    refresh: Option<BearerRefresh>,
    refresh_used: std::sync::atomic::AtomicBool,
    cfg: RelayConfig,
    usage: Option<UsageHook>,
    /// Byte length of the last submitted batch, for the estimated usage row written when its
    /// results land (the relay wire reports no token counts). One client serves one batch in
    /// every call path (dream / recap construct a fresh client per run), so a single slot is
    /// enough.
    submitted_bytes: std::sync::atomic::AtomicUsize,
    /// How the relay last said "not now", if its last answer was a throttle (issue #29
    /// FR-SVC-02). Read by the poll loop through [`BatchLane::last_throttle`] so it can honour a
    /// `Retry-After` instead of re-asking on its own schedule. Same one-client-one-batch reasoning
    /// as `submitted_bytes`.
    throttle: std::sync::Mutex<Option<SharedThrottle>>,
}

impl<T: HttpTransport, S: TraceabilitySink> RelayBatchClient<T, S> {
    pub fn new(transport: T, sink: S, token: LicenseBearer, cfg: RelayConfig) -> Self {
        Self {
            transport,
            sink,
            token: std::sync::Mutex::new(token),
            refresh: None,
            refresh_used: std::sync::atomic::AtomicBool::new(false),
            cfg,
            usage: None,
            submitted_bytes: std::sync::atomic::AtomicUsize::new(0),
            throttle: std::sync::Mutex::new(None),
        }
    }

    /// On a 401 from the relay, ask `refresh` for a new bearer and resend that request once
    /// (issue #777). One refresh per client: a run whose refreshed token is refused too stops
    /// with `Unauthorized` and falls back to the local lane.
    pub fn with_bearer_refresh(mut self, refresh: BearerRefresh) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// The bearer to present now (the refreshed one after a successful refresh).
    fn bearer(&self) -> LicenseBearer {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A new bearer after a 401, or `None`: no refresh configured, this client already used its
    /// one refresh, or the refresh had nothing newer.
    async fn refreshed_bearer(&self) -> Option<LicenseBearer> {
        let refresh = self.refresh.as_ref()?;
        if self.refresh_used.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let fresh = refresh().await?;
        *self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh.clone();
        Some(fresh)
    }

    /// One authenticated exchange: `build` the request with the current bearer and send it; on a
    /// 401, rebuild it with a refreshed bearer and send it once more. `retry_purpose` records the
    /// resend on the traceability screen when it carries chunk content again (a create).
    async fn send_authed<B>(
        &self,
        build: B,
        retry_purpose: Option<&str>,
    ) -> Result<HttpResponse, LlmError>
    where
        B: Fn(&Secret) -> Result<HttpRequest, LlmError>,
    {
        let bearer = self.bearer();
        let resp = self.exchange(build(bearer.secret())?).await?;
        if resp.status != 401 {
            return Ok(resp);
        }
        let Some(fresh) = self.refreshed_bearer().await else {
            return Ok(resp);
        };
        if let Some(purpose) = retry_purpose {
            self.sink.record(TraceRecord::marker(
                Route::BatchRelay,
                purpose,
                self.cfg.destination(),
                TraceKind::Retry,
            ));
        }
        self.exchange(build(fresh.secret())?).await
    }

    /// One HTTP exchange with the relay — the only place this client touches the transport. It
    /// times the round trip for the `shogun metrics` shared-service line (issue #29 §4-6; route,
    /// milliseconds and status only) and remembers whether the answer was a throttle.
    async fn exchange(&self, req: HttpRequest) -> Result<HttpResponse, LlmError> {
        let started = std::time::Instant::now();
        let sent = self.transport.send(req).await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = sent.as_ref().map_or(0, |resp| resp.status);
        self.sink.round_trip(RoundTrip { route: Route::BatchRelay, ms, status });
        let throttle = sent
            .as_ref()
            .ok()
            .filter(|resp| !resp.is_success())
            .and_then(|resp| classify_shared_throttle(resp.status, &resp.body, resp.retry_after));
        if let Ok(mut slot) = self.throttle.lock() {
            *slot = throttle;
        }
        Ok(sent?)
    }

    /// The throttle the relay's last answer carried, if any.
    pub fn last_throttle(&self) -> Option<SharedThrottle> {
        self.throttle.lock().ok().and_then(|slot| *slot)
    }

    /// Attach usage accounting (FR-HN-01 / #313). A relay that passes the provider's per-result
    /// token counts through (#667, §4.3) yields exact rows, prompt-cache read/write included;
    /// an older relay's wire carries none, and rows on it are byte-length **estimates**, marked
    /// as such. The `model` column carries the `model_class` intent — the device never knows
    /// the concrete model (§4.4).
    pub fn with_usage(mut self, hook: UsageHook) -> Self {
        self.usage = Some(hook);
        self
    }

    fn record_usage(&self, usage: Usage, total_ms: u64, outcome: UsageOutcome) {
        if let Some(hook) = &self.usage {
            hook.sink.record(UsageRecord::new(
                Route::BatchRelay,
                hook.tag,
                self.cfg.model_class.as_str(),
                usage,
                None,
                total_ms,
                outcome,
            ));
        }
    }

    /// Create a batch from `items`. Each item's traceability row carries [`Route::BatchRelay`] so
    /// the viewer can say "via operator server" (§3.3).
    pub async fn submit(
        &self,
        items: &[BatchItem],
        client_request_id: Option<&str>,
    ) -> Result<BatchHandle, LlmError> {
        let build =
            |token: &Secret| build_relay_submit_request(&self.cfg, token, items, client_request_id);
        // Refuse a malformed batch before anything is recorded as sent.
        build(self.bearer().secret())?;
        let purpose = items.first().map_or("batch", |it| it.purpose.as_str());
        let dest = self.cfg.destination();
        for it in items {
            // Fail-closed: a batch whose rows the ledger would not take is not submitted.
            self.sink.try_record(TraceRecord::for_chunk(
                Route::BatchRelay,
                it.purpose.clone(),
                dest.clone(),
                &it.chunk,
                false,
            ))?;
        }
        let started = std::time::Instant::now();
        let result = async {
            let mut attempt = 0u32;
            let mut submission_unknown = false;
            loop {
                let resp = self.send_authed(&build, Some(purpose)).await?;
                if resp.is_success() {
                    return parse_relay_submit(&resp.body);
                }
                submission_unknown |= resp.status == 503 && resp.body.contains("submission unknown");
                if attempt < CREATE_RETRY_LIMIT {
                    let delay = create_retry_delay(resp.status, &resp.body, resp.retry_after);
                    if let Some(delay) = delay {
                        attempt += 1;
                        // Issue #29 §5: the traceability screen shows the relay asked us to wait.
                        self.sink.record(TraceRecord::marker(
                            Route::BatchRelay,
                            purpose,
                            dest.clone(),
                            TraceKind::Retry,
                        ));
                        // Spread the wait: every 503 asks for the same `Retry-After: 1`, and
                        // a fleet retrying at exactly +1 s re-forms the burst (PR #753).
                        let spread = super::retry::spread_retry_after(delay, super::retry::jitter_unit());
                        tokio::time::sleep(spread.min(CREATE_RETRY_AFTER_CAP)).await;
                        continue;
                    }
                }
                // A later refusal says nothing about the earlier in-flight create. Keep the
                // original request ID recoverable instead of treating this as a safe rejection.
                if submission_unknown {
                    return Err(LlmError::Provider("relay create submission unknown".into()));
                }
                return Err(relay_status_error("create", resp.status, &resp.body));
            }
        }
        .await;
        match &result {
            // Success is metered when the results land (`results`), where the output side is
            // known too; remember the input size for that row.
            Ok(_) => {
                // Per item, shared half included: it is billed on every item (#667).
                let bytes: usize = items.iter().map(BatchItem::input_bytes).sum();
                self.submitted_bytes.store(bytes, std::sync::atomic::Ordering::Relaxed);
            }
            // A refused submit (401/402/429/5xx) still deserves a row — that is exactly the
            // failure class the meter must make visible.
            Err(e) => self.record_usage(
                Usage::default(),
                started.elapsed().as_millis() as u64,
                UsageOutcome::from_error(e),
            ),
        }
        result
    }

    /// Recover by the original owner/device/purpose/id without resending captured content.
    pub async fn recover_submission(
        &self,
        purpose: &str,
        client_request_id: &str,
    ) -> Result<Option<BatchHandle>, LlmError> {
        let body = json!({ "purpose": purpose, "client_request_id": client_request_id }).to_string();
        let build = |token: &Secret| -> Result<HttpRequest, LlmError> {
            Ok(HttpRequest::new(
                Method::Post,
                format!("{}/v1/batch/recover", self.cfg.base_url),
                self.cfg.headers(token),
                Some(body.clone()),
            )?)
        };
        let resp = self.send_authed(build, None).await?;
        if !resp.is_success() {
            return Err(relay_status_error("recover", resp.status, &resp.body));
        }
        let body: Value = serde_json::from_str(&resp.body).map_err(|e| LlmError::Parse(e.to_string()))?;
        match body.get("found").and_then(Value::as_bool) {
            Some(false) => Ok(None),
            Some(true) => parse_relay_submit(&resp.body).map(Some),
            None => Err(LlmError::Parse("relay recovery missing found".into())),
        }
    }

    /// Retire only before provider reservation; an in-flight or bound job cannot be retired.
    pub async fn retire_submission(&self, purpose: &str, client_request_id: &str) -> Result<bool, LlmError> {
        let body = json!({ "purpose": purpose, "client_request_id": client_request_id, "retire_if_missing": true }).to_string();
        let build = |token: &Secret| -> Result<HttpRequest, LlmError> {
            Ok(HttpRequest::new(
                Method::Post,
                format!("{}/v1/batch/recover", self.cfg.base_url),
                self.cfg.headers(token),
                Some(body.clone()),
            )?)
        };
        let resp = self.send_authed(build, None).await?;
        if !resp.is_success() { return Err(relay_status_error("retire", resp.status, &resp.body)); }
        let body: Value = serde_json::from_str(&resp.body).map_err(|e| LlmError::Parse(e.to_string()))?;
        Ok(body.get("found").and_then(Value::as_bool) == Some(false)
            && body.get("retired").and_then(Value::as_bool) == Some(true))
    }

    /// Poll a batch's status. Callers loop on their own cadence until
    /// [`BatchStatus::is_ended`].
    pub async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
        let resp = self
            .send_authed(|token: &Secret| build_relay_status_request(&self.cfg, token, batch_id), None)
            .await?;
        if !resp.is_success() {
            return Err(relay_status_error("poll", resp.status, &resp.body));
        }
        parse_relay_status(batch_id, &resp.body)
    }

    /// Fetch results once the batch has ended (same endpoint as poll; the ended response carries
    /// them inline). Keyed by `custom_id` (any order).
    pub async fn results(&self, batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
        let started = std::time::Instant::now();
        let resp = self
            .send_authed(|token: &Secret| build_relay_status_request(&self.cfg, token, batch_id), None)
            .await?;
        if !resp.is_success() {
            return Err(relay_status_error("results", resp.status, &resp.body));
        }
        let results = parse_relay_results(&resp.body)?;
        // One aggregate row per fetched batch. Provider-reported counts (incl. prompt-cache
        // read/write, #667) when the relay passes them through; otherwise an estimate (input
        // remembered at submit, output from the returned texts). Recorded here — not in `run` —
        // so the generic `run_batch_to_completion` path the Dream Cycle uses is metered too.
        let input = self.submitted_bytes.swap(0, std::sync::atomic::Ordering::Relaxed);
        let usage = parse_relay_results_usage(&resp.body).unwrap_or_else(|| {
            let output: usize =
                results.iter().filter_map(|r| r.text.as_deref()).map(str::len).sum();
            Usage::estimated_from_bytes(input, output)
        });
        self.record_usage(usage, started.elapsed().as_millis() as u64, UsageOutcome::Ok);
        Ok(results)
    }

    /// Hourly review: one plain synchronous call per item, in order. No job, no id, no poll. Any
    /// failure aborts the whole window so it retries (it is never half-applied: results are only
    /// returned when every call succeeded). 401 refreshes the bearer once like every relay call.
    pub async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
        let mut out = Vec::with_capacity(items.len());
        let mut total = Usage::default();
        let started = std::time::Instant::now();
        // Four calls in flight at a time; results stay in item order.
        let mut failed = None;
        for group in items.chunks(4) {
            for r in self.review_group(group).await {
                match r {
                    Ok((result, usage)) => {
                        total.input_tokens += usage.input_tokens;
                        total.output_tokens += usage.output_tokens;
                        total.cache_read_tokens += usage.cache_read_tokens;
                        out.push(result);
                    }
                    Err(e) => failed = failed.or(Some(e)),
                }
            }
            if failed.is_some() {
                break;
            }
        }
        // The calls that did answer were paid for, so they are metered even when the window fails.
        let outcome = if failed.is_some() { UsageOutcome::Error } else { UsageOutcome::Ok };
        self.record_usage(total, started.elapsed().as_millis() as u64, outcome);
        match failed {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    #[cfg(feature = "net")]
    async fn review_group(&self, group: &[BatchItem]) -> Vec<Result<(BatchResult, Usage), LlmError>> {
        let mut it = group.iter();
        let (a, b, c, d) = (it.next(), it.next(), it.next(), it.next());
        let done = futures_util::future::join4(self.review_one(a), self.review_one(b), self.review_one(c), self.review_one(d)).await;
        [done.0, done.1, done.2, done.3].into_iter().flatten().collect()
    }

    #[cfg(not(feature = "net"))]
    async fn review_group(&self, group: &[BatchItem]) -> Vec<Result<(BatchResult, Usage), LlmError>> {
        let mut out = Vec::new();
        for it in group {
            out.extend(self.review_one(Some(it)).await);
        }
        out
    }

    async fn review_one(&self, item: Option<&BatchItem>) -> Option<Result<(BatchResult, Usage), LlmError>> {
        let it = item?;
        Some(async {
            // Fail-closed traceability row at the true egress point (invariant 3).
            self.sink.try_record(TraceRecord::for_chunk(Route::BatchRelay, it.purpose.clone(), self.cfg.destination(), &it.chunk, false))?;
            // A per-minute rate limit answers 429 with Retry-After (the daily cap has none): wait
            // it out, jittered so parallel calls do not wake together, within a total budget,
            // instead of failing the whole window. An unreachable relay stays a transport error
            // (an outage, retried on backoff), not a review failure.
            let mut waited = std::time::Duration::ZERO;
            let resp = loop {
                let resp = self
                    .send_authed(|token: &Secret| build_relay_review_request(&self.cfg, token, it), Some(&it.purpose))
                    .await
                    .map_err(|e| match e {
                        LlmError::Transport(_) => e,
                        e => LlmError::Provider(format!("{REVIEW_FAILED}: {e}")),
                    })?;
                match resp.retry_after {
                    // 503 + Retry-After is the relay's "metering unavailable" blip: same wait.
                    Some(wait) if matches!(resp.status, 429 | 503) && waited < REVIEW_WAIT_BUDGET => {
                        let wait = crate::llm::retry::spread_retry_after(
                            wait.clamp(std::time::Duration::from_secs(1), std::time::Duration::from_secs(30)),
                            crate::llm::retry::jitter_unit(),
                        );
                        waited += wait;
                        tokio::time::sleep(wait).await;
                    }
                    _ => break resp,
                }
            };
            if resp.status == 422 && ["upstream_refused", "upstream_unusable"].iter().any(|r| resp.body.contains(r)) {
                return Err(LlmError::Provider(format!("{REVIEW_REFUSED}: relay review HTTP 422")));
            }
            if !resp.is_success() {
                return Err(match relay_status_error("review", resp.status, &resp.body) {
                    e @ (LlmError::Unauthorized(..) | LlmError::QuotaExhausted(_) | LlmError::RateLimited(..)) => e,
                    e => LlmError::Provider(format!("{REVIEW_FAILED}: {e}")),
                });
            }
            let (text, usage) = parse_relay_review(&resp.body).map_err(|e| LlmError::Provider(format!("{REVIEW_FAILED}: {e}")))?;
            Ok((BatchResult { custom_id: it.custom_id.clone(), text: Some(text), error: None }, usage))
        }.await)
    }

    /// Run an explicitly identified attempt. Production callers persist the identity first.
    pub async fn run<F, Fut>(&self, items: &[BatchItem], request_id: &str, max_polls: u32, mut sleep: F) -> Result<Vec<BatchResult>, LlmError>
    where F: FnMut() -> Fut, Fut: std::future::Future<Output = ()> {
        let handle = self.submit(items, Some(request_id)).await?;
        self.finish_run(handle, max_polls, &mut sleep).await
    }

    async fn finish_run<F, Fut>(&self, mut handle: BatchHandle, max_polls: u32, sleep: &mut F) -> Result<Vec<BatchResult>, LlmError>
    where F: FnMut() -> Fut, Fut: std::future::Future<Output = ()> {
        for _ in 0..max_polls {
            if handle.status.is_ended() { break; }
            sleep().await;
            handle = self.poll(&handle.id).await?;
        }
        if !handle.status.is_ended() { return Err(LlmError::Provider("batch did not end within the poll budget".into())); }
        self.results(&handle.id).await
    }

    /// Persist before egress and recover the same attempt after a crash or an uncertain reply.
    /// Repeated invocations resume unfinished work; a new invocation after terminal results
    /// receives a fresh identity even when its payload is identical.
    #[cfg(feature = "db")]
    pub async fn run_durable<F, Fut>(
        &self,
        db: &crate::daemon::Db,
        items: &[BatchItem],
        max_polls: u32,
        mut sleep: F,
    ) -> Result<Vec<BatchResult>, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use sha2::{Digest, Sha256};
        let bearer = self.bearer();
        let request =
            build_relay_submit_request(&self.cfg, bearer.secret(), items, Some("fingerprint"))?;
        let mut body: Value = serde_json::from_str(request.body.as_deref().unwrap_or_default())
            .map_err(|e| LlmError::Parse(e.to_string()))?;
        body.as_object_mut()
            .ok_or_else(|| LlmError::Parse("relay body must be an object".into()))?
            .remove("client_request_id");
        let fingerprint = format!("{:x}", Sha256::digest(body.to_string().as_bytes()));
        let purpose = items.first().map_or("batch", |item| item.purpose.as_str());
        let (request_id, provider_id) =
            db.relay_attempt(&fingerprint).map_err(LlmError::Provider)?;
        let mut handle = match provider_id {
            Some(id) => self.poll(&id).await?,
            None => match self.recover_submission(purpose, &request_id).await? {
                Some(handle) => handle,
                None => self.submit(items, Some(&request_id)).await?,
            },
        };
        db.relay_attempt_bind(&fingerprint, &request_id, &handle.id)
            .map_err(LlmError::Provider)?;
        for _ in 0..max_polls {
            if handle.status.is_ended() {
                break;
            }
            sleep().await;
            handle = self.poll(&handle.id).await?;
        }
        if !handle.status.is_ended() {
            return Err(LlmError::Provider(
                "batch did not end within the poll budget".into(),
            ));
        }
        // Failed retrieval keeps this handle: a network error is not proof the results are lost.
        let results = self.results(&handle.id).await?;
        db.relay_attempt_finish(&fingerprint, &request_id)
            .map_err(LlmError::Provider)?;
        Ok(results)
    }
}

impl<T: HttpTransport, S: TraceabilitySink> BatchLane for RelayBatchClient<T, S> {
    fn supports_recovery(&self) -> bool { true }

    fn last_throttle(&self) -> Option<SharedThrottle> {
        RelayBatchClient::last_throttle(self)
    }

    fn shared_service(&self) -> Option<(Route, String)> {
        Some((Route::BatchRelay, self.cfg.destination()))
    }

    fn submit(
        &self,
        items: &[BatchItem],
        client_request_id: Option<&str>,
    ) -> impl std::future::Future<Output = Result<BatchHandle, LlmError>> + Send {
        RelayBatchClient::submit(self, items, client_request_id)
    }

    fn review(&self, items: &[BatchItem]) -> impl std::future::Future<Output = Result<Vec<BatchResult>, LlmError>> + Send {
        RelayBatchClient::review(self, items)
    }

    fn synchronous_review(&self) -> bool {
        true
    }

    fn recover_submission(
        &self,
        purpose: &str,
        client_request_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<BatchHandle>, LlmError>> + Send {
        RelayBatchClient::recover_submission(self, purpose, client_request_id)
    }

    fn retire_submission(&self, purpose: &str, client_request_id: &str) -> impl std::future::Future<Output = Result<bool, LlmError>> + Send {
        RelayBatchClient::retire_submission(self, purpose, client_request_id)
    }

    fn poll(
        &self,
        batch_id: &str,
    ) -> impl std::future::Future<Output = Result<BatchHandle, LlmError>> + Send {
        RelayBatchClient::poll(self, batch_id)
    }

    fn results(
        &self,
        batch_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<BatchResult>, LlmError>> + Send {
        RelayBatchClient::results(self, batch_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;
    use crate::llm::transport::{HttpResponse, MockTransport};

    fn cfg() -> RelayConfig {
        RelayConfig::new(ModelClass::Classify).with_base_url("https://api.shogunaios.com")
    }

    /// #334: the default is where a licence bearer token and chunk content actually go. Pinning
    /// it to a hostname under the domain SHOGUN operates is the check that catches the previous
    /// value — `relay.shogun.app`, a domain nobody here registered, parked by a registrar.
    #[test]
    fn loopback_override_accepts_only_loopback() {
        assert_eq!(loopback_relay_url("http://127.0.0.1:8787/").as_deref(), Some("http://127.0.0.1:8787"));
        assert!(loopback_relay_url("http://localhost:1").is_some());
        assert!(loopback_relay_url("http://[::1]:1").is_some());
        assert!(loopback_relay_url("https://api.shogunaios.com").is_none());
        assert!(loopback_relay_url("http://127.0.0.1.evil.com").is_none());
        assert!(loopback_relay_url("http://u:p@127.0.0.1").is_none());
        assert!(loopback_relay_url("ftp://127.0.0.1").is_none());
        assert!(loopback_relay_url("garbage").is_none());
    }

    #[test]
    fn default_relay_host_is_https_and_under_the_owned_domain() {
        let host = DEFAULT_RELAY_BASE_URL
            .strip_prefix("https://")
            .expect("relay base URL must be https");
        assert!(
            host == "shogunaios.com" || host.ends_with(".shogunaios.com"),
            "default relay host must be a domain SHOGUN controls, got {host}"
        );
    }

    fn client(
        responses: impl IntoIterator<Item = HttpResponse>,
    ) -> RelayBatchClient<MockTransport, RecordingSink> {
        RelayBatchClient::new(
            MockTransport::new(responses),
            RecordingSink::new(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            cfg(),
        )
    }

    fn items() -> Vec<BatchItem> {
        vec![
            BatchItem { custom_id: "1".into(), purpose: "consolidation".into(), chunk: "one".into(), system: None },
            BatchItem { custom_id: "2".into(), purpose: "consolidation".into(), chunk: "two".into(), system: None },
        ]
    }

    #[test]
    fn submit_request_carries_bearer_token_and_model_class_never_a_model_id() {
        let token = Secret::new("eyJ-license-jwt");
        let req = build_relay_submit_request(&cfg(), &token, &items(), Some("test-attempt")).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.url, "https://api.shogunaios.com/v1/batch");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer eyJ-license-jwt"));
        // The relay lane never sends an x-api-key — the device holds no Anthropic key.
        assert!(!req.headers.iter().any(|(k, _)| k == "x-api-key"));

        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["purpose"], "consolidation");
        assert_eq!(body["model_class"], "classify");
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["items"][0]["custom_id"], "1");
        assert_eq!(body["items"][1]["chunk"], "two");
        // §4.4: the device does not name a model — there must be no model key anywhere.
        assert!(body.get("model").is_none());
        assert!(!req.body.as_deref().unwrap().contains("\"model\":"));
        assert_eq!(body["client_request_id"], "test-attempt");
    }

    #[test]
    fn review_request_is_one_plain_call_and_the_answer_parses() {
        let item = BatchItem { custom_id: "s1".into(), purpose: "consolidation".into(), chunk: "data".into(), system: Some("sys".into()) };
        let req = build_relay_review_request(&cfg(), &Secret::new("tok"), &item).unwrap();
        assert_eq!(req.url, "https://api.shogunaios.com/v1/review");
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!((body["system"].as_str(), body["user"].as_str()), (Some("sys"), Some("data")));
        assert!(body.get("model").is_none() && body.get("client_request_id").is_none());
        let (text, usage) = parse_relay_review(r#"{"json":{"commitments":[]},"usage":{"input_tokens":5,"output_tokens":2}}"#).unwrap();
        assert_eq!(text, r#"{"commitments":[]}"#);
        assert_eq!((usage.input_tokens, usage.output_tokens), (5, 2));
        assert!(parse_relay_review(r#"{"usage":{}}"#).is_err());
    }

    /// Only the relay's explicit 422 refusal is about the content; a 400 (an old relay that does
    /// not know the request), a 404 or a 502 (the model provider down) is an outage.
    #[tokio::test]
    async fn only_an_explicit_refusal_is_a_content_failure() {
        let item = BatchItem { custom_id: "w".into(), purpose: "consolidation_window".into(), chunk: "d".into(), system: None };
        for (status, body, refused) in [
            (422, r#"{"error":"upstream_refused"}"#, true),
            (422, r#"{"error":"upstream_unusable"}"#, true),
            (400, r#"{"error":"purpose, system and user are required"}"#, false),
            (404, "", false),
            (502, r#"{"error":"upstream failure"}"#, false),
        ] {
            let err = client([HttpResponse { status, body: body.into(), retry_after: None }])
                .review(std::slice::from_ref(&item))
                .await
                .unwrap_err();
            let LlmError::Provider(message) = err else { panic!("{status}: {err:?}") };
            assert_eq!(message.starts_with(REVIEW_REFUSED), refused, "{status}: {message}");
            assert_eq!(message.starts_with(REVIEW_FAILED), !refused, "{status}: {message}");
        }
    }

    fn shared_items(system: &str) -> Vec<BatchItem> {
        let system: std::sync::Arc<str> = system.into();
        ["one", "two"]
            .iter()
            .enumerate()
            .map(|(i, data)| BatchItem {
                custom_id: (i + 1).to_string(),
                purpose: "consolidation".into(),
                chunk: (*data).into(),
                system: Some(system.clone()),
            })
            .collect()
    }

    /// #667: a shared instruction rides once, top-level, and the items carry only their data
    /// under `data` (never `chunk`, so a relay that predates `system` 400s instead of
    /// forwarding instruction-less text). The device never sends `cache_control` — whether and
    /// how to cache is a cost decision the relay makes for the model it picked (§4.4).
    #[test]
    fn a_shared_instruction_is_sent_once_and_items_carry_only_their_data() {
        let token = Secret::new("eyJ-license-jwt");
        let req =
            build_relay_submit_request(&cfg(), &token, &shared_items("RULES"), Some("test-attempt")).unwrap();
        let raw = req.body.as_deref().unwrap();
        let body: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(body["system"], "RULES");
        assert_eq!(raw.matches("RULES").count(), 1, "the shared half is not repeated per item");
        assert_eq!(body["items"][0], json!({ "custom_id": "1", "data": "one" }));
        assert_eq!(body["items"][1], json!({ "custom_id": "2", "data": "two" }));
        assert!(!raw.contains("\"chunk\""));
        assert!(!raw.contains("cache_control"));
        assert!(body.get("model").is_none());

        // Without a shared half the body is byte-for-byte the original contract.
        let legacy = build_relay_submit_request(&cfg(), &token, &items(), Some("test-attempt")).unwrap();
        let legacy: Value = serde_json::from_str(legacy.body.as_deref().unwrap()).unwrap();
        assert!(legacy.get("system").is_none());
        assert_eq!(legacy["items"][1], json!({ "custom_id": "2", "chunk": "two" }));
    }

    #[tokio::test]
    async fn items_that_disagree_on_the_shared_half_never_leave_the_device() {
        let c = client([]);
        let mut mixed = shared_items("RULES v1");
        mixed[1].system = Some("RULES v2".into());
        assert!(c.submit(&mixed, None).await.is_err());
        assert!(c.sink.records().is_empty(), "refused before any egress trace");
    }

    /// The relay passes the provider's per-result usage through (counts only). The row is then
    /// exact, cache read/write included — the only place a cache hit becomes visible (#667).
    #[tokio::test]
    async fn relay_reported_usage_is_recorded_exactly_including_cache_counts() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook};
        let usage_sink = std::sync::Arc::new(RecordingUsageSink::new());
        let ended = r#"{"status":"ended","results":[
            {"custom_id":"1","text":"a","usage":{"input_tokens":40,"output_tokens":7,"cache_read_input_tokens":700,"cache_creation_input_tokens":0}},
            {"custom_id":"2","text":"b","usage":{"input_tokens":35,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":700}},
            {"custom_id":"3","error":"errored"}]}"#;
        let c = RelayBatchClient::new(
            MockTransport::new([
                HttpResponse { status: 202, body: r#"{"batch_id":"rb_1","accepted":3}"#.into(), retry_after: None },
                HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
                HttpResponse { status: 200, body: ended.into(), retry_after: None },
            ]),
            RecordingSink::new(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            cfg(),
        )
        .with_usage(UsageHook::new(usage_sink.clone(), prompts::DREAM_CONSOLIDATION));
        c.run(&shared_items("RULES"), "test-attempt", 5, || async {})
            .await
            .unwrap();
        let recs = usage_sink.records();
        assert_eq!(recs.len(), 1);
        assert!(!recs[0].usage.estimated);
        assert_eq!(recs[0].usage.input_tokens, 75);
        assert_eq!(recs[0].usage.output_tokens, 12);
        assert_eq!(recs[0].usage.cache_read_tokens, 700);
        assert_eq!(recs[0].usage.cache_write_tokens, 700);
        assert_eq!(parse_relay_results_usage(r#"{"status":"ended","results":[{"custom_id":"1","text":"a"}]}"#), None);
    }

    #[test]
    fn missing_attempt_identity_is_rejected_before_egress() {
        assert!(build_relay_submit_request(&cfg(), &Secret::new("license"), &items(), None).is_err());
    }

    #[test]
    fn submit_request_includes_client_request_id_when_given() {
        let token = Secret::new("eyJ-license-jwt");
        let req =
            build_relay_submit_request(&cfg(), &token, &items(), Some("20260921")).unwrap();
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["client_request_id"], "20260921");
        assert!(body.get("model").is_none());
    }

    #[test]
    fn the_license_token_is_redacted_in_a_captured_request_debug() {
        let token = Secret::new("eyJ-license-jwt-super-secret");
        let req = build_relay_submit_request(&cfg(), &token, &items(), Some("test-attempt")).unwrap();
        let dumped = format!("{req:?}");
        assert!(!dumped.contains("super-secret"), "license token leaked via Debug: {dumped}");
    }

    #[test]
    fn parse_submit_reads_batch_id_and_is_in_progress() {
        let h = parse_relay_submit(r#"{"batch_id":"rb_01","accepted":812}"#).unwrap();
        assert_eq!(h.id, "rb_01");
        assert_eq!(h.status, BatchStatus::InProgress);
        assert!(parse_relay_submit(r#"{"accepted":1}"#).is_err()); // no batch_id
        assert!(parse_relay_submit("not json").is_err());
    }

    #[test]
    fn parse_status_maps_the_relay_vocabulary() {
        let h = parse_relay_status("rb_1", r#"{"status":"in_progress","completed":300,"total":812}"#)
            .unwrap();
        assert_eq!(h.id, "rb_1");
        assert!(!h.status.is_ended());
        let ended = parse_relay_status("rb_1", r#"{"status":"ended","results":[]}"#).unwrap();
        assert!(ended.status.is_ended());
        assert!(parse_relay_status("rb_1", r#"{"nope":true}"#).is_err());
    }

    #[test]
    fn parse_results_keys_by_custom_id_any_order_and_surfaces_errors() {
        let body = r#"{"status":"ended","results":[
            {"custom_id":"b","text":"B-label"},
            {"custom_id":"a","text":"A-label"},
            {"custom_id":"c","error":"errored"}
        ]}"#;
        let results = parse_relay_results(body).unwrap();
        assert_eq!(results.len(), 3);
        let by_id = |id: &str| results.iter().find(|r| r.custom_id == id).unwrap();
        assert_eq!(by_id("a").text.as_deref(), Some("A-label"));
        assert_eq!(by_id("b").text.as_deref(), Some("B-label"));
        assert!(by_id("c").text.is_none());
        assert_eq!(by_id("c").error.as_deref(), Some("errored"));
    }

    #[test]
    fn parse_results_refuses_a_batch_that_has_not_ended() {
        let err = parse_relay_results(r#"{"status":"in_progress","completed":1,"total":2}"#);
        assert!(matches!(err, Err(LlmError::Provider(_))));
    }

    #[tokio::test]
    async fn recovery_requires_explicit_absence_before_resubmitting() -> Result<(), LlmError> {
        let c = client([
            HttpResponse { status: 200, body: r#"{"found":true,"batch_id":"rb_original"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"found":false}"#.into(), retry_after: None },
            HttpResponse { status: 404, body: "old relay".into(), retry_after: None },
        ]);
        let recovered = c.recover_submission("consolidation", "night").await?;
        assert!(matches!(recovered, Some(handle) if handle.id == "rb_original"));
        assert!(c.recover_submission("consolidation", "night").await?.is_none());
        assert!(c.recover_submission("consolidation", "night").await.is_err());
        assert!(c.sink.records().is_empty(), "recovery sends no captured content");
        Ok(())
    }

    #[tokio::test]
    async fn retirement_requires_explicit_durable_proof_not_a_missing_snapshot() -> Result<(), LlmError> {
        let c = client([
            HttpResponse { status: 200, body: r#"{"found":false}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"found":false,"retired":true}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"found":true,"batch_id":"bound"}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: "submission unknown".into(), retry_after: None },
        ]);
        assert!(!c.retire_submission("mail_triage", "old").await?);
        assert!(c.retire_submission("mail_triage", "old").await?);
        assert!(!c.retire_submission("mail_triage", "old").await?);
        assert!(c.retire_submission("mail_triage", "old").await.is_err());
        for request in c.transport.sent() {
            let body: Value = serde_json::from_str(request.body.as_deref().unwrap_or_default()).map_err(|e| LlmError::Parse(e.to_string()))?;
            assert_eq!(body, json!({ "purpose": "mail_triage", "client_request_id": "old", "retire_if_missing": true }));
        }
        assert!(c.sink.records().is_empty());
        Ok(())
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn durable_run_recovers_unknown_then_allows_identical_terminal_retry() {
        use crate::daemon::Db;
        use std::sync::Arc;
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let c = client([
            HttpResponse { status: 200, body: r#"{"found":false}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: r#"{"error":"submission unknown"}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: r#"{"error":"submission unknown"}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: r#"{"error":"submission unknown"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"found":true,"batch_id":"original-job","accepted":2}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended","results":[]}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"found":false}"#.into(), retry_after: None },
            HttpResponse { status: 202, body: r#"{"batch_id":"retry-job","accepted":2}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended","results":[]}"#.into(), retry_after: None },
        ]);
        assert!(c.run_durable(&db, &items(), 1, || async {}).await.is_err());
        c.run_durable(&db, &items(), 1, || async {}).await.unwrap();
        c.run_durable(&db, &items(), 1, || async {}).await.unwrap();
        let requests = c.transport.sent();
        let bodies: Vec<Value> = requests.iter().filter_map(|r| r.body.as_deref()).map(|body| serde_json::from_str(body).unwrap()).collect();
        let ids: Vec<&str> = bodies.iter().map(|b| b["client_request_id"].as_str().unwrap()).collect();
        assert!(ids[..5].iter().all(|id| *id == ids[0]), "uncertain create and recovery use exactly the same attempt");
        assert_ne!(ids[0], ids[5], "confirmed terminal work permits new identity for identical content");
        assert_eq!(egress(&c.sink), 4, "only two provider create calls, not a replacement on recovery");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn durable_terminal_results_failure_reuses_bound_job_without_new_create() {
        use crate::daemon::Db;
        use std::sync::Arc;
        let db = Db::open_in_memory(Arc::new(|| 100)).unwrap();
        let c = client([
            HttpResponse { status: 200, body: r#"{"found":false}"#.into(), retry_after: None },
            HttpResponse { status: 202, body: r#"{"batch_id":"paid-job","accepted":2}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: "unavailable".into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended","results":[]}"#.into(), retry_after: None },
        ]);
        assert!(c.run_durable(&db, &items(), 1, || async {}).await.is_err());
        c.run_durable(&db, &items(), 1, || async {}).await.unwrap();
        let requests = c.transport.sent();
        assert_eq!(requests.iter().filter(|r| r.method == Method::Post && r.url.ends_with("/v1/batch")).count(), 1);
        assert!(requests[4].url.ends_with("/v1/batch/paid-job"));
        assert_eq!(c.sink.records().len(), 2);
    }

    #[tokio::test]
    async fn submit_records_one_batch_relay_trace_per_item() {
        let c = client([HttpResponse { status: 202, body: r#"{"batch_id":"rb_9","accepted":2}"#.into(), retry_after: None }]);
        let handle = c.submit(&items(), Some("test-attempt")).await.unwrap();
        assert_eq!(handle.id, "rb_9");
        let recs = c.sink.records();
        assert_eq!(recs.len(), 2);
        // §3.3: the relay route is distinguishable from the direct one.
        assert!(recs.iter().all(|r| r.route == Route::BatchRelay));
        assert!(recs.iter().all(|r| r.destination == "api.shogunaios.com"));
        // Digest-only, as everywhere (G8). `None` is the empty `url` field, not the chunk.
        assert!(!format!("{:?}", recs[0]).replace("None", "").contains("one"));
    }

    /// Same property the direct client tests pin down: a rejected credential must be
    /// distinguishable from a bad night (§4.5 — 401 falls back to the local lane, 5xx carries
    /// over).
    #[tokio::test]
    async fn a_rejected_license_is_distinguishable_from_an_outage() {
        for status in [401u16, 403] {
            let c = client([HttpResponse { status, body: "{}".into(), retry_after: None }]);
            assert!(
                matches!(c.submit(&items(), Some("test-attempt")).await, Err(LlmError::Unauthorized(s, _)) if s == status),
                "HTTP {status} is a credential problem"
            );
        }
        let c = client([HttpResponse { status: 503, body: "{}".into(), retry_after: None }]);
        assert!(matches!(c.submit(&items(), Some("test-attempt")).await, Err(LlmError::Provider(_))));
    }

    /// A refresh hook that answers `fresh` and counts its calls.
    fn refresh_to(
        fresh: Option<&'static str>,
    ) -> (BearerRefresh, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let hook: BearerRefresh = std::sync::Arc::new(move || -> BearerFuture {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let bearer = fresh.map(|t| LicenseBearer::new(Secret::new(t)));
            Box::pin(async move { bearer })
        });
        (hook, calls)
    }

    fn bearer_of(req: &HttpRequest) -> &str {
        req.headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    /// Issue #777: an expired token's 401 re-verifies and resends once with the new bearer, and
    /// every later call of the run presents the new one.
    #[tokio::test]
    async fn a_401_refreshes_the_bearer_and_resends_once() {
        let (hook, calls) = refresh_to(Some("v1.renewed-token-654321"));
        let c = client([
            HttpResponse { status: 401, body: "{}".into(), retry_after: None },
            HttpResponse { status: 202, body: r#"{"batch_id":"rb_1","accepted":2}"#.into(), retry_after: None },
            HttpResponse { status: 503, body: "{}".into(), retry_after: None },
        ])
        .with_bearer_refresh(hook);
        let handle = c.submit(&items(), Some("attempt-1")).await.unwrap();
        assert_eq!(handle.id, "rb_1");
        let _ = c.poll("rb_1").await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let sent = c.transport.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(bearer_of(&sent[0]), "Bearer v1.license-token-123456");
        assert_eq!(bearer_of(&sent[1]), "Bearer v1.renewed-token-654321");
        assert_eq!(bearer_of(&sent[2]), "Bearer v1.renewed-token-654321");
        // The same attempt identity both times; one digest per chunk plus one retry marker.
        for req in &sent[..2] {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            assert_eq!(body["client_request_id"], "attempt-1");
        }
        assert_eq!(egress(&c.sink), 2);
        let retries = c.sink.records().iter().filter(|r| r.kind == TraceKind::Retry).count();
        assert_eq!(retries, 1);
    }

    /// One refresh per client: a refreshed token that is refused too ends the run as
    /// `Unauthorized` (local lane), and a later 401 does not ask again.
    #[tokio::test]
    async fn the_bearer_is_refreshed_at_most_once_per_client() {
        let (hook, calls) = refresh_to(Some("v1.renewed-token-654321"));
        let c = client([
            HttpResponse { status: 401, body: "{}".into(), retry_after: None },
            HttpResponse { status: 401, body: "{}".into(), retry_after: None },
            HttpResponse { status: 401, body: "{}".into(), retry_after: None },
        ])
        .with_bearer_refresh(hook);
        assert!(matches!(
            c.submit(&items(), Some("attempt-1")).await,
            Err(LlmError::Unauthorized(401, _))
        ));
        assert!(matches!(c.poll("rb_1").await, Err(LlmError::Unauthorized(401, _))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(c.transport.sent().len(), 3);
    }

    #[tokio::test]
    async fn no_new_bearer_or_another_refusal_does_not_resend() {
        let (hook, calls) = refresh_to(None);
        let c = client([HttpResponse { status: 401, body: "{}".into(), retry_after: None }])
            .with_bearer_refresh(hook);
        assert!(matches!(c.poll("rb_1").await, Err(LlmError::Unauthorized(401, _))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(c.transport.sent().len(), 1);

        for status in [402u16, 403, 429, 500] {
            let (hook, calls) = refresh_to(Some("v1.renewed-token-654321"));
            let c = client([HttpResponse { status, body: "{}".into(), retry_after: None }])
                .with_bearer_refresh(hook);
            assert!(c.poll("rb_1").await.is_err());
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "HTTP {status}");
            assert_eq!(c.transport.sent().len(), 1);
        }
    }

    /// §4.5: 402 (plan does not cover the lane) and 429 (daily cap) are their own outcomes —
    /// the caller continues on the local lane rather than retrying or blaming the credential.
    #[tokio::test]
    async fn plan_and_cap_refusals_map_to_their_own_errors() {
        let c = client([HttpResponse { status: 402, body: "{}".into(), retry_after: None }]);
        assert!(matches!(c.submit(&items(), Some("test-attempt")).await, Err(LlmError::QuotaExhausted(_))));
        let c = client([HttpResponse { status: 429, body: "daily cap".into(), retry_after: None }]);
        assert!(matches!(c.submit(&items(), Some("test-attempt")).await, Err(LlmError::RateLimited(429, _))));
    }

    #[tokio::test]
    async fn in_flight_submission_unknown_retries_the_same_create() {
        let c = client([
            HttpResponse { status: 503, body: r#"{"error":"submission unknown"}"#.into(), retry_after: None },
            HttpResponse { status: 202, body: r#"{"batch_id":"rb_retry","accepted":2}"#.into(), retry_after: None },
        ]);
        let handle = c.submit(&items(), Some("20260922")).await.unwrap();
        assert_eq!(handle.id, "rb_retry");
        let sent = c.transport.sent();
        assert_eq!(sent.len(), 2);
        for req in &sent {
            let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
            assert_eq!(body["client_request_id"], "20260922");
        }
        // One digest per chunk, not per attempt; the wait itself shows as one retry marker
        // (issue #29 §5).
        assert_eq!(egress(&c.sink), 2);
        assert_eq!(c.sink.records().len() - egress(&c.sink), 1);
    }

    #[tokio::test]
    async fn later_refusal_does_not_clear_an_earlier_unknown_submission() {
        for status in [400, 401, 402, 429] {
            let c = client([
                HttpResponse { status: 503, body: r#"{"error":"submission unknown"}"#.into(), retry_after: None },
                HttpResponse { status, body: "refused".into(), retry_after: None },
            ]);
            assert!(matches!(c.submit(&items(), Some("same-attempt")).await, Err(LlmError::Provider(_))));
            assert_eq!(c.transport.sent().len(), 2);
        }
    }

    #[tokio::test]
    async fn a_claim_that_stays_unknown_stops_after_two_retries() {
        let unknown = || HttpResponse {
            retry_after: None,
            status: 503,
            body: r#"{"error":"submission unknown"}"#.into(),
        };
        let c = client([unknown(), unknown(), unknown()]);
        assert!(matches!(c.submit(&items(), Some("20260922")).await, Err(LlmError::Provider(_))));
        assert_eq!(c.transport.sent().len(), 3);
        assert_eq!(egress(&c.sink), 2);
        let retries = c.sink.records().iter().filter(|r| r.kind == TraceKind::Retry).count();
        assert_eq!(retries, 2, "each honoured wait is visible on the traceability screen");
    }

    /// Egress rows only — retry / fell-back markers (issue #29 §5) are not chunks sent.
    fn egress(sink: &RecordingSink) -> usize {
        sink.records().iter().filter(|r| r.kind == TraceKind::Egress).count()
    }

    /// Issue #29 FR-SVC-02: the server's own `Retry-After` wins over the documented interval,
    /// capped so a create never parks its lease on a long inline sleep.
    #[test]
    fn create_retries_honour_retry_after_within_a_cap() {
        use std::time::Duration;
        let unknown = r#"{"error":"submission unknown"}"#;
        let metering = r#"{"error":"metering unavailable"}"#;
        assert_eq!(create_retry_delay(503, unknown, None), Some(Duration::from_secs(1)));
        assert_eq!(create_retry_delay(503, metering, None), Some(Duration::from_millis(200)));
        assert_eq!(
            create_retry_delay(503, metering, Some(Duration::from_secs(3))),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            create_retry_delay(503, unknown, Some(Duration::from_secs(3_600))),
            Some(CREATE_RETRY_AFTER_CAP)
        );
        assert_eq!(create_retry_delay(429, metering, Some(Duration::from_secs(3))), None);
        assert_eq!(create_retry_delay(503, "other", Some(Duration::from_secs(3))), None);
    }

    /// Issue #29 §4-6: every exchange is timed (route, ms, status — nothing else) and a throttle
    /// is remembered for the poll loop, then cleared by the next ordinary answer.
    #[tokio::test]
    async fn exchanges_are_timed_and_throttles_remembered() {
        let c = client([
            HttpResponse::new(429, r#"{"error":"too many requests"}"#)
                .with_retry_after(std::time::Duration::from_secs(30)),
            HttpResponse::new(200, r#"{"status":"in_progress"}"#),
        ]);
        assert!(c.poll("rb_1").await.is_err());
        assert_eq!(
            c.last_throttle(),
            Some(SharedThrottle::Transient(std::time::Duration::from_secs(30)))
        );
        assert!(c.poll("rb_1").await.is_ok());
        assert_eq!(c.last_throttle(), None);
        let trips = c.sink.round_trips();
        assert_eq!(trips.len(), 2);
        assert!(trips.iter().all(|t| t.route == Route::BatchRelay));
        assert_eq!((trips[0].status, trips[1].status), (429, 200));
        assert_eq!(
            BatchLane::shared_service(&c),
            Some((Route::BatchRelay, "api.shogunaios.com".to_string()))
        );
    }

    #[tokio::test]
    async fn a_payload_conflict_is_not_retried() {
        let c = client([HttpResponse { status: 409, body: r#"{"error":"client_request_id conflict"}"#.into(), retry_after: None }]);
        assert!(matches!(c.submit(&items(), Some("20260922")).await, Err(LlmError::Provider(_))));
        assert_eq!(c.transport.sent().len(), 1);
    }

    #[tokio::test]
    async fn poll_then_results_roundtrip_on_the_single_endpoint() {
        let c = client([
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"status":"ended","results":[{"custom_id":"1","text":"meeting"}]}"#.into(),
            },
        ]);
        let handle = c.poll("rb_1").await.unwrap();
        assert!(handle.status.is_ended());
        let results = c.results("rb_1").await.unwrap();
        assert_eq!(results[0].text.as_deref(), Some("meeting"));
        // poll/results do not write traceability (only submit does) — same as the direct client.
        assert!(c.sink.records().is_empty());
        // Both calls hit the one status endpoint.
        let sent = c.transport.sent();
        assert!(sent.iter().all(|r| r.url == "https://api.shogunaios.com/v1/batch/rb_1"));
    }

    #[tokio::test]
    async fn run_submits_polls_until_ended_then_fetches_results() {
        let c = client([
            HttpResponse { status: 202, body: r#"{"batch_id":"rb_1","accepted":1}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"in_progress","completed":0,"total":1}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"status":"ended","results":[{"custom_id":"1","text":"consolidated"}]}"#.into(),
            },
        ]);
        let one = vec![BatchItem {
            custom_id: "1".into(),
            purpose: "consolidation".into(),
            chunk: "today's events".into(),
            system: None,
        }];
        let results = c.run(&one, "test-attempt", 5, || async {}).await.unwrap();
        assert_eq!(results[0].text.as_deref(), Some("consolidated"));
        assert_eq!(c.sink.records().len(), 1);
    }

    #[tokio::test]
    async fn run_errors_when_the_batch_never_ends_within_budget() {
        let c = client([
            HttpResponse { status: 202, body: r#"{"batch_id":"rb_1","accepted":1}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"in_progress","completed":0,"total":1}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"status":"in_progress","completed":0,"total":1}"#.into(), retry_after: None },
        ]);
        let one = vec![BatchItem { custom_id: "1".into(), purpose: "index".into(), chunk: "c".into(), system: None }];
        let err = c.run(&one, "test-attempt", 2, || async {}).await.unwrap_err();
        assert!(matches!(err, LlmError::Provider(_)));
    }

    // usage accounting (FR-HN-01 / #313)

    /// relay は usage を返さないので、run 完了で「入力=提出チャンク・出力=返却テキスト」の
    /// バイト長推定が estimated 印付きで1行残る。model 列は model_class（§4.4 — デバイスは
    /// 具体モデルを知らない）。generic な run_batch_to_completion 経路（Dream Cycle）でも
    /// 記録されるよう、行は results() で書かれる。
    #[tokio::test]
    async fn a_completed_run_records_one_estimated_usage_row() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let usage_sink = std::sync::Arc::new(RecordingUsageSink::new());
        let c = RelayBatchClient::new(
            MockTransport::new([
                HttpResponse { status: 202, body: r#"{"batch_id":"rb_1","accepted":1}"#.into(), retry_after: None },
                HttpResponse { status: 200, body: r#"{"status":"ended"}"#.into(), retry_after: None },
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"status":"ended","results":[{"custom_id":"1","text":"label"}]}"#
                        .into(),
                },
            ]),
            RecordingSink::new(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            cfg(),
        )
        .with_usage(UsageHook::new(usage_sink.clone(), prompts::DREAM_CONSOLIDATION));
        let one = vec![BatchItem {
            custom_id: "1".into(),
            purpose: "consolidation".into(),
            chunk: "today's events".into(), // 14 bytes
            system: None,
        }];
        // generic 経路（trait 越し）で回す — dream.rs が使うのはこちら。
        c.run(&one, "test-attempt", 5, || async {}).await.unwrap();

        let recs = usage_sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::BatchRelay);
        assert_eq!(recs[0].model, "classify");
        assert!(recs[0].usage.estimated);
        assert_eq!(recs[0].usage.input_tokens, 4); // ceil(14/4)
        assert_eq!(recs[0].usage.output_tokens, 2); // ceil(5/4)
        assert_eq!(recs[0].outcome, UsageOutcome::Ok);
    }

    /// 402（プランがレーンを含まない）も行として残る — メータが見せるべきはまさにこれ。
    #[tokio::test]
    async fn a_plan_refusal_records_a_quota_usage_row() {
        use crate::llm::prompts;
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        let usage_sink = std::sync::Arc::new(RecordingUsageSink::new());
        let c = RelayBatchClient::new(
            MockTransport::new([HttpResponse { status: 402, body: "{}".into(), retry_after: None }]),
            RecordingSink::new(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            cfg(),
        )
        .with_usage(UsageHook::new(usage_sink.clone(), prompts::DREAM_CONSOLIDATION));
        let _ = c.submit(&items(), Some("test-attempt")).await;
        let recs = usage_sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].outcome, UsageOutcome::Quota);
    }

    // Invariant-5 note: `RelayBatchClient::new` takes a `LicenseBearer` (the licence token).
    // Handing it a `ByokKey` OR a `SelectKkKey` (a raw Anthropic key) does not compile:
    //     RelayBatchClient::new(t, s, SelectKkKey::new(Secret::new("sk-ant-x")), cfg());

    #[tokio::test]
    async fn relay_submit_does_not_send_when_the_ledger_refuses_the_row() {
        use crate::llm::traceability::RefusingSink;
        let transport = MockTransport::ok(r#"{"id":"msgbatch_1","processing_status":"in_progress"}"#);
        let c = RelayBatchClient::new(
            transport,
            RefusingSink::new(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            cfg(),
        );
        let err = c.submit(&items(), Some("attempt-1")).await.expect_err("no row, no send");
        assert!(matches!(err, LlmError::TraceUnrecorded), "{err:?}");
        assert!(c.transport.sent().is_empty(), "the batch must not reach the transport");
    }
}
