//! Connected tools on the subscription delegates (issue #484): Claude Code and Codex. The Gemini
//! CLI has no bridged form (see `subscription::BridgedDelegate`).
//!
//! The API-key lanes hand the model a `tools` array and drive
//! [`shogun_mcp::tool_loop::run_read_loop_observed`] one Messages/Responses turn at a time. A
//! delegated CLI runs its own agent loop, so instead it is started with exactly one MCP server —
//! SHOGUN's chat bridge ([`shogun_mcp::chat_bridge`]) — and **the CLI is treated as the loop's
//! model**: [`BridgedDelegateTurnSource`] turns each `tools/call` the CLI sends to the bridge into
//! a [`ModelTurn::ToolUses`], hands the loop's results back as the MCP replies, and turns the
//! CLI's final answer into [`ModelTurn::Final`]. The loop itself is reused unchanged, so the gate
//! (`classify_call`), the untrusted fencing of results, the tool budget, L3 proposals into the
//! approval queue, the events, and the desktop's policy wrappers are the same code as on the
//! API-key lanes. The CLI never reaches a service, the Memory API or the user's MCP servers.
//!
//! **Invariant 5.** This runs only on the user's own subscription (the Agent lane): it is built
//! from a [`CommandRunner`] running the user's `claude` or `codex`, and traces on
//! [`Route::LocalAgent`]. There is no Batch client anywhere in it, and nothing here can be handed
//! a Select KK key.
//!
//! **Invariant 3.** One trace row for the prompt when the CLI starts, and one for every tool
//! result handed back (it leaves the device inside the CLI's next request) — digest and length
//! only, like every other row.
//!
//! **Text.** Claude Code's stream-json text deltas (Codex: each finished agent message —
//! `codex exec --json` does not stream tokens, #388) go out as [`ToolTextEvent::Delta`]; a model
//! turn that ends in tool calls is [`ToolTextEvent::Discard`]ed, the final one
//! [`ToolTextEvent::Commit`]ted — the contract of the other tool lanes. Text and calls travel on
//! different pipes (stdout and the bridge socket), so text that arrives within the gathering
//! window after a call is still counted to the turn the call ends.
//!
//! **Mid-run notes (#395)** cannot reach a CLI that is already running, so
//! [`ModelTurnSource::accepts_user_notes`] stays `false` and the run is not steerable.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use shogun_mcp::chat_bridge::{self, BridgeCall, ChatBridgeHost};
use shogun_mcp::tool_loop::{ModelTurn, ModelTurnSource, ToolResult, ToolUse, MAX_TOOL_USES};

use super::conversation::ChatRequest;
use super::subscription::{
    bridged_spec, classify, codex_error_message, with_codex_error, BridgedDelegate, ChatBridgeLaunch,
    CommandOutput, CommandRunner, CommandSpec, Delegate, DelegateFailure, DelegateOptions, DeltaParser,
    RunError, StreamFormat,
};
use super::tool_turns::{system_with_connected_services, ToolTextEvent};
use super::traceability::{Route, TraceRecord, TraceabilitySink};
use super::usage::{Usage, UsageHook, UsageOutcome, UsageRecord};
use super::LlmError;

/// How long after the first call of a turn further calls are gathered into the same turn. The
/// CLI sends parallel calls together; the loop runs reads of one turn in parallel (FR-HN-07).
pub const GATHER_WINDOW: Duration = Duration::from_millis(50);

/// How long the source waits without any event from the run before giving up. The runner's own
/// inactivity deadline (`subscription::COMPLETION_TIMEOUT`, 120 s) normally ends a silent run
/// first; this only guards against a runner that never reports at all.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(150);

/// The traceability purpose of a bridged run's rows (the same as the API-key tool lanes').
pub const TRACE_PURPOSE: &str = "agent_tools";

const POLL: Duration = Duration::from_millis(20);

/// What the delegated run reports to its source.
pub enum DelegateRunEvent {
    /// The CLI called one of SHOGUN's tools through the bridge.
    Call(BridgeCall),
    /// A text delta of the CLI's answer.
    Text(String),
    /// The CLI exited: its final answer (empty when it reported none of its own), or a short
    /// reason it failed — never body text.
    Finished(Result<String, String>),
}

/// The classified failure of a bridged run, for the caller to show the same way as on the
/// tool-free delegate path (sign-in, plan limit, update needed) instead of a generic error.
#[derive(Clone, Default)]
pub struct FailureSlot(Arc<Mutex<Option<LlmError>>>);

impl FailureSlot {
    fn set(&self, e: LlmError) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(e);
        }
    }

    /// The run's failure, once (`None` when it did not fail or was already taken).
    pub fn take(&self) -> Option<LlmError> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// The [`ModelTurnSource`] over one delegated CLI run (see the module docs).
pub struct BridgedDelegateTurnSource {
    events: Receiver<DelegateRunEvent>,
    /// Events read ahead while gathering a turn (a finish that raced the last call).
    stash: VecDeque<DelegateRunEvent>,
    /// Calls handed to the loop and not yet answered, by the id this source gave them.
    pending: Vec<(String, BridgeCall)>,
    next_id: u64,
    max_calls_per_turn: usize,
    gather_window: Duration,
    silence_limit: Duration,
    /// The run's own stop flag: set on Stop, on a give-up and on drop; the runner kills the CLI.
    abort: Arc<AtomicBool>,
    cancelled: Option<Arc<AtomicBool>>,
    text_out: Option<Sender<ToolTextEvent>>,
    model_turn: u32,
    turn_text: String,
    trace: Option<Arc<dyn TraceabilitySink>>,
    failure: FailureSlot,
    /// The CLI behind the run: its trace destination and the name in its error messages.
    delegate: Delegate,
    /// Kept for its lifetime only: dropping the source ends the run's bridge.
    host: Option<ChatBridgeHost>,
}

impl BridgedDelegateTurnSource {
    /// A source over `events`. `abort` is the flag the run's runner watches.
    pub fn new(events: Receiver<DelegateRunEvent>, abort: Arc<AtomicBool>) -> Self {
        Self {
            events,
            stash: VecDeque::new(),
            pending: Vec::new(),
            next_id: 1,
            max_calls_per_turn: MAX_TOOL_USES as usize,
            gather_window: GATHER_WINDOW,
            silence_limit: SILENCE_LIMIT,
            abort,
            cancelled: None,
            text_out: None,
            model_turn: 0,
            turn_text: String::new(),
            trace: None,
            failure: FailureSlot::default(),
            delegate: Delegate::ClaudeCode,
            host: None,
        }
    }

    /// The CLI behind the run (Claude Code unless set).
    pub fn with_delegate(mut self, delegate: Delegate) -> Self {
        self.delegate = delegate;
        self
    }

