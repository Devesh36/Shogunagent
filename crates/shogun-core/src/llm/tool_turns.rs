//! Agent-lane tool turns (issue #81 step 4): the [`ModelTurnSource`] the tool loop drives, over
//! the real Anthropic Messages API shape.
//!
//! [`shogun_mcp::tool_loop::run_read_loop`] owns the policy — gate, budgets, refusals — and sees
//! the model only through [`ModelTurnSource`]. This module is that seam's Anthropic half: a
//! conversation state that accumulates `messages` across turns, a pure body builder, and a pure
//! response parser, glued together by [`AnthropicToolTurnSource`]. Everything except the socket
//! write is a pure function, mirroring `anthropic.rs`, so the whole exchange is testable on
//! Linux with a scripted [`MessagesHttp`].
//!
//! **Only the Agent chat tool lanes carry tools (invariant 5).** The `tools` array exists in three
//! request builders, one per tool lane — [`build_tool_turn_body`] here (Anthropic key),
//! `openai_responses::build_responses_body` given tools (OpenAI key) and
//! `openai_compat::tool_turns::build_compat_tool_turn_body` (OpenRouter / Gemini keys) — every
//! one of them reached only with a BYOK key. The Batch / Select-KK builders in `anthropic.rs` and
//! the tool-free chat builders in `openai_compat.rs` are pinned tool-free by test. The tools array itself
//! comes from `shogun_mcp::tool_catalog::tool_definitions` (already in the API's own shape) and
//! the system block from `connected_services_block`; nothing here re-derives either. The fourth
//! tool lane, the Claude Code subscription delegate (issue #484, `llm::delegate_tools`), carries
//! no `tools` array at all: the same catalog reaches it as the one MCP server it is started with,
//! on the user's own plan (also Agent lane).
//!
//! **No key, no transport.** [`MessagesHttp`] takes a body and returns a body — auth headers,
//! the BYOK key / subscription delegate choice, and retries belong to the wiring. Streaming is
//! the HTTP half ([`ToolTurnHttp`]): the loop still sees one JSON turn; text deltas paint as
//! they arrive so connected chat keeps the first-token SLO (FR-HN-07).
//! Keeping the credential out of this seam means no code path here can log or misroute it.
//!
//! **Fabrication is structurally out of reach.** The parser maps only what the API said:
//! `stop_reason: "tool_use"` becomes [`ModelTurn::ToolUses`] in block order, anything else
//! becomes [`ModelTurn::Final`] with the concatenated text blocks. Malformed content is an
//! `Err(String)` whose message never echoes body text (same error hygiene as `anthropic.rs`) —
//! the loop ends the conversation rather than building an answer on a guess.

use serde_json::{json, Value};

use super::anthropic::AnthropicConfig;
use shogun_mcp::tool_loop::{ModelTurn, ModelTurnSource, ToolResult, ToolUse};

/// The transport seam: post one Messages API body, get one response body back.
///
/// Deliberately this thin — no URL, no headers, no key. The implementation (wiring PR) owns the
/// endpoint and the Agent-lane credential (BYOK / subscription delegation, never a Select-KK
/// key), and records traceability at the true egress point exactly like `AnthropicAgentClient`.
/// `Err` is a short reason for the loop, never response content.
pub trait MessagesHttp {
    fn post(&mut self, body: &Value) -> Result<Value, String>;
}

/// Appended to a final tool-lane answer the provider cut at its output-token limit, on every
/// tool lane (Messages and Responses alike), so a partial answer never reads as complete.
pub const LENGTH_LIMIT_NOTE: &str = "\n\n[Answer reached its length limit. Ask to continue.]";

/// Tool-lane text is provisional until the provider confirms a final answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ToolTextEvent {
    Delta { model_turn: u32, text: String },
    Commit { model_turn: u32 },
    Discard { model_turn: u32 },
}

impl crate::metrics::FirstTokenClock {
    /// Feed one tool-lane text event, stamped `at_ms` after the user's action (NFR-SLO-03,
    /// issue #750): a delta times the first visible character; the turn's commit or discard
    /// decides whether that turn's first character was the final answer or a preamble.
    pub fn observe_tool_text(&mut self, event: &ToolTextEvent, at_ms: f64) {
        match event {
            ToolTextEvent::Delta { model_turn, text } => self.tool_delta(*model_turn, text, at_ms),
            ToolTextEvent::Commit { model_turn } => self.tool_commit(*model_turn),
            ToolTextEvent::Discard { model_turn } => self.tool_discard(*model_turn),
        }
    }
}

/// The `messages` array of one tool conversation, accumulated across turns:
/// user prompt → assistant (`tool_use` blocks) → user (`tool_result` blocks) → … → final.
///
/// The assistant turn is kept **verbatim** (the API's own content blocks, text and `tool_use`
/// alike): the next request must carry the exact `tool_use` ids the results answer, and
/// re-synthesising blocks from a parsed form is how ids drift.
#[derive(Debug, Clone)]
pub struct ToolConversation {
    messages: Vec<Value>,
}

impl ToolConversation {
    /// Start a conversation from the user's prompt.
    pub fn new(user_prompt: &str) -> Self {
        Self { messages: vec![json!({ "role": "user", "content": user_prompt })] }
    }

    /// The same turns the streaming chat path already assembled (history + this question),
    /// so a tool conversation does not drop the thread or the live screen block.
    pub fn from_chat_request(request: &super::conversation::ChatRequest) -> Self {
        let messages = request
            .normalized_turns()
            .into_iter()
            .map(|t| {
                json!({
                    "role": match t.role {
                        crate::chat_prompt::Role::User => "user",
                        crate::chat_prompt::Role::Assistant => "assistant",
                    },
                    "content": t.anthropic_content(),
                })
            })
            .collect();
        Self { messages }
    }

    /// Append the assistant's turn as the API returned it (its `content` block array).
    pub fn push_assistant_content(&mut self, content: Value) {
        self.messages.push(json!({ "role": "assistant", "content": content }));
    }

    /// Append the tool results for the previous assistant turn, as one user message of
    /// `tool_result` blocks — the shape the API requires — in the order given, each carrying its
    /// call's id back unchanged and its `is_error` flag intact (a refusal must stay a refusal on
    /// the wire, or the model builds on data it never got). Content passes
    /// [`egress_tool_result_content`] on the way in.
    pub fn push_tool_results(&mut self, results: &[ToolResult]) {
        let blocks: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": egress_tool_result_content(r),
                    "is_error": r.is_error,
                })
            })
            .collect();
        self.messages.push(json!({ "role": "user", "content": blocks }));
    }

    /// Put a mid-run user note (issue #395, already fenced by
    /// `shogun_agents::steer::append_to_user_half`) on the **last user message** — after its
    /// `tool_result` blocks as a text block (the API requires results first), or after the
    /// question on the first turn. Never a system block and never a message of its own. Returns
    /// `false` (nothing changed) when the last message is not the user's.
    pub fn append_user_note(&mut self, note: &str) -> bool {
        let Some(last) = self.messages.last_mut() else {
            return false;
        };
        if last.get("role").and_then(Value::as_str) != Some("user") {
            return false;
        }
        match last.get_mut("content") {
            Some(Value::Array(blocks)) => {
                blocks.push(json!({ "type": "text", "text": note }));
                true
            }
            Some(Value::String(text)) => {
                text.push_str("\n\n");
                text.push_str(note);
                true
            }
            _ => false,
        }
    }

    /// The accumulated `messages` array, ready for [`build_tool_turn_body`].
    pub fn messages(&self) -> &[Value] {
        &self.messages
    }
}

/// A tool result's content as it may leave the device: through the egress redactor
/// ([`shogun_redact::redact_egress`] — secrets plus card, SSN and national-ID numbers).
///
/// Tool results are the one place stored memory, mail and pages re-enter the prompt *after* the
/// request was assembled, so the redaction the context builders apply up front does not reach
/// them. Every provider's tool-turn encoder and the delegate bridge call this on the content.
/// The call id and `is_error` flag are not content and stay as they are.
pub fn egress_tool_result_content(r: &ToolResult) -> String {
    shogun_redact::redact_egress(&r.content).into_owned()
}

/// Append the Connected-services block to the chat system prompt when anything is connected.
/// Empty/None leaves the system prompt unchanged — we do not announce an empty list.
pub fn system_with_connected_services(system: &str, block: Option<&str>) -> String {
    match block.map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => format!("{}\n\n{b}", system.trim_end()),
        None => system.to_string(),
    }
}

/// Render the full Messages API request body for one tool turn: model + `max_tokens` from `cfg`
/// (temperature only when configured, mirroring `build_messages_exchange`), the system prompt
/// (instructions + the `connected_services_block`), the catalog's `tools` array, and the
/// conversation so far. Streaming: the HTTP client asks for SSE so text can paint as it
/// arrives; [`MessagesHttp`] still returns the assembled JSON message the loop parses.
///
/// One of the three tool-lane builders that emit a `"tools"` key (invariant 5; see the module
/// docs for the other two); the Batch and Select-KK builders are pinned tool-free in
/// `anthropic.rs`'s tests.
///
/// System, optional stable facts, and the last tool carry `cache_control: ephemeral`. Facts are
/// a second system block (FR-HN-05 layer 2) so a focus change does not bust the standing
/// instructions. Callers' `tools` slice is not mutated.
pub fn build_tool_turn_body(
    cfg: &AnthropicConfig,
    system: &str,
    stable_context: &str,
    tools: &[Value],
    messages: &[Value],
) -> Value {
    let mut cached_tools: Vec<Value> = tools.to_vec();
    if let Some(last) = cached_tools.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "tools": cached_tools,
        "messages": messages,
        "stream": true,
    });
    let mut system_blocks = Vec::new();
    let system = system.trim();
    if !system.is_empty() {
        system_blocks.push(json!({
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" }
        }));
    }
    let stable = stable_context.trim();
    if !stable.is_empty() {
        let mut messages = messages.to_vec();
        messages.insert(0, json!({"role": "user", "content": [{
            "type": "text", "text": super::prompt_cache::untrusted_stable_context(stable),
            "cache_control": {"type": "ephemeral"}
        }]}));
        body["messages"] = json!(messages);
    }
    if !system_blocks.is_empty() {
        body["system"] = Value::Array(system_blocks);
    }
    if let Some(temperature) = cfg.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

