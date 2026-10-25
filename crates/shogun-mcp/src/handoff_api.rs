//! `handoff.create` and `handoff.accept` (plan E35, docs/api/handoff.md): the A2A state-of-work
//! handoff, assembled and accepted in the core so every face answers it the same way (invariant
//! 4) and the hosted gateway only relays it (invariant 1; threat model R5 resolved).
//!
//! - **create** runs one budgeted recall ([`crate::backend::MemoryBackend::recall`], the same
//!   read `memory.recall` serves, so the confidence rules and the source filter apply unchanged)
//!   and maps its state items onto the handoff shape: people and projects as export v1
//!   `entity` records, commitments and open loops as `fact` records, each with its band and the
//!   provenance (event id + quote) that proves it. Low never travels; a row without an event id
//!   cannot be proven to the receiver and is left out; duplicates of one `(kind, id)` merge their
//!   provenance. A read: free (invariant 10).
//! - **accept** validates a received handoff (format, addressee, expiry, row shape) and appends it
//!   to the receiver's **own `agent_scratch` namespace** through the `memory.ingest` path, under
//!   the receiver's actor and an idempotency key derived from the handoff id (a repeat replays).
//!   What another agent claims is working context for the receiver, never the principal's world.
//!   An L1 write like `memory.ingest`, metered per event appended.
//!
//! The principal boundary is the store the call resolves to: create reads the caller's own
//! principal (or, on REST, a teammate's world through the Team grant check, `?principal=`), and
//! accept writes only the caller's own scratch. Nothing here reads another principal's file.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::ingest_api::{IngestItem, IngestOutcome, IngestRequest, Namespace, RecallOutcome, RecallRequest, Refusal};
use crate::memory_api::Tool;

pub const FORMAT: &str = "shogunagent-handoff/v1";
pub const SCHEMA_URL: &str = "https://shogunagent.dev/schemas/handoff.schema.json";
pub const MIN_BUDGET_TOKENS: usize = 100;
pub const MAX_BUDGET_TOKENS: usize = 32_000;
pub const DEFAULT_BUDGET_TOKENS: usize = 2_000;
pub const MIN_TTL_SECONDS: i64 = 60;
pub const MAX_TTL_SECONDS: i64 = 86_400;
pub const DEFAULT_TTL_SECONDS: i64 = 3_600;
/// Longest `task` / `query`, in chars.
pub const MAX_TEXT: usize = 2_000;
/// Longest `thread_key`, in chars.
pub const MAX_THREAD_KEY: usize = 256;
/// Most entities and most facts one handoff carries (each).
pub const MAX_ROWS: usize = 200;
/// Longest agent id.
pub const MAX_AGENT_ID: usize = 128;
/// The `source` label accepted rows carry.
pub const ACCEPT_SOURCE: &str = "handoff";

/// Whether `tool` is served by this module.
pub fn is_handoff_tool(tool: Tool) -> bool {
    matches!(tool, Tool::HandoffCreate | Tool::HandoffAccept)
}

fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_AGENT_ID
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '@' | '/' | '-'))
}

fn valid_handoff_id(id: &str) -> bool {
    id.strip_prefix("hof_")
        .is_some_and(|rest| (8..=64).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

fn bounded_text(args: &Value, key: &str, max: usize, required: bool) -> Result<Option<String>, String> {
    let missing = || format!("missing_{key}: pass {key}, a non-empty string of at most {max} characters.");
    match args.get(key) {
        None | Some(Value::Null) => {
            if required {
                Err(missing())
            } else {
                Ok(None)
            }
        }
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return if required { Err(missing()) } else { Ok(None) };
            }
            if trimmed.chars().count() > max {
                return Err(format!("invalid_{key}: {key} is at most {max} characters."));
            }
            if trimmed.chars().any(char::is_control) {
                return Err(format!("invalid_{key}: {key} must not contain control characters."));
            }
            Ok(Some(trimmed.to_string()))
        }
        Some(_) => Err(format!("invalid_{key}: {key} must be a string.")),
    }
}

fn bounded_int(args: &Value, key: &str, min: i64, max: i64, fallback: i64) -> Result<i64, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(fallback),
        Some(v) => v
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| format!("invalid_{key}: {key} must be an integer between {min} and {max}.")),
    }
}