    /// The user's Stop flag: once set, the next wait returns `Err("cancelled")` and the run is
    /// aborted.
    pub fn with_cancellation(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    /// Where text events go (the panel's tool-text channel).
    pub fn with_text_out(mut self, tx: Sender<ToolTextEvent>) -> Self {
        self.text_out = Some(tx);
        self
    }

    /// Trace every tool result handed back to the CLI (invariant 3).
    pub fn with_trace(mut self, trace: Arc<dyn TraceabilitySink>) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn with_gather_window(mut self, window: Duration) -> Self {
        self.gather_window = window;
        self
    }

    pub fn with_silence_limit(mut self, limit: Duration) -> Self {
        self.silence_limit = limit;
        self
    }

    fn with_host(mut self, host: ChatBridgeHost) -> Self {
        self.host = Some(host);
        self
    }

    fn with_failure(mut self, failure: FailureSlot) -> Self {
        self.failure = failure;
        self
    }

    /// The slot the run's classified failure lands in. Clone it before wrapping the source.
    pub fn failure(&self) -> FailureSlot {
        self.failure.clone()
    }

    fn emit(&self, event: ToolTextEvent) {
        if let Some(tx) = &self.text_out {
            let _ = tx.send(event);
        }
    }

    fn delta(&mut self, text: String) {
        self.turn_text.push_str(&text);
        self.emit(ToolTextEvent::Delta { model_turn: self.model_turn, text });
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.as_ref().is_some_and(|c| c.load(Ordering::Acquire))
    }

    /// Answer the calls of the previous turn, by id. A call the loop did not answer (never the
    /// case on the lock-step path) is told so rather than left hanging.
    fn deliver(&mut self, results: &[ToolResult]) {
        for r in results {
            let Some(at) = self.pending.iter().position(|(id, _)| *id == r.id) else { continue };
            let (_, call) = self.pending.remove(at);
            // The result leaves the process for the delegate: egress redaction, and no
            // ledger row means the content does not go — the call is told so instead.
            let content = crate::llm::tool_turns::egress_tool_result_content(r);
            let traced = match &self.trace {
                Some(trace) => trace
                    .try_record(TraceRecord::for_chunk(
                        Route::LocalAgent,
                        TRACE_PURPOSE,
                        self.delegate.destination(),
                        &content,
                        false,
                    ))
                    .is_ok(),
                None => true,
            };
            if traced {
                call.reply(content, r.is_error);
            } else {
                call.reply(crate::llm::LlmError::TraceUnrecorded.to_string(), true);
            }
        }
        for (_, call) in self.pending.drain(..) {
            call.reply("That call was not answered.".to_string(), true);
        }
    }

    /// The next event, or why there is none.
    fn next_event(&mut self) -> Result<DelegateRunEvent, String> {
        if let Some(e) = self.stash.pop_front() {
            return Ok(e);
        }
        let started = Instant::now();
        loop {
            if self.is_cancelled() {
                self.abort.store(true, Ordering::Release);
                return Err("cancelled".to_string());
            }
            match self.events.recv_timeout(POLL) {
                Ok(e) => return Ok(e),
                Err(RecvTimeoutError::Timeout) => {
                    if started.elapsed() >= self.silence_limit {
                        self.abort.store(true, Ordering::Release);
                        return Err("the model did not answer in time".to_string());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("{} stopped without an answer", self.delegate.label()));
                }
            }
        }
    }

    /// Gather the calls of one turn: `first`, then whatever arrives within the window, up to the
    /// per-turn cap (more stay queued for the next turn).
    fn gather(&mut self, first: BridgeCall) -> Vec<BridgeCall> {
        let mut calls = vec![first];
        let deadline = Instant::now() + self.gather_window;
        while calls.len() < self.max_calls_per_turn {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.events.recv_timeout(left) {
                Ok(DelegateRunEvent::Call(c)) => calls.push(c),
                // Text written before the call but read after it still belongs to this turn.
                Ok(DelegateRunEvent::Text(t)) => self.delta(t),
                Ok(finished @ DelegateRunEvent::Finished(_)) => {
                    self.stash.push_back(finished);
                    break;
                }
                Err(_) => break,
            }
        }
        calls
    }
}

impl ModelTurnSource for BridgedDelegateTurnSource {
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        self.deliver(results);
        loop {
            let event = match self.next_event() {
                Ok(e) => e,
                Err(e) => {
                    self.emit(ToolTextEvent::Discard { model_turn: self.model_turn });
                    return Err(e);
                }
            };
            match event {
                DelegateRunEvent::Text(t) => self.delta(t),
                DelegateRunEvent::Call(first) => {
                    let calls = self.gather(first);
                    self.emit(ToolTextEvent::Discard { model_turn: self.model_turn });
                    self.model_turn += 1;
                    self.turn_text.clear();
                    let uses = calls
                        .into_iter()
                        .map(|call| {
                            let id = format!("b{}", self.next_id);
                            self.next_id += 1;
                            let input = call.arguments.clone();
                            let name = call.name.clone();
                            self.pending.push((id.clone(), call));
                            ToolUse { id, name, input }
                        })
                        .collect();
                    return Ok(ModelTurn::ToolUses(uses));
                }
                DelegateRunEvent::Finished(Ok(answer)) => {
                    // The CLI's own final text when it reported one, else this turn's deltas.
                    let text = if answer.trim().is_empty() {
                        std::mem::take(&mut self.turn_text)
                    } else {
                        answer
                    };
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        self.emit(ToolTextEvent::Discard { model_turn: self.model_turn });
                        return Err(format!("{} returned an empty response", self.delegate.label()));
                    }
                    self.emit(ToolTextEvent::Commit { model_turn: self.model_turn });
                    return Ok(ModelTurn::Final(text));
                }
                DelegateRunEvent::Finished(Err(why)) => {
                    self.emit(ToolTextEvent::Discard { model_turn: self.model_turn });
                    return Err(why);
                }
            }
        }
    }
}

impl Drop for BridgedDelegateTurnSource {
    fn drop(&mut self) {
        // However the loop ended: the CLI is stopped, unanswered calls end, the socket goes.
        self.abort.store(true, Ordering::Release);
        for (_, call) in self.pending.drain(..) {
            call.reply(chat_bridge::RUN_ENDED.to_string(), true);
        }
        self.host.take();
    }
}

/// The prompt a bridged run is given: the chat request flattened the way the tool-free delegate
/// path flattens it (`ChatRequest::render_flat`), with the Connected-services block on the system
/// half exactly as the API-key tool lanes append it. No new prompt text.
pub fn bridged_chat_prompt(request: &ChatRequest, connected_block: Option<&str>) -> String {
    let mut request = request.clone();
    request.system = system_with_connected_services(&request.system, connected_block);
    request.render_flat()
}

/// [`bridged_chat_prompt`] for a run whose `opts` carry SHOGUN's chat contract in the CLI's
/// system role ([`DelegateOptions::shogun_system`], owner decision 2026-10-14): the contract
/// (`delegated_system_text`) leaves the stdin prompt so it is said once, in the stronger role.
/// Directives, skills, the connected-services block and this turn's context stay on stdin.
pub fn bridged_chat_prompt_for(
    request: &ChatRequest,
    connected_block: Option<&str>,
    opts: &DelegateOptions,
) -> String {
    if !opts.shogun_system {
        return bridged_chat_prompt(request, connected_block);
    }
    use crate::llm::prompt_cache::{CHAT_SYSTEM_PREAMBLE, MEMORY_TOOL_RULES};
    let mut request = request.clone();
    request.system = request.system.replacen(CHAT_SYSTEM_PREAMBLE, "", 1).replacen(MEMORY_TOOL_RULES, "", 1);
    bridged_chat_prompt(&request, connected_block)
}

