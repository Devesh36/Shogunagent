//! The chat tool lane over `chat/completions` function calling (issue #396, owner decision
//! 2026-10-08): OpenRouter and Gemini keys run the same tool loop as an Anthropic key (Messages
//! API, `llm::tool_turns`) and an OpenAI key (Responses API, `openai_responses::tool_turns`).
//! This module is the third [`ModelTurnSource`] behind `agent_source::chat_tool_lane`, built to
//! the Responses lane's layout so the two OpenAI-shaped lanes read the same way.
//!
//! Scope and invariants — the same as the other tool lanes:
//! - **Agent lane only, BYOK only (invariant 5).** [`CompatToolTurnHttp`] is constructed with a
//!   [`ByokKey`](crate::llm::ByokKey); a Select KK key is a type error. Nothing here is reachable
//!   from the Batch lane, and subscription delegates stay tool-free (issue #484).
//! - **Function tools only.** [`function_tools`] emits `type: "function"` entries built from
//!   SHOGUN's own catalog, so every tool the model sees is a catalog entry gated by
//!   `service_gate::authorize_op`. [`build_compat_tool_turn_body`] is the only `chat/completions`
//!   builder that adds a `tools` key; the tool-free builders in `openai_compat.rs` are pinned by
//!   test.
//! - **Stateless.** chat/completions has no provider-side continuation: every turn replays the
//!   system message, the opening messages, each assistant message with its `tool_calls`, and one
//!   `tool` message per result.
//! - **Traceability at the true egress point (invariant 3, AR-11).** One digest-only row per HTTP
//!   attempt, written *before* the socket, on `Route::MessagesApi` with the `agent_tools` purpose.
//! - **Usage (FR-HN-01).** One `UsageRecord` per HTTP attempt, from the provider's `usage` (the
//!   streamed request asks for it with `stream_options.include_usage`), or a byte estimate when
//!   the provider sent none.
//! - **Secrets.** The key reaches the wire only through `openai_compat::bearer_headers`; `Err`
//!   strings never carry body text or the key.
//!
//! ## Wire shape (OpenAI `chat/completions`, as OpenRouter and Gemini's compat layer document it)
//! - request: `tools: [{type: "function", function: {name, description, parameters}}]`;
//!   assistant history `{role: "assistant", content, tool_calls: [{id, type: "function",
//!   function: {name, arguments}}]}` (arguments is a JSON string); results
//!   `{role: "tool", tool_call_id, content}`;
//! - response: `choices[0].message {content, tool_calls, refusal}`, `choices[0].finish_reason`
//!   (`stop` / `length` / `tool_calls` / `content_filter` / `error`), `usage {prompt_tokens,
//!   completion_tokens, prompt_tokens_details.cached_tokens}`;
//! - stream: `choices[0].delta.content` text, `choices[0].delta.tool_calls[] {index, id, type,
//!   function {name, arguments}}` fragments (id and name first, arguments as string pieces),
//!   `finish_reason` on the last choice chunk, a `usage` chunk with empty `choices`, `[DONE]`.
//!
//! NOT verified against a vendor source (flagged where used): that every compatible host fills
//! `index` on streamed tool-call fragments ([`CompatStream`] falls back to the fragment's position
//! in the array), and that provider-specific fields on a tool call (Gemini's `extra_content`
//! thought signature) must come back unchanged — they are kept verbatim, streamed or not.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use shogun_mcp::tool_loop::{ModelTurn, ModelTurnSource, ToolResult, ToolUse};

use super::{build_chat_body, OpenAiCompatConfig};
use crate::llm::openai_responses::TOOL_ERROR_PREFIX;
use crate::llm::tool_turns::{system_with_connected_services, MessagesHttp, LENGTH_LIMIT_NOTE};
use crate::llm::LlmError;