/// What one response parses to: the loop-facing turn plus the assistant `content` blocks
/// verbatim, which the caller must append to the conversation before the next request.
#[derive(Debug, Clone)]
pub struct ParsedTurn {
    pub turn: ModelTurn,
    pub assistant_content: Value,
}

/// Concatenated `text` blocks of a content array (same rule as the batch-lane parser).
fn text_blocks(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect()
}

/// Parse one Messages API response body into a [`ParsedTurn`].
///
/// `stop_reason: "tool_use"` → [`ModelTurn::ToolUses`] with the `tool_use` blocks in the order
/// the API sent them; any other stop reason → [`ModelTurn::Final`] with the concatenated text.
/// Malformed content — missing/non-array `content`, a `tool_use` block without id/name/input, a
/// `tool_use` stop with no `tool_use` block — is an `Err` whose message names the missing shape
/// and **never echoes body text** (the body is model output over captured data; an error string
/// travels into logs, which captured content must not).
pub fn parse_tool_turn(body: &Value) -> Result<ParsedTurn, String> {
    let stop_reason = body.get("stop_reason").and_then(Value::as_str).unwrap_or_default();
    if stop_reason == "refusal" {
        return Err("provider_policy_stop".to_string());
    }
    let content = body
        .get("content")
        .ok_or_else(|| "messages response missing content".to_string())?;
    let blocks = content
        .as_array()
        .ok_or_else(|| "messages response content is not an array".to_string())?;

    let turn = if stop_reason == "tool_use" {
        let uses = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|b| {
                let id = b
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "tool_use block missing id".to_string())?;
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "tool_use block missing name".to_string())?;
                let input = b
                    .get("input")
                    .filter(|v| v.is_object())
                    .cloned()
                    .ok_or_else(|| "tool_use block missing input".to_string())?;
                Ok(ToolUse { id: id.to_string(), name: name.to_string(), input })
            })
            .collect::<Result<Vec<ToolUse>, String>>()?;
        if uses.is_empty() {
            // The API said "I stopped to use a tool" and provided none — answering with the text
            // as if the turn were final would be a fabricated conclusion.
            return Err("stop_reason is tool_use but no tool_use block is present".to_string());
        }
        ModelTurn::ToolUses(uses)
    } else if matches!(stop_reason, "end_turn" | "stop_sequence" | "max_tokens") {
        if blocks.iter().any(|b| b["type"] == "tool_use") {
            return Err("Unfinished tool call.".into());
        }
        let mut text = text_blocks(blocks);
        if text.trim().is_empty() { return Err("The model returned no answer.".into()); }
        if stop_reason == "max_tokens" { text.push_str(LENGTH_LIMIT_NOTE); }
        ModelTurn::Final(text)
    } else {
        return Err("Missing or unsupported response stop reason.".into());
    };

    Ok(ParsedTurn { turn, assistant_content: content.clone() })
}

/// The Agent-lane [`ModelTurnSource`]: holds the conversation, and on each turn appends the
/// pending `tool_result`s, posts the assembled body through [`MessagesHttp`], appends the
/// assistant's reply, and hands the loop its [`ModelTurn`].
///
/// `system` and `tools` are fixed for the turn by construction — the catalog regenerates them on
/// the *next* conversation when connection state changes (spec §4-1: no mid-turn regeneration).
pub struct AnthropicToolTurnSource<H: MessagesHttp> {
    http: H,
    cfg: AnthropicConfig,
    system: String,
    /// Layer 2 (FR-HN-05): context-cache facts. A second cached system block so a focus
    /// change does not bust standing instructions. Empty when the caller has none.
    stable_context: String,
    tools: Vec<Value>,
    conversation: ToolConversation,
    /// Mid-run user notes (issue #395) waiting for the next user message.
    pending_note: Option<String>,
}

impl<H: MessagesHttp> AnthropicToolTurnSource<H> {
    /// Build a source for one conversation. `system` is the full system prompt (drafting
    /// instructions + `connected_services_block`), `tools` the `tool_definitions` array.
    pub fn new(
        http: H,
        cfg: AnthropicConfig,
        system: impl Into<String>,
        tools: Vec<Value>,
        user_prompt: &str,
    ) -> Self {
        Self {
            http,
            cfg,
            system: system.into(),
            stable_context: String::new(),
            tools,
            conversation: ToolConversation::new(user_prompt),
            pending_note: None,
        }
    }

    /// Same as [`Self::new`], but the conversation is the chat path's already-normalized
    /// history + this turn (screen, transcript, question).
    pub fn from_chat(
        http: H,
        cfg: AnthropicConfig,
        request: &super::conversation::ChatRequest,
        connected_block: Option<&str>,
        tools: Vec<Value>,
    ) -> Self {
        Self {
            http,
            cfg,
            system: system_with_connected_services(&request.system, connected_block),
            stable_context: request.stable_context.clone(),
            tools,
            conversation: ToolConversation::from_chat_request(request),
            pending_note: None,
        }
    }
}

impl<H: MessagesHttp> ModelTurnSource for AnthropicToolTurnSource<H> {
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        // First turn carries no results; an empty `tool_result` message would be an API error.
        if !results.is_empty() {
            self.conversation.push_tool_results(results);
        }
        // A note rides on this turn's user message (after its results), never on its own.
        if let Some(note) = self.pending_note.take() {
            if !self.conversation.append_user_note(&note) {
                self.pending_note = Some(note);
            }
        }
        let body = build_tool_turn_body(
            &self.cfg,
            &self.system,
            &self.stable_context,
            &self.tools,
            self.conversation.messages(),
        );
        let resp = self.http.post(&body)?;
        let parsed = parse_tool_turn(&resp)?;
        // The assistant turn joins the conversation only once it parsed: a malformed reply must
        // not sit in the history a retry would build on.
        self.conversation.push_assistant_content(parsed.assistant_content);
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

/// The chat tool loop's source on whichever lane `agent_source::chat_tool_lane` chose (issues
/// #390 / #396 / #484): the Anthropic Messages source, the OpenAI Responses source, the
/// chat/completions source (OpenRouter / Gemini keys), or the Claude Code subscription delegate
/// through SHOGUN's chat bridge (`llm::delegate_tools`). A plain forwarder, so the shell wraps one
/// type in the same steering, authorization and policy layers either way and no safety property
/// depends on which credential the user chose.
pub enum LaneTurnSource<A, O, C, B = NoBridgedLane> {
    Anthropic(A),
    OpenAi(O),
    ChatCompletions(C),
    /// The Claude Code delegate driven through the chat bridge (issue #484).
    Bridged(B),
}

/// The bridged lane's type where a caller has none (it cannot be constructed).
pub enum NoBridgedLane {}

impl ModelTurnSource for NoBridgedLane {
    fn next_turn(&mut self, _results: &[ToolResult]) -> Result<ModelTurn, String> {
        match *self {}
    }
}

impl<A: ModelTurnSource, O: ModelTurnSource, C: ModelTurnSource, B: ModelTurnSource> ModelTurnSource
    for LaneTurnSource<A, O, C, B>
{
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        match self {
            Self::Anthropic(s) => s.next_turn(results),
            Self::OpenAi(s) => s.next_turn(results),
            Self::ChatCompletions(s) => s.next_turn(results),
            Self::Bridged(s) => s.next_turn(results),
        }
    }

    fn supports_async_results(&self) -> bool {
        match self {
            Self::Anthropic(s) => s.supports_async_results(),
            Self::OpenAi(s) => s.supports_async_results(),
            Self::ChatCompletions(s) => s.supports_async_results(),
            Self::Bridged(s) => s.supports_async_results(),
        }
    }

    fn accepts_user_notes(&self) -> bool {
        match self {
            Self::Anthropic(s) => s.accepts_user_notes(),
            Self::OpenAi(s) => s.accepts_user_notes(),
            Self::ChatCompletions(s) => s.accepts_user_notes(),
            Self::Bridged(s) => s.accepts_user_notes(),
        }
    }

    fn push_user_note(&mut self, note: String) {
        match self {
            Self::Anthropic(s) => s.push_user_note(note),
            Self::OpenAi(s) => s.push_user_note(note),
            Self::ChatCompletions(s) => s.push_user_note(note),
            Self::Bridged(s) => s.push_user_note(note),
        }
    }
}

// the real transport (wiring, issue #81 step 4 second half)