fn agent_arg(args: &Value, key: &str, required: bool) -> Result<Option<String>, String> {
    let rule = format!("{key} is an agent id: letters, digits and `_ . : @ / -`, at most {MAX_AGENT_ID} characters.");
    match args.get(key) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(id)) if valid_agent_id(id) => Ok(Some(id.clone())),
        None | Some(Value::Null) => Err(format!("missing_{key}: {rule}")),
        Some(_) => Err(format!("invalid_{key}: {rule}")),
    }
}

/// A parsed `handoff.create` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub task: String,
    pub to_agent: String,
    /// The sender's label; `None` takes the call's actor ([`default_from_agent`]).
    pub from_agent: Option<String>,
    pub query: String,
    pub budget_tokens: usize,
    pub ttl_seconds: i64,
    pub thread_key: Option<String>,
}

/// Parse `handoff.create` arguments. `Err` is `<code>: <sentence>`, the caller's to fix.
pub fn create_request(args: &Value) -> Result<CreateRequest, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let task = bounded_text(args, "task", MAX_TEXT, true)?.unwrap_or_default();
    let to_agent = agent_arg(args, "to_agent", true)?.unwrap_or_default();
    let from_agent = agent_arg(args, "from_agent", false)?;
    let query = bounded_text(args, "query", MAX_TEXT, false)?.unwrap_or_else(|| task.clone());
    let thread_key = bounded_text(args, "thread_key", MAX_THREAD_KEY, false)?;
    let budget = bounded_int(args, "budget_tokens", MIN_BUDGET_TOKENS as i64, MAX_BUDGET_TOKENS as i64, DEFAULT_BUDGET_TOKENS as i64)?;
    let ttl_seconds = bounded_int(args, "ttl_seconds", MIN_TTL_SECONDS, MAX_TTL_SECONDS, DEFAULT_TTL_SECONDS)?;
    Ok(CreateRequest {
        task,
        to_agent,
        from_agent,
        query,
        budget_tokens: usize::try_from(budget).unwrap_or(DEFAULT_BUDGET_TOKENS),
        ttl_seconds,
        thread_key,
    })
}

/// The sender when the request names none: the call's writer (`agent:<key id>` on the hosted
/// server, `human:owner` locally), when it is a valid agent id.
pub fn default_from_agent() -> Option<String> {
    let actor = shogun_memory::actor::current_write().actor;
    valid_agent_id(actor.as_str()).then(|| actor.as_str().to_string())
}

/// A fresh `hof_<uuid v4>` (or `msg_…`) id from the OS random source.
fn fresh_id(prefix: &str) -> Result<String, Refusal> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| Refusal::Unavailable("random_unavailable"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{prefix}_{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

/// Map a recall onto the handoff object. Pure: the id and the clock are the caller's.
pub fn build(recall: &RecallOutcome, req: &CreateRequest, from_agent: &str, now_ms: i64, handoff_id: &str) -> Value {
    let mut entities: Vec<Value> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut capped = false;
    for item in &recall.items {
        if item.band != "high" && item.band != "medium" {
            continue; // Low never travels.
        }
        let Some(event_id) = item.event_id else { continue };
        let text = item.text.trim();
        if text.is_empty() || !(0.0..=1.0).contains(&item.confidence) {
            continue;
        }
        let quote = item.quote.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let provenance = json!({ "event_id": event_id, "quote": quote });
        let (rows, record) = match item.kind {
            "person" | "project" => (&mut entities, json!({
                "type": "entity", "kind": item.kind, "id": item.id, "name": text,
                "confidence": item.confidence, "band": item.band, "provenance": [provenance.clone()],
            })),
            "commitment" | "open_loop" => (&mut facts, json!({
                "type": "fact", "kind": item.kind, "id": item.id, "text": text,
                "confidence": item.confidence, "band": item.band, "provenance": [provenance.clone()],
            })),
            _ => continue,
        };
        if let Some(existing) = rows.iter_mut().find(|r| r["kind"] == item.kind && r["id"] == item.id) {
            if let Some(list) = existing["provenance"].as_array_mut() {
                if !list.iter().any(|p| p["event_id"] == event_id) {
                    list.push(provenance);
                }
            }
        } else if rows.len() < MAX_ROWS {
            rows.push(record);
        } else {
            capped = true;
        }
    }
    json!({
        "format": FORMAT,
        "handoff_id": handoff_id,
        "created_at": now_ms,
        "expires_at": now_ms.saturating_add(req.ttl_seconds.saturating_mul(1000)),
        "from_agent": from_agent,
        "to_agent": req.to_agent,
        "task": { "summary": req.task, "query": req.query, "thread_key": req.thread_key },
        "entities": entities,
        "facts": facts,
        "truncated": recall.truncated || capped,
    })
}

/// The A2A data part that carries `handoff`, and a ready agent message.
pub fn a2a(handoff: &Value, message_id: &str) -> Value {
    let part = json!({ "kind": "data", "data": handoff, "metadata": { "schema": SCHEMA_URL, "format": FORMAT } });
    json!({
        "part": part,
        "message": { "kind": "message", "role": "agent", "messageId": message_id, "parts": [part], "metadata": { "handoff_id": handoff["handoff_id"] } },
    })
}

/// The `handoff.create` envelope: the object, its A2A part, a free `usage`.
pub fn render_create(handoff: &Value, a2a: &Value, latency_ms: u64) -> String {
    let truncated = handoff["truncated"].as_bool().unwrap_or(false);
    json!({
        "tool": Tool::HandoffCreate.wire_name(),
        "result": { "handoff": handoff, "a2a": a2a },
        "usage": { "unit": "read", "quantity": 1, "cost_micro_usd": 0 },
        "latency_ms": latency_ms,
        "hints": [],
        "next_action": if truncated { Value::String("narrow_query".into()) } else { Value::Null },
    })
    .to_string()
}

/// One row of a received handoff, validated.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    fact: bool,
    kind: String,
    id: i64,
    text: String,
    confidence: f64,
    medium: bool,
    provenance: Vec<(i64, Option<String>)>,
}

/// A received handoff, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub handoff_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub from_agent: String,
    pub to_agent: String,
    pub task: String,
    rows: Vec<Row>,
    /// Rows left out: Low band (never travels) or without provenance.
    pub skipped: usize,
}