/// Map catalog tool definitions (`shogun_mcp::tool_catalog::tool_definitions`, Messages-API
/// shape: `name` / `description` / `input_schema`) onto chat/completions **function** tools.
///
/// Only `type: "function"` is ever produced. `strict` is not sent (the endpoint's default is
/// non-strict, and the catalog's schemas are not written for strict mode). Entries with no name
/// cannot be called and are skipped.
pub fn function_tools(catalog: &[Value]) -> Vec<Value> {
    catalog
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?;
            let parameters = t
                .get("input_schema")
                .or_else(|| t.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let mut function = json!({ "name": name, "parameters": parameters });
            if let Some(d) = t.get("description").and_then(Value::as_str) {
                function["description"] = json!(d);
            }
            Some(json!({ "type": "function", "function": function }))
        })
        .collect()
}

/// Render one tool turn's `chat/completions` body: [`build_chat_body`] (model, the configured
/// output cap and reasoning fields, the streaming flags when `stream`) plus `tools` when there
/// are any. `tools` must already be in function-tool shape ([`function_tools`]).
pub fn build_compat_tool_turn_body(
    cfg: &OpenAiCompatConfig,
    messages: Vec<Value>,
    tools: &[Value],
    stream: bool,
) -> Value {
    let mut body = build_chat_body(cfg, messages, stream);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

/// A `{role, content}` message.
pub fn message(role: &str, text: &str) -> Value {
    json!({ "role": role, "content": text })
}

/// A tool result as a `tool` message. The message has no `is_error` field, so a failure or
/// refusal is marked in the content ([`TOOL_ERROR_PREFIX`], the Responses lane's convention).
pub fn tool_result_message(r: &ToolResult) -> Value {
    let content = crate::llm::tool_turns::egress_tool_result_content(r);
    let content = if r.is_error { format!("{TOOL_ERROR_PREFIX}{content}") } else { content };
    json!({ "role": "tool", "tool_call_id": r.id, "content": content })
}

/// What one `chat/completions` body parses to for the loop.
#[derive(Debug, Clone)]
pub struct ParsedCompatTurn {
    pub turn: ModelTurn,
    /// The assistant message to replay on the next turn: `role`, `content` and the `tool_calls`
    /// exactly as the provider sent them (ids, and any provider fields on a call).
    pub assistant_message: Value,
}

/// The error an `error` object stands for, from its code only — the free-text `message` may
/// quote the request, so it never travels into an error string.
fn error_object_failure(error: &Value) -> LlmError {
    if crate::llm::policy_stop_code(&json!({ "error": error }).to_string()) {
        return LlmError::PolicyStopped;
    }
    let status = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..600).contains(c));
    match status {
        Some(status) => crate::llm::status_error("tool turn", status, ""),
        None => LlmError::Provider("tool turn: provider error".to_string()),
    }
}

/// `Some` when a 200 body reports a failure instead of an answer (an `error` object).
pub fn body_failure(body: &Value) -> Option<LlmError> {
    body.get("error").filter(|e| !e.is_null()).map(error_object_failure)
}

/// The text of a message `content`: a string, or an array of parts whose `text` is joined.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn parse_tool_call(call: &Value) -> Result<ToolUse, String> {
    if call.get("type").and_then(Value::as_str).is_some_and(|t| t != "function") {
        return Err("tool call is not a function call".into());
    }
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "tool call missing id".to_string())?;
    let function = call.get("function").ok_or_else(|| "tool call missing function".to_string())?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "tool call missing name".to_string())?;
    let raw = match function.get("arguments") {
        Some(Value::String(s)) => s.as_str(),
        None | Some(Value::Null) => "",
        Some(_) => return Err("tool call arguments are not a JSON string".into()),
    };
    let raw = if raw.trim().is_empty() { "{}" } else { raw };
    let input = serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| "tool call arguments are not a JSON object".to_string())?;
    Ok(ToolUse { id: id.to_string(), name: name.to_string(), input })
}

