//! Usage accounting for every LLM call (FR-HN-01, Issue #313 / #103).
//!
//! Mirrors [`super::traceability`] deliberately: a typed record with **no text field**, a sink
//! trait the clients write to, and an in-memory recording mock for tests. The real sink persists
//! `llm_usage` rows (shogun-memory V25) through the daemon's shared connection.
//!
//! Why this exists: cost, cache effectiveness (FR-HN-05), latency per prompt version, and failure
//! rates per provider are all derived from these rows; without them every optimisation in
//! docs/harness-design-and-optimization.md is unmeasurable.
//!
//! Two guarantees, both structural:
//! - A [`UsageRecord`] cannot carry prompt or completion text — there is no field for it.
//! - The `model` string passes [`redact_secrets`](super::redact_secrets) at construction, because
//!   a key pasted where a model id belongs has leaked through exactly that path before
//!   (`loggable_model` in the desktop shell exists for the same reason).
//!
//! Recording is fire-and-forget and optional: a client without a hook records nothing.

use serde_json::Value;

use super::prompts::PromptTag;
use super::traceability::Route;
use super::LlmError;

/// Token counts for one completion. All zeros is a legal value (an errored call that consumed
/// nothing we could observe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt-cache read tokens (billed ~0.1× input on Anthropic). 0 when the provider reports
    /// none or has no cache.
    pub cache_read_tokens: u64,
    /// Prompt-cache write tokens (billed 1.25×/2× input on Anthropic). 0 elsewhere.
    pub cache_write_tokens: u64,
    /// True when the counts are a byte-length heuristic, not provider-reported numbers. The
    /// subscription CLI (and a relay older than #667) return no usage, so their rows are
    /// estimates and must be distinguishable from exact ones (FR-HN-01d).
    pub estimated: bool,
}

impl Usage {
    /// Provider-reported counts.
    pub fn exact(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            estimated: false,
        }
    }

    /// A byte-length estimate for lanes that report no usage (subscription CLI, relay). The
    /// ~4-bytes-per-token divisor is the same order of accuracy as `shogun_fusion::budget`'s
    /// heuristic (±10% on English, worse on CJK) — good enough for a meter, never for billing.
    pub fn estimated_from_bytes(input_bytes: usize, output_bytes: usize) -> Self {
        Self {
            input_tokens: (input_bytes as u64).div_ceil(4),
            output_tokens: (output_bytes as u64).div_ceil(4),
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            estimated: true,
        }
    }

    /// True when nothing was observed (all counts zero).
    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
    }
}

/// How the call ended. Mirrors the `llm_usage.outcome` CHECK set exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageOutcome {
    Ok,
    /// Transport failures, provider 5xx, delegate failures — the retryable kind.
    Error,
    RateLimited,
    Unauthorized,
    /// Plan/quota refusals (relay 402, delegate quota) — not a bug, not the user's key.
    Quota,
    /// The provider answered but the body didn't parse.
    Parse,
}

impl UsageOutcome {
    /// The exact string stored in `llm_usage.outcome`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            UsageOutcome::Ok => "ok",
            UsageOutcome::Error => "error",
            UsageOutcome::RateLimited => "rate_limited",
            UsageOutcome::Unauthorized => "unauthorized",
            UsageOutcome::Quota => "quota",
            UsageOutcome::Parse => "parse",
        }
    }

    /// Classify a failed call. Kept total on purpose: a new error variant lands in `Error`
    /// (retryable) rather than being unrepresentable.
    pub fn from_error(e: &LlmError) -> Self {
        match e {
            LlmError::Unauthorized(..) => UsageOutcome::Unauthorized,
            LlmError::RateLimited(..) => UsageOutcome::RateLimited,
            LlmError::QuotaExhausted(_) => UsageOutcome::Quota,
            LlmError::Parse(_) => UsageOutcome::Parse,
            LlmError::Provider(_)
            | LlmError::InvalidRequest
            | LlmError::NotConfigured
            | LlmError::NeedsUpdate(_)
            | LlmError::Transport(_)
            | LlmError::PolicyStopped
            | LlmError::TraceUnrecorded => UsageOutcome::Error,
        }
    }
}