/// A parsed `handoff.accept` request.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptRequest {
    pub handoff: Received,
    /// Who accepts: the receiver's agent id. Required (invariant 5).
    pub actor: String,
    pub idempotency_key: String,
    pub check_addressee: bool,
}

/// Find the handoff in what arrived: the object, its A2A data part, an A2A message, or a
/// `handoff.create` result.
fn locate(value: &Value) -> Option<&Value> {
    let mut v = value;
    if v.get("handoff").is_some_and(Value::is_object) {
        v = &v["handoff"];
    }
    if let Some(parts) = v.get("parts").and_then(Value::as_array) {
        v = parts.iter().find(|p| p["kind"] == "data" && p["data"]["format"] == FORMAT)?;
    }
    if v["kind"] == "data" && v.get("data").is_some_and(Value::is_object) {
        v = &v["data"];
    }
    (v["format"] == FORMAT).then_some(v)
}

fn invalid(why: &str) -> String {
    format!("invalid_handoff: {why}")
}

fn parse_rows(list: &Value, field: &str, fact: bool, rows: &mut Vec<Row>, skipped: &mut usize) -> Result<(), String> {
    let Some(items) = list.as_array() else {
        return Err(invalid(&format!("{field} must be an array.")));
    };
    if items.len() > MAX_ROWS {
        return Err(invalid(&format!("{field} holds at most {MAX_ROWS} rows.")));
    }
    for (i, row) in items.iter().enumerate() {
        let at = |what: &str| invalid(&format!("{field}[{i}]: {what}"));
        let kind = row["kind"].as_str().unwrap_or_default();
        let kinds: &[&str] = if fact { &["commitment", "open_loop"] } else { &["person", "project"] };
        if row["type"] != if fact { "fact" } else { "entity" } || !kinds.contains(&kind) {
            return Err(at("type / kind is not an export v1 record a handoff carries."));
        }
        let id = row["id"].as_i64().ok_or_else(|| at("id must be an integer."))?;
        let text = row[if fact { "text" } else { "name" }].as_str().map(str::trim).unwrap_or_default();
        if text.is_empty() || text.chars().count() > crate::ingest_api::MAX_ITEM_CHARS {
            return Err(at(if fact { "text must be a non-empty string." } else { "name must be a non-empty string." }));
        }
        let confidence = row["confidence"].as_f64().filter(|c| (0.0..=1.0).contains(c)).ok_or_else(|| at("confidence must be 0 to 1."))?;
        let medium = match row["band"].as_str() {
            Some("high") => false,
            Some("medium") => true,
            Some("low") => {
                *skipped += 1;
                continue;
            }
            _ => return Err(at("band must be high or medium.")),
        };
        let provenance: Vec<(i64, Option<String>)> = match row["provenance"].as_array() {
            Some(list) => list
                .iter()
                .map(|p| {
                    let event_id = p["event_id"].as_i64().ok_or_else(|| at("provenance[].event_id must be an integer."))?;
                    let quote = match &p["quote"] {
                        Value::Null => None,
                        Value::String(q) => Some(q.trim().to_string()).filter(|q| !q.is_empty()),
                        _ => return Err(at("provenance[].quote must be a string or null.")),
                    };
                    Ok((event_id, quote))
                })
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        if provenance.is_empty() {
            *skipped += 1;
            continue;
        }
        rows.push(Row { fact, kind: kind.to_string(), id, text: text.to_string(), confidence, medium, provenance });
    }
    Ok(())
}

/// Validate a received handoff object (located by [`locate`]).
pub fn received(value: &Value) -> Result<Received, String> {
    let h = locate(value).ok_or_else(|| invalid(&format!("pass handoff: a {FORMAT} object, its A2A data part or the A2A message.")))?;
    let handoff_id = h["handoff_id"].as_str().filter(|id| valid_handoff_id(id)).ok_or_else(|| invalid("handoff_id must be hof_ followed by 8-64 letters, digits or -."))?;
    let created_at = h["created_at"].as_i64().ok_or_else(|| invalid("created_at must be unix ms."))?;
    let expires_at = h["expires_at"].as_i64().ok_or_else(|| invalid("expires_at must be unix ms."))?;
    let from_agent = h["from_agent"].as_str().filter(|a| valid_agent_id(a)).ok_or_else(|| invalid("from_agent must be an agent id."))?;
    let to_agent = h["to_agent"].as_str().filter(|a| valid_agent_id(a)).ok_or_else(|| invalid("to_agent must be an agent id."))?;
    let task = h["task"]["summary"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_TEXT)
        .ok_or_else(|| invalid("task.summary must be a non-empty string of at most 2000 characters."))?;
    let mut rows = Vec::new();
    let mut skipped = 0;
    parse_rows(&h["entities"], "entities", false, &mut rows, &mut skipped)?;
    parse_rows(&h["facts"], "facts", true, &mut rows, &mut skipped)?;
    Ok(Received {
        handoff_id: handoff_id.to_string(),
        created_at,
        expires_at,
        from_agent: from_agent.to_string(),
        to_agent: to_agent.to_string(),
        task: task.to_string(),
        rows,
        skipped,
    })
}

/// The idempotency key of an accept that names none: one per handoff and receiver, so two
/// receivers in one store each get their own copy and a repeat by either replays.
pub fn default_idempotency_key(handoff_id: &str, actor: &str) -> String {
    let key = format!("handoff:{handoff_id}:{actor}");
    if key.chars().count() <= crate::ingest_api::MAX_KEY_CHARS {
        return key;
    }
    let digest = Sha256::digest(actor.as_bytes());
    let short: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("handoff:{handoff_id}:{short}")
}

/// Parse `handoff.accept` arguments and validate the handoff at `now_ms`.
pub fn accept_request(args: &Value, now_ms: i64) -> Result<AcceptRequest, String> {
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let actor = bounded_text(args, "actor", crate::ingest_api::MAX_ACTOR_CHARS, true)?.unwrap_or_default();
    let explicit_key = bounded_text(args, "idempotency_key", crate::ingest_api::MAX_KEY_CHARS - 4, false)?;
    let check_addressee = match args.get("check_addressee") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("invalid_check_addressee: check_addressee must be a boolean.".into()),
    };
    let handoff = received(args.get("handoff").unwrap_or(&Value::Null))?;
    if handoff.expires_at <= now_ms {
        return Err(format!(
            "handoff_expired: the handoff expired at {} (unix ms); ask the sender for a fresh one.",
            handoff.expires_at
        ));
    }
    if check_addressee && handoff.to_agent != actor {
        return Err(format!(
            "handoff_not_for_you: the handoff is addressed to {}, not {actor}; pass check_addressee: false to accept it anyway.",
            handoff.to_agent
        ));
    }
    let idempotency_key = explicit_key.unwrap_or_else(|| default_idempotency_key(&handoff.handoff_id, &actor));
    Ok(AcceptRequest { handoff, actor, idempotency_key, check_addressee })
}

