//! `memory.ingest` and `memory.recall` (plan E4-T3; the full semantics are E5-T1 / E5-T3). MCP
//! passes its `arguments`, REST its POST body (the same JSON), the CLI posts that body, so all
//! three faces share one parser here, one backend call each
//! ([`crate::backend::MemoryBackend::ingest`] / [`crate::backend::MemoryBackend::recall`]) and
//! one response envelope ([`render_ingest`] / [`render_recall`], plan §7.2: `result`, `usage`,
//! `latency_ms`).
//!
//! What this epic implements, honestly:
//! - **ingest** appends each item to the event log through the existing append path (hidden
//!   characters stripped, secrets masked, exact duplicates collapsed per thread) under a required
//!   `idempotency_key`; the same key replays the first outcome and appends nothing. `turns[]`
//!   are stored one event per turn with the role as the title; sessionizing them for the review
//!   (E5-T2), the near-duplicate (Dice ≥ 0.98) collapse and the instruction-shaped gate are later.
//!   `namespace` and `actor` are validated and carried to the store; the per-row columns land
//!   with E2 (V92), so until then `actor` is kept on the row's app column and `namespace` is
//!   accepted but not yet stored.
//! - **recall** runs the context-pack assembly (federated search + state facts) and cuts the
//!   result to `budget_tokens` with the fusion crate's token estimator: High facts first, then
//!   `possibly:` facts, then dated evidence. `include_scratch` is accepted; a scratch namespace
//!   has nothing to add until E2-T5 lands.

use serde_json::{json, Value};

use crate::memory_api::Tool;

/// Most items one ingest carries.
pub const MAX_ITEMS: usize = 200;
/// Longest `content` per item, in chars.
pub const MAX_ITEM_CHARS: usize = 16_000;
/// Longest `title` per item, in chars.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest `kind` / `role`, in chars.
pub const MAX_KIND_CHARS: usize = 32;
/// Longest `idempotency_key`, in chars.
pub const MAX_KEY_CHARS: usize = 128;
/// Longest `source`, in chars.
pub const MAX_SOURCE_CHARS: usize = 64;
/// Longest `actor`, in chars.
pub const MAX_ACTOR_CHARS: usize = 128;
/// Longest `thread_key`, in chars.
pub const MAX_THREAD_KEY_CHARS: usize = 256;
/// Largest `metadata` object, as serialized JSON bytes.
pub const MAX_METADATA_BYTES: usize = 4_096;
/// Smallest / largest / default `budget_tokens` of a recall.
pub const MIN_BUDGET_TOKENS: usize = 100;
pub const MAX_BUDGET_TOKENS: usize = 32_000;
pub const DEFAULT_BUDGET_TOKENS: usize = 2_000;
/// The `source` tag ingested rows carry in `event_log.source`, whatever `source` the caller
/// named: the caller's label travels as the row's kind prefix so one agent cannot pose as a
/// capture or a connector row.
pub const EVENT_SOURCE: &str = "agent";

/// The namespace a write lands in (plan §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// The principal's work: what state is derived from.
    World,
    /// The calling agent's own working notes.
    AgentScratch,
}

impl Namespace {
    pub fn as_str(self) -> &'static str {
        match self {
            Namespace::World => "world",
            Namespace::AgentScratch => "agent_scratch",
        }
    }
}

/// One event or turn, lifted off the wire and bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestItem {
    /// Occurrence time (unix ms); `None` means the store's clock at append.
    pub ts_ms: Option<i64>,
    /// `event_log.kind`: the caller's `kind` for events, `turn` for turns.
    pub kind: String,
    /// The short label (`window_title`): the caller's `title`, or the role of a turn.
    pub title: Option<String>,
    pub content: String,
}