/// One usage row. Carries counts, latency and identity — never text (the same guarantee as
/// [`TraceRecord`](super::traceability::TraceRecord): there is no field a body could go in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub route: Route,
    /// The registered prompt identity (FR-HN-02). Every metric row is attributable to the prompt
    /// version that produced it, or prompt optimisation is guesswork.
    pub prompt_id: &'static str,
    pub prompt_version: u32,
    /// The model id (or `model_class` intent on the relay, or delegate name on the CLI lane).
    /// Redacted at construction — see module docs.
    pub model: String,
    pub usage: Usage,
    /// Milliseconds to the first streamed token, when the path can observe it (SLO-03's number).
    pub first_token_ms: Option<u64>,
    /// Milliseconds for the whole call.
    pub total_ms: u64,
    pub outcome: UsageOutcome,
}

impl UsageRecord {
    /// Build a record. `model` is redacted here — the single choke point — so no call site can
    /// forget it.
    pub fn new(
        route: Route,
        tag: PromptTag,
        model: &str,
        usage: Usage,
        first_token_ms: Option<u64>,
        total_ms: u64,
        outcome: UsageOutcome,
    ) -> Self {
        Self {
            route,
            prompt_id: tag.id,
            prompt_version: tag.version,
            model: super::redact_secrets(model),
            usage,
            first_token_ms,
            total_ms,
            outcome,
        }
    }
}

/// The sink usage rows are written to. Fire-and-forget by contract (`record` returns `()`), the
/// same crash-resilience rule as [`TraceabilitySink`](super::traceability::TraceabilitySink): a
/// failed metric write must never interrupt the user's work.
pub trait UsageSink: Send + Sync {
    fn record(&self, rec: UsageRecord);
}

/// An in-memory sink for tests and offline development.
#[derive(Default)]
pub struct RecordingUsageSink {
    records: std::sync::Mutex<Vec<UsageRecord>>,
}

impl RecordingUsageSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record written, in order.
    pub fn records(&self) -> Vec<UsageRecord> {
        self.records.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl UsageSink for RecordingUsageSink {
    fn record(&self, rec: UsageRecord) {
        if let Ok(mut g) = self.records.lock() {
            g.push(rec);
        }
    }
}

/// What a client needs to record usage: where to write and which prompt this client serves.
/// Cloneable and cheap — the sink is shared (`Arc`), the tag is two words.
#[derive(Clone)]
pub struct UsageHook {
    pub sink: std::sync::Arc<dyn UsageSink>,
    pub tag: PromptTag,
}

impl UsageHook {
    pub fn new(sink: std::sync::Arc<dyn UsageSink>, tag: PromptTag) -> Self {
        Self { sink, tag }
    }
}

/// Read Anthropic's `usage` object (`{"input_tokens":…,"output_tokens":…,
/// "cache_read_input_tokens":…,"cache_creation_input_tokens":…}`). `None` when absent.
pub fn anthropic_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?.as_object()?;
    // Require the provider's own keys: an OpenAI-shaped usage object (prompt_tokens/…) or an
    // empty one must not read back as an all-zero "exact" Anthropic count.
    if !u.contains_key("input_tokens") && !u.contains_key("output_tokens") {
        return None;
    }
    let field = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage::exact(
        field("input_tokens"),
        field("output_tokens"),
        field("cache_read_input_tokens"),
        field("cache_creation_input_tokens"),
    ))
}

/// Read an OpenAI-compatible `usage` object (`{"prompt_tokens":…,"completion_tokens":…,
/// "prompt_tokens_details":{"cached_tokens":…}}`). `None` when absent. OpenAI caching has no
/// write premium, so `cache_write_tokens` is always 0 on this surface.
pub fn openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?.as_object()?;
    if !u.contains_key("prompt_tokens") && !u.contains_key("completion_tokens") {
        return None;
    }
    let field = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage::exact(field("prompt_tokens"), field("completion_tokens"), cached, 0))
}