fn chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The events an accept appends: one for the task, one per entity and fact. Medium rows keep the
/// `possibly:` prefix; the quotes and the sender's event ids ride in the text.
pub fn accept_items(h: &Received) -> Vec<IngestItem> {
    let mut items = vec![IngestItem {
        ts_ms: None,
        kind: "handoff_task".into(),
        title: Some(chars(&format!("handoff from {}", h.from_agent), crate::ingest_api::MAX_TITLE_CHARS)),
        content: chars(&format!("Task handed over by {}: {}", h.from_agent, h.task), crate::ingest_api::MAX_ITEM_CHARS),
    }];
    for row in &h.rows {
        let mark = if row.medium { shogun_fusion::confidence::POSSIBLY_PREFIX } else { "" };
        let quoted: Vec<String> = row
            .provenance
            .iter()
            .filter_map(|(event, quote)| quote.as_ref().map(|q| format!("\"{q}\" (event {event})")))
            .collect();
        let source = if quoted.is_empty() { String::new() } else { format!(" Source: {}", quoted.join("; ")) };
        let content = if row.fact {
            format!("{mark}{}: {} (confidence {}).{source}", row.kind.replace('_', " "), row.text, row.confidence)
        } else {
            format!("{mark}{} {} (confidence {}).{source}", row.kind, row.text, row.confidence)
        };
        items.push(IngestItem {
            ts_ms: None,
            kind: if row.fact { "handoff_fact".into() } else { "handoff_entity".into() },
            title: Some(format!("{} #{}", row.kind, row.id)),
            content: chars(&content, crate::ingest_api::MAX_ITEM_CHARS),
        });
    }
    items
}