/// A parsed `memory.ingest` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRequest {
    pub items: Vec<IngestItem>,
    /// Whether the items came as `turns[]` (role / content) rather than `events[]`.
    pub turns: bool,
    /// The caller's source label (an agent or harness name).
    pub source: String,
    /// Who writes: the agent id or the person. Required (invariant 5).
    pub actor: String,
    /// The conversation / task the items belong to; the event log's thread key derives from it.
    pub thread_key: Option<String>,
    /// Caller metadata, bounded, carried back in the receipt and not interpreted.
    pub metadata: Option<Value>,
    /// Replays the first outcome when repeated. Required.
    pub idempotency_key: String,
    pub namespace: Namespace,
}

/// A parsed `memory.recall` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallRequest {
    pub query: String,
    pub budget_tokens: usize,
    pub include_scratch: bool,
}

/// What an ingest did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestOutcome {
    /// One id per item, in request order (a collapsed duplicate names the row it touched).
    pub event_ids: Vec<i64>,
    /// Items that became new rows.
    pub inserted: usize,
    /// Items that collapsed onto an existing row.
    pub deduplicated: usize,
    /// The key was seen before: this is the first outcome, nothing was appended now.
    pub replayed: bool,
    /// The event log's thread key for the items, when one was derived.
    pub thread_key: Option<String>,
}

/// One evidence line of a recall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallEvidence {
    pub event_id: i64,
    pub ts_ms: i64,
    pub source: String,
    pub title: Option<String>,
    pub excerpt: String,
}

/// What a recall found, already cut to the budget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecallOutcome {
    /// High-confidence facts, as facts.
    pub facts: Vec<String>,
    /// Medium-confidence facts, each already prefixed `possibly: `.
    pub possibly: Vec<String>,
    pub evidence: Vec<RecallEvidence>,
    /// Something was left out to stay within the budget.
    pub truncated: bool,
    /// Estimated tokens of what is returned.
    pub used_tokens: usize,
}

/// Whether `tool` is served by this module.
pub fn is_ingest_tool(tool: Tool) -> bool {
    matches!(tool, Tool::MemoryIngest | Tool::MemoryRecall)
}

fn bounded_string(args: &Value, key: &str, max_chars: usize, required: bool) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => {
            if required {
                Err(format!("missing_{key}: pass {key}, a non-empty string."))
            } else {
                Ok(None)
            }
        }
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                if required {
                    return Err(format!("missing_{key}: pass {key}, a non-empty string."));
                }
                return Ok(None);
            }
            if trimmed.chars().count() > max_chars {
                return Err(format!("invalid_{key}: {key} is at most {max_chars} characters."));
            }
            if trimmed.chars().any(char::is_control) {
                return Err(format!("invalid_{key}: {key} must not contain control characters."));
            }
            Ok(Some(trimmed.to_string()))
        }
        Some(_) => Err(format!("invalid_{key}: {key} must be a string.")),
    }
}

fn parse_item(value: &Value, turn: bool, index: usize) -> Result<IngestItem, String> {
    let list = if turn { "turns" } else { "events" };
    let Some(item) = value.as_object() else {
        return Err(format!("invalid_{list}: {list}[{index}] must be an object."));
    };
    let item = Value::Object(item.clone());
    let content = match item.get("content") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
        _ => return Err(format!("invalid_{list}: {list}[{index}].content must be a non-empty string.")),
    };
    if content.chars().count() > MAX_ITEM_CHARS {
        return Err(format!("invalid_{list}: {list}[{index}].content is at most {MAX_ITEM_CHARS} characters."));
    }
    let ts_ms = match item.get("ts_ms") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .filter(|ts| *ts > 0)
                .ok_or_else(|| format!("invalid_{list}: {list}[{index}].ts_ms must be a positive integer (unix ms)."))?,
        ),
    };
    let (kind, title) = if turn {
        let role = bounded_string(&item, "role", MAX_KIND_CHARS, true)
            .map_err(|e| e.replace("role", &format!("{list}[{index}].role")))?
            .unwrap_or_default();
        if !matches!(role.as_str(), "user" | "assistant" | "system" | "tool") {
            return Err(format!("invalid_{list}: {list}[{index}].role must be user, assistant, system or tool."));
        }
        ("turn".to_string(), Some(role))
    } else {
        let kind = bounded_string(&item, "kind", MAX_KIND_CHARS, false)
            .map_err(|e| e.replace("kind", &format!("{list}[{index}].kind")))?
            .unwrap_or_else(|| "text".to_string());
        let title = bounded_string(&item, "title", MAX_TITLE_CHARS, false)
            .map_err(|e| e.replace("title", &format!("{list}[{index}].title")))?;
        (kind, title)
    };
    Ok(IngestItem { ts_ms, kind, title, content })
}