/// Parse one `chat/completions` body (non-streaming, or [`CompatStream::into_body`]) into the
/// loop's [`ModelTurn`]:
///
/// - `message.tool_calls` → [`ModelTurn::ToolUses`] in call order (id = the tool call's id, the
///   arguments parsed from their JSON string). A call cut at the length limit, or one whose
///   arguments are not a JSON object, is refused — never run on a guess.
/// - otherwise [`ModelTurn::Final`] with the text, by the other tool lanes' rules: an empty answer
///   is an error, an answer cut at the output-token limit carries [`LENGTH_LIMIT_NOTE`], and an
///   answer with no finish reason (the stream ended before the provider finished it) is refused.
/// - `content_filter`, a refusal, or a policy-coded error object is `"provider_policy_stop"`.
///
/// `Err` never echoes body text.
pub fn parse_compat_tool_turn(body: &Value) -> Result<ParsedCompatTurn, String> {
    if let Some(e) = body_failure(body) {
        return Err(e.to_string());
    }
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| "chat/completions response has no choices".to_string())?;
    let finish = choice.get("finish_reason").and_then(Value::as_str);
    if finish == Some("content_filter") {
        return Err(LlmError::PolicyStopped.to_string());
    }
    if finish == Some("error") {
        return Err("tool turn: provider error".into());
    }
    let msg = choice
        .get("message")
        .filter(|m| m.is_object())
        .ok_or_else(|| "chat/completions response has no message".to_string())?;
    if msg.get("refusal").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
        return Err(LlmError::PolicyStopped.to_string());
    }
    let calls: &[Value] = match msg.get("tool_calls") {
        Some(Value::Array(calls)) => calls,
        None | Some(Value::Null) => &[],
        Some(_) => return Err("tool_calls is not an array".into()),
    };
    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let turn = if !calls.is_empty() {
        match finish {
            Some("length") => return Err("a tool call was cut at the length limit".into()),
            None => return Err("a tool call ended before the provider finished it".into()),
            Some(_) => {}
        }
        ModelTurn::ToolUses(calls.iter().map(parse_tool_call).collect::<Result<_, _>>()?)
    } else {
        if matches!(finish, Some("tool_calls") | Some("function_call")) {
            return Err("finish_reason is tool_calls but no tool call is present".into());
        }
        let mut text = content_text(Some(&content));
        if text.trim().is_empty() {
            return Err("The model returned no answer.".into());
        }
        match finish {
            None => return Err("the answer ended before the provider finished it".into()),
            Some("length") => text.push_str(LENGTH_LIMIT_NOTE),
            Some(_) => {}
        }
        ModelTurn::Final(text)
    };
    let mut assistant_message = json!({ "role": "assistant", "content": content });
    if !calls.is_empty() {
        assistant_message["tool_calls"] = json!(calls);
    }
    Ok(ParsedCompatTurn { turn, assistant_message })
}

/// Rebuilds one `chat/completions` body from its streamed chunks, so a streamed turn is parsed by
/// the same [`parse_compat_tool_turn`] as a non-streaming one — the chat/completions counterpart
/// of `openai_responses::ResponsesStream`.
///
/// - `delta.content` is forwarded to the caller **until the first tool-call fragment arrives**;
///   text after that is history only (the rule on every tool lane).
/// - `delta.tool_calls[]` fragments are accumulated by `index` (or, when a host leaves it out,
///   the fragment's position): `id`, `type` and `function.name` are taken from the first fragment
///   that carries them, `function.arguments` pieces are concatenated, and any other field on the
///   call (a provider signature) is kept as sent. Fragments that never add up to a JSON object
///   make [`parse_compat_tool_turn`] refuse the turn — never an invented call.
/// - `finish_reason` and the `usage` chunk are kept for the rebuilt body; a `refusal` delta marks
///   the turn as a policy stop.
#[derive(Debug, Default)]
pub struct CompatStream {
    text: String,
    calls: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    finish_reason: Option<String>,
    refused: bool,
    usage: Option<Value>,
    saw_tool_call: bool,
}

/// Absent, `null`, or an empty string: a streamed fragment that says nothing about the field.
fn is_blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