/// Everything one bridged delegate chat run needs except the process runner.
pub struct BridgedDelegateRun {
    /// Which CLI runs (Claude Code or Codex; the Gemini CLI has no bridged form).
    pub delegate: BridgedDelegate,
    /// Where the run's socket goes (`chat_bridge::socket_path_in`).
    pub socket_path: PathBuf,
    /// The `shogun-mcp` the CLI starts in bridge mode (`chat_bridge::bridge_server_path`).
    pub server: PathBuf,
    /// The loop's catalog (`tool_definitions`), Messages-API shape.
    pub tools: Vec<Value>,
    /// The flat prompt ([`bridged_chat_prompt`]); travels on stdin.
    pub prompt: String,
    pub options: DelegateOptions,
    pub trace: Arc<dyn TraceabilitySink>,
    pub usage: Option<UsageHook>,
    /// The user's Stop.
    pub cancelled: Arc<AtomicBool>,
    pub text_out: Option<Sender<ToolTextEvent>>,
}

impl BridgedDelegateRun {
    /// Open the run's bridge, start the delegate on `runner` with it, and return the source the
    /// tool loop drives. `Err` (a short reason) when the bridge cannot be opened — nothing has
    /// been started or sent then, and the caller can fall back to the tool-free path.
    pub fn start<R: CommandRunner + 'static>(self, runner: R) -> Result<BridgedDelegateTurnSource, String> {
        let (tx, rx) = mpsc::channel::<DelegateRunEvent>();
        let host = ChatBridgeHost::start_into(
            &self.socket_path,
            chat_bridge::mcp_tool_list(&self.tools),
            tx.clone(),
            DelegateRunEvent::Call,
        )?;
        let launch = ChatBridgeLaunch {
            server: self.server.to_str().ok_or("the bridge server path is not UTF-8")?.to_string(),
            socket: host
                .socket_path()
                .to_str()
                .ok_or("the bridge socket path is not UTF-8")?
                .to_string(),
            nonce: host.nonce().to_string(),
        };
        let spec = bridged_spec(self.delegate, &self.prompt, &launch, &self.options);
        let delegate = self.delegate.delegate();

        // Egress is traced before the process starts: the prompt leaves either way (AR-11), and
        // without a ledger row it does not leave at all (fail-closed).
        self.trace
            .try_record(TraceRecord::for_chunk(
                Route::LocalAgent,
                TRACE_PURPOSE,
                delegate.destination(),
                &self.prompt,
                false,
            ))
            .map_err(|_| crate::llm::LlmError::TraceUnrecorded.to_string())?;

        let abort = Arc::new(AtomicBool::new(false));
        // Stop reaches the runner through the run's own flag, which drop and give-ups also set.
        {
            let (cancelled, abort) = (self.cancelled.clone(), abort.clone());
            std::thread::spawn(move || {
                while !abort.load(Ordering::Acquire) {
                    if cancelled.load(Ordering::Acquire) {
                        abort.store(true, Ordering::Release);
                        return;
                    }
                    std::thread::sleep(POLL);
                }
            });
        }
        let failure = FailureSlot::default();
        {
            let (abort, failure, usage) = (abort.clone(), failure.clone(), self.usage.clone());
            let prompt_bytes = self.prompt.len();
            std::thread::spawn(move || {
                run_delegate(delegate, &runner, &spec, &abort, &tx, &failure, usage.as_ref(), prompt_bytes);
            });
        }

        let mut source = BridgedDelegateTurnSource::new(rx, abort)
            .with_delegate(delegate)
            .with_cancellation(self.cancelled)
            .with_trace(self.trace)
            .with_failure(failure)
            .with_host(host);
        if let Some(text_out) = self.text_out {
            source = source.with_text_out(text_out);
        }
        Ok(source)
    }
}

/// `(text, is_error)` of a Claude Code stream-json `result` event.
fn result_event(line: &str) -> Option<(String, bool)> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    let text = v.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
    Some((text, v.get("is_error").and_then(Value::as_bool).unwrap_or(false)))
}

enum RunEnd {
    Answer(String),
    Cancelled,
    Failed(LlmError),
}