/// Parse `memory.ingest` arguments. `Err` is `<code>: <sentence>`, the caller's to fix (MCP
/// -32602, REST 400). A missing `idempotency_key` or `actor` is refused before anything is read.
pub fn ingest_request(args: &Value) -> Result<IngestRequest, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let idempotency_key = bounded_string(args, "idempotency_key", MAX_KEY_CHARS, true)?.unwrap_or_default();
    let actor = bounded_string(args, "actor", MAX_ACTOR_CHARS, true)?.unwrap_or_default();
    let source = bounded_string(args, "source", MAX_SOURCE_CHARS, true)?.unwrap_or_default();
    let thread_key = bounded_string(args, "thread_key", MAX_THREAD_KEY_CHARS, false)?;
    let namespace = match args.get("namespace") {
        None | Some(Value::Null) => Namespace::World,
        Some(Value::String(ns)) if ns == "world" => Namespace::World,
        Some(Value::String(ns)) if ns == "agent_scratch" => Namespace::AgentScratch,
        Some(_) => return Err("invalid_namespace: namespace must be world or agent_scratch.".into()),
    };
    let metadata = match args.get("metadata") {
        None | Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => {
            if value.to_string().len() > MAX_METADATA_BYTES {
                return Err(format!("invalid_metadata: metadata is at most {MAX_METADATA_BYTES} bytes."));
            }
            Some(value.clone())
        }
        Some(_) => return Err("invalid_metadata: metadata must be an object.".into()),
    };
    let (raw, turns) = match (args.get("events"), args.get("turns")) {
        (Some(_), Some(_)) => return Err("invalid_request: pass events or turns, not both.".into()),
        (Some(Value::Array(events)), None) => (events, false),
        (None, Some(Value::Array(turns))) => (turns, true),
        (Some(_), None) => return Err("invalid_events: events must be an array.".into()),
        (None, Some(_)) => return Err("invalid_turns: turns must be an array.".into()),
        (None, None) => return Err("missing_events: pass events[] or turns[].".into()),
    };
    let list = if turns { "turns" } else { "events" };
    if raw.is_empty() || raw.len() > MAX_ITEMS {
        return Err(format!("invalid_{list}: pass 1 to {MAX_ITEMS} {list}."));
    }
    let items = raw
        .iter()
        .enumerate()
        .map(|(index, value)| parse_item(value, turns, index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(IngestRequest { items, turns, source, actor, thread_key, metadata, idempotency_key, namespace })
}

/// Parse `memory.recall` arguments.
pub fn recall_request(args: &Value) -> Result<RecallRequest, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let query = bounded_string(args, "query", crate::backend::MAX_SEARCH_QUERY_CHARS, true)?.unwrap_or_default();
    crate::backend::validate_search_query(&query).map_err(|policy| format!("{}: the query was refused.", policy.as_str()))?;
    let budget_tokens = match args.get("budget_tokens") {
        None | Some(Value::Null) => DEFAULT_BUDGET_TOKENS,
        Some(v) => match v.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) if (MIN_BUDGET_TOKENS..=MAX_BUDGET_TOKENS).contains(&n) => n,
            _ => {
                return Err(format!(
                    "invalid_budget_tokens: budget_tokens must be {MIN_BUDGET_TOKENS} to {MAX_BUDGET_TOKENS}."
                ))
            }
        },
    };
    let include_scratch = match args.get("include_scratch") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("invalid_include_scratch: include_scratch must be a boolean.".into()),
    };
    Ok(RecallRequest { query, budget_tokens, include_scratch })
}