impl CompatStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a tool-call fragment has arrived (after which text is no longer forwarded).
    pub fn saw_tool_call(&self) -> bool {
        self.saw_tool_call
    }

    fn merge_call(&mut self, index: u64, fragment: &Value) {
        let Some(fragment) = fragment.as_object() else { return };
        let call = self.calls.entry(index).or_insert_with(|| json!({}));
        for (k, v) in fragment {
            match k.as_str() {
                "index" => {}
                "function" => {
                    let Some(function) = v.as_object() else { continue };
                    for (fk, fv) in function {
                        match (fk.as_str(), fv) {
                            ("arguments", Value::String(piece)) => {
                                self.arguments.entry(index).or_default().push_str(piece);
                            }
                            ("arguments", _) => {}
                            (_, Value::String(s)) if s.is_empty() => {}
                            (_, Value::Null) => {}
                            _ => {
                                if is_blank(call.get("function").and_then(|f| f.get(fk))) {
                                    call["function"][fk.as_str()] = fv.clone();
                                }
                            }
                        }
                    }
                }
                "id" | "type" => {
                    if !is_blank(Some(v)) && is_blank(call.get(k)) {
                        call[k.as_str()] = v.clone();
                    }
                }
                _ if !v.is_null() => call[k.as_str()] = v.clone(),
                _ => {}
            }
        }
    }

    /// Feed one SSE `data:` object. Returns the text the caller should paint now, if any.
    pub fn observe(&mut self, event: &Value) -> Option<String> {
        if let Some(u) = event.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let choice = event.get("choices")?.as_array()?.first()?;
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = choice.get("delta")?;
        if delta.get("refusal").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
            self.refused = true;
        }
        if let Some(fragments) = delta.get("tool_calls").and_then(Value::as_array) {
            for (pos, fragment) in fragments.iter().enumerate() {
                let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
                self.saw_tool_call = true;
                self.merge_call(index, fragment);
            }
        }
        let text = delta.get("content")?.as_str()?;
        if text.is_empty() {
            return None;
        }
        self.text.push_str(text);
        (!self.saw_tool_call && !self.refused).then(|| text.to_string())
    }

    /// The turn in the non-streaming shape, ready for [`parse_compat_tool_turn`]. `finished` is
    /// whether the provider ended the stream ([`SseDecoder::finished`]): `[DONE]` with no finish
    /// reason reads as `stop`; a stream that simply closed keeps `finish_reason: null`, so a
    /// partial answer or a half-streamed call is refused.
    ///
    /// [`SseDecoder::finished`]: crate::llm::sse::SseDecoder::finished
    pub fn into_body(self, finished: bool) -> Value {
        let calls: Vec<Value> = self
            .calls
            .into_iter()
            .map(|(index, mut call)| {
                if call.get("function").is_none() {
                    call["function"] = json!({});
                }
                let args = self.arguments.get(&index).cloned().unwrap_or_default();
                call["function"]["arguments"] = json!(args);
                call
            })
            .collect();
        let content = if self.text.is_empty() { Value::Null } else { json!(self.text) };
        let mut message = json!({ "role": "assistant", "content": content });
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        if self.refused {
            // A flag only: the refusal text is never kept.
            message["refusal"] = json!("refused");
        }
        let finish = match self.finish_reason {
            Some(reason) => json!(reason),
            None if finished => json!("stop"),
            None => Value::Null,
        };
        let mut body = json!({ "choices": [{ "index": 0, "message": message, "finish_reason": finish }] });
        if let Some(usage) = self.usage {
            body["usage"] = usage;
        }
        body
    }
}

/// The Agent-lane [`ModelTurnSource`] over `chat/completions`.
///
/// Stateless: each turn sends the system message, every earlier message (the opening thread,
/// each assistant message with its `tool_calls`, each `tool` result), then this turn's results.
/// The system message (instructions + Connected-services block) and `tools` are fixed for the
/// conversation (spec §4-1). State advances only after a reply parses, so a failed turn leaves
/// nothing half-applied for a retry to build on.
///
/// Tool results go back exactly as the loop produced them — already fenced as untrusted data by
/// `shogun_mcp::tool_loop`, refusals marked with [`TOOL_ERROR_PREFIX`] — and a mid-run user note
/// (issue #395) rides as a user message after them, never as a system message.
pub struct CompatToolTurnSource<H: MessagesHttp> {
    http: H,
    cfg: OpenAiCompatConfig,
    system: String,
    tools: Vec<Value>,
    /// The opening messages (the question, or the chat thread), until the first turn succeeds.
    opening: Vec<Value>,
    /// Every message sent or received after the system message, so far.
    history: Vec<Value>,
    /// Mid-run user notes (issue #395) waiting for the next request.
    pending_note: Option<String>,
}