/// Read the `usage` of an OpenAI **Responses API** `response` object (issue #390):
/// `{"input_tokens":…,"output_tokens":…,"input_tokens_details":{"cached_tokens":…,
/// "cache_write_tokens":…},"output_tokens_details":{"reasoning_tokens":…}}`. `None` when absent.
///
/// Takes the `response` object itself (the non-streaming body, or the `response` field of a
/// `response.completed` / `response.incomplete` / `response.failed` event). Deliberately not
/// folded into [`usage_from_json_body`]: the top-level keys are the same names Anthropic uses,
/// so a shape sniff would read the cache fields as zero. Reasoning tokens are already inside
/// `output_tokens` (they are "a breakdown of the output tokens"); see
/// [`responses_reasoning_tokens`] for the breakdown.
pub fn responses_usage(response: &Value) -> Option<Usage> {
    let u = response.get("usage")?.as_object()?;
    if !u.contains_key("input_tokens") && !u.contains_key("output_tokens") {
        return None;
    }
    let field = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let input_detail = |k: &str| {
        u.get("input_tokens_details")
            .and_then(|d| d.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Some(Usage::exact(
        field("input_tokens"),
        field("output_tokens"),
        input_detail("cached_tokens"),
        input_detail("cache_write_tokens"),
    ))
}

/// The reasoning share of a Responses API `output_tokens` count
/// (`usage.output_tokens_details.reasoning_tokens`), when the provider reported one.
pub fn responses_reasoning_tokens(response: &Value) -> Option<u64> {
    response
        .get("usage")?
        .get("output_tokens_details")?
        .get("reasoning_tokens")?
        .as_u64()
}

/// Parse the usage out of a non-streaming JSON body (either provider shape). `None` when the
/// body is not JSON or carries no usage.
pub fn usage_from_json_body(body: &str) -> Option<Usage> {
    let v: Value = serde_json::from_str(body).ok()?;
    anthropic_usage(&v).or_else(|| openai_usage(&v))
}

#[cfg(test)]
mod tests {
    use super::super::prompts;
    use super::*;

    #[test]
    fn a_usage_record_has_no_field_that_could_hold_text() {
        // The structural guarantee: build a record while sensitive text is in scope, then assert
        // the record's Debug carries none of it. (Mirrors the TraceRecord test.)
        let secret_prompt = "TOP SECRET meeting notes about the roadmap";
        let rec = UsageRecord::new(
            Route::MessagesApi,
            prompts::INLINE_CHAT,
            "some-model",
            Usage::estimated_from_bytes(secret_prompt.len(), 10),
            Some(120),
            900,
            UsageOutcome::Ok,
        );
        let dumped = format!("{rec:?}");
        assert!(!dumped.contains("SECRET"), "usage record must never carry text: {dumped}");
        assert!(!dumped.contains("roadmap"));
    }

    #[test]
    fn a_key_pasted_as_a_model_id_is_redacted() {
        // The shape that leaked once already (loggable_model in the shell exists because of it).
        let fake_key = format!("sk-ant-{}", "x".repeat(40));
        let rec = UsageRecord::new(
            Route::MessagesApi,
            prompts::INLINE_DRAFT,
            &fake_key,
            Usage::default(),
            None,
            10,
            UsageOutcome::Unauthorized,
        );
        assert!(!rec.model.contains(&fake_key), "key survived into the model column: {}", rec.model);
    }

    #[test]
    fn estimation_is_marked_and_roughly_four_bytes_per_token() {
        let u = Usage::estimated_from_bytes(400, 41);
        assert!(u.estimated);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 11); // ceil(41/4)
        assert_eq!(u.cache_read_tokens, 0);
        // Provider-reported usage is never marked estimated.
        assert!(!Usage::exact(1, 2, 3, 4).estimated);
    }

    #[test]
    fn outcome_strings_match_the_schema_check() {
        for (o, s) in [
            (UsageOutcome::Ok, "ok"),
            (UsageOutcome::Error, "error"),
            (UsageOutcome::RateLimited, "rate_limited"),
            (UsageOutcome::Unauthorized, "unauthorized"),
            (UsageOutcome::Quota, "quota"),
            (UsageOutcome::Parse, "parse"),
        ] {
            assert_eq!(o.as_db_str(), s);
        }
    }

    #[test]
    fn errors_classify_to_their_own_outcomes() {
        assert_eq!(
            UsageOutcome::from_error(&LlmError::Unauthorized(401, String::new())),
            UsageOutcome::Unauthorized
        );
        assert_eq!(
            UsageOutcome::from_error(&LlmError::RateLimited(429, String::new())),
            UsageOutcome::RateLimited
        );
        assert_eq!(
            UsageOutcome::from_error(&LlmError::QuotaExhausted("plan".into())),
            UsageOutcome::Quota
        );
        assert_eq!(UsageOutcome::from_error(&LlmError::Parse("x".into())), UsageOutcome::Parse);
        assert_eq!(UsageOutcome::from_error(&LlmError::Provider("x".into())), UsageOutcome::Error);
        assert_eq!(UsageOutcome::from_error(&LlmError::NotConfigured), UsageOutcome::Error);
        assert_eq!(
            UsageOutcome::from_error(&LlmError::PolicyStopped),
            UsageOutcome::Error
        );
    }

    #[test]
    fn anthropic_usage_reads_cache_fields() {
        let body = r#"{"content":[],"usage":{"input_tokens":11,"output_tokens":7,
            "cache_read_input_tokens":900,"cache_creation_input_tokens":100}}"#;
        let u = usage_from_json_body(body).unwrap();
        assert_eq!(u, Usage::exact(11, 7, 900, 100));
    }

    #[test]
    fn anthropic_usage_without_cache_fields_defaults_to_zero() {
        let body = r#"{"usage":{"input_tokens":5,"output_tokens":3}}"#;
        assert_eq!(usage_from_json_body(body).unwrap(), Usage::exact(5, 3, 0, 0));
    }

    #[test]
    fn openai_usage_reads_cached_prompt_tokens() {
        let body = r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":9,
            "prompt_tokens_details":{"cached_tokens":40}}}"#;
        assert_eq!(usage_from_json_body(body).unwrap(), Usage::exact(50, 9, 40, 0));
    }

    #[test]
    fn responses_usage_reads_cached_cache_write_and_reasoning_tokens() {
        let v: Value = serde_json::from_str(
            r#"{"id":"resp_1","usage":{"input_tokens":120,"output_tokens":40,"total_tokens":160,
                "input_tokens_details":{"cached_tokens":96,"cache_write_tokens":8},
                "output_tokens_details":{"reasoning_tokens":25}}}"#,
        )
        .unwrap();
        assert_eq!(responses_usage(&v).unwrap(), Usage::exact(120, 40, 96, 8));
        assert_eq!(responses_reasoning_tokens(&v), Some(25));
        // Missing detail objects default to zero; missing usage is None.
        let bare: Value =
            serde_json::from_str(r#"{"usage":{"input_tokens":3,"output_tokens":1}}"#).unwrap();
        assert_eq!(responses_usage(&bare).unwrap(), Usage::exact(3, 1, 0, 0));
        assert_eq!(responses_reasoning_tokens(&bare), None);
        assert!(responses_usage(&serde_json::json!({"id": "resp_1"})).is_none());
        assert!(responses_usage(&serde_json::json!({"usage": null})).is_none());
    }

    #[test]
    fn a_body_without_usage_yields_none() {
        assert!(usage_from_json_body(r#"{"content":[]}"#).is_none());
        assert!(usage_from_json_body("not json").is_none());
        assert!(usage_from_json_body(r#"{"usage":null}"#).is_none());
    }

    #[test]
    fn recording_sink_captures_in_order() {
        let sink = RecordingUsageSink::new();
        for (i, tag) in [prompts::INLINE_CHAT, prompts::INLINE_DRAFT]
            .iter()
            .enumerate()
        {
            sink.record(UsageRecord::new(
                Route::MessagesApi,
                *tag,
                "m",
                Usage::default(),
                None,
                i as u64,
                UsageOutcome::Ok,
            ));
        }
        let recs = sink.records();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].prompt_id, prompts::INLINE_CHAT.id);
        assert_eq!(recs[1].prompt_id, prompts::INLINE_DRAFT.id);
    }
}