/// The `memory.ingest` envelope (plan §7.2): the receipt, the usage the gateway bills (events),
/// and the latency.
pub fn render_ingest(request: &IngestRequest, outcome: &IngestOutcome, latency_ms: u64) -> String {
    json!({
        "tool": Tool::MemoryIngest.wire_name(),
        "result": {
            "accepted": outcome.event_ids.len(),
            "inserted": outcome.inserted,
            "deduplicated": outcome.deduplicated,
            "replayed": outcome.replayed,
            "event_ids": outcome.event_ids,
            "thread_key": outcome.thread_key,
            "namespace": request.namespace.as_str(),
            "idempotency_key": request.idempotency_key,
            "metadata": request.metadata,
        },
        "usage": { "unit": "events", "quantity": if outcome.replayed { 0 } else { outcome.event_ids.len() } },
        "latency_ms": latency_ms,
        "next_action": Value::Null,
    })
    .to_string()
}

/// The `memory.recall` envelope: facts, hedged facts, evidence, and a free `usage`.
pub fn render_recall(request: &RecallRequest, outcome: &RecallOutcome, latency_ms: u64) -> String {
    json!({
        "tool": Tool::MemoryRecall.wire_name(),
        "result": {
            "facts": outcome.facts,
            "possibly": outcome.possibly,
            "evidence": outcome.evidence.iter().map(|e| json!({
                "event_id": e.event_id,
                "ts_ms": e.ts_ms,
                "source": e.source,
                "title": e.title,
                "excerpt": e.excerpt,
            })).collect::<Vec<_>>(),
            "truncated": outcome.truncated,
            "budget_tokens": request.budget_tokens,
            "used_tokens": outcome.used_tokens,
            "include_scratch": request.include_scratch,
        },
        "usage": { "unit": "read", "quantity": 1, "cost_micro_usd": 0 },
        "latency_ms": latency_ms,
        "next_action": if outcome.truncated { Value::String("narrow_query".into()) } else { Value::Null },
    })
    .to_string()
}

/// Why a call did not produce an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `<code>: <sentence>`: the caller's to fix (MCP -32602, REST 400).
    BadArguments(String),
    /// The store could not serve it (MCP -32000, REST 503); the code names which call.
    Unavailable(&'static str),
}

/// Parse, run and render one call for any face. The latency is measured around the backend.
pub fn serve<B: crate::backend::MemoryBackend + ?Sized>(tool: Tool, args: &Value, backend: &B) -> Result<String, Refusal> {
    let started = std::time::Instant::now();
    let elapsed = |started: std::time::Instant| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match tool {
        Tool::MemoryIngest => {
            let request = ingest_request(args).map_err(Refusal::BadArguments)?;
            let outcome = backend.ingest(&request).map_err(|_| Refusal::Unavailable("ingest_unavailable"))?;
            Ok(render_ingest(&request, &outcome, elapsed(started)))
        }
        Tool::MemoryRecall => {
            let request = recall_request(args).map_err(Refusal::BadArguments)?;
            let outcome = backend.recall(&request).map_err(|_| Refusal::Unavailable("recall_unavailable"))?;
            Ok(render_recall(&request, &outcome, elapsed(started)))
        }
        _ => Err(Refusal::BadArguments("invalid_request: not an ingest tool.".into())),
    }
}