impl<H: MessagesHttp> CompatToolTurnSource<H> {
    /// `catalog_tools` is `tool_definitions` output (Messages shape); it is mapped to function
    /// tools here.
    pub fn new(
        http: H,
        cfg: OpenAiCompatConfig,
        system: impl Into<String>,
        catalog_tools: &[Value],
        user_prompt: &str,
    ) -> Self {
        Self {
            http,
            cfg,
            system: system.into(),
            tools: function_tools(catalog_tools),
            opening: vec![message("user", user_prompt)],
            history: Vec::new(),
            pending_note: None,
        }
    }

    /// The chat path's source: the same inputs `AnthropicToolTurnSource::from_chat` and
    /// `ResponsesToolTurnSource::from_chat` take. The system message is the chat system prompt
    /// plus the Connected-services block; the opening messages are the remembered facts (fenced as
    /// untrusted user evidence, FR-HN-05 layer 2, never system-level) followed by the normalized
    /// thread and this turn — the same shape the tool-free chat/completions chat sends.
    pub fn from_chat(
        http: H,
        cfg: OpenAiCompatConfig,
        request: &crate::llm::conversation::ChatRequest,
        connected_block: Option<&str>,
        catalog_tools: &[Value],
    ) -> Self {
        use crate::chat_prompt::Role;
        let stable = request.stable_context.trim();
        let facts = (!stable.is_empty())
            .then(|| message("user", &crate::llm::prompt_cache::untrusted_stable_context(stable)));
        let opening = facts
            .into_iter()
            .chain(request.normalized_turns().iter().map(|t| {
                let role = match t.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                json!({"role":role, "content":t.compat_content()})
            }))
            .collect();
        Self {
            http,
            cfg,
            system: system_with_connected_services(&request.system, connected_block),
            tools: function_tools(catalog_tools),
            opening,
            history: Vec::new(),
            pending_note: None,
        }
    }

    pub fn http(&self) -> &H {
        &self.http
    }
}

impl<H: MessagesHttp> ModelTurnSource for CompatToolTurnSource<H> {
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        let mut fresh: Vec<Value> = self.opening.clone();
        fresh.extend(results.iter().map(tool_result_message));
        // A note rides after this turn's results as the user's own message. It is consumed only
        // once the reply parses, so a failed turn resends it.
        if let Some(note) = &self.pending_note {
            fresh.push(message("user", note));
        }
        let system = self.system.trim();
        let messages: Vec<Value> = (!system.is_empty())
            .then(|| message("system", system))
            .into_iter()
            .chain(self.history.iter().cloned())
            .chain(fresh.iter().cloned())
            .collect();
        let body = build_compat_tool_turn_body(&self.cfg, messages, &self.tools, false);
        let resp = self.http.post(&body)?;
        let parsed = parse_compat_tool_turn(&resp)?;
        self.history.extend(fresh);
        self.history.push(parsed.assistant_message);
        self.opening.clear();
        self.pending_note = None;
        Ok(parsed.turn)
    }

    fn accepts_user_notes(&self) -> bool {
        true
    }

    fn push_user_note(&mut self, note: String) {
        match &mut self.pending_note {
            Some(waiting) => {
                waiting.push_str("\n\n");
                waiting.push_str(&note);
            }
            None => self.pending_note = Some(note),
        }
    }
}