/// The run's thread: the delegate with the bridge, text deltas out as they arrive, one usage row
/// and one `Finished` at the end.
#[allow(clippy::too_many_arguments)] // one call site (`BridgedDelegateRun::start`)
fn run_delegate<R: CommandRunner>(
    delegate: Delegate,
    runner: &R,
    spec: &CommandSpec,
    abort: &AtomicBool,
    tx: &Sender<DelegateRunEvent>,
    failure: &FailureSlot,
    usage: Option<&UsageHook>,
    prompt_bytes: usize,
) {
    let started = Instant::now();
    // Claude Code: stream-json; Codex: `exec --json` events (the bridged specs' output formats).
    let format = delegate.stream_format().unwrap_or(StreamFormat::ClaudeStreamJson);
    let mut parser = DeltaParser::new(format);
    let mut result: Option<(String, bool)> = None;
    let mut first_token_ms: Option<u64> = None;
    let mut streamed_bytes = 0usize;
    let mut receiver_gone = false;
    let mut on_line = |line: &str| {
        if format == StreamFormat::ClaudeStreamJson {
            if let Some(r) = result_event(line) {
                result = Some(r);
            }
        }
        match parser.push(line) {
            Some(delta) if !delta.is_empty() => {
                streamed_bytes += delta.len();
                first_token_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                if tx.send(DelegateRunEvent::Text(delta)).is_err() {
                    receiver_gone = true;
                    return false;
                }
                true
            }
            _ => true,
        }
    };
    let run = runner.run_streaming_cancellable(spec, &mut on_line, abort);
    let end = match run {
        Err(RunError::NotFound) => RunEnd::Failed(DelegateFailure::NotInstalled.into_llm_error(delegate)),
        Err(RunError::Timeout) => RunEnd::Failed(DelegateFailure::Timeout.into_llm_error(delegate)),
        Err(RunError::Cancelled) => RunEnd::Cancelled,
        Err(RunError::Io(e)) => RunEnd::Failed(DelegateFailure::Failed(e).into_llm_error(delegate)),
        Ok(_) if receiver_gone || abort.load(Ordering::Acquire) => RunEnd::Cancelled,
        // Codex reports why on stdout, inside its JSON events (`subscription::with_codex_error`).
        Ok(out) if !out.ok() && format == StreamFormat::CodexJsonl => {
            RunEnd::Failed(classify(&with_codex_error(out)).into_llm_error(delegate))
        }
        Ok(out) if !out.ok() => RunEnd::Failed(classify(&out).into_llm_error(delegate)),
        // A zero exit whose JSON reported a failed turn and no answer, in Codex's own words.
        Ok(out)
            if format == StreamFormat::CodexJsonl
                && streamed_bytes == 0
                && codex_error_message(&out.stdout).is_some() =>
        {
            RunEnd::Failed(classify(&with_codex_error(out)).into_llm_error(delegate))
        }
        Ok(_) => match result {
            // A zero exit that still reported a failed turn: the CLI's own words, classified the
            // same way as a failed exit (sign-in, plan limit…).
            Some((text, true)) => {
                let out = CommandOutput { code: Some(1), stdout: String::new(), stderr: text };
                RunEnd::Failed(classify(&out).into_llm_error(delegate))
            }
            Some((text, false)) => RunEnd::Answer(text),
            None => RunEnd::Answer(String::new()),
        },
    };
    if let Some(hook) = usage {
        let (u, outcome) = match &end {
            RunEnd::Answer(text) => (
                Usage::estimated_from_bytes(prompt_bytes, text.len().max(streamed_bytes)),
                UsageOutcome::Ok,
            ),
            RunEnd::Cancelled => (Usage::estimated_from_bytes(prompt_bytes, streamed_bytes), UsageOutcome::Ok),
            RunEnd::Failed(e) => (Usage::default(), UsageOutcome::from_error(e)),
        };
        hook.sink.record(UsageRecord::new(
            Route::LocalAgent,
            hook.tag,
            delegate.binary(),
            u,
            first_token_ms,
            started.elapsed().as_millis() as u64,
            outcome,
        ));
    }
    let finished = match end {
        RunEnd::Answer(text) => Ok(text),
        RunEnd::Cancelled => Err("cancelled".to_string()),
        RunEnd::Failed(e) => {
            let why = e.to_string();
            failure.set(e);
            Err(why)
        }
    };
    let _ = tx.send(DelegateRunEvent::Finished(finished));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A source over a hand-fed event channel (no CLI, no socket).
    fn fed() -> (Sender<DelegateRunEvent>, BridgedDelegateTurnSource, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let abort = Arc::new(AtomicBool::new(false));
        (tx, BridgedDelegateTurnSource::new(rx, abort.clone()), abort)
    }

    fn call(name: &str, args: Value) -> (DelegateRunEvent, chat_bridge::BridgeCallAnswer) {
        let (c, a) = BridgeCall::detached(name, args);
        (DelegateRunEvent::Call(c), a)
    }

    const WAIT: Duration = Duration::from_secs(5);

    // the source over hand-fed events

    #[test]
    fn one_call_becomes_a_tool_use_and_its_result_goes_back_as_the_reply() {
        let (tx, mut source, _) = fed();
        let (ev, answer) = call("list_calendar_events", json!({ "day": "today" }));
        tx.send(ev).unwrap();
        let turn = source.next_turn(&[]).unwrap();
        let ModelTurn::ToolUses(uses) = turn else { panic!("{turn:?}") };
        assert_eq!(uses, vec![ToolUse { id: "b1".into(), name: "list_calendar_events".into(), input: json!({ "day": "today" }) }]);
        tx.send(DelegateRunEvent::Finished(Ok("Two meetings.".into()))).unwrap();
        let result = ToolResult { id: "b1".into(), content: "fenced data".into(), is_error: false };
        assert_eq!(source.next_turn(&[result]).unwrap(), ModelTurn::Final("Two meetings.".into()));
        assert_eq!(answer.wait(WAIT), Some(("fenced data".to_string(), false)));
    }

    #[test]
    fn parallel_calls_within_the_window_are_one_turn_and_answered_by_id() {
        let (tx, source, _) = fed();
        let mut source = source.with_gather_window(Duration::from_millis(200));
        let (a, ans_a) = call("list_calendar_events", json!({}));
        let (b, ans_b) = call("check_calendar_availability", json!({}));
        tx.send(a).unwrap();
        tx.send(b).unwrap();
        let ModelTurn::ToolUses(uses) = source.next_turn(&[]).unwrap() else { panic!() };
        assert_eq!(uses.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(), ["b1", "b2"]);
        tx.send(DelegateRunEvent::Finished(Ok("ok".into()))).unwrap();
        // Results in reverse order still reach their own calls.
        let results = [
            ToolResult { id: "b2".into(), content: "B".into(), is_error: true },
            ToolResult { id: "b1".into(), content: "A".into(), is_error: false },
        ];
        source.next_turn(&results).unwrap();
        assert_eq!(ans_a.wait(WAIT), Some(("A".into(), false)));
        assert_eq!(ans_b.wait(WAIT), Some(("B".into(), true)));
    }

    #[test]
    fn calls_past_the_per_turn_cap_wait_for_the_next_turn() {
        let (tx, source, _) = fed();
        let mut source = source.with_gather_window(Duration::from_millis(200));
        let mut answers = Vec::new();
        for i in 0..(MAX_TOOL_USES + 2) {
            let (ev, a) = call(&format!("t{i}"), json!({}));
            tx.send(ev).unwrap();
            answers.push(a);
        }
        let ModelTurn::ToolUses(first) = source.next_turn(&[]).unwrap() else { panic!() };
        assert_eq!(first.len(), MAX_TOOL_USES as usize);
        let results: Vec<ToolResult> =
            first.iter().map(|u| ToolResult { id: u.id.clone(), content: "x".into(), is_error: false }).collect();
        let ModelTurn::ToolUses(rest) = source.next_turn(&results).unwrap() else { panic!() };
        assert_eq!(rest.len(), 2);
    }

    #[test]
    fn text_streams_as_deltas_then_discard_on_calls_and_commit_on_the_answer() {
        let (tx, source, _) = fed();
        let (text_tx, text_rx) = mpsc::channel();
        let mut source = source.with_text_out(text_tx);
        tx.send(DelegateRunEvent::Text("Let me check.".into())).unwrap();
        let (ev, _answer) = call("list_calendar_events", json!({}));
        tx.send(ev).unwrap();
        // Preamble text that was read after the call still belongs to the turn the call ends.
        tx.send(DelegateRunEvent::Text(" One sec.".into())).unwrap();
        let _ = source.next_turn(&[]).unwrap();
        tx.send(DelegateRunEvent::Text("Two ".into())).unwrap();
        tx.send(DelegateRunEvent::Text("meetings.".into())).unwrap();
        tx.send(DelegateRunEvent::Finished(Ok(String::new()))).unwrap();
        let r = ToolResult { id: "b1".into(), content: "d".into(), is_error: false };
        // No `result` text of its own: the final turn's deltas are the answer.
        assert_eq!(source.next_turn(&[r]).unwrap(), ModelTurn::Final("Two meetings.".into()));
        drop(source);
        let events: Vec<ToolTextEvent> = text_rx.try_iter().collect();
        assert_eq!(
            events,
            vec![
                ToolTextEvent::Delta { model_turn: 0, text: "Let me check.".into() },
                ToolTextEvent::Delta { model_turn: 0, text: " One sec.".into() },
                ToolTextEvent::Discard { model_turn: 0 },
                ToolTextEvent::Delta { model_turn: 1, text: "Two ".into() },
                ToolTextEvent::Delta { model_turn: 1, text: "meetings.".into() },
                ToolTextEvent::Commit { model_turn: 1 },
            ]
        );
        // Same reading the panel's first-token clock does (issue #750).
        let mut clock = crate::metrics::FirstTokenClock::new();
        for (i, e) in events.iter().enumerate() {
            clock.observe_tool_text(e, i as f64);
        }
        assert_eq!(clock.first_visible_ms(), Some(0.0));
        assert_eq!(clock.first_answer_ms(), Some(3.0));
    }

    #[test]
    fn a_failed_or_silent_run_is_an_error_without_body_text() {
        let (tx, mut source, _) = fed();
        tx.send(DelegateRunEvent::Finished(Err("Claude Code: needs sign-in".into()))).unwrap();
        assert_eq!(source.next_turn(&[]), Err("Claude Code: needs sign-in".into()));

        let (tx, mut source, _) = fed();
        tx.send(DelegateRunEvent::Finished(Ok("   ".into()))).unwrap();
        assert!(source.next_turn(&[]).is_err(), "an empty answer is not an answer");

        let (tx, mut source, _) = fed();
        drop(tx);
        assert!(source.next_turn(&[]).is_err(), "a run that vanished");

        let (_tx, source, abort) = fed();
        let mut source = source.with_silence_limit(Duration::from_millis(60));
        assert_eq!(source.next_turn(&[]), Err("the model did not answer in time".into()));
        assert!(abort.load(Ordering::Acquire), "a give-up stops the CLI");
    }

    #[test]
    fn stop_aborts_the_run_and_ends_the_waiting_calls() {
        let (tx, source, abort) = fed();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut source = source.with_cancellation(cancelled.clone());
        let (ev, answer) = call("list_calendar_events", json!({}));
        tx.send(ev).unwrap();
        let _ = source.next_turn(&[]).unwrap();
        cancelled.store(true, Ordering::Release);
        assert_eq!(source.next_turn(&[]), Err("cancelled".into()));
        assert!(abort.load(Ordering::Acquire));
        // The unanswered call was told, not left hanging.
        assert_eq!(answer.wait(WAIT).map(|(_, e)| e), Some(true));
        drop(source);
    }

    #[test]
    fn dropping_the_source_stops_the_run_and_answers_what_is_pending() {
        let (tx, mut source, abort) = fed();
        let (ev, answer) = call("list_calendar_events", json!({}));
        tx.send(ev).unwrap();
        let _ = source.next_turn(&[]).unwrap();
        drop(source);
        assert!(abort.load(Ordering::Acquire));
        assert_eq!(answer.wait(WAIT), Some((chat_bridge::RUN_ENDED.to_string(), true)));
    }

    #[test]
    fn notes_are_not_taken_and_results_are_lock_step() {
        let (_tx, source, _) = fed();
        assert!(!source.accepts_user_notes());
        assert!(!source.supports_async_results());
    }

    #[test]
    fn the_bridged_run_is_agent_lane_only() {
        // Invariant 5: nothing here is a Batch client, so indexing / Dream Cycle / Brief can never
        // be routed through the user's plan by way of the bridge. Evaluated at compile time, the
        // same probe `subscription.rs` uses for the delegate client.
        struct Probe<T: ?Sized>(std::marker::PhantomData<T>);
        trait NotBatchClient {
            const IS_BATCH: bool = false;
        }
        impl<T: ?Sized> NotBatchClient for Probe<T> {}
        #[allow(dead_code)]
        impl<T: ?Sized + crate::llm::BatchClient> Probe<T> {
            const IS_BATCH: bool = true;
        }
        const PROBES: [bool; 2] =
            [Probe::<BridgedDelegateTurnSource>::IS_BATCH, Probe::<BridgedDelegateRun>::IS_BATCH];
        assert_eq!(PROBES, [false, false]);
    }

    #[test]
    fn the_bridge_names_agree_across_the_two_crates() {
        use crate::llm::subscription as sub;
        assert_eq!(sub::CHAT_BRIDGE_SERVER, chat_bridge::SERVER_NAME);
        assert_eq!(sub::CHAT_BRIDGE_ARG, chat_bridge::BRIDGE_ARG);
        assert_eq!(sub::CHAT_BRIDGE_SOCKET_ENV, chat_bridge::SOCKET_ENV);
        assert_eq!(sub::CHAT_BRIDGE_NONCE_ENV, chat_bridge::NONCE_ENV);
        assert_eq!(sub::CHAT_BRIDGE_CONFIG_ARG, chat_bridge::CONFIG_ARG);
    }

    #[test]
    fn the_bridge_file_a_codex_run_writes_is_what_the_bridge_reads() {
        // The two crates agree on the bridge file's shape.
        let launch = ChatBridgeLaunch { server: "/s/shogun-mcp".into(), socket: "/tmp/b/x.sock".into(), nonce: "abc".into() };
        let dir = std::env::temp_dir().join(format!("sdt-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = crate::llm::subscription::CHAT_BRIDGE_CONFIG_FILE;
        std::fs::write(dir.join(file), launch.bridge_config()).unwrap();
        assert_eq!(chat_bridge::read_config_file(&dir, file), Ok((PathBuf::from("/tmp/b/x.sock"), "abc".to_string())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_prompt_is_the_flat_chat_request_with_the_connected_services_block() {
        let request = ChatRequest {
            system: "You are SHOGUN.".into(),
            stable_context: String::new(),
            turns: Vec::new(),
            context: String::new(),
            message: "what's on today?".into(),
            images: Vec::new(),
        };
        let block = "## Connected services\n- Calendar";
        let prompt = bridged_chat_prompt(&request, Some(block));
        assert!(prompt.starts_with("You are SHOGUN.\n\n## Connected services"));
        assert!(prompt.ends_with("User: what's on today?\nSHOGUN:"));
        assert_eq!(bridged_chat_prompt(&request, None), request.render_flat());
    }

    /// With the contract in the CLI's system role, stdin does not repeat it; the user's own
    /// directives and the turn stay on stdin.
    #[test]
    fn a_system_role_run_moves_only_shoguns_contract_off_stdin() {
        use crate::llm::prompt_cache::{chat_system_prompt, CHAT_SYSTEM_PREAMBLE, MEMORY_TOOL_RULES};
        let mut system = chat_system_prompt("Tone: direct");
        system.push('\n');
        system.push_str(MEMORY_TOOL_RULES);
        let request = ChatRequest {
            system,
            stable_context: String::new(),
            turns: Vec::new(),
            context: "Now: 2026-10-14 (Wednesday)\n".into(),
            message: "what's on today?".into(),
            images: Vec::new(),
        };
        let plain = bridged_chat_prompt_for(&request, None, &DelegateOptions::default());
        assert_eq!(plain, bridged_chat_prompt(&request, None));
        let opts = DelegateOptions { shogun_system: true, ..Default::default() };
        let moved = bridged_chat_prompt_for(&request, Some("## Connected services\n- Calendar"), &opts);
        assert!(!moved.contains(CHAT_SYSTEM_PREAMBLE) && !moved.contains(MEMORY_TOOL_RULES), "{moved}");
        assert!(moved.contains("Tone: direct") && moved.contains("## Connected services"));
        assert!(moved.contains("Now: 2026-10-14") && moved.ends_with("User: what's on today?\nSHOGUN:"));
    }

    /// The loop-driving tests name `shogun_agents::permission::Action` (the proposal sink's
    /// type), a dependency of this crate only with `db`.
    #[cfg(feature = "db")]
    mod through_the_loop {
        use super::*;
        use crate::llm::traceability::RecordingSink;
        use shogun_agents::permission::{Action, Level};
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::entitlement::{entitlements, Plan};
        use shogun_mcp::scope::{Service, Wave};
        use shogun_mcp::tool_catalog::{tool_definitions, ServiceState, ToolContext};
        use shogun_mcp::tool_loop::{run_read_loop, LoopLimits, ProposalSink, ReadToolRunner, ToolRunError};

        fn ctx() -> ToolContext {
            ToolContext { highest_released: Wave::One, draft_stop: true, plan: entitlements(Plan::Pro, 0) }
        }

        fn calendar() -> Vec<ServiceState> {
            vec![ServiceState { service: Service::GoogleCalendar, conn: ConnState::Connected { last_sync_ms: 0 } }]
        }

        /// Answers every read with `<op>-data`, recording what reached "the service".
        #[derive(Default)]
        struct EchoRunner {
            reached: Mutex<Vec<&'static str>>,
        }
        impl ReadToolRunner for EchoRunner {
            fn run(&self, _s: Service, op: &'static str, _i: &Value, _t: u64) -> Result<String, ToolRunError> {
                if let Ok(mut g) = self.reached.lock() {
                    g.push(op);
                }
                Ok(format!("{op}-data"))
            }
        }

        #[derive(Default)]
        struct Queue {
            queued: Vec<(Action, String)>,
        }
        impl ProposalSink for Queue {
            fn propose(&mut self, action: Action, body: &str) -> Result<(), String> {
                self.queued.push((action, body.to_string()));
                Ok(())
            }
        }

        #[test]
        fn through_the_loop_a_refused_and_a_hallucinated_call_are_answered_not_run() {
            let (tx, mut source, _) = fed();
            let (gmail, ans_gmail) = call("search_mail", json!({ "query": "x" }));
            let (made_up, ans_made_up) = call("delete_everything", json!({}));
            tx.send(gmail).unwrap();
            tx.send(made_up).unwrap();
            let feeder = std::thread::spawn(move || {
                // The CLI answers once both replies came back.
                let a = ans_gmail.wait(WAIT);
                let b = ans_made_up.wait(WAIT);
                tx.send(DelegateRunEvent::Finished(Ok("Mail is not connected.".into()))).unwrap();
                (a, b)
            });
            let mut runner = EchoRunner::default();
            let mut queue = Queue::default();
            let out = run_read_loop(&mut source, &mut runner, &mut queue, &calendar(), &ctx(), LoopLimits::default()).unwrap();
            let (a, b) = feeder.join().unwrap();
            assert_eq!(out.answer, "Mail is not connected.");
            assert_eq!(out.refused, 2);
            assert!(runner.reached.lock().unwrap().is_empty(), "nothing reached a service");
            assert!(a.is_some_and(|(t, e)| e && t.contains("not connected")));
            assert!(b.is_some_and(|(t, e)| e && t.contains("No such tool")));
        }

        #[test]
        fn through_the_loop_the_budget_runs_out_and_the_cli_is_told_to_answer() {
            let (tx, source, _) = fed();
            let mut source = source.with_gather_window(Duration::from_millis(1));
            let limits = LoopLimits { max_tool_uses: 2, ..LoopLimits::default() };
            // The CLI calls one tool at a time, three times, then answers.
            let feeder = std::thread::spawn(move || {
                let mut last = None;
                for _ in 0..3 {
                    let (ev, ans) = call("list_calendar_events", json!({}));
                    tx.send(ev).unwrap();
                    last = ans.wait(WAIT);
                }
                tx.send(DelegateRunEvent::Finished(Ok("Here is what I found.".into()))).unwrap();
                last
            });
            let mut runner = EchoRunner::default();
            let out = run_read_loop(&mut source, &mut runner, &mut Queue::default(), &calendar(), &ctx(), limits).unwrap();
            let last = feeder.join().unwrap();
            assert!(out.hit_tool_budget);
            assert_eq!(out.executed, 2);
            assert_eq!(out.answer, "Here is what I found.");
            assert!(last.is_some_and(|(t, e)| e && t.contains("budget")));
        }

        // a real bridge, a scripted "CLI"

        /// One step of a scripted CLI run.
        #[derive(Clone)]
        enum Step {
            /// A text delta (Claude Code: stream-json; Codex: a finished agent message).
            Text(&'static str),
            /// Tool calls made at the same time through the bridge, each answered before going on.
            Calls(Vec<(&'static str, Value)>),
            /// The final answer (Claude Code: the `result` event; Codex: its last agent message),
            /// then exit 0.
            Result(&'static str),
            /// Exit with this code and reason (Claude Code: stderr; Codex: a `turn.failed` event
            /// on stdout, as `codex exec --json` reports it).
            Exit(i32, &'static str),
            /// Wait until killed (Stop).
            Hang,
        }

        /// Plays the delegate: finds the bridge the way the real CLI's MCP server would, talks to
        /// it over the real socket the way `shogun-mcp --chat-bridge` does, and writes its output
        /// format. Claude Code: the `--mcp-config` file's env. Codex: the `--chat-bridge-config`
        /// file named in `-c mcp_servers.shogun_chat.args`, read by the bridge's own reader from
        /// a copy of the scratch cwd.
        struct FakeCli {
            delegate: BridgedDelegate,
            script: Vec<Step>,
            seen: Arc<Mutex<Vec<CommandSpec>>>,
            replies: Arc<Mutex<Vec<Value>>>,
        }

        impl FakeCli {
            fn claude(script: Vec<Step>) -> Self {
                Self { delegate: BridgedDelegate::ClaudeCode, script, seen: Arc::default(), replies: Arc::default() }
            }

            fn codex(script: Vec<Step>) -> Self {
                Self { delegate: BridgedDelegate::Codex, ..Self::claude(script) }
            }

            fn text_line(&self, text: &str) -> String {
                match self.delegate {
                    BridgedDelegate::ClaudeCode => json!({ "type": "stream_event", "event": {
                        "type": "content_block_delta", "delta": { "type": "text_delta", "text": text } } })
                    .to_string(),
                    BridgedDelegate::Codex => json!({ "type": "item.completed",
                        "item": { "id": "item_1", "type": "agent_message", "text": text } })
                    .to_string(),
                }
            }

            /// The socket and nonce, found the way this CLI's bridge process finds them.
            fn bridge(&self, spec: &CommandSpec) -> (PathBuf, String) {
                match self.delegate {
                    BridgedDelegate::ClaudeCode => {
                        let at = spec.args.iter().position(|a| a == "--mcp-config").unwrap();
                        let file = &spec.args[at + 1];
                        let (_, contents) = spec.cwd_files.iter().find(|(n, _)| n == file).unwrap();
                        let cfg: Value = serde_json::from_str(contents).unwrap();
                        let env = &cfg["mcpServers"]["shogun_chat"]["env"];
                        (
                            PathBuf::from(env["SHOGUN_CHAT_BRIDGE_SOCKET"].as_str().unwrap()),
                            env["SHOGUN_CHAT_BRIDGE_NONCE"].as_str().unwrap().to_string(),
                        )
                    }
                    BridgedDelegate::Codex => {
                        let prefix = "mcp_servers.shogun_chat.args=";
                        let args = spec.args.iter().find_map(|a| a.strip_prefix(prefix)).unwrap();
                        let args: Vec<String> = serde_json::from_str(args).unwrap();
                        assert_eq!(args[0], chat_bridge::BRIDGE_ARG);
                        assert_eq!(args[1], chat_bridge::CONFIG_ARG);
                        // The scratch cwd, as the runner would prepare it.
                        let cwd = socket_dir("cwd");
                        std::fs::create_dir_all(&cwd).unwrap();
                        for (name, contents) in &spec.cwd_files {
                            std::fs::write(cwd.join(name), contents).unwrap();
                        }
                        let found = chat_bridge::read_config_file(&cwd, &args[2]).unwrap();
                        let _ = std::fs::remove_dir_all(cwd);
                        found
                    }
                }
            }
        }

        impl CommandRunner for FakeCli {
            fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, RunError> {
                Err(RunError::Io("buffered runs are not used here".into()))
            }

            fn run_streaming_cancellable(
                &self,
                spec: &CommandSpec,
                on_line: &mut dyn FnMut(&str) -> bool,
                cancelled: &AtomicBool,
            ) -> Result<CommandOutput, RunError> {
                self.seen.lock().unwrap().push(spec.clone());
                let (socket, nonce) = self.bridge(spec);
                // The CLI lists the server's tools first.
                let listed = chat_bridge::request(&socket, &nonce, "tools/list", None).unwrap();
                self.replies.lock().unwrap().push(listed);
                for step in &self.script {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(RunError::Cancelled);
                    }
                    match step {
                        Step::Text(t) => {
                            if !on_line(&self.text_line(t)) {
                                return Ok(CommandOutput { code: None, stdout: String::new(), stderr: String::new() });
                            }
                        }
                        Step::Calls(calls) => {
                            let handles: Vec<_> = calls
                                .iter()
                                .map(|(name, args)| {
                                    let (name, args) = (*name, args.clone());
                                    let (socket, nonce) = (socket.clone(), nonce.clone());
                                    std::thread::spawn(move || {
                                        chat_bridge::request(
                                            &socket,
                                            &nonce,
                                            "tools/call",
                                            Some(json!({ "name": name, "arguments": args })),
                                        )
                                    })
                                })
                                .collect();
                            for h in handles {
                                let reply = h.join().unwrap().unwrap_or_else(|e| json!({ "transport_error": e }));
                                self.replies.lock().unwrap().push(reply);
                            }
                        }
                        Step::Result(text) => {
                            let line = match self.delegate {
                                BridgedDelegate::ClaudeCode => json!({ "type": "result", "subtype": "success",
                                    "is_error": false, "result": text })
                                .to_string(),
                                BridgedDelegate::Codex => self.text_line(text),
                            };
                            on_line(&line);
                            return Ok(CommandOutput { code: Some(0), stdout: String::new(), stderr: String::new() });
                        }
                        Step::Exit(code, why) => {
                            let (stdout, stderr) = match self.delegate {
                                BridgedDelegate::ClaudeCode => (String::new(), why.to_string()),
                                BridgedDelegate::Codex => (
                                    json!({ "type": "turn.failed", "error": { "message": why } }).to_string(),
                                    String::new(),
                                ),
                            };
                            return Ok(CommandOutput { code: Some(*code), stdout, stderr });
                        }
                        Step::Hang => {
                            while !cancelled.load(Ordering::Acquire) {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            return Err(RunError::Cancelled);
                        }
                    }
                }
                Ok(CommandOutput { code: Some(0), stdout: String::new(), stderr: String::new() })
            }
        }

        fn socket_dir(tag: &str) -> PathBuf {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!("sdt-{tag}-{}-{n}", std::process::id()))
        }

        struct Started {
            source: BridgedDelegateTurnSource,
            trace: Arc<RecordingSink>,
            usage: Arc<crate::llm::usage::RecordingUsageSink>,
            text: Receiver<ToolTextEvent>,
            cancelled: Arc<AtomicBool>,
            dir: PathBuf,
        }

        fn start(fake: FakeCli, tag: &str) -> Started {
            let dir = socket_dir(tag);
            let trace = Arc::new(RecordingSink::new());
            let usage = Arc::new(crate::llm::usage::RecordingUsageSink::new());
            let (text_tx, text) = mpsc::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            let run = BridgedDelegateRun {
                delegate: fake.delegate,
                socket_path: chat_bridge::socket_path_in(std::slice::from_ref(&dir)).unwrap(),
                server: PathBuf::from("/Applications/ShogunAI.app/Contents/MacOS/shogun-mcp"),
                tools: tool_definitions(&calendar(), &ctx()),
                prompt: "SYSTEM\nUser: what is on today?\nSHOGUN:".into(),
                options: DelegateOptions::default(),
                trace: trace.clone(),
                usage: Some(UsageHook::new(usage.clone(), crate::llm::prompts::INLINE_CHAT)),
                cancelled: cancelled.clone(),
                text_out: Some(text_tx),
            };
            let source = run.start(fake).unwrap();
            Started { source, trace, usage, text, cancelled, dir }
        }

        #[test]
        fn end_to_end_a_read_and_a_proposal_through_the_bridge_and_the_loop() {
            let fake = FakeCli::claude(vec![
                Step::Text("Checking."),
                Step::Calls(vec![
                    ("list_calendar_events", json!({ "day": "today" })),
                    ("propose_calendar_event", json!({ "title": "Vendor sync", "body": "Tuesday 3pm with Acme" })),
                ]),
                Step::Text("You have one event; the invite is waiting for your approval."),
                Step::Result("You have one event; the invite is waiting for your approval."),
            ]);
            let (seen, replies) = (fake.seen.clone(), fake.replies.clone());
            let mut s = start(fake, "e2e");
            let socket = s.source.host.as_ref().unwrap().socket_path().to_path_buf();
            let mut runner = EchoRunner::default();
            let mut queue = Queue::default();
            let out = run_read_loop(&mut s.source, &mut runner, &mut queue, &calendar(), &ctx(), LoopLimits::default()).unwrap();

            assert_eq!(out.answer, "You have one event; the invite is waiting for your approval.");
            assert_eq!((out.executed, out.proposed, out.refused), (1, 1, 0));
            // The read ran through the loop's runner; the proposal never did.
            assert_eq!(*runner.reached.lock().unwrap(), vec!["read_sync"]);
            // Invariant 4: the proposal is one L3 send in the queue, executed by nothing here.
            assert_eq!(queue.queued.len(), 1);
            let (action, body) = &queue.queued[0];
            assert!(action.is_external_send());
            assert_eq!(action.required_level(), Level::L3);
            assert_eq!(body, "Tuesday 3pm with Acme");

            // What the CLI saw: the catalog, a fenced read result, and "waiting for approval".
            let replies = replies.lock().unwrap().clone();
            let listed: Vec<&str> =
                replies[0]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
            assert!(listed.contains(&"list_calendar_events") && listed.contains(&"propose_calendar_event"));
            let read = replies[1]["content"][0]["text"].as_str().unwrap();
            assert!(read.contains("read_sync-data"), "{read}");
            assert_ne!(read, "read_sync-data", "service data goes back fenced, as on the API-key lanes");
            assert_eq!(replies[1]["isError"], false);
            assert!(replies[2]["content"][0]["text"].as_str().unwrap().contains("approval"));

            // The invocation was the bridged, contained one.
            let spec = seen.lock().unwrap()[0].clone();
            assert_eq!(spec.program, "claude");
            assert_eq!(spec.stdin.as_deref(), Some("SYSTEM\nUser: what is on today?\nSHOGUN:"));
            let allowed = spec.args.iter().position(|a| a == "--allowedTools").unwrap();
            assert_eq!(spec.args[allowed + 1], "mcp__shogun_chat");

            // Invariant 3: the prompt and each result handed back are traced, on the local-agent
            // route, digest only.
            let rows = s.trace.records();
            assert_eq!(rows.len(), 3, "prompt + two results");
            assert!(rows.iter().all(|r| r.route == Route::LocalAgent && r.purpose == TRACE_PURPOSE));
            assert!(rows.iter().all(|r| r.destination == Delegate::ClaudeCode.destination()));

            drop(s.source);
            assert!(std::fs::symlink_metadata(&socket).is_err(), "the run's socket is gone with it");
            let usage = s.usage.records();
            assert_eq!(usage.len(), 1);
            assert_eq!(usage[0].route, Route::LocalAgent);
            let text: Vec<ToolTextEvent> = s.text.try_iter().collect();
            assert_eq!(text.first(), Some(&ToolTextEvent::Delta { model_turn: 0, text: "Checking.".into() }));
            assert!(text.contains(&ToolTextEvent::Discard { model_turn: 0 }));
            // (Turn numbers past the first depend on whether both calls landed in one gathering
            // window; the shape does not.)
            assert!(matches!(text.last(), Some(ToolTextEvent::Commit { .. })), "{text:?}");
            let _ = std::fs::remove_dir_all(s.dir);
        }

        #[test]
        fn end_to_end_a_failed_cli_keeps_its_classified_error() {
            let fake = FakeCli::claude(vec![Step::Exit(1, "Claude AI usage limit reached")]);
            let mut s = start(fake, "fail");
            let failure = s.source.failure();
            let err = run_read_loop(&mut s.source, &mut EchoRunner::default(), &mut Queue::default(), &calendar(), &ctx(), LoopLimits::default())
                .unwrap_err();
            assert!(!err.is_empty());
            assert!(matches!(failure.take(), Some(LlmError::QuotaExhausted(_))), "{err}");
            let _ = std::fs::remove_dir_all(s.dir);
        }

        #[test]
        fn end_to_end_stop_kills_a_thinking_cli() {
            let fake = FakeCli::claude(vec![Step::Text("Thinking"), Step::Hang]);
            let mut s = start(fake, "stop");
            let cancelled = s.cancelled.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Release);
            });
            let err = run_read_loop(&mut s.source, &mut EchoRunner::default(), &mut Queue::default(), &calendar(), &ctx(), LoopLimits::default())
                .unwrap_err();
            assert_eq!(err, "cancelled");
            let _ = std::fs::remove_dir_all(s.dir);
        }


        // Codex

        #[test]
        fn end_to_end_codex_a_read_and_a_proposal_through_the_bridge_and_the_loop() {
            let fake = FakeCli::codex(vec![
                Step::Text("Checking."),
                Step::Calls(vec![
                    ("list_calendar_events", json!({ "day": "today" })),
                    ("propose_calendar_event", json!({ "title": "Vendor sync", "body": "Tuesday 3pm with Acme" })),
                ]),
                Step::Result("You have one event; the invite is waiting for your approval."),
            ]);
            let (seen, replies) = (fake.seen.clone(), fake.replies.clone());
            let mut s = start(fake, "cx-e2e");
            let socket = s.source.host.as_ref().unwrap().socket_path().to_path_buf();
            let mut runner = EchoRunner::default();
            let mut queue = Queue::default();
            let out = run_read_loop(&mut s.source, &mut runner, &mut queue, &calendar(), &ctx(), LoopLimits::default()).unwrap();

            // The final agent message is the answer (Codex reports no separate result).
            assert_eq!(out.answer, "You have one event; the invite is waiting for your approval.");
            assert_eq!((out.executed, out.proposed, out.refused), (1, 1, 0));
            assert_eq!(*runner.reached.lock().unwrap(), vec!["read_sync"]);
            // Invariant 4: one L3 send in the queue, executed by nothing here.
            assert_eq!(queue.queued.len(), 1);
            let (action, body) = &queue.queued[0];
            assert!(action.is_external_send());
            assert_eq!(action.required_level(), Level::L3);
            assert_eq!(body, "Tuesday 3pm with Acme");
            let replies = replies.lock().unwrap().clone();
            let read = replies[1]["content"][0]["text"].as_str().unwrap();
            assert!(read.contains("read_sync-data") && read != "read_sync-data", "{read}");
            assert!(replies[2]["content"][0]["text"].as_str().unwrap().contains("approval"));

            // The invocation was the bridged, contained Codex one.
            let spec = seen.lock().unwrap()[0].clone();
            assert_eq!(spec.program, "codex");
            assert_eq!(&spec.args[..3], ["exec", "--json", "-"]);
            assert!(spec.args.iter().any(|a| a == "--ignore-user-config"));
            assert!(spec.args.iter().any(|a| a == r#"mcp_servers.shogun_chat.default_tools_approval_mode="approve""#));

            // Invariant 3: prompt + two results, on the local-agent route, to Codex's upstream.
            let rows = s.trace.records();
            assert_eq!(rows.len(), 3, "prompt + two results");
            assert!(rows.iter().all(|r| r.route == Route::LocalAgent && r.purpose == TRACE_PURPOSE));
            assert!(rows.iter().all(|r| r.destination == Delegate::Codex.destination()));

            drop(s.source);
            assert!(std::fs::symlink_metadata(&socket).is_err(), "the run's socket is gone with it");
            let usage = s.usage.records();
            assert_eq!(usage.len(), 1);
            assert_eq!(usage[0].route, Route::LocalAgent);
            let text: Vec<ToolTextEvent> = s.text.try_iter().collect();
            assert_eq!(text.first(), Some(&ToolTextEvent::Delta { model_turn: 0, text: "Checking.".into() }));
            assert!(text.contains(&ToolTextEvent::Discard { model_turn: 0 }));
            assert!(matches!(text.last(), Some(ToolTextEvent::Commit { .. })), "{text:?}");
            let _ = std::fs::remove_dir_all(s.dir);
        }

        #[test]
        fn end_to_end_codex_a_send_by_its_raw_name_is_refused_and_never_runs() {
            // A model told (by text in a mail body) to call the send op directly gets a refusal;
            // nothing reaches a service and nothing is queued.
            let fake = FakeCli::codex(vec![
                Step::Calls(vec![("send_email", json!({ "to": "a@b.com", "body": "wire the money" }))]),
                Step::Result("I can't send that."),
            ]);
            let replies = fake.replies.clone();
            let mut s = start(fake, "cx-send");
            let mut runner = EchoRunner::default();
            let mut queue = Queue::default();
            let out = run_read_loop(&mut s.source, &mut runner, &mut queue, &calendar(), &ctx(), LoopLimits::default()).unwrap();
            assert_eq!(out.refused, 1);
            assert!(queue.queued.is_empty());
            assert!(runner.reached.lock().unwrap().is_empty());
            assert_eq!(replies.lock().unwrap()[1]["isError"], true);
            drop(s.source);
            let _ = std::fs::remove_dir_all(s.dir);
        }

        #[test]
        fn end_to_end_codex_a_failed_turn_keeps_its_classified_error() {
            // `codex exec --json` reports why on stdout; the plan limit is still a plan limit.
            let fake = FakeCli::codex(vec![Step::Exit(1, "You've hit your usage limit.")]);
            let mut s = start(fake, "cx-fail");
            let failure = s.source.failure();
            let err = run_read_loop(&mut s.source, &mut EchoRunner::default(), &mut Queue::default(), &calendar(), &ctx(), LoopLimits::default())
                .unwrap_err();
            assert!(!err.is_empty());
            assert!(matches!(failure.take(), Some(LlmError::QuotaExhausted(_))), "{err}");
            let _ = std::fs::remove_dir_all(s.dir);

            // A zero exit that only reported a failed turn is a failure too, not an empty answer.
            let fake = FakeCli::codex(vec![Step::Exit(0, "Not logged in")]);
            let mut s = start(fake, "cx-fail0");
            let failure = s.source.failure();
            assert!(run_read_loop(&mut s.source, &mut EchoRunner::default(), &mut Queue::default(), &calendar(), &ctx(), LoopLimits::default())
                .is_err());
            assert!(matches!(failure.take(), Some(LlmError::Unauthorized(..))));
            let _ = std::fs::remove_dir_all(s.dir);
        }

        #[test]
        fn end_to_end_codex_stop_kills_a_thinking_cli() {
            let fake = FakeCli::codex(vec![Step::Hang]);
            let mut s = start(fake, "cx-stop");
            let cancelled = s.cancelled.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Release);
            });
            let err = run_read_loop(&mut s.source, &mut EchoRunner::default(), &mut Queue::default(), &calendar(), &ctx(), LoopLimits::default())
                .unwrap_err();
            assert_eq!(err, "cancelled");
            let _ = std::fs::remove_dir_all(s.dir);
        }

    }
}