/// The MCP `inputSchema` of `memory.ingest`.
pub fn ingest_schema() -> Value {
    let item_common = json!({
        "ts_ms": { "type": "integer", "minimum": 1, "description": "When it happened (unix ms); default now" },
        "content": { "type": "string", "minLength": 1, "maxLength": MAX_ITEM_CHARS }
    });
    let mut event = item_common.clone();
    event["kind"] = json!({ "type": "string", "maxLength": MAX_KIND_CHARS, "description": "text (default), message, note, decision, ..." });
    event["title"] = json!({ "type": "string", "maxLength": MAX_TITLE_CHARS });
    let mut turn = item_common;
    turn["role"] = json!({ "type": "string", "enum": ["user", "assistant", "system", "tool"] });
    json!({
        "events": { "type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": { "type": "object", "properties": event, "required": ["content"] } },
        "turns": { "type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": { "type": "object", "properties": turn, "required": ["role", "content"] } },
        "source": { "type": "string", "maxLength": MAX_SOURCE_CHARS, "description": "Your harness or agent name" },
        "actor": { "type": "string", "maxLength": MAX_ACTOR_CHARS, "description": "Who writes: your agent id" },
        "thread_key": { "type": "string", "maxLength": MAX_THREAD_KEY_CHARS, "description": "Your task or conversation id; keeps the items one thread" },
        "metadata": { "type": "object", "description": "Carried back in the receipt, not interpreted" },
        "idempotency_key": { "type": "string", "maxLength": MAX_KEY_CHARS, "description": "Required; the same key replays the first result" },
        "namespace": { "type": "string", "enum": ["world", "agent_scratch"], "default": "world" }
    })
}