/// The ingest requests an accept sends: the items in chunks of [`crate::ingest_api::MAX_ITEMS`],
/// each under `<key>:<n>` (a single chunk keeps the key as given).
pub fn ingest_requests(req: &AcceptRequest) -> Vec<IngestRequest> {
    let items = accept_items(&req.handoff);
    let chunks: Vec<&[IngestItem]> = items.chunks(crate::ingest_api::MAX_ITEMS).collect();
    let many = chunks.len() > 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(n, chunk)| IngestRequest {
            items: chunk.to_vec(),
            turns: false,
            source: ACCEPT_SOURCE.into(),
            actor: req.actor.clone(),
            thread_key: Some(format!("handoff:{}", req.handoff.handoff_id)),
            metadata: Some(json!({
                "handoff_id": req.handoff.handoff_id,
                "from_agent": req.handoff.from_agent,
                "to_agent": req.handoff.to_agent,
                "expires_at": req.handoff.expires_at,
            })),
            idempotency_key: if many { format!("{}:{n}", req.idempotency_key) } else { req.idempotency_key.clone() },
            namespace: Namespace::AgentScratch,
        })
        .collect()
}

/// The `handoff.accept` envelope: the combined receipt and the events billed.
pub fn render_accept(req: &AcceptRequest, outcomes: &[IngestOutcome], latency_ms: u64) -> String {
    let event_ids: Vec<i64> = outcomes.iter().flat_map(|o| o.event_ids.iter().copied()).collect();
    let replayed = !outcomes.is_empty() && outcomes.iter().all(|o| o.replayed);
    let billed: usize = outcomes.iter().filter(|o| !o.replayed).map(|o| o.event_ids.len()).sum();
    json!({
        "tool": Tool::HandoffAccept.wire_name(),
        "result": {
            "handoff_id": req.handoff.handoff_id,
            "from_agent": req.handoff.from_agent,
            "to_agent": req.handoff.to_agent,
            "task": req.handoff.task,
            "accepted": event_ids.len(),
            "inserted": outcomes.iter().map(|o| o.inserted).sum::<usize>(),
            "deduplicated": outcomes.iter().map(|o| o.deduplicated).sum::<usize>(),
            "rejected": outcomes.iter().map(|o| o.rejected).sum::<usize>(),
            "skipped": req.handoff.skipped,
            "replayed": replayed,
            "event_ids": event_ids,
            "thread_key": outcomes.iter().find_map(|o| o.thread_key.clone()),
            "namespace": Namespace::AgentScratch.as_str(),
            "idempotency_key": req.idempotency_key,
        },
        "usage": { "unit": "events", "quantity": billed },
        "latency_ms": latency_ms,
        "hints": ["Read it back with memory.recall and include_scratch: true."],
        "next_action": Value::Null,
    })
    .to_string()
}

/// The `next_action` a refusal's code calls for (REST / gateway bodies).
pub fn refusal_next_action(code: &str) -> &'static str {
    match code {
        "handoff_expired" => "retry_after",
        "handoff_not_for_you" => "not_allowed",
        _ => "fix_input",
    }
}