/// [`MessagesHttp`] over a real transport for `chat/completions`, carrying the BYOK key — the twin
/// of `openai_responses::tool_turns::ResponsesToolTurnHttp`, with the same guarantees (the key is
/// a [`ByokKey`](crate::llm::ByokKey), invariant 5).
#[cfg(feature = "net")]
pub struct CompatToolTurnHttp<T, S> {
    pub(super) transport: T,
    pub(super) sink: S,
    key: crate::llm::ByokKey,
    cfg: OpenAiCompatConfig,
    turn_timeout: std::time::Duration,
    usage: Option<crate::llm::usage::UsageHook>,
    /// Streamed answer text as provisional
    /// [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s — the same events the other tool
    /// lanes send. Tool-call arguments never go here.
    text_out: Option<std::sync::mpsc::Sender<crate::llm::tool_turns::ToolTextEvent>>,
    model_turn: std::sync::atomic::AtomicU32,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(feature = "net")]
type TurnErr = (String, crate::llm::usage::UsageOutcome);

#[cfg(feature = "net")]
impl<T, S: crate::llm::traceability::TraceabilitySink> CompatToolTurnHttp<T, S> {
    pub fn new(transport: T, sink: S, key: crate::llm::ByokKey, cfg: OpenAiCompatConfig) -> Self {
        Self {
            transport,
            sink,
            key,
            cfg,
            turn_timeout: std::time::Duration::from_millis(
                crate::llm::tool_turns::ToolTurnHttp::<T, S>::DEFAULT_TURN_TIMEOUT_MS,
            ),
            usage: None,
            text_out: None,
            model_turn: std::sync::atomic::AtomicU32::new(0),
            cancelled: None,
        }
    }

    /// Paint streamed answer text as [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s
    /// (used by [`Self::post_streaming_events`]).
    pub fn with_text_out(
        mut self,
        tx: std::sync::mpsc::Sender<crate::llm::tool_turns::ToolTextEvent>,
    ) -> Self {
        self.text_out = Some(tx);
        self
    }

    /// Abandon an in-flight turn as soon as `cancelled` is set (the user pressed Stop).
    pub fn with_cancellation(mut self, cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    pub fn with_turn_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.turn_timeout = timeout;
        self
    }

    pub fn with_usage(mut self, hook: crate::llm::usage::UsageHook) -> Self {
        self.usage = Some(hook);
        self
    }

    async fn wait_for_cancel(&self) {
        let Some(cancelled) = &self.cancelled else {
            return std::future::pending().await;
        };
        while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn record_usage(
        &self,
        started: std::time::Instant,
        first_token_ms: Option<u64>,
        usage: crate::llm::usage::Usage,
        outcome: crate::llm::usage::UsageOutcome,
    ) {
        if let Some(hook) = &self.usage {
            hook.sink.record(crate::llm::usage::UsageRecord::new(
                crate::llm::traceability::Route::MessagesApi,
                hook.tag,
                &self.cfg.model,
                usage,
                first_token_ms,
                started.elapsed().as_millis() as u64,
                outcome,
            ));
        }
    }

    /// Traceability at the true egress point, before the socket. Digest and byte length only.
    /// `Err` when the ledger would not take the row; the caller then does not send
    /// (fail-closed).
    fn trace(&self, payload: &str) -> Result<(), LlmError> {
        self.sink.try_record(crate::llm::traceability::TraceRecord::for_chunk(
            crate::llm::traceability::Route::MessagesApi,
            "agent_tools",
            self.cfg.destination(),
            payload,
            false,
        ))?;
        Ok(())
    }

    fn failed(e: &LlmError) -> TurnErr {
        (e.to_string(), crate::llm::usage::UsageOutcome::from_error(e))
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::HttpTransport, S: crate::llm::traceability::TraceabilitySink>
    CompatToolTurnHttp<T, S>
{
    /// One model turn, non-streaming. Records egress, sends, returns the `chat/completions` body.
    pub async fn post_async(&self, body: &Value) -> Result<Value, String> {
        use crate::llm::usage::{Usage, UsageOutcome};
        let started = std::time::Instant::now();
        match tokio::time::timeout(self.turn_timeout, self.send_turn(body)).await {
            Ok(Ok((value, usage))) => {
                self.record_usage(started, None, usage, UsageOutcome::Ok);
                Ok(value)
            }
            Ok(Err((reason, outcome))) => {
                self.record_usage(started, None, Usage::default(), outcome);
                Err(reason)
            }
            Err(_) => {
                self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                Err("the model did not answer in time".to_string())
            }
        }
    }

    async fn send_turn(&self, body: &Value) -> Result<(Value, crate::llm::usage::Usage), TurnErr> {
        use crate::llm::usage::{Usage, UsageOutcome};
        let mut wire = body.clone();
        if let Some(obj) = wire.as_object_mut() {
            obj.remove("stream");
            obj.remove("stream_options");
        }
        let payload = wire.to_string();
        let req = super::build_chat_body_request(&self.cfg, self.key.secret(), payload.clone())
            .map_err(|e| (format!("request build failed: {e}"), UsageOutcome::Error))?;
        self.trace(&payload).map_err(|e| Self::failed(&e))?;
        let resp = self
            .transport
            .send(req)
            .await
            .map_err(|e| (format!("transport error: {e}"), UsageOutcome::Error))?;
        if !resp.is_success() {
            return Err(Self::failed(&crate::llm::status_error("tool turn", resp.status, &resp.body)));
        }
        let value: Value = serde_json::from_str(&resp.body)
            .map_err(|_| ("malformed response body".to_string(), UsageOutcome::Parse))?;
        if let Some(e) = body_failure(&value) {
            return Err(Self::failed(&e));
        }
        let usage = crate::llm::usage::openai_usage(&value)
            .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), resp.body.len()));
        Ok((value, usage))
    }