/// The MCP `inputSchema` of `memory.recall`.
pub fn recall_schema() -> Value {
    json!({
        "query": { "type": "string", "maxLength": crate::backend::MAX_SEARCH_QUERY_CHARS },
        "budget_tokens": { "type": "integer", "minimum": MIN_BUDGET_TOKENS, "maximum": MAX_BUDGET_TOKENS, "description": "Most tokens to return (default 2000)" },
        "include_scratch": { "type": "boolean", "default": false, "description": "Also read your own agent_scratch namespace" }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest_args() -> Value {
        json!({
            "events": [{ "content": "Alice agreed to send the deck by Friday", "kind": "message", "title": "standup" }],
            "source": "harness",
            "actor": "agent:test",
            "thread_key": "task-1",
            "idempotency_key": "k-1"
        })
    }

    #[test]
    fn ingest_requires_an_idempotency_key_an_actor_and_items() {
        let mut args = ingest_args();
        args.as_object_mut().unwrap().remove("idempotency_key");
        assert!(ingest_request(&args).unwrap_err().starts_with("missing_idempotency_key:"));
        let mut args = ingest_args();
        args["actor"] = json!("   ");
        assert!(ingest_request(&args).unwrap_err().starts_with("missing_actor:"));
        let mut args = ingest_args();
        args.as_object_mut().unwrap().remove("events");
        assert!(ingest_request(&args).unwrap_err().starts_with("missing_events:"));
        let mut args = ingest_args();
        args["turns"] = json!([{ "role": "user", "content": "x" }]);
        assert!(ingest_request(&args).unwrap_err().starts_with("invalid_request:"));
        assert!(ingest_request(&json!("text")).unwrap_err().starts_with("invalid_request:"));
    }

    #[test]
    fn events_and_turns_parse_to_bounded_items() {
        let request = ingest_request(&ingest_args()).unwrap();
        assert!(!request.turns);
        assert_eq!(request.items[0].kind, "message");
        assert_eq!(request.items[0].title.as_deref(), Some("standup"));
        assert_eq!(request.namespace, Namespace::World);
        let turns = ingest_request(&json!({
            "turns": [{ "role": "user", "content": "do x" }, { "role": "assistant", "content": "done", "ts_ms": 5 }],
            "source": "s", "actor": "a", "idempotency_key": "k", "namespace": "agent_scratch",
            "metadata": { "run": 3 }
        }))
        .unwrap();
        assert!(turns.turns);
        assert_eq!(turns.items.len(), 2);
        assert_eq!(turns.items[0].kind, "turn");
        assert_eq!(turns.items[0].title.as_deref(), Some("user"));
        assert_eq!(turns.items[1].ts_ms, Some(5));
        assert_eq!(turns.namespace, Namespace::AgentScratch);
        assert_eq!(turns.metadata, Some(json!({ "run": 3 })));
        let bad_role = ingest_request(&json!({
            "turns": [{ "role": "robot", "content": "x" }], "source": "s", "actor": "a", "idempotency_key": "k"
        }))
        .unwrap_err();
        assert!(bad_role.starts_with("invalid_turns:"), "{bad_role}");
        let too_many = ingest_request(&json!({
            "events": vec![json!({ "content": "x" }); MAX_ITEMS + 1], "source": "s", "actor": "a", "idempotency_key": "k"
        }))
        .unwrap_err();
        assert!(too_many.starts_with("invalid_events:"), "{too_many}");
        let bad_ns = ingest_request(&json!({
            "events": [{ "content": "x" }], "source": "s", "actor": "a", "idempotency_key": "k", "namespace": "shared"
        }))
        .unwrap_err();
        assert!(bad_ns.starts_with("invalid_namespace:"), "{bad_ns}");
    }

    #[test]
    fn recall_takes_a_query_and_a_bounded_budget() {
        let request = recall_request(&json!({ "query": "deck" })).unwrap();
        assert_eq!(request.budget_tokens, DEFAULT_BUDGET_TOKENS);
        assert!(!request.include_scratch);
        let request = recall_request(&json!({ "query": "deck", "budget_tokens": 500, "include_scratch": true })).unwrap();
        assert_eq!((request.budget_tokens, request.include_scratch), (500, true));
        assert!(recall_request(&json!({})).unwrap_err().starts_with("missing_query:"));
        assert!(recall_request(&json!({ "query": "deck", "budget_tokens": 1 }))
            .unwrap_err()
            .starts_with("invalid_budget_tokens:"));
        assert!(recall_request(&json!({ "query": "deck", "include_scratch": "yes" }))
            .unwrap_err()
            .starts_with("invalid_include_scratch:"));
    }

    #[test]
    fn envelopes_carry_result_usage_and_latency() {
        let request = ingest_request(&ingest_args()).unwrap();
        let outcome = IngestOutcome { event_ids: vec![7], inserted: 1, deduplicated: 0, replayed: false, thread_key: Some("agent:task-1".into()) };
        let value: Value = serde_json::from_str(&render_ingest(&request, &outcome, 3)).unwrap();
        assert_eq!(value["tool"], "memory.ingest");
        assert_eq!(value["result"]["event_ids"], json!([7]));
        assert_eq!(value["usage"], json!({ "unit": "events", "quantity": 1 }));
        assert_eq!(value["latency_ms"], 3);
        let replay = IngestOutcome { replayed: true, ..outcome };
        let value: Value = serde_json::from_str(&render_ingest(&request, &replay, 1)).unwrap();
        assert_eq!(value["usage"]["quantity"], 0, "a replay bills nothing");
        let recall = RecallRequest { query: "deck".into(), budget_tokens: 300, include_scratch: false };
        let outcome = RecallOutcome { facts: vec!["you committed: send the deck".into()], truncated: true, used_tokens: 290, ..Default::default() };
        let value: Value = serde_json::from_str(&render_recall(&recall, &outcome, 2)).unwrap();
        assert_eq!(value["usage"]["cost_micro_usd"], 0);
        assert_eq!(value["next_action"], "narrow_query");
        assert_eq!(value["result"]["budget_tokens"], 300);
    }

    #[test]
    fn a_backend_without_ingest_answers_unavailable_not_a_panic() {
        let backend = crate::backend::StubBackend;
        assert_eq!(
            serve(Tool::MemoryIngest, &ingest_args(), &backend),
            Err(Refusal::Unavailable("ingest_unavailable"))
        );
        assert_eq!(
            serve(Tool::MemoryRecall, &json!({ "query": "deck" }), &backend),
            Err(Refusal::Unavailable("recall_unavailable"))
        );
        assert!(matches!(serve(Tool::MemoryIngest, &json!({}), &backend), Err(Refusal::BadArguments(_))));
    }
}