/// Parse, run and render one call for any face. `now_ms` stamps `created_at` and judges expiry.
pub fn serve<B: crate::backend::MemoryBackend + ?Sized>(tool: Tool, args: &Value, backend: &B, now_ms: i64) -> Result<String, Refusal> {
    let started = std::time::Instant::now();
    let elapsed = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match tool {
        Tool::HandoffCreate => {
            let request = create_request(args).map_err(Refusal::BadArguments)?;
            let from_agent = request.from_agent.clone().or_else(default_from_agent).ok_or_else(|| {
                Refusal::BadArguments("missing_from_agent: pass from_agent, your agent id; the caller has none to default to.".into())
            })?;
            let recall = RecallRequest { query: request.query.clone(), budget_tokens: request.budget_tokens, include_scratch: false };
            crate::backend::validate_search_query(&recall.query)
                .map_err(|policy| Refusal::BadArguments(format!("{}: the query was refused.", policy.as_str())))?;
            let outcome = backend.recall(&recall).map_err(|_| Refusal::Unavailable("recall_unavailable"))?;
            let handoff = build(&outcome, &request, &from_agent, now_ms, &fresh_id("hof")?);
            let parts = a2a(&handoff, &fresh_id("msg")?);
            Ok(render_create(&handoff, &parts, elapsed()))
        }
        Tool::HandoffAccept => {
            let request = accept_request(args, now_ms).map_err(Refusal::BadArguments)?;
            let mut outcomes = Vec::new();
            for ingest in ingest_requests(&request) {
                outcomes.push(backend.ingest(&ingest).map_err(|_| Refusal::Unavailable("ingest_unavailable"))?);
            }
            Ok(render_accept(&request, &outcomes, elapsed()))
        }
        _ => Err(Refusal::BadArguments("invalid_request: not a handoff tool.".into())),
    }
}

/// The MCP `inputSchema` of `handoff.create`.
pub fn create_schema() -> Value {
    json!({
        "task": { "type": "string", "maxLength": MAX_TEXT, "description": "What the receiving agent is asked to do" },
        "to_agent": { "type": "string", "maxLength": MAX_AGENT_ID, "description": "The receiving agent's id" },
        "from_agent": { "type": "string", "maxLength": MAX_AGENT_ID, "description": "Your agent id; default the caller's" },
        "query": { "type": "string", "maxLength": MAX_TEXT, "description": "What to recall for the task; default task" },
        "budget_tokens": { "type": "integer", "minimum": MIN_BUDGET_TOKENS, "maximum": MAX_BUDGET_TOKENS, "description": "The recall's token budget (default 2000)" },
        "ttl_seconds": { "type": "integer", "minimum": MIN_TTL_SECONDS, "maximum": MAX_TTL_SECONDS, "description": "How long the handoff is valid (default 3600)" },
        "thread_key": { "type": "string", "maxLength": MAX_THREAD_KEY, "description": "Your task or conversation id" }
    })
}

/// The MCP `inputSchema` of `handoff.accept`.
pub fn accept_schema() -> Value {
    let mut handoff = Map::new();
    handoff.insert("type".into(), json!("object"));
    handoff.insert(
        "description".into(),
        json!("The shogunagent-handoff/v1 object, its A2A data part, or the A2A message that carries it"),
    );
    json!({
        "handoff": handoff,
        "actor": { "type": "string", "maxLength": crate::ingest_api::MAX_ACTOR_CHARS, "description": "Who accepts: your agent id (must equal the handoff's to_agent)" },
        "idempotency_key": { "type": "string", "maxLength": crate::ingest_api::MAX_KEY_CHARS - 4, "description": "Default handoff:<handoff_id>:<actor>; a repeat replays" },
        "check_addressee": { "type": "boolean", "default": true, "description": "Refuse a handoff addressed to another agent" }
    })
}

/// The `tools/list` description of `handoff.create`.
pub const CREATE_DESCRIPTION: &str = "Package the state of work for a task to hand to another agent: the people, projects, commitments and open loops the task touches, each with its confidence band and the quote and event id that prove it, as a typed shogunagent-handoff/v1 object plus the A2A data part to send. Low-confidence rows never travel. Not a search (memory.recall) and not an export of everything (state.export). Free read.";