    pub fn post_blocking(&self, rt: &tokio::runtime::Runtime, body: &Value) -> Result<Value, String> {
        rt.block_on(self.post_async(body))
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
    CompatToolTurnHttp<T, S>
{
    /// One chat tool-lane turn: the twin of the Responses lane's `post_streaming_events`, with
    /// the same guarantees (streamed `Delta`s then one `Commit` / `Discard`, cancellation via
    /// [`Self::with_cancellation`], one pre-text retry, traced before the socket, no body text in
    /// `Err`).
    pub async fn post_streaming_events(&self, body: &Value) -> Result<Value, String> {
        use crate::llm::tool_turns::ToolTextEvent;
        use crate::llm::usage::{Usage, UsageOutcome};
        let started = std::time::Instant::now();
        let model_turn = self.model_turn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = tokio::select! {
            biased;
            () = self.wait_for_cancel() => Err("cancelled".to_string()),
            result = tokio::time::timeout(self.turn_timeout, self.send_events(body, model_turn)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                        Err("the model did not answer in time".to_string())
                    }
                }
            }
        };
        let parsed = result.and_then(|body| {
            let turn = parse_compat_tool_turn(&body)?;
            Ok((body, matches!(turn.turn, ModelTurn::Final(_))))
        });
        if let Some(tx) = &self.text_out {
            let event = if matches!(parsed, Ok((_, true))) {
                ToolTextEvent::Commit { model_turn }
            } else {
                ToolTextEvent::Discard { model_turn }
            };
            let _ = tx.send(event);
        }
        parsed.map(|(body, _)| body)
    }

    async fn send_events(&self, body: &Value, model_turn: u32) -> Result<Value, String> {
        use crate::llm::conversation::{retryable_before_first_token, STREAM_RETRY_BACKOFF};
        let mut saw_text = false;
        let mut result = self.send_events_attempt(body, model_turn, &mut saw_text).await;
        if !saw_text && result.as_ref().is_err_and(retryable_before_first_token) {
            // Retry only this model request: the loop has received and run nothing from it.
            tokio::time::sleep(STREAM_RETRY_BACKOFF).await;
            result = self.send_events_attempt(body, model_turn, &mut saw_text).await;
        }
        result.map_err(|e| match e {
            LlmError::Parse(_) => "malformed response body".to_string(),
            other => other.to_string(),
        })
    }