/// [`MessagesHttp`] over a real [`HttpTransport`], carrying the Agent-lane BYOK key.
///
/// This is where the credential enters and where the egress is recorded, so both facts live in
/// one place instead of being spread through the loop:
///
/// - **Invariant 5 by type.** The key is a [`ByokKey`]; a `SelectKkKey` will not compile here.
///   Together with the tools-array tests in `anthropic.rs` this makes "the Batch / Select-KK lane
///   cannot drive the tool loop" a property of the types, not of a review.
/// - **Traceability at the true egress point** (invariant 3, AR-11): the row is written *before*
///   the request goes out, so a turn that left the device and came back 401 is still traced. What
///   is digested is the turn's own outbound payload — the conversation, which after the first
///   turn contains service data the tool results brought back. Digest and byte length only.
/// - **Errors never echo the body.** The loop turns an `Err` into a conversation failure the user
///   sees, so the string is a short reason: a status line or a transport summary, run through the
///   same `status_error` redaction the other clients use.
///
/// The trait method is synchronous because [`shogun_mcp::tool_loop`] is: it is pure orchestration
/// with no runtime of its own. The async boundary is [`ToolTurnHttp::post_async`], which the
/// shell drives on its own runtime — the same split the Agent client and Dream Cycle already use.
#[cfg(feature = "net")]
pub struct ToolTurnHttp<T, S> {
    transport: T,
    sink: S,
    key: crate::llm::ByokKey,
    cfg: AnthropicConfig,
    /// How long one model turn may take before the conversation is abandoned. A turn that never
    /// returns would otherwise hold the chat thread open forever; the tool loop's own per-tool
    /// budget does not cover the model's side of the exchange.
    turn_timeout: std::time::Duration,
    /// FR-HN-01: every model turn on this lane writes one `llm_usage` row. Optional so tests that
    /// only care about the wire shape do not have to invent a sink.
    usage: Option<crate::llm::usage::UsageHook>,
    /// When set, `text_delta`s from a streaming turn are forwarded as they arrive so the panel
    /// can paint before the loop sees the assembled JSON. Tool-use JSON never goes here.
    text_out: Option<std::sync::mpsc::Sender<ToolTextEvent>>,
    model_turn: std::sync::atomic::AtomicU32,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Timing reported by streamed model turns, including OpenAI Responses.
#[cfg(feature = "net")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnStreamStats {
    /// Milliseconds until the first text delta; absent when no text was forwarded.
    pub first_token_ms: Option<u64>,
}

#[cfg(feature = "net")]
impl<T, S: crate::llm::traceability::TraceabilitySink> ToolTurnHttp<T, S> {
    /// Default ceiling on a single model turn. Generous next to the 10s per-tool budget because a
    /// turn that has to read several tool results legitimately thinks for longer.
    pub const DEFAULT_TURN_TIMEOUT_MS: u64 = 120_000;

    pub fn new(transport: T, sink: S, key: crate::llm::ByokKey, cfg: AnthropicConfig) -> Self {
        Self {
            transport,
            sink,
            key,
            cfg,
            turn_timeout: std::time::Duration::from_millis(Self::DEFAULT_TURN_TIMEOUT_MS),
            usage: None,
            text_out: None,
            model_turn: std::sync::atomic::AtomicU32::new(0),
            cancelled: None,
        }
    }

    pub fn with_turn_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.turn_timeout = timeout;
        self
    }

    /// Attach usage accounting (FR-HN-01 / #485): every model turn writes one
    /// [`crate::llm::usage::UsageRecord`] tagged with the caller's prompt identity. Without a
    /// hook nothing is recorded.
    pub fn with_usage(mut self, hook: crate::llm::usage::UsageHook) -> Self {
        self.usage = Some(hook);
        self
    }

    pub fn with_text_out(mut self, tx: std::sync::mpsc::Sender<ToolTextEvent>) -> Self {
        self.text_out = Some(tx);
        self
    }

    pub fn with_cancellation(mut self, cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    async fn wait_for_cancel(&self) {
        let Some(cancelled) = &self.cancelled else { return std::future::pending().await; };
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

    /// The request for one turn: same URL, headers and key as the non-streaming path.
    fn turn_request(&self, payload: &str) -> Result<crate::llm::transport::HttpRequest, String> {
        use crate::llm::transport::{HttpRequest, Method};
        HttpRequest::new(
            Method::Post,
            format!("{}/v1/messages", self.cfg.base_url),
            self.cfg.headers(self.key.secret()),
            Some(payload.to_string()),
        )
        .map_err(|e| format!("request build failed: {e}"))
    }

    /// Traceability at the true egress point, before the socket: an unanswered send still
    /// crossed the boundary. Digest and byte length only. `Err` when the ledger would not take
    /// the row; the caller then does not send (fail-closed).
    fn trace_turn(&self, payload: &str) -> Result<(), crate::llm::LlmError> {
        self.sink.try_record(crate::llm::traceability::TraceRecord::for_chunk(
            crate::llm::traceability::Route::MessagesApi,
            "agent_tools",
            self.cfg.destination(),
            payload,
            false,
        ))?;
        Ok(())
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::HttpTransport, S: crate::llm::traceability::TraceabilitySink> ToolTurnHttp<T, S> {
    /// One model turn against the Messages API. Records egress, sends, parses the JSON body, and
    /// gives up after [`Self::turn_timeout`].
    ///
    /// The deadline lives here rather than in [`Self::post_blocking`] because this is the real
    /// boundary: a caller that is already async awaits this directly, and a timeout that only
    /// existed in the blocking wrapper would leave that caller parked on a silent provider for
    /// however long its transport happened to allow.
    ///
    /// `Err` is always a short reason with no response text in it.
    ///
    /// Requires a runtime with timers enabled (`enable_all` / `enable_time`), like every runtime
    /// in this workspace.
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
            // Worth distinguishing: unlike a refusal or a 4xx, a retry of this often works.
            Err(_) => {
                self.record_usage(started, None, Usage::default(), UsageOutcome::Error);
                Err("the model did not answer in time".to_string())
            }
        }
    }

    async fn send_turn(
        &self,
        body: &Value,
    ) -> Result<(Value, crate::llm::usage::Usage), (String, crate::llm::usage::UsageOutcome)> {
        use crate::llm::usage::{Usage, UsageOutcome};

        let payload = body.to_string();
        let req = self.turn_request(&payload).map_err(|e| (e, UsageOutcome::Error))?;
        self.trace_turn(&payload)
            .map_err(|e| (e.to_string(), UsageOutcome::from_error(&e)))?;

        let resp = self
            .transport
            .send(req)
            .await
            .map_err(|e| (format!("transport error: {e}"), UsageOutcome::Error))?;
        if !resp.is_success() {
            // status_error redacts and trims; its Display is a one-line reason, never the body.
            let error = crate::llm::status_error("tool turn", resp.status, &resp.body);
            return Err((error.to_string(), UsageOutcome::from_error(&error)));
        }
        let value: Value = serde_json::from_str(&resp.body)
            .map_err(|_| ("malformed response body".to_string(), UsageOutcome::Parse))?;
        let usage = crate::llm::usage::anthropic_usage(&value)
            .unwrap_or_else(|| Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), resp.body.len()));
        Ok((value, usage))
    }

    /// The [`MessagesHttp`] shape, driven on `rt`. Separate from the trait impl so a caller that
    /// is already async can await [`Self::post_async`] directly — which carries the same deadline.
    pub fn post_blocking(
        &self,
        rt: &tokio::runtime::Runtime,
        body: &Value,
    ) -> Result<Value, String> {
        rt.block_on(self.post_async(body))
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink>
    ToolTurnHttp<T, S>
{
    /// One model turn as SSE (FR-HN-07). Text deltas paint on [`Self::with_text_out`] as they
    /// arrive; the returned JSON is the assembled message [`parse_tool_turn`] reads. Non-2xx
    /// never reaches the text channel ([`crate::llm::transport::StreamOutcome::Failed`]).
    pub async fn post_streaming(&self, body: &Value) -> Result<Value, String> {
        let started = std::time::Instant::now();
        let model_turn = self.model_turn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = tokio::select! {
            biased;
            () = self.wait_for_cancel() => {
                crate::elog!("[chat_tools] model_turn={} stage=model class=cancelled", model_turn.saturating_add(1));
                Err("cancelled".to_string())
            },
            result = tokio::time::timeout(self.turn_timeout, self.send_turn_streaming(body, model_turn)) => match result {
              Ok(result) => result,
              Err(_) => {
                crate::elog!("[chat_tools] model_turn={} stage=model class=timeout", model_turn.saturating_add(1));
                self.record_usage(started, None, crate::llm::usage::Usage::default(), crate::llm::usage::UsageOutcome::Error);
                Err("the model did not answer in time".to_string())
              }
            }
        };
        let parsed = result.and_then(|body| {
            parse_tool_turn(&body).inspect_err(|_| {
                crate::elog!("[chat_tools] model_turn={} stage=tool_parse class=parse", model_turn.saturating_add(1));
            })?;
            Ok(body)
        });
        if let Some(tx) = &self.text_out {
            let event = if parsed.as_ref().is_ok_and(|body| body["stop_reason"] != "tool_use") {
                ToolTextEvent::Commit { model_turn }
            } else { ToolTextEvent::Discard { model_turn } };
            let _ = tx.send(event);
        }
        parsed
    }

    pub fn post_blocking_streaming(
        &self,
        rt: &tokio::runtime::Runtime,
        body: &Value,
    ) -> Result<Value, String> {
        rt.block_on(self.post_streaming(body))
    }

    async fn send_turn_streaming(&self, body: &Value, model_turn: u32) -> Result<Value, String> {
        use crate::llm::conversation::{retryable_before_first_token, STREAM_RETRY_BACKOFF};
        let mut saw_text = false;
        let mut result = self.send_turn_streaming_attempt(body, model_turn, &mut saw_text).await;
        if !saw_text && result.as_ref().is_err_and(retryable_before_first_token) {
            // Retry only this model request. The outer cancellation/deadline covers the backoff;
            // the tool loop has not received or executed any calls from this attempt.
            tokio::time::sleep(STREAM_RETRY_BACKOFF).await;
            result = self.send_turn_streaming_attempt(body, model_turn, &mut saw_text).await;
        }
        result.map_err(|e| match e {
            crate::llm::LlmError::Parse(_) => "malformed response body".to_string(),
            other => other.to_string(),
        })
    }

    async fn send_turn_streaming_attempt(
        &self,
        body: &Value,
        model_turn: u32,
        saw_text: &mut bool,
    ) -> Result<Value, crate::llm::LlmError> {
        use crate::llm::sse::AnthropicSseMessage;
        use crate::llm::transport::{HttpRequest, Method, StreamOutcome};
        use crate::llm::{status_error, LlmError};

        let started = std::time::Instant::now();
        let mut wire = body.clone();
        wire["stream"] = json!(true);
        let payload = wire.to_string();
        let req = HttpRequest::new(
            Method::Post,
            format!("{}/v1/messages", self.cfg.base_url),
            self.cfg.headers(self.key.secret()),
            Some(payload.clone()),
        )?;

        self.trace_turn(&payload)?;

        let mut assembler = AnthropicSseMessage::new();
        let text_out = self.text_out.clone();
        let mut first_token = None::<std::time::Instant>;
        crate::elog!("[chat_tools] model_turn={} stage=provider_request provider=anthropic", model_turn.saturating_add(1));
        let outcome = self
            .transport
            .send_streaming(req, |chunk| {
                for delta in assembler.push(chunk) {
                    *saw_text = true;
                    first_token.get_or_insert_with(std::time::Instant::now);
                    if let Some(tx) = &text_out {
                        if tx.send(ToolTextEvent::Delta { model_turn, text: delta }).is_err() {
                            return false;
                        }
                    }
                }
                true
            })
            .await;

        let first_token_ms = first_token.map(|t| t.duration_since(started).as_millis() as u64);

        let result: Result<Value, LlmError> = match outcome {
            Ok(StreamOutcome::Failed { status, body }) => {
                crate::elog!("[chat_tools] model_turn={} stage=provider_response http_status={}", model_turn.saturating_add(1), status);
                Err(status_error("tool turn", status, &body))
            }
            Ok(StreamOutcome::Streamed { status }) => {
                crate::elog!("[chat_tools] model_turn={} stage=provider_response http_status={}", model_turn.saturating_add(1), status);
                match assembler.retryable_error_status() {
                    Some(status) => Err(status_error("tool turn", status, "")),
                    None => assembler.finish().map_err(LlmError::Parse),
                }
            },
            Err(e) => Err(e.into()),
        };

        match &result {
            Ok(v) => {
                let usage = crate::llm::usage::anthropic_usage(v).unwrap_or_else(|| {
                    crate::llm::usage::Usage::estimated_from_bytes(crate::llm::chat_image::estimated_input_bytes(&payload), 0)
                });
                self.record_usage(started, first_token_ms, usage, crate::llm::usage::UsageOutcome::Ok);
            }
            Err(e) => {
                let class = match e {
                    LlmError::InvalidRequest => "invalid_request",
                    LlmError::Unauthorized(..) => "unauthorized",
                    LlmError::RateLimited(..) => "rate_limited",
                    LlmError::Parse(..) => "parse",
                    LlmError::Transport(..) => "transport",
                    LlmError::PolicyStopped => "policy_stopped",
                    LlmError::Provider(..) => "provider",
                    LlmError::QuotaExhausted(..) => "quota",
                    LlmError::NeedsUpdate(..) => "needs_update",
                    LlmError::NotConfigured => "not_configured",
                    LlmError::TraceUnrecorded => "trace_unrecorded",
                };
                crate::elog!("[chat_tools] model_turn={} stage=model_response class={}", model_turn.saturating_add(1), class);
                self.record_usage(started, None, crate::llm::usage::Usage::default(), crate::llm::usage::UsageOutcome::from_error(e));
            }
        }

        result
    }
}

/// [`ToolTurnHttp`] bound to a runtime, so it satisfies the loop's synchronous seam.
///
/// The runtime is borrowed rather than owned: the shell keeps one long-lived runtime for the
/// whole inline flow, and `block_on` is safe here because that flow runs on its own std thread —
/// never on a tokio worker (the same reasoning as `InlineAgent`).
#[cfg(feature = "net")]
pub struct RuntimeBoundToolTurnHttp<'a, T, S> {
    inner: ToolTurnHttp<T, S>,
    rt: &'a tokio::runtime::Runtime,
}