/// The `tools/list` description of `handoff.accept`.
pub const ACCEPT_DESCRIPTION: &str = "Accept a handoff another agent sent you: checks the format, that it is addressed to you and not expired, then stores it in your own agent_scratch namespace (never the user's world) under thread handoff:<id>; accepting it again replays. Read it back with memory.recall include_scratch. Not for your own notes (memory.ingest). Metered per event stored (ingest).";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest_api::RecallItem;

    fn item(kind: &'static str, id: i64, band: &'static str, event: Option<i64>) -> RecallItem {
        RecallItem { kind, id, text: format!("{kind} {id}"), confidence: if band == "high" { 0.9 } else { 0.6 }, band, quote: Some(format!("quote {id}")), event_id: event }
    }

    fn create() -> CreateRequest {
        create_request(&json!({ "task": "Draft the Heron note", "to_agent": "agent:writer", "ttl_seconds": 600 })).unwrap()
    }

    #[test]
    fn create_parses_and_bounds_its_arguments() {
        let req = create();
        assert_eq!((req.query.as_str(), req.budget_tokens, req.ttl_seconds), ("Draft the Heron note", DEFAULT_BUDGET_TOKENS, 600));
        assert!(create_request(&json!({ "to_agent": "a:b" })).unwrap_err().starts_with("missing_task:"));
        assert!(create_request(&json!({ "task": "x" })).unwrap_err().starts_with("missing_to_agent:"));
        assert!(create_request(&json!({ "task": "x", "to_agent": "bad id" })).unwrap_err().starts_with("invalid_to_agent:"));
        assert!(create_request(&json!({ "task": "x", "to_agent": "a:b", "ttl_seconds": 5 })).unwrap_err().starts_with("invalid_ttl_seconds:"));
        assert!(create_request(&json!({ "task": "x", "to_agent": "a:b", "budget_tokens": 50 })).unwrap_err().starts_with("invalid_budget_tokens:"));
        assert!(create_request(&json!("x")).unwrap_err().starts_with("invalid_request:"));
    }

    #[test]
    fn build_keeps_high_and_medium_rows_with_provenance_and_merges_duplicates() {
        let recall = RecallOutcome {
            items: vec![
                item("person", 1, "high", Some(10)),
                item("person", 1, "high", Some(11)),
                item("commitment", 7, "medium", Some(12)),
                item("open_loop", 3, "low", Some(13)),
                item("project", 4, "high", None),
            ],
            truncated: false,
            ..Default::default()
        };
        let h = build(&recall, &create(), "agent:planner", 1_000, "hof_00000000-aaaa");
        assert_eq!(h["format"], FORMAT);
        assert_eq!(h["expires_at"], 601_000);
        assert_eq!(h["entities"].as_array().unwrap().len(), 1);
        assert_eq!(h["entities"][0]["provenance"].as_array().unwrap().len(), 2);
        assert_eq!(h["facts"][0]["band"], "medium");
        assert_eq!(h["facts"].as_array().unwrap().len(), 1, "low never travels");
        assert_eq!(h["truncated"], false);
        let parts = a2a(&h, "msg_1");
        assert_eq!(parts["part"]["data"], h);
        assert_eq!(parts["message"]["metadata"]["handoff_id"], "hof_00000000-aaaa");
    }

    /// The object `build` makes carries exactly the fields docs/api/handoff.schema.json declares
    /// (every required one, nothing it forbids), so the gateway's schema and export-v1 subset
    /// tests (apps/api/test/handoff.test.ts) speak about what the core really sends.
    #[test]
    fn the_built_object_has_the_schema_fields() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/handoff.schema.json");
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("schema")).expect("JSON");
        let recall = RecallOutcome { items: vec![item("person", 1, "high", Some(10)), item("commitment", 7, "medium", Some(12))], ..Default::default() };
        let h = build(&recall, &create(), "agent:planner", 1_000, "hof_00000000-0000-4000-8000-000000000000");
        let keys = |v: &Value| -> Vec<String> {
            let mut k: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
            k.sort();
            k
        };
        let check = |object: &Value, def: &Value| {
            let allowed = keys(&def["properties"]);
            for key in keys(object) {
                assert!(allowed.contains(&key), "{key} is not in the schema");
            }
            for required in def["required"].as_array().expect("required") {
                assert!(object.get(required.as_str().expect("name")).is_some(), "{required} missing");
            }
        };
        check(&h, &schema);
        check(&h["task"], &schema["properties"]["task"]);
        check(&h["entities"][0], &schema["$defs"]["entity"]);
        check(&h["facts"][0], &schema["$defs"]["fact"]);
        assert!(valid_handoff_id(h["handoff_id"].as_str().expect("id")));
        let fresh = fresh_id("hof").expect("rng");
        assert!(valid_handoff_id(&fresh), "{fresh}");
    }

    fn sample(expires_at: i64) -> Value {
        json!({
            "format": FORMAT, "handoff_id": "hof_12345678-abcd", "created_at": 1, "expires_at": expires_at,
            "from_agent": "agent:planner", "to_agent": "agent:writer",
            "task": { "summary": "Draft the Heron note", "query": "Heron", "thread_key": null },
            "entities": [{ "type": "entity", "kind": "person", "id": 1, "name": "Oren", "confidence": 0.9, "band": "high", "provenance": [{ "event_id": 4, "quote": "Oren: hi" }] }],
            "facts": [
                { "type": "fact", "kind": "open_loop", "id": 3, "text": "budget numbers", "confidence": 0.6, "band": "medium", "provenance": [{ "event_id": 5, "quote": null }] },
                { "type": "fact", "kind": "commitment", "id": 9, "text": "x", "confidence": 0.2, "band": "low", "provenance": [{ "event_id": 6, "quote": null }] }
            ],
            "truncated": false
        })
    }

    #[test]
    fn accept_validates_format_addressee_and_expiry() {
        let ok = accept_request(&json!({ "handoff": sample(100), "actor": "agent:writer" }), 50).unwrap();
        assert_eq!(ok.idempotency_key, "handoff:hof_12345678-abcd:agent:writer");
        assert_eq!(ok.handoff.skipped, 1);
        // The A2A message and the create result locate the same object.
        let message = json!({ "kind": "message", "parts": [{ "kind": "data", "data": sample(100) }] });
        assert_eq!(accept_request(&json!({ "handoff": message, "actor": "agent:writer" }), 50).unwrap(), ok);
        let wrapped = json!({ "handoff": sample(100), "a2a": {} });
        assert_eq!(accept_request(&json!({ "handoff": wrapped, "actor": "agent:writer" }), 50).unwrap(), ok);
        assert!(accept_request(&json!({ "handoff": sample(100), "actor": "agent:writer" }), 100).unwrap_err().starts_with("handoff_expired:"));
        assert!(accept_request(&json!({ "handoff": sample(100), "actor": "agent:other" }), 50).unwrap_err().starts_with("handoff_not_for_you:"));
        assert!(accept_request(&json!({ "handoff": sample(100), "actor": "agent:other", "check_addressee": false }), 50).is_ok());
        assert!(accept_request(&json!({ "handoff": sample(100) }), 50).unwrap_err().starts_with("missing_actor:"));
        assert!(accept_request(&json!({ "handoff": { "format": "x" }, "actor": "a:b" }), 50).unwrap_err().starts_with("invalid_handoff:"));
        let mut bad = sample(100);
        bad["facts"][0]["band"] = json!("certain");
        assert!(accept_request(&json!({ "handoff": bad, "actor": "agent:writer" }), 50).unwrap_err().starts_with("invalid_handoff:"));
    }

    #[test]
    fn accept_lands_in_scratch_with_possibly_on_medium_rows() {
        let req = accept_request(&json!({ "handoff": sample(100), "actor": "agent:writer" }), 50).unwrap();
        let ingests = ingest_requests(&req);
        assert_eq!(ingests.len(), 1);
        let ingest = &ingests[0];
        assert_eq!(ingest.namespace, Namespace::AgentScratch);
        assert_eq!(ingest.thread_key.as_deref(), Some("handoff:hof_12345678-abcd"));
        assert_eq!(ingest.items.len(), 3, "task + entity + medium fact; low skipped");
        assert!(ingest.items[1].content.contains("\"Oren: hi\" (event 4)"));
        assert!(ingest.items[2].content.starts_with("possibly: open loop: budget numbers"));
    }

    #[test]
    fn a_long_actor_gets_a_hashed_key_that_fits() {
        let actor = format!("agent:{}", "a".repeat(120));
        let key = default_idempotency_key("hof_12345678-abcd", &actor);
        assert!(key.chars().count() <= crate::ingest_api::MAX_KEY_CHARS);
        assert_eq!(key, default_idempotency_key("hof_12345678-abcd", &actor));
    }

    #[test]
    fn a_backend_without_recall_or_ingest_answers_unavailable() {
        let backend = crate::backend::StubBackend;
        let args = json!({ "task": "x", "to_agent": "agent:w", "from_agent": "agent:p" });
        assert_eq!(serve(Tool::HandoffCreate, &args, &backend, 1), Err(Refusal::Unavailable("recall_unavailable")));
        let args = json!({ "handoff": sample(100), "actor": "agent:writer" });
        assert_eq!(serve(Tool::HandoffAccept, &args, &backend, 1), Err(Refusal::Unavailable("ingest_unavailable")));
    }
}