    async fn send_events_attempt(
        &self,
        body: &Value,
        model_turn: u32,
        saw_text: &mut bool,
    ) -> Result<Value, LlmError> {
        use crate::llm::tool_turns::ToolTextEvent;
        use crate::llm::transport::StreamOutcome;
        use crate::llm::usage::{Usage, UsageOutcome};

        let started = std::time::Instant::now();
        let mut wire = body.clone();
        wire["stream"] = json!(true);
        // The usage chunk (FR-HN-01): without it a streamed turn reports no token counts.
        wire["stream_options"] = json!({ "include_usage": true });
        let payload = wire.to_string();
        let req = super::build_chat_body_request(&self.cfg, self.key.secret(), payload.clone())?;
        self.trace(&payload)?;

        let mut decoder = crate::llm::sse::SseDecoder::openai();
        let mut stream = CompatStream::new();
        let text_out = self.text_out.clone();
        let mut first_token: Option<std::time::Instant> = None;
        let mut out_bytes = 0usize;
        let mut abandoned = false;
        let outcome = self
            .transport
            .send_streaming(req, |chunk| {
                for event in decoder.push_events(chunk) {
                    let Some(text) = stream.observe(&event) else { continue };
                    *saw_text = true;
                    first_token.get_or_insert_with(std::time::Instant::now);
                    out_bytes += text.len();
                    if let Some(tx) = &text_out {
                        if tx.send(ToolTextEvent::Delta { model_turn, text }).is_err() {
                            abandoned = true;
                            return false;
                        }
                    }
                }
                true
            })
            .await;
        let first_token_ms = first_token.map(|t| t.duration_since(started).as_millis() as u64);

        let result = match outcome {
            Ok(StreamOutcome::Streamed { .. }) if abandoned => {
                Err(LlmError::Provider("the answer was abandoned".to_string()))
            }
            Ok(StreamOutcome::Streamed { .. }) => match decoder.stream_error("tool turn") {
                Some(e) => Err(e),
                // No finish reason and no `[DONE]`: the stream was cut, so this is not the whole
                // turn (#895) — a transport failure, retried before the first token.
                None if !decoder.finished() => Err(crate::llm::sse::closed_early("tool turn")),
                None => Ok(stream.into_body(true)),
            },
            Ok(StreamOutcome::Failed { status, body }) => {
                Err(crate::llm::status_error("tool turn", status, &body))
            }
            Err(e) => Err(e.into()),
        };
        match &result {
            Ok(_) => {
                let usage = decoder
                    .usage()
                    .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), out_bytes));
                self.record_usage(started, first_token_ms, usage, UsageOutcome::Ok);
            }
            Err(e) => self.record_usage(
                started,
                first_token_ms,
                decoder.usage().unwrap_or_default(),
                UsageOutcome::from_error(e),
            ),
        }
        result
    }

    pub fn post_streaming_events_blocking(
        &self,
        rt: &tokio::runtime::Runtime,
        body: &Value,
    ) -> Result<Value, String> {
        rt.block_on(self.post_streaming_events(body))
    }
}

/// The chat tool lane's transport for OpenRouter / Gemini keys: [`CompatToolTurnHttp`] streaming
/// [`ToolTextEvent`](crate::llm::tool_turns::ToolTextEvent)s through
/// [`CompatToolTurnHttp::post_streaming_events`], bound to the shell's runtime — the counterpart
/// of `RuntimeBoundResponsesChatTurnHttp` and `tool_turns::RuntimeBoundToolTurnHttp`.
#[cfg(feature = "net")]
pub struct RuntimeBoundCompatChatTurnHttp<'a, T, S> {
    inner: CompatToolTurnHttp<T, S>,
    rt: &'a tokio::runtime::Runtime,
}

#[cfg(feature = "net")]
impl<'a, T, S> RuntimeBoundCompatChatTurnHttp<'a, T, S> {
    pub fn new(inner: CompatToolTurnHttp<T, S>, rt: &'a tokio::runtime::Runtime) -> Self {
        Self { inner, rt }
    }

    pub fn inner(&self) -> &CompatToolTurnHttp<T, S> {
        &self.inner
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
    MessagesHttp for RuntimeBoundCompatChatTurnHttp<'_, T, S>
{
    fn post(&mut self, body: &Value) -> Result<Value, String> {
        self.inner.post_streaming_events_blocking(self.rt, body)
    }
}

#[cfg(test)]
mod tests;