#[cfg(feature = "net")]
impl<'a, T, S> RuntimeBoundToolTurnHttp<'a, T, S> {
    pub fn new(inner: ToolTurnHttp<T, S>, rt: &'a tokio::runtime::Runtime) -> Self {
        Self { inner, rt }
    }
}

#[cfg(feature = "net")]
impl<T: crate::llm::transport::StreamingTransport, S: crate::llm::traceability::TraceabilitySink> MessagesHttp
    for RuntimeBoundToolTurnHttp<'_, T, S>
{
    fn post(&mut self, body: &Value) -> Result<Value, String> {
        self.inner.post_blocking_streaming(self.rt, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_token_clock_reads_the_tool_text_events() {
        // The event sequence a "Let me check." → tool call → answer turn paints (issue #750).
        let events = [
            (ToolTextEvent::Delta { model_turn: 0, text: "Let me check.".into() }, 250.0),
            (ToolTextEvent::Discard { model_turn: 0 }, 400.0),
            (ToolTextEvent::Delta { model_turn: 1, text: String::new() }, 1500.0),
            (ToolTextEvent::Delta { model_turn: 1, text: "Two ".into() }, 1600.0),
            (ToolTextEvent::Delta { model_turn: 1, text: "meetings.".into() }, 1650.0),
            (ToolTextEvent::Commit { model_turn: 1 }, 1700.0),
        ];
        let mut clock = crate::metrics::FirstTokenClock::new();
        for (event, at) in &events {
            clock.observe_tool_text(event, *at);
        }
        assert_eq!(clock.first_visible_ms(), Some(250.0));
        assert_eq!(clock.first_answer_ms(), Some(1600.0));
    }

    fn cfg() -> AnthropicConfig {
        AnthropicConfig::new("test-model")
    }

    /// A scripted transport: pops canned responses in order, records every posted body.
    struct FakeHttp {
        responses: Vec<Value>,
        posted: Vec<Value>,
    }

    impl FakeHttp {
        fn new(responses: Vec<Value>) -> Self {
            Self { responses, posted: Vec::new() }
        }
    }

    impl MessagesHttp for FakeHttp {
        fn post(&mut self, body: &Value) -> Result<Value, String> {
            self.posted.push(body.clone());
            if self.responses.is_empty() {
                return Err("script exhausted".into());
            }
            Ok(self.responses.remove(0))
        }
    }

    fn tool_use_response() -> Value {
        json!({
            "stop_reason": "tool_use",
            "content": [
                { "type": "text", "text": "Let me check." },
                { "type": "tool_use", "id": "toolu_1", "name": "list_calendar_events",
                  "input": { "day": "tomorrow" } },
                { "type": "tool_use", "id": "toolu_2", "name": "check_calendar_availability",
                  "input": {} },
            ],
        })
    }

    fn final_response(text: &str) -> Value {
        json!({
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": text }],
        })
    }

    #[test]
    fn a_lane_source_forwards_turns_and_notes_on_every_lane() {
        use crate::llm::openai_compat::tool_turns::CompatToolTurnSource;
        use crate::llm::openai_compat::{OpenAiCompatConfig, OPENROUTER_BASE_URL};
        use crate::llm::openai_responses::tool_turns::ResponsesToolTurnSource;
        use crate::llm::openai_responses::OpenAiResponsesConfig;

        type Lane = LaneTurnSource<
            AnthropicToolTurnSource<FakeHttp>,
            ResponsesToolTurnSource<FakeHttp>,
            CompatToolTurnSource<FakeHttp>,
        >;
        let mut anthropic: Lane = LaneTurnSource::Anthropic(AnthropicToolTurnSource::new(
            FakeHttp::new(vec![final_response("A")]),
            cfg(),
            "s",
            vec![],
            "q",
        ));
        let responses_body = json!({ "id": "r", "status": "completed", "output": [{ "type": "message",
            "content": [{ "type": "output_text", "text": "O" }] }] });
        let mut openai: Lane = LaneTurnSource::OpenAi(ResponsesToolTurnSource::new(
            FakeHttp::new(vec![responses_body]),
            OpenAiResponsesConfig::new("https://api.openai.com/v1", "m"),
            "s",
            &[],
            "q",
        ));
        let compat_body = json!({ "choices": [{ "finish_reason": "stop",
            "message": { "role": "assistant", "content": "C" } }] });
        let mut compat: Lane = LaneTurnSource::ChatCompletions(CompatToolTurnSource::new(
            FakeHttp::new(vec![compat_body]),
            OpenAiCompatConfig::new(OPENROUTER_BASE_URL, "m"),
            "s",
            &[],
            "q",
        ));
        for (source, answer) in [(&mut anthropic, "A"), (&mut openai, "O"), (&mut compat, "C")] {
            // Every lane carries mid-run notes, so steering works the same on any key.
            assert!(source.accepts_user_notes());
            assert!(!source.supports_async_results());
            source.push_user_note("NOTE-MARKER".into());
            assert_eq!(source.next_turn(&[]).unwrap(), ModelTurn::Final(answer.into()));
        }
        let LaneTurnSource::Anthropic(a) = &anthropic else { unreachable!() };
        assert!(a.http.posted[0].to_string().contains("NOTE-MARKER"));
        let LaneTurnSource::OpenAi(o) = &openai else { unreachable!() };
        assert!(o.http().posted[0]["input"].to_string().contains("NOTE-MARKER"));
        let LaneTurnSource::ChatCompletions(c) = &compat else { unreachable!() };
        assert!(c.http().posted[0]["messages"].to_string().contains("NOTE-MARKER"));
    }

    #[test]
    fn body_carries_model_system_tools_and_messages() {
        let cfg = cfg().with_temperature(0.5);
        let tools = vec![json!({ "name": "list_calendar_events", "description": "d",
                                 "input_schema": { "type": "object" } })];
        let convo = ToolConversation::new("What's on my calendar tomorrow?");
        let body = build_tool_turn_body(&cfg, "## Connected services\n…", "", &tools, convo.messages());
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "## Connected services\n…",
                "cache_control": { "type": "ephemeral" }
            }])
        );
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["name"], "list_calendar_events");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(tools[0].get("cache_control"), None, "caller slice stays untouched");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "What's on my calendar tomorrow?");
    }

    #[test]
    fn an_empty_catalog_still_emits_tools_without_a_cache_breakpoint() {
        let body = build_tool_turn_body(&cfg(), "s", "", &[], ToolConversation::new("q").messages());
        assert!(body["tools"].as_array().unwrap().is_empty());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn stable_facts_are_cached_untrusted_user_evidence() {
        let body = build_tool_turn_body(
            &cfg(),
            "Be brief.",
            "What you remember about their work:\n- ship Orchid\n",
            &[],
            ToolConversation::new("tomorrow?").messages(),
        );
        assert_eq!(body["system"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["messages"][0]["content"][0]["text"].as_str().is_some_and(|s| s.contains("untrusted evidence") && s.contains("Orchid")));
        assert_eq!(body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][1]["content"], "tomorrow?");
    }

    #[test]
    fn from_chat_puts_stable_facts_on_the_source_not_the_user_turn() {
        use crate::chat_prompt::{HistoryTurn, Role};
        use crate::llm::conversation::ChatRequest;

        let request = ChatRequest {
            system: "Be brief.".into(),
            stable_context: "What you remember about their work:\n- ship Orchid\n".into(),
            turns: vec![HistoryTurn {
                role: Role::User,
                text: "yesterday?".into(),
                screen_label: None,
            }],
            context: "Screen: Calendar".into(),
            message: "and tomorrow?".into(),
            images: Vec::new(),
        };
        let mut source = AnthropicToolTurnSource::from_chat(
            FakeHttp::new(vec![final_response("ok")]),
            cfg(),
            &request,
            None,
            vec![],
        );
        source.next_turn(&[]).unwrap();
        let posted = &source.http.posted[0];
        assert_eq!(posted["messages"][0]["content"][0]["text"], super::super::prompt_cache::untrusted_stable_context(request.stable_context.trim()));
        assert_eq!(posted["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        let last = posted["messages"].as_array().unwrap().last().unwrap();
        let user = last["content"].as_str().unwrap();
        assert!(user.contains("Screen: Calendar"));
        assert!(user.contains("and tomorrow?"));
        assert!(!user.contains("Orchid"));
    }

    #[test]
    fn tool_results_become_one_user_message_of_tool_result_blocks_in_order() {
        let mut convo = ToolConversation::new("q");
        convo.push_assistant_content(tool_use_response()["content"].clone());
        convo.push_tool_results(&[
            ToolResult { id: "toolu_1".into(), content: "2 events".into(), is_error: false },
            ToolResult { id: "toolu_2".into(), content: "That took too long.".into(), is_error: true },
        ]);
        let msgs = convo.messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        let blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(blocks[0]["content"], "2 events");
        assert_eq!(blocks[0]["is_error"], false);
        // the failure flag survives to the wire — a refusal must not arrive looking like data
        assert_eq!(blocks[1]["tool_use_id"], "toolu_2");
        assert_eq!(blocks[1]["is_error"], true);
    }

    #[test]
    fn a_tool_use_stop_maps_to_tool_uses_in_block_order() {
        let parsed = parse_tool_turn(&tool_use_response()).unwrap();
        let ModelTurn::ToolUses(uses) = parsed.turn else { panic!("expected ToolUses") };
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].id, "toolu_1");
        assert_eq!(uses[0].name, "list_calendar_events");
        assert_eq!(uses[0].input, json!({ "day": "tomorrow" }));
        assert_eq!(uses[1].id, "toolu_2");
        // the assistant content is preserved verbatim (text block included) for the history
        assert_eq!(parsed.assistant_content, tool_use_response()["content"]);
    }

    #[test]
    fn supported_final_reasons_preserve_text_and_disclose_truncation() {
        let body = json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "text", "text": "Two " },
                { "type": "text", "text": "meetings." },
            ],
        });
        let parsed = parse_tool_turn(&body).unwrap();
        assert_eq!(parsed.turn, ModelTurn::Final("Two meetings.".into()));
        // A length cut is disclosed; missing terminal state fails closed.
        let cut = json!({ "stop_reason": "max_tokens", "content": [{ "type": "text", "text": "part" }] });
        assert!(matches!(parse_tool_turn(&cut), Ok(ParsedTurn { turn: ModelTurn::Final(text), .. }) if text.starts_with("part") && text.contains("length limit")));
        let none = json!({ "content": [{ "type": "text", "text": "t" }] });
        assert!(parse_tool_turn(&none).is_err());
    }

    #[test]
    fn a_refusal_is_a_body_free_policy_stop_not_an_empty_final_answer() {
        let body = json!({ "stop_reason": "refusal", "content": [] });
        assert!(matches!(parse_tool_turn(&body), Err(ref error) if error == "provider_policy_stop"));
    }

    #[test]
    fn malformed_responses_err_without_echoing_body_text() {
        // A marker that must never travel into the error string: response bodies are model
        // output over captured data, and Err strings reach logs.
        let secretish = "MEETING-WITH-ACME-CONFIDENTIAL";
        let cases = [
            json!({ "stop_reason": "end_turn", "note": secretish }), // no content
            json!({ "stop_reason": "end_turn", "content": secretish }), // content not an array
            json!({ "stop_reason": "tool_use",
                    "content": [{ "type": "tool_use", "name": secretish, "input": {} }] }), // no id
            json!({ "stop_reason": "tool_use",
                    "content": [{ "type": "tool_use", "id": "t", "input": { "x": secretish } }] }), // no name
            json!({ "stop_reason": "tool_use",
                    "content": [{ "type": "tool_use", "id": "t", "name": "n" }] }), // no input
            json!({ "stop_reason": "tool_use",
                    "content": [{ "type": "text", "text": secretish }] }), // tool_use stop, no block
        ];
        for body in cases {
            let err = parse_tool_turn(&body).unwrap_err();
            assert!(!err.is_empty());
            assert!(!err.contains(secretish), "error echoes body content: {err}");
        }
    }

    // the source over a scripted transport

    #[test]
    fn a_multi_turn_round_trip_accumulates_the_conversation() {
        let http = FakeHttp::new(vec![tool_use_response(), final_response("You have two meetings.")]);
        let mut source = AnthropicToolTurnSource::new(
            http,
            cfg(),
            "system block",
            vec![json!({ "name": "list_calendar_events" })],
            "What's on tomorrow?",
        );

        // Turn 1: no results yet → the model asks for tools.
        let turn1 = source.next_turn(&[]).unwrap();
        assert!(matches!(turn1, ModelTurn::ToolUses(ref uses) if uses.len() == 2));

        // Turn 2: results go back, the model answers.
        let results = [
            ToolResult { id: "toolu_1".into(), content: "2 events".into(), is_error: false },
            ToolResult { id: "toolu_2".into(), content: "refused".into(), is_error: true },
        ];
        let turn2 = source.next_turn(&results).unwrap();
        assert_eq!(turn2, ModelTurn::Final("You have two meetings.".into()));

        let posted = &source.http.posted;
        assert_eq!(posted.len(), 2);
        // First request: exactly the user prompt, no tool_result anywhere.
        assert_eq!(posted[0]["messages"].as_array().unwrap().len(), 1);
        // Second request: prompt → assistant (verbatim tool_use blocks) → tool results, and the
        // ids the API issued come back on the matching tool_result blocks.
        let msgs = posted[1]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], tool_use_response()["content"]);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(msgs[2]["content"][1]["tool_use_id"], "toolu_2");
        assert_eq!(msgs[2]["content"][1]["is_error"], true);
        // Both requests carry the same system + tools — fixed for the turn (spec §4-1).
        assert_eq!(posted[0]["system"], posted[1]["system"]);
        assert_eq!(posted[0]["tools"], posted[1]["tools"]);
    }

    #[test]
    fn a_mid_run_note_rides_on_the_next_user_message_after_the_results() {
        // Issue #395: a steering note joins the user half of the next turn — after its
        // tool_result blocks (the API requires results first), never the system prompt and never
        // a message of its own; the conversation keeps it, so a later turn does not repeat it.
        let http = FakeHttp::new(vec![
            tool_use_response(),
            tool_use_response(),
            final_response("Shorter now."),
        ]);
        let mut source = AnthropicToolTurnSource::new(
            http,
            cfg(),
            "system block",
            vec![json!({ "name": "list_calendar_events" })],
            "What's on tomorrow?",
        );
        assert!(source.accepts_user_notes());
        source.push_user_note("NOTE-A".into());
        source.next_turn(&[]).unwrap();
        let results = [
            ToolResult { id: "toolu_1".into(), content: "2 events".into(), is_error: false },
            ToolResult { id: "toolu_2".into(), content: "refused".into(), is_error: true },
        ];
        source.push_user_note("NOTE-B".into());
        source.push_user_note("NOTE-C".into());
        source.next_turn(&results).unwrap();
        source.next_turn(&results).unwrap();

        let posted = &source.http.posted;
        // First turn: on the question itself.
        assert_eq!(posted[0]["messages"][0]["content"], "What's on tomorrow?\n\nNOTE-A");
        // Second turn: one user message, results first, then the notes as one text block.
        let msgs = posted[1]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        let blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[2], json!({ "type": "text", "text": "NOTE-B\n\nNOTE-C" }));
        // Third turn: the note is history now, not repeated on the new results.
        let msgs = posted[2]["messages"].as_array().unwrap();
        assert_eq!(msgs[4]["content"].as_array().unwrap().len(), 2);
        for p in posted {
            assert!(!p["system"].to_string().contains("NOTE"), "a note never reaches the system half");
        }
    }

    #[test]
    fn a_transport_failure_is_the_conversation_failing() {
        struct DeadHttp;
        impl MessagesHttp for DeadHttp {
            fn post(&mut self, _body: &Value) -> Result<Value, String> {
                Err("transport down".into())
            }
        }
        let mut source = AnthropicToolTurnSource::new(DeadHttp, cfg(), "s", vec![], "q");
        assert!(source.next_turn(&[]).is_err());
    }

    #[test]
    fn a_malformed_reply_is_not_appended_to_the_history() {
        // A retry after a parse failure must not carry the broken assistant turn.
        let http = FakeHttp::new(vec![
            json!({ "stop_reason": "end_turn" }), // no content → parse error
            final_response("ok"),
        ]);
        let mut source = AnthropicToolTurnSource::new(http, cfg(), "s", vec![], "q");
        assert!(source.next_turn(&[]).is_err());
        assert_eq!(source.next_turn(&[]).unwrap(), ModelTurn::Final("ok".into()));
        // the second request still holds only the original prompt
        assert_eq!(source.http.posted[1]["messages"].as_array().unwrap().len(), 1);
    }

    // driven by the real loop

    /// Gated on `db` (not the module's `net`-or-`db`) because
    /// the loop's context types need `shogun_agents`, which the `net` feature alone does not pull.
    #[cfg(feature = "db")]
    #[test]
    fn the_real_read_loop_drives_the_source_to_an_answer() {
        use shogun_agents::entitlement::{entitlements, Plan};
        use shogun_agents::permission::Action;
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::scope::{Service, Wave};
        use shogun_mcp::tool_catalog::{ServiceState, ToolContext};
        use shogun_mcp::tool_loop::{
            run_read_loop, LoopLimits, ProposalSink, ReadToolRunner, ToolRunError,
        };

        struct OkRunner;
        impl ReadToolRunner for OkRunner {
            fn run(
                &self,
                _service: Service,
                _scope_op: &'static str,
                _input: &Value,
                _timeout_ms: u64,
            ) -> Result<String, ToolRunError> {
                Ok("2 events tomorrow".into())
            }
        }
        struct NoProposals;
        impl ProposalSink for NoProposals {
            fn propose(&mut self, _action: Action, _body: &str) -> Result<(), String> {
                Err("unexpected".into())
            }
        }

        let one_use = json!({
            "stop_reason": "tool_use",
            "content": [{ "type": "tool_use", "id": "toolu_1", "name": "list_calendar_events",
                          "input": {} }],
        });
        let http = FakeHttp::new(vec![one_use, final_response("Two meetings tomorrow.")]);
        let mut source = AnthropicToolTurnSource::new(http, cfg(), "s", vec![], "tomorrow?");
        let out = run_read_loop(
            &mut source,
            &mut OkRunner,
            &mut NoProposals,
            &[ServiceState {
                service: Service::GoogleCalendar,
                conn: ConnState::Connected { last_sync_ms: 0 },
            }],
            &ToolContext {
                highest_released: Wave::One,
                draft_stop: true,
                plan: entitlements(Plan::Pro, 0),
            },
            LoopLimits::default(),
        )
        .unwrap();
        assert_eq!(out.answer, "Two meetings tomorrow.");
        assert_eq!(out.executed, 1);
        // the read's data went back on the wire, fenced, against the model's own id
        let msgs = source.http.posted[1]["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_1");
        let sent = msgs[2]["content"][0]["content"].as_str().unwrap();
        assert!(sent.contains("2 events tomorrow"));
    }

    // key separation (invariant 5, spec §5)

    #[test]
    fn streamed_thinking_is_replayed_unchanged_after_a_tool_result() -> Result<(), String> {
        let mut stream = super::super::sse::AnthropicSseMessage::new();
        for event in [
            json!({"type":"content_block_start","index":0,"content_block":{
                "type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{
                "type":"thinking_delta","thinking":"opaque reasoning"}}),
            json!({"type":"content_block_delta","index":0,"delta":{
                "type":"signature_delta","signature":"opaque signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{
                "type":"redacted_thinking","data":"opaque redacted"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{
                "type":"tool_use","id":"call","name":"search_mail","input":{}}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ] {
            assert!(stream.push(&format!("data: {event}\n\n")).is_empty());
        }
        let parsed = parse_tool_turn(&stream.finish()?)?;
        let mut conversation = ToolConversation::new("synthetic question");
        conversation.push_assistant_content(parsed.assistant_content);
        conversation.push_tool_results(&[ToolResult {
            id: "call".into(), content: "synthetic result".into(), is_error: false,
        }]);
        let body = build_tool_turn_body(&cfg(), "system", "", &[], conversation.messages());
        assert_eq!(body["messages"][1]["content"], json!([
            {"type":"thinking","thinking":"opaque reasoning","signature":"opaque signature"},
            {"type":"redacted_thinking","data":"opaque redacted"},
            {"type":"tool_use","id":"call","name":"search_mail","input":{}},
        ]));
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call");
        Ok(())
    }

    /// Does any object anywhere in `v` carry a `tools` key?
    fn carries_tools(v: &Value) -> bool {
        match v {
            Value::Object(map) => {
                map.contains_key("tools") || map.values().any(carries_tools)
            }
            Value::Array(items) => items.iter().any(carries_tools),
            _ => false,
        }
    }

    /// The Messages tool lane's builder always emits `tools`, even with an empty catalog: the
    /// key's presence is what marks the lane, so its absence in every non-tool-lane builder
    /// (pinned in `anthropic.rs` and `openai_compat.rs`) is meaningful.
    #[test]
    fn this_builder_always_emits_a_tools_key() {
        let convo = ToolConversation::new("q");
        let body = build_tool_turn_body(&cfg(), "s", "", &[], convo.messages());
        assert!(carries_tools(&body));
        assert!(body["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn from_chat_request_keeps_history_then_this_turn_as_one_user_tail() {
        use crate::chat_prompt::{HistoryTurn, Role};
        use crate::llm::conversation::ChatRequest;

        let request = ChatRequest {
            system: "Be brief.".into(),
            turns: vec![HistoryTurn {
                role: Role::User,
                text: "yesterday?".into(),
                screen_label: None,
            }],
            context: "Screen: Calendar".into(),
            message: "and tomorrow?".into(),
            ..Default::default()
        };
        let convo = ToolConversation::from_chat_request(&request);
        let msgs = convo.messages();
        assert_eq!(msgs[0]["role"], "user");
        assert!(msgs[0]["content"].as_str().unwrap().contains("yesterday?"));
        assert_eq!(msgs.last().unwrap()["role"], "user");
        let last = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(last.contains("Screen: Calendar"));
        assert!(last.contains("and tomorrow?"));
        assert_eq!(
            system_with_connected_services("Be brief.", Some("## Connected services\n- calendar")),
            "Be brief.\n\n## Connected services\n- calendar"
        );
        assert_eq!(system_with_connected_services("Be brief.", None), "Be brief.");
        assert_eq!(system_with_connected_services("Be brief.", Some("  ")), "Be brief.");
    }

    #[test]
    fn a_tool_turn_returns_every_result_in_one_user_message() {
        let mut convo = ToolConversation::new("q");
        convo.push_assistant_content(json!([{ "type": "tool_use", "id": "a" }]));
        convo.push_tool_results(&[
            ToolResult { id: "a".into(), content: "one".into(), is_error: false },
            ToolResult { id: "b".into(), content: "two".into(), is_error: true },
        ]);
        let msgs = convo.messages();
        let blocks = msgs.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(msgs.last().unwrap()["role"], "user");
        assert_eq!(blocks.len(), 2, "FR-HN-07: one user message, every tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
        assert_eq!(blocks[1]["is_error"], true);
    }

    // the real transport (wiring)

    #[cfg(feature = "net")]
    mod http {
        use super::super::*;
        use crate::llm::traceability::{RecordingSink, Route};
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{ByokKey, Secret};

        fn cfg() -> AnthropicConfig {
            AnthropicConfig::new("test-model").with_base_url("https://api.example.com")
        }

        fn client(
            transport: MockTransport,
        ) -> ToolTurnHttp<MockTransport, RecordingSink> {
            ToolTurnHttp::new(
                transport,
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
        }


        #[tokio::test]
        async fn a_rejected_turn_is_still_traced() {
            let c = client(MockTransport::new([HttpResponse {
                retry_after: None,
                status: 401,
                body: r#"{"error":{"message":"invalid x-api-key sk-secret"}}"#.into(),
            }]));
            let err = c.post_async(&json!({"messages": []})).await.unwrap_err();

            let recs = c.sink.records();
            assert_eq!(recs.len(), 1, "one row per turn, success or not");
            assert_eq!(recs[0].route, Route::MessagesApi);
            assert_eq!(recs[0].purpose, "agent_tools");
            // Whatever the provider said, the reason we hand the loop carries no body text and
            // no key material.
            assert!(!err.contains("sk-secret"), "the error echoed a secret: {err}");
        }

        /// A traceability record has no text field at all, so the conversation — which after the
        /// first turn carries service data the tool results brought back — cannot be stored.
        #[tokio::test]
        async fn the_traced_row_holds_a_digest_not_the_conversation() {
            let c = client(MockTransport::ok(
                r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#,
            ));
            let body = json!({"messages": [{"role": "user", "content": "the secret agenda"}]});
            c.post_async(&body).await.unwrap();

            let recs = c.sink.records();
            assert_eq!(recs[0].chunk_bytes, body.to_string().len());
            assert!(!format!("{:?}", recs[0]).contains("secret agenda"));
        }

        /// FR-HN-01: a tool-turn completion writes one usage row tagged as inline chat, never text.
        #[tokio::test]
        async fn a_successful_turn_records_provider_usage() {
            use crate::llm::prompts;
            use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
            use std::sync::Arc;

            let usage_sink = Arc::new(RecordingUsageSink::new());
            let c = ToolTurnHttp::new(
                MockTransport::ok(
                    r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],
                        "usage":{"input_tokens":40,"output_tokens":6,
                                 "cache_read_input_tokens":30,"cache_creation_input_tokens":10}}"#,
                ),
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_usage(UsageHook::new(usage_sink.clone(), prompts::INLINE_CHAT));
            c.post_async(&json!({"messages": [{"role": "user", "content": "secret agenda"}]}))
                .await
                .unwrap();

            let recs = usage_sink.records();
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].prompt_id, "inline_chat");
            assert_eq!(recs[0].model, "test-model");
            assert_eq!(recs[0].usage.input_tokens, 40);
            assert_eq!(recs[0].usage.cache_read_tokens, 30);
            assert_eq!(recs[0].usage.cache_write_tokens, 10);
            assert!(!recs[0].usage.estimated);
            assert_eq!(recs[0].outcome, UsageOutcome::Ok);
            assert!(!format!("{:?}", recs[0]).contains("secret agenda"));
        }

        /// The BYOK key rides as `x-api-key` and is never printed, including by the mock's own
        /// captured-request Debug.
        #[tokio::test]
        async fn the_byok_key_is_sent_as_a_header_and_never_printed() {
            let c = client(MockTransport::ok(
                r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#,
            ));
            c.post_async(&json!({"messages": []})).await.unwrap();

            let sent = c.transport.sent();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].url, "https://api.example.com/v1/messages");
            assert!(sent[0].headers.iter().any(|(k, v)| k == "x-api-key" && v == "byok-123456"));
            assert!(!format!("{:?}", sent[0]).contains("byok-123456"));
        }

        /// A body that is not JSON is a failed turn, not a parse panic — and the reason says so
        /// without quoting what came back.
        #[tokio::test]
        async fn a_non_json_body_fails_the_turn_without_echoing_it() {
            let c = client(MockTransport::ok("<html>gateway error: token abc123</html>"));
            let err = c.post_async(&json!({"messages": []})).await.unwrap_err();
            assert_eq!(err, "malformed response body");
        }

        /// The deadline is on `post_async`, so a caller that awaits it directly — the shape the
        /// docs advertise for async callers — is covered too, not just the blocking wrapper.
        #[tokio::test]
        async fn the_deadline_covers_the_directly_awaited_path() {
            let c = ToolTurnHttp::new(
                Silent,
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_turn_timeout(std::time::Duration::from_millis(20));
            let err = c.post_async(&json!({"messages": []})).await.unwrap_err();
            assert_eq!(err, "the model did not answer in time");
        }

        /// A transport that accepts the request and never answers.
        struct Silent;
        impl crate::llm::transport::HttpTransport for Silent {
            async fn send(
                &self,
                _req: crate::llm::transport::HttpRequest,
            ) -> Result<HttpResponse, crate::llm::transport::TransportError> {
                std::future::pending::<()>().await;
                unreachable!("pending never resolves")
            }
        }

        /// The turn timeout is the model's side of the exchange — the loop's per-tool budget does
        /// not cover it, and without one a silent provider holds the thread open forever.
        #[test]
        fn a_model_turn_that_never_answers_ends_the_conversation() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let c = ToolTurnHttp::new(
                Silent,
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_turn_timeout(std::time::Duration::from_millis(20));

            let err = c.post_blocking(&rt, &json!({"messages": []})).unwrap_err();
            assert_eq!(err, "the model did not answer in time");
            // Still traced: the request did leave.
            assert_eq!(c.sink.records().len(), 1);
        }
    }

    #[cfg(feature = "net")]
    mod usage_rows {
        use super::super::*;
        use crate::llm::prompts::INLINE_CHAT;
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::usage::{RecordingUsageSink, UsageHook, UsageOutcome};
        use crate::llm::{ByokKey, Secret};

        fn cfg() -> AnthropicConfig {
            AnthropicConfig::new("test-model").with_base_url("https://api.example.com")
        }

        #[tokio::test]
        async fn a_plain_turn_records_exact_usage() {
            let usage = std::sync::Arc::new(RecordingUsageSink::new());
            let http = ToolTurnHttp::new(
                MockTransport::new([HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: json!({
                        "stop_reason": "end_turn",
                        "content": [{ "type": "text", "text": "done" }],
                        "usage": { "input_tokens": 30, "output_tokens": 4, "cache_read_input_tokens": 20 }
                    })
                    .to_string(),
                }]),
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_usage(UsageHook::new(usage.clone(), INLINE_CHAT));
            let body = build_tool_turn_body(&cfg(), "sys", "", &[], ToolConversation::new("q").messages());
            http.post_async(&body).await.unwrap();
            let rows = usage.records();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].outcome, UsageOutcome::Ok);
            assert_eq!(rows[0].usage.input_tokens, 30);
            assert_eq!(rows[0].usage.output_tokens, 4);
            assert_eq!(rows[0].usage.cache_read_tokens, 20);
            assert_eq!(rows[0].first_token_ms, None, "a body arrives whole; there is no first token");
        }
    }
    #[cfg(feature = "net")]
    mod streaming {
        use super::super::*;
        use crate::llm::traceability::{RecordingSink, Route};
        use crate::llm::transport::MockStreamingTransport;
        use crate::llm::{ByokKey, Secret};
        use serde_json::json;
        use shogun_mcp::tool_loop::ModelTurn;

        fn cfg() -> AnthropicConfig {
            AnthropicConfig::new("test-model").with_base_url("https://api.example.com")
        }

        fn sse_text_hello() -> Vec<String> {
            vec![
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into(),
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n".into(),
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n".into(),
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n".into(),
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\ndata: {\"type\":\"message_stop\"}\n\n".into(),
            ]
        }

        fn sse_tool_use() -> Vec<String> {
            vec![concat!(
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"list_calendar_events\",\"input\":{}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"day\\\":\\\"tomorrow\\\"}\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            )
            .into()]
        }

        fn client(
            chunks: Vec<String>,
        ) -> ToolTurnHttp<MockStreamingTransport, RecordingSink> {
            ToolTurnHttp::new(
                MockStreamingTransport::new(200, chunks),
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
        }

        struct FlakyStream {
            calls: std::sync::atomic::AtomicUsize,
            failures: usize,
            status: u16,
            partial: bool,
        }

        impl crate::llm::transport::StreamingTransport for FlakyStream {
            async fn send_streaming<F>(&self, _: crate::llm::transport::HttpRequest, mut on_chunk: F)
                -> Result<crate::llm::transport::StreamOutcome, crate::llm::transport::TransportError>
            where F: FnMut(&str) -> bool + Send {
                use crate::llm::transport::{StreamOutcome, TransportError};
                if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < self.failures {
                    if self.partial {
                        for chunk in sse_text_hello().iter().take(2) { on_chunk(chunk); }
                    }
                    return if self.status == 200 {
                        on_chunk("data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"private provider body\"}}\n\n");
                        Ok(StreamOutcome::Streamed { status: 200 })
                    } else if self.status == 0 {
                        Err(TransportError::Io("connection reset".into()))
                    } else {
                        Ok(StreamOutcome::Failed { status: self.status, body: String::new() })
                    };
                }
                for chunk in sse_text_hello() { on_chunk(&chunk); }
                Ok(StreamOutcome::Streamed { status: 200 })
            }
        }

        fn flaky_client(status: u16, failures: usize, partial: bool) -> ToolTurnHttp<FlakyStream, RecordingSink> {
            ToolTurnHttp::new(
                FlakyStream { calls: std::sync::atomic::AtomicUsize::new(0), failures, status, partial },
                RecordingSink::new(), ByokKey::new(Secret::new("test-key")), cfg(),
            )
        }

        #[tokio::test]
        async fn transient_failure_before_text_retries_once_and_commits_only_the_answer() {
            for status in [429, 503, 0, 200] {
                let (tx, rx) = std::sync::mpsc::channel();
                let c = flaky_client(status, 1, false).with_text_out(tx);
                let answer = c.post_streaming(&json!({"messages": []})).await.unwrap();
                assert!(matches!(parse_tool_turn(&answer).unwrap().turn, ModelTurn::Final(text) if text == "Hello"));
                assert_eq!(c.transport.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
                assert_eq!(c.sink.records().len(), 2, "each HTTP attempt must be traced");
                assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![
                    ToolTextEvent::Delta { model_turn: 0, text: "Hel".into() },
                    ToolTextEvent::Delta { model_turn: 0, text: "lo".into() },
                    ToolTextEvent::Commit { model_turn: 0 },
                ]);
            }
        }

        #[tokio::test]
        async fn stream_retry_never_repeats_partial_text_or_auth_errors_and_is_bounded() {
            for (status, partial, expected_calls) in [(0, true, 1), (401, false, 1), (503, false, 2), (200, true, 1), (200, false, 2)] {
                let (tx, rx) = std::sync::mpsc::channel();
                let c = flaky_client(status, usize::MAX, partial).with_text_out(tx);
                let result = c.post_streaming(&json!({"messages": []})).await;
                assert!(result.is_err());
                assert!(!format!("{result:?}").contains("private provider body"));
                assert_eq!(c.transport.calls.load(std::sync::atomic::Ordering::SeqCst), expected_calls);
                assert_eq!(rx.try_iter().last(), Some(ToolTextEvent::Discard { model_turn: 0 }));
            }
        }

        #[tokio::test]
        async fn stop_during_retry_backoff_prevents_a_second_request() {
            use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
            let cancelled = Arc::new(AtomicBool::new(false));
            let c = flaky_client(503, 1, false).with_cancellation(cancelled.clone());
            let body = json!({"messages": []});
            let (result, ()) = tokio::join!(c.post_streaming(&body), async {
                while c.transport.calls.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
                cancelled.store(true, Ordering::Release);
            });
            assert_eq!(result, Err("cancelled".into()));
            assert_eq!(c.transport.calls.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn provider_error_or_incomplete_eof_discards_visible_text() -> Result<(), String> {
            for provider_error in [false, true] {
                let mut chunks = sse_text_hello();
                chunks.pop();
                if provider_error {
                    chunks.push("data: {\"type\":\"error\",\"error\":{\"message\":\"private provider body\"}}\n\n".into());
                }
                let (tx, rx) = std::sync::mpsc::channel();
                let c = client(chunks).with_text_out(tx);
                let result = c.post_streaming(&json!({ "messages": [] })).await;
                assert!(result.is_err());
                assert!(!format!("{result:?}").contains("private provider body"));
                drop(c);
                assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![
                    ToolTextEvent::Delta { model_turn: 0, text: "Hel".into() },
                    ToolTextEvent::Delta { model_turn: 0, text: "lo".into() },
                    ToolTextEvent::Discard { model_turn: 0 },
                ]);
            }
            Ok(())
        }

        /// The row is written before the socket, so a turn that left the device and came back
        /// unauthorized is still accounted for (invariant 3 / AR-11).
        #[tokio::test]
        async fn a_streamed_final_turn_paints_text_then_returns_the_assembled_message() {
            let (tx, rx) = std::sync::mpsc::channel();
            let c = client(sse_text_hello()).with_text_out(tx);
            let msg = c.post_streaming(&json!({ "messages": [] })).await.unwrap();
            drop(c);
            let painted: Vec<_> = rx.try_iter().collect();
            assert_eq!(painted, vec![ToolTextEvent::Delta { model_turn: 0, text: "Hel".into() }, ToolTextEvent::Delta { model_turn: 0, text: "lo".into() }, ToolTextEvent::Commit { model_turn: 0 }]);
            let parsed = parse_tool_turn(&msg).unwrap();
            assert!(matches!(parsed.turn, ModelTurn::Final(t) if t == "Hello"));
        }

        #[tokio::test]
        async fn a_streamed_tool_use_does_not_paint_json_as_chat_text() {
            let (tx, rx) = std::sync::mpsc::channel();
            let c = client(sse_tool_use()).with_text_out(tx);
            let msg = c.post_streaming(&json!({ "messages": [] })).await.unwrap();
            drop(c);
            assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![ToolTextEvent::Discard { model_turn: 0 }]);
            let parsed = parse_tool_turn(&msg).unwrap();
            let ModelTurn::ToolUses(uses) = parsed.turn else {
                panic!("expected tool_use, got {:?}", parsed.turn);
            };
            assert_eq!(uses[0].name, "list_calendar_events");
            assert_eq!(uses[0].input, json!({ "day": "tomorrow" }));
        }

        #[tokio::test]
        async fn a_rejected_stream_is_traced_and_never_painted() {
            let (tx, rx) = std::sync::mpsc::channel();
            let c = ToolTurnHttp::new(
                MockStreamingTransport::new(
                    401,
                    vec![r#"{"error":{"message":"invalid x-api-key sk-secret"}}"#.into()],
                ),
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_text_out(tx);
            let err = c.post_streaming(&json!({ "messages": [] })).await.unwrap_err();
            assert!(!err.contains("sk-secret"), "the error echoed a secret: {err}");
            assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![ToolTextEvent::Discard { model_turn: 0 }]);
            let recs = c.sink.records();
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].route, Route::MessagesApi);
            assert_eq!(recs[0].purpose, "agent_tools");
        }

        #[tokio::test]
        async fn stop_drops_a_silent_stream_without_another_token() -> Result<(), String> {
            use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
            struct Silent { entered: Arc<AtomicBool>, dropped: Arc<AtomicBool> }
            struct OnDrop(Arc<AtomicBool>);
            impl Drop for OnDrop {
                fn drop(&mut self) { self.0.store(true, Ordering::Release); }
            }
            impl crate::llm::transport::StreamingTransport for Silent {
                async fn send_streaming<F>(&self, _: crate::llm::transport::HttpRequest, _: F)
                    -> Result<crate::llm::transport::StreamOutcome, crate::llm::transport::TransportError>
                where F: FnMut(&str) -> bool + Send {
                    let _drop = OnDrop(self.dropped.clone());
                    self.entered.store(true, Ordering::Release);
                    std::future::pending().await
                }
            }
            let entered = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            let (tx, rx) = std::sync::mpsc::channel();
            let c = ToolTurnHttp::new(Silent { entered: entered.clone(), dropped: dropped.clone() }, RecordingSink::new(), ByokKey::new(Secret::new("k")), cfg())
                .with_cancellation(cancelled.clone()).with_text_out(tx);
            let body = json!({ "messages": [] });
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                tokio::join!(c.post_streaming(&body), async {
                    while !entered.load(Ordering::Acquire) { tokio::task::yield_now().await; }
                    cancelled.store(true, Ordering::Release);
                })
            }).await.map_err(|_| "Stop left silent HTTP running")?;
            assert_eq!(result, Err("cancelled".into()));
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(c.sink.records().len(), 1);
            assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![ToolTextEvent::Discard { model_turn: 0 }]);
            Ok(())
        }

        #[tokio::test]
        async fn the_streaming_deadline_covers_a_silent_provider() {
            struct Silent;
            impl crate::llm::transport::StreamingTransport for Silent {
                async fn send_streaming<F>(
                    &self,
                    _req: crate::llm::transport::HttpRequest,
                    _on_chunk: F,
                ) -> Result<
                    crate::llm::transport::StreamOutcome,
                    crate::llm::transport::TransportError,
                >
                where
                    F: FnMut(&str) -> bool + Send,
                {
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves")
                }
            }

            let c = ToolTurnHttp::new(
                Silent,
                RecordingSink::new(),
                ByokKey::new(Secret::new("byok-123456")),
                cfg(),
            )
            .with_turn_timeout(std::time::Duration::from_millis(20));
            let err = c.post_streaming(&json!({ "messages": [] })).await.unwrap_err();
            assert_eq!(err, "the model did not answer in time");
        }
    }

    /// A tool result (memory search, mail) re-enters the prompt after the
    /// context was assembled, so it is redacted on the way in. Id and error flag are untouched.
    #[test]
    fn tool_results_mask_card_ssn_and_my_number_before_they_reach_the_model() {
        let mut convo = ToolConversation::new("q");
        convo.push_assistant_content(tool_use_response()["content"].clone());
        convo.push_tool_results(&[ToolResult {
            id: "toolu_1".into(),
            content: "Found: card 4111 1111 1111 1111, ssn 123-45-6789, マイナンバー 123456789012".into(),
            is_error: false,
        }]);
        let last = convo.messages().last().cloned().unwrap_or_default();
        let wire = last.to_string();
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(last["content"][0]["is_error"], false);
        assert!(wire.contains("Found:"), "{wire}");
        assert!(!wire.contains("4111"), "{wire}");
        assert!(!wire.contains("123-45-6789"), "{wire}");
        assert!(!wire.contains("123456789012"), "{wire}");
    }
}
