//! The tool loop's real read runner (issue #81 step 4).
//!
//! [`shogun_mcp::tool_loop::run_read_loop`] is pure orchestration over seams; this is the seam
//! implementation that reaches actual services. It adapts the shared
//! [`ConnectorRuntime`] — the same gated runtime the sync poller and the confirmed-write path
//! use — into a [`ReadToolRunner`], so a model-driven read passes exactly the gates a poller
//! read passes (wave, connection, plan). The gates are checked before dispatch
//! and before returning data to the model. Already-dispatched blocking HTTP cannot be recalled.
//!
//! **Traceability** (invariant 3 / spec §4-5): every dispatched read is recorded before it is
//! sent as `Route::Mcp`, with an empty chunk: the record proves the model read without
//! retaining what came back. The read-sync poller follows the same rule through the ingest sink's
//! `record_fetch_attempt` (issue #782).
//!
//! **Timeouts**: a bounded receive enforces the loop's deadline for both MCP and Gmail.
//! Already-sent blocking requests finish under their transport's own ceiling; late results are
//! dropped. Approved writes retain their separate transport timeout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use shogun_integrations::transport::WriteExecutor;
use shogun_integrations::ConnectorRuntime;
use shogun_mcp::scope::Service;
use shogun_mcp::sync::IntegrationTransport;
use shogun_mcp::tool_catalog::{comparable_link, user_supplied_link};
use shogun_mcp::tool_loop::{ModelTurn, ModelTurnSource, ToolResult};
use shogun_mcp::tool_loop::{ReadToolRunner, ToolRunError};

use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};

/// Purpose string for a first-layer (direct MCP) read performed for the model.
pub const PURPOSE_MCP_TOOL_READ: &str = "mcp_tool_read";
/// Purpose string for a direct Gmail read performed for the model.
pub const PURPOSE_GMAIL_TOOL_READ: &str = "gmail_tool_read";

/// One conversation's authorization and Stop boundary, shared by model, reads and proposals.
pub struct ToolAccess<T> {
    runtime: Arc<Mutex<ConnectorRuntime<T>>>,
    revision: u64,
    cancelled: Arc<AtomicBool>,
}

impl<T> ToolAccess<T> {
    pub fn new(
        runtime: Arc<Mutex<ConnectorRuntime<T>>>,
        revision: u64,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            runtime,
            revision,
            cancelled,
        }
    }

    /// Stop, without touching the runtime mutex. This is the only check a per-token path may
    /// make: the sync poller holds that mutex across its HTTP round-trips, so locking it per
    /// streamed delta parks the whole answer behind a 15-minute sync tick.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Stop or a changed connection set. Once per model turn, not per token — and bounded like a
    /// read: if the poller keeps the runtime busy past [`CHECK_LOCK_BUDGET`], the turn fails
    /// closed rather than stalling for the length of a sync.
    pub fn check(&self) -> Result<(), String> {
        let runtime = lock_tool_runtime(&self.runtime, &self.cancelled)?;
        if runtime.tool_auth_revision() != self.revision {
            return Err("Connections changed. Ask again to use the current connections.".into());
        }
        Ok(())
    }
}

/// How long a per-turn authorization check may wait on the runtime mutex.
const CHECK_LOCK_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum LockWait {
    Timeout,
    Poisoned,
    Cancelled,
}

/// Bound desktop policy/snapshot lock waits and let Stop interrupt connection sync contention.
pub fn lock_tool_runtime<'a, T>(
    runtime: &'a Mutex<T>,
    cancelled: &AtomicBool,
) -> Result<std::sync::MutexGuard<'a, T>, String> {
    let deadline = Instant::now().checked_add(CHECK_LOCK_BUDGET)
        .ok_or("connections unavailable")?;
    bounded_lock(runtime, deadline, Some(cancelled)).map_err(|e| match e {
        LockWait::Timeout => "Connections are busy syncing. Ask again in a moment.".into(),
        LockWait::Poisoned => "connections unavailable".into(),
        LockWait::Cancelled => "cancelled".into(),
    })
}

// The sync poller can hold this mutex across HTTP; never let its wait escape the caller's budget.
fn bounded_lock<'a, T>(
    runtime: &'a Mutex<T>,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<std::sync::MutexGuard<'a, T>, LockWait> {
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(LockWait::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LockWait::Timeout);
        }
        match runtime.try_lock() {
            Ok(guard) => {
                return if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    Err(LockWait::Cancelled)
                } else if Instant::now() < deadline {
                    Ok(guard)
                } else {
                    Err(LockWait::Timeout)
                }
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(LockWait::Poisoned),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(remaining.min(Duration::from_millis(1)))
            }
        }
    }
}

/// Check both sides of model work: Stop during HTTP cannot dispatch returned tools.
pub struct AuthorizedTurnSource<'a, S, T> {
    pub inner: S,
    pub access: &'a ToolAccess<T>,
}

impl<S: ModelTurnSource, T> ModelTurnSource for AuthorizedTurnSource<'_, S, T> {
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        self.access.check().inspect_err(|_| {
            crate::elog!("[chat_tools] stage=fence_before class=authorization");
        })?;
        let turn = self.inner.next_turn(results)?;
        self.access.check().inspect_err(|_| {
            crate::elog!("[chat_tools] stage=fence_after class=authorization");
        })?;
        Ok(turn)
    }
}

/// A [`ReadToolRunner`] over the shared connector runtime. Holds the same `Arc<Mutex<…>>` the
/// poller and the write path share. The lock is held only for the gate check; the HTTP call
/// uses a cloned transport handle so two reads in one model turn can overlap (FR-HN-07).
pub struct RuntimeReadRunner<'a, T, S: TraceabilitySink + ?Sized> {
    runtime: &'a Arc<Mutex<ConnectorRuntime<T>>>,
    sink: &'a S,
    access: Option<&'a ToolAccess<T>>,
    /// Links a web read may fetch without asking (#775): the ones the user wrote, plus links on
    /// pages already read this turn. Anything else is refused before a request is made, so text
    /// planted in an email or a page cannot send the user's data out inside a URL. Kept in the
    /// [`comparable_link`] form; compared with [`user_supplied_link`], the same rule the loop's
    /// level decision uses (the model may drop a link's query, never add to or change it).
    web_links: Mutex<Vec<String>>,
    /// Fixtures must not read the host Mac's consent policy or attempt real network requests.
    #[cfg(test)]
    web_consent_override: Option<bool>,
}

impl<'a, T, S: TraceabilitySink + ?Sized> RuntimeReadRunner<'a, T, S> {
    pub fn new(runtime: &'a Arc<Mutex<ConnectorRuntime<T>>>, sink: &'a S) -> Self {
        Self {
            runtime,
            sink,
            access: None,
            web_links: Mutex::new(Vec::new()),
            #[cfg(test)]
            web_consent_override: None,
        }
    }

    #[cfg(test)]
    fn with_web_consent(mut self, consented: bool) -> Self {
        self.web_consent_override = Some(consented);
        self
    }

    pub fn with_access(mut self, access: &'a ToolAccess<T>) -> Self {
        self.access = Some(access);
        self
    }

    /// The links the user wrote in this thread ([`crate::llm::conversation::ChatRequest::user_links`]).
    /// Without them, every model-chosen web read is refused.
    pub fn with_user_links(self, links: impl IntoIterator<Item = String>) -> Self {
        self.allow_links(links);
        self
    }

    fn allow_links(&self, links: impl IntoIterator<Item = String>) {
        if let Ok(mut allowed) = self.web_links.lock() {
            for link in links {
                let link = comparable_link(&link);
                if !allowed.contains(&link) {
                    allowed.push(link);
                }
            }
        }
    }

    fn link_allowed(&self, url: &str) -> bool {
        self.web_links
            .lock()
            .map(|allowed| user_supplied_link(url, &allowed))
            .unwrap_or(false)
    }
}

/// Every string inside `value`, one per line, for finding the links a read returned.
fn collect_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(fields) => fields.values().for_each(|field| collect_strings(field, out)),
        _ => {}
    }
}

/// How many links one read page may add to the allow-list.
const MAX_LINKS_FROM_A_PAGE: usize = 32;

/// The links on a page read from `page_url` that a later read may follow: the ones on the same
/// host, and at most [`MAX_LINKS_FROM_A_PAGE`] of them. A page is not the user, so it does not
/// get to open other hosts, and it cannot fill the list.
fn links_a_page_may_add(page_url: &str, page_text: &str) -> Vec<String> {
    let host = crate::web_fetch::host_of(page_url);
    crate::web_fetch::https_links(page_text)
        .into_iter()
        .filter(|link| !host.is_empty() && crate::web_fetch::host_of(link) == host)
        .take(MAX_LINKS_FROM_A_PAGE)
        .collect()
}

/// Direct provider reads never cross the legacy Composio transport.
fn trace_parts(service: Service) -> (Route, &'static str, bool) {
    if service == Service::Gmail {
        (Route::Mcp, PURPOSE_GMAIL_TOOL_READ, false)
    } else {
        (Route::Mcp, PURPOSE_MCP_TOOL_READ, false)
    }
}

/// [`bounded_lock`] inside a read's own deadline.
fn read_lock<T>(
    runtime: &Mutex<T>,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'_, T>, ToolRunError> {
    bounded_lock(runtime, deadline, None).map_err(|e| match e {
        LockWait::Timeout => ToolRunError::Timeout,
        LockWait::Poisoned => ToolRunError::Failed("connections unavailable".into()),
        LockWait::Cancelled => ToolRunError::Failed("cancelled".into()),
    })
}

impl<T, S: TraceabilitySink + ?Sized> RuntimeReadRunner<'_, T, S> {
    /// One public read for the model. The gate already folded the consent into the service's
    /// connection state; it is re-read here as defence in depth, the way the connector runtime
    /// re-checks its own gate — a loop that somehow skipped `classify_call` still fetches nothing.
    fn run_web(&self, scope_op: &str, input: &Value) -> Result<String, ToolRunError> {
        use crate::web_fetch::read::{mine_language_tag, read_blocking, WebReadKind};
        use crate::web_fetch::WebFetchError;
        let Some(kind) = WebReadKind::from_scope_op(scope_op) else {
            return Err(ToolRunError::Failed("not a web read".into()));
        };
        let url = input.get("url").and_then(Value::as_str).unwrap_or_default().trim();
        // Issue #775: checked before consent and before any request, so a refused link never
        // leaves the Mac, not even as a DNS lookup.
        if crate::web_fetch::is_private_host(url) {
            return Err(ToolRunError::Failed(
                WebFetchError::PrivateDestination.message().into(),
            ));
        }
        if !self.link_allowed(url) {
            return Err(ToolRunError::Failed(
                "the link is not one the user gave; ask the user to paste it if they want it read"
                    .into(),
            ));
        }
        #[cfg(test)]
        let consented = self.web_consent_override.unwrap_or_else(shogun_mcp::web_consent::consented);
        #[cfg(not(test))]
        let consented = shogun_mcp::web_consent::consented();
        if !consented {
            return Err(ToolRunError::Failed("web fetch is not switched on".into()));
        }
        match read_blocking(kind, url, &mine_language_tag(), self.sink) {
            Ok(read) => {
                let json = read.to_json();
                // A page the user asked for may link to the next one they want read, on the
                // same site. The links are taken from the decoded text, not the JSON, so
                // escapes cannot glue onto them.
                let mut text = String::new();
                collect_strings(&json, &mut text);
                self.allow_links(links_a_page_may_add(url, &text));
                Ok(json.to_string())
            }
            Err(WebFetchError::Network(reason)) if reason.contains("timed out") => {
                Err(ToolRunError::Timeout)
            }
            Err(e) => Err(ToolRunError::Failed(e.message().to_string())),
        }
    }
}

impl<T, S> ReadToolRunner for RuntimeReadRunner<'_, T, S>
where
    T: IntegrationTransport + WriteExecutor + Send + Sync + 'static,
    S: TraceabilitySink + Sync + ?Sized,
{
    fn run(
        &self,
        service: Service,
        scope_op: &'static str,
        input: &Value,
        timeout_ms: u64,
    ) -> Result<String, ToolRunError> {
        // Web (issue #555) is not a connector: SHOGUN fetches the link itself, and the fetcher
        // writes its own ledger rows (one per request, before it is sent) to this runner's sink.
        if service == Service::Web {
            return self.run_web(scope_op, input);
        }
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .ok_or(ToolRunError::Timeout)?;
        let (handle, revision, connection_revision) = {
            let rt = read_lock(self.runtime, deadline)?;
            if self.access.is_some_and(|access| {
                access.revision != rt.tool_auth_revision()
                    || access.cancelled.load(Ordering::Acquire)
            }) {
                return Err(ToolRunError::Failed(
                    "This tool turn is no longer authorized.".into(),
                ));
            }
            rt.precheck_read(service, scope_op)
                .map_err(ToolRunError::Failed)?;
            (rt.transport_handle(), rt.tool_auth_revision(), rt.connection_revision(service))
        };
        // Trace dispatch, including requests that time out or lose authorization later.
        let (route, purpose, third_party) = trace_parts(service);
        self.sink.record(TraceRecord::for_chunk(
            route,
            purpose,
            service.source_str(),
            "",
            third_party,
        ));
        let input = input.clone();
        let runtime = Arc::clone(self.runtime);
        let cancelled = self.access.map(|access| Arc::clone(&access.cancelled));
        let stop = cancelled.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let authorized = read_lock(&runtime, deadline).is_ok_and(|runtime| {
                runtime.tool_auth_revision() == revision
                    && runtime.precheck_read(service, scope_op).is_ok()
            }) && !cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
            if !authorized || Instant::now() >= deadline {
                let _ = tx.send(Err("Read cancelled before dispatch.".into()));
                return;
            }
            let outcome = handle.execute(service, scope_op, input);
            // Observe even a late failure after Stop/timeout, but never age a newer grant.
            let outcome = match runtime.lock() {
                Ok(mut runtime) if runtime.connection_revision(service) == connection_revision => {
                    runtime.observe_provider_result(service, outcome)
                }
                _ => outcome,
            };
            let _ = tx.send(outcome);
        });
        // ponytail: at most eight bounded transport workers per question; propagate per-request
        // deadlines through transports if their existing HTTP ceilings cease to bound lifetime.
        // Wait in short slices so Stop returns promptly; the worker is detached and its late
        // result is dropped with `rx`.
        let outcome = loop {
            if stop.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err(ToolRunError::Failed("cancelled".into()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(25))) {
                Ok(outcome) => break outcome,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {}
                Err(_) => return Err(ToolRunError::Timeout),
            }
        };
        let runtime = read_lock(self.runtime, deadline)?;
        if self
            .access
            .is_some_and(|access| access.cancelled.load(Ordering::Acquire))
        {
            return Err(ToolRunError::Failed("cancelled".into()));
        }
        if runtime.tool_auth_revision() != revision {
            return Err(ToolRunError::Failed(
                "Connections changed during the read.".into(),
            ));
        }
        match outcome {
            Ok(value) => {
                // A disconnect while HTTP was running must not release its result to the model.
                runtime
                    .precheck_read(service, scope_op)
                    .map_err(ToolRunError::Failed)?;
                let serialized = serde_json::to_string(&value)
                    .map_err(|_| ToolRunError::Failed("result was not serializable".into()))?;
                if Instant::now() >= deadline {
                    return Err(ToolRunError::Timeout);
                }
                Ok(serialized)
            }
            // The HTTP client's ceiling (`mcp_http::HTTP_TIMEOUT_MS`) produced this; count it as
            // the loop's per-tool timeout. Error strings are content-free by transport contract.
            Err(e) if e.contains("timed out") => Err(ToolRunError::Timeout),
            Err(_) => Err(ToolRunError::Failed(
                "The connected service could not complete this read.".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;
    use shogun_mcp::scope::Wave;
    use shogun_mcp::sync::FetchedItem;

    #[test]
    fn stop_interrupts_a_busy_tool_runtime_without_unlocking_sync() {
        let runtime = Mutex::new(());
        let cancelled = AtomicBool::new(false);
        let _sync = runtime.lock().unwrap();
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let runtime = &runtime;
            let cancelled = &cancelled;
            scope.spawn(move || {
                let result = lock_tool_runtime(runtime, cancelled).map(|_| ());
                tx.send(result).unwrap();
            });
            assert!(matches!(rx.recv_timeout(Duration::from_millis(10)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)));
            cancelled.store(true, Ordering::Release);
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Err("cancelled".into()));
        });
        cancelled.store(false, Ordering::Release);
        assert!(matches!(bounded_lock(&runtime, Instant::now() + Duration::from_millis(10), Some(&cancelled)), Err(LockWait::Timeout)));
    }

    #[test]
    fn stop_during_model_work_cannot_dispatch_tools_or_proposals() {
        use shogun_mcp::tool_loop::{run_read_loop, LoopLimits, ProposalSink, ToolUse};
        struct LateModel(Arc<AtomicBool>);
        impl ModelTurnSource for LateModel {
            fn next_turn(&mut self, _: &[ToolResult]) -> Result<ModelTurn, String> {
                self.0.store(true, Ordering::Release);
                Ok(ModelTurn::ToolUses(vec![
                    ToolUse {
                        id: "read".into(),
                        name: "list_calendar_events".into(),
                        input: serde_json::json!({}),
                    },
                    ToolUse {
                        id: "send".into(),
                        name: "propose_send_email".into(),
                        input: serde_json::json!({"to":"a@example.com","body":"hello"}),
                    },
                ]))
            }
        }
        struct NoDispatch;
        impl ReadToolRunner for NoDispatch {
            fn run(
                &self,
                _: Service,
                _: &'static str,
                _: &Value,
                _: u64,
            ) -> Result<String, ToolRunError> {
                panic!("read after Stop")
            }
        }
        impl ProposalSink for NoDispatch {
            fn propose(
                &mut self,
                _: shogun_agents::permission::Action,
                _: &str,
            ) -> Result<(), String> {
                panic!("proposal after Stop")
            }
        }
        let runtime = runtime_with(Ok(serde_json::json!({})), true);
        let cancelled = Arc::new(AtomicBool::new(false));
        let rt = runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (services, ctx) = rt.tool_loop_view();
        let revision = rt.tool_auth_revision();
        drop(rt);
        let access = ToolAccess::new(runtime, revision, cancelled.clone());
        let mut source = AuthorizedTurnSource {
            inner: LateModel(cancelled),
            access: &access,
        };
        assert!(
            matches!(run_read_loop(&mut source, &mut NoDispatch, &mut NoDispatch, &services, &ctx, LoopLimits::default()), Err(reason) if reason == "cancelled")
        );
    }

    #[test]
    fn completed_fast_read_is_not_forwarded_after_sibling_wait_and_reconnect() -> Result<(), String>
    {
        struct TwoReads(Arc<std::sync::Barrier>);
        impl IntegrationTransport for TwoReads {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(vec![])
            }
        }
        impl WriteExecutor for TwoReads {
            fn execute(&self, service: Service, _: &str, _: Value) -> Result<Value, String> {
                if service == Service::Gmail {
                    self.0.wait();
                    self.0.wait();
                }
                Ok(serde_json::json!({"private":"old account"}))
            }
        }
        struct NeverPost;
        impl ModelTurnSource for NeverPost {
            fn next_turn(&mut self, _: &[ToolResult]) -> Result<ModelTurn, String> {
                panic!("revoked result reached next model request")
            }
        }
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut rt = ConnectorRuntime::new(TwoReads(barrier.clone()), Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        rt.use_direct_gmail();
        rt.mark_connected(Service::Gmail, 1);
        rt.mark_connected(Service::GoogleCalendar, 1);
        let revision = rt.tool_auth_revision();
        let runtime = Arc::new(Mutex::new(rt));
        let access = ToolAccess::new(runtime.clone(), revision, Arc::new(AtomicBool::new(false)));
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&runtime, &sink).with_access(&access);
        std::thread::scope(|scope| -> Result<(), String> {
            let slow = scope
                .spawn(|| runner.run(Service::Gmail, "read_sync", &serde_json::json!({}), 10_000));
            barrier.wait();
            let fast = runner
                .run(
                    Service::GoogleCalendar,
                    "read_sync",
                    &serde_json::json!({}),
                    10_000,
                )
                .map_err(|_| "fast read failed")?;
            let mut rt = runtime.lock().map_err(|_| "lock")?;
            rt.disconnect(Service::GoogleCalendar, false);
            rt.mark_connected(Service::GoogleCalendar, 2);
            drop(rt);
            barrier.wait();
            assert!(matches!(slow.join(), Ok(Err(_))));
            let mut source = AuthorizedTurnSource {
                inner: NeverPost,
                access: &access,
            };
            assert!(source
                .next_turn(&[ToolResult {
                    id: "fast".into(),
                    content: fast,
                    is_error: false
                }])
                .is_err());
            Ok(())
        })?;
        assert_eq!(sink.records().len(), 2);
        Ok(())
    }

    #[test]
    fn stop_returns_promptly_while_a_tool_is_blocked() -> Result<(), String> {
        struct Slow;
        impl IntegrationTransport for Slow {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(vec![])
            }
        }
        impl WriteExecutor for Slow {
            fn execute(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                Ok(serde_json::json!({}))
            }
        }
        let mut rt = ConnectorRuntime::new(Slow, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        rt.use_direct_gmail();
        rt.mark_connected(Service::Gmail, 1);
        let runtime = Arc::new(Mutex::new(rt));
        let revision = runtime.lock().map_err(|_| "lock")?.tool_auth_revision();
        let cancelled = Arc::new(AtomicBool::new(false));
        let access = ToolAccess::new(runtime.clone(), revision, cancelled.clone());
        let sink = RecordingSink::default();
        let flag = cancelled.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            flag.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let result = RuntimeReadRunner::new(&runtime, &sink)
            .with_access(&access)
            .run(Service::Gmail, "read_sync", &serde_json::json!({}), 10_000);
        let _ = stopper.join();
        assert_eq!(result, Err(ToolRunError::Failed("cancelled".into())));
        assert!(started.elapsed() < std::time::Duration::from_millis(700));
        Ok(())
    }

    #[test]
    fn read_deadline_does_not_wait_for_transport_or_return_its_late_result() {
        struct Slow;
        impl IntegrationTransport for Slow {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(vec![])
            }
        }
        impl WriteExecutor for Slow {
            fn execute(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(serde_json::json!({"private":"late"}))
            }
        }
        let mut rt = ConnectorRuntime::new(Slow, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        rt.use_direct_gmail();
        rt.mark_connected(Service::Gmail, 1);
        let runtime = Arc::new(Mutex::new(rt));
        let sink = RecordingSink::default();
        let started = std::time::Instant::now();
        assert_eq!(
            RuntimeReadRunner::new(&runtime, &sink).run(
                Service::Gmail,
                "read_sync",
                &serde_json::json!({}),
                10
            ),
            Err(ToolRunError::Timeout)
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(150));
        assert_eq!(sink.records().len(), 1);
    }

    #[test]
    fn read_deadline_covers_lock_contention_before_dispatch_and_after_completion(
    ) -> Result<(), Box<dyn std::error::Error>> {
        struct Transport {
            calls: Arc<std::sync::atomic::AtomicUsize>,
            pause: Option<Arc<std::sync::Barrier>>,
        }
        impl IntegrationTransport for Transport {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(vec![])
            }
        }
        impl WriteExecutor for Transport {
            fn execute(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(pause) = &self.pause {
                    pause.wait();
                    pause.wait();
                }
                Ok(serde_json::json!({ "private": "read result" }))
            }
        }
        struct Sink(Option<Arc<std::sync::Barrier>>);
        impl TraceabilitySink for Sink {
            fn record(&self, _: TraceRecord) {
                if let Some(pause) = &self.0 {
                    pause.wait();
                    pause.wait();
                }
            }
        }
        for stage in ["initial lock", "dispatch lock", "completion lock"] {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let pause = Arc::new(std::sync::Barrier::new(2));
            let mut rt = ConnectorRuntime::new(
                Transport {
                    calls: calls.clone(),
                    pause: (stage == "completion lock").then(|| pause.clone()),
                },
                Wave::One,
                true,
            );
            rt.set_plan(shogun_agents::entitlement::entitlements(
                shogun_agents::entitlement::Plan::Pro,
                0,
            ));
            rt.mark_connected(Service::GoogleCalendar, 1);
            let revision = rt.tool_auth_revision();
            let runtime = Arc::new(Mutex::new(rt));
            let access =
                ToolAccess::new(runtime.clone(), revision, Arc::new(AtomicBool::new(false)));
            let sink = Sink((stage == "dispatch lock").then(|| pause.clone()));
            let initial_guard = (stage == "initial lock")
                .then(|| runtime.lock())
                .transpose()
                .map_err(|_| "lock")?;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| -> Result<(), String> {
                let reader = scope.spawn(|| {
                    let result = RuntimeReadRunner::new(&runtime, &sink)
                        .with_access(&access)
                        .run(
                            Service::GoogleCalendar,
                            "read_sync",
                            &serde_json::json!({}),
                            30,
                        );
                    let _ = tx.send(result);
                });
                let later_guard = if stage != "initial lock" {
                    pause.wait();
                    let guard = runtime.lock().map_err(|_| "lock")?;
                    pause.wait();
                    Some(guard)
                } else {
                    None
                };
                // Keep the lock until the caller has returned; release it even on a failed assertion.
                let result = rx.recv_timeout(Duration::from_millis(500));
                drop(initial_guard);
                drop(later_guard);
                reader.join().map_err(|_| "reader panicked")?;
                assert_eq!(result, Ok(Err(ToolRunError::Timeout)), "{stage}");
                // A worker that was waiting to dispatch must not start HTTP after lock release.
                std::thread::sleep(Duration::from_millis(20));
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    usize::from(stage == "completion lock"),
                    "{stage}"
                );
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn authorization_change_after_one_model_step_blocks_the_next() -> Result<(), String> {
        struct Model(usize);
        impl ModelTurnSource for Model {
            fn next_turn(&mut self, _: &[ToolResult]) -> Result<ModelTurn, String> {
                self.0 += 1;
                Ok(ModelTurn::Final("answer".into()))
            }
        }
        let runtime = runtime_with(Ok(serde_json::json!({})), true);
        let revision = runtime.lock().map_err(|_| "lock")?.tool_auth_revision();
        let cancelled = Arc::new(AtomicBool::new(false));
        let access = ToolAccess::new(runtime.clone(), revision, cancelled.clone());
        let mut model = AuthorizedTurnSource {
            inner: Model(0),
            access: &access,
        };
        model.next_turn(&[ToolResult {
            id: "prior".into(),
            content: "old account data".into(),
            is_error: false,
        }])?;
        runtime
            .lock()
            .map_err(|_| "lock")?
            .disconnect(Service::Gmail, false);
        assert!(model.next_turn(&[]).is_err());
        assert_eq!(model.inner.0, 1);
        // Stop before a later parallel runner begins is checked at that dispatch boundary.
        cancelled.store(true, Ordering::Release);
        let sink = RecordingSink::default();
        assert!(RuntimeReadRunner::new(&runtime, &sink)
            .with_access(&access)
            .run(
                Service::GoogleCalendar,
                "read_sync",
                &serde_json::json!({}),
                10_000
            )
            .is_err());
        assert!(sink.records().is_empty());
        Ok(())
    }

    #[test]
    fn disconnect_reconnect_or_runtime_replacement_discards_inflight_result(
    ) -> Result<(), Box<dyn std::error::Error>> {
        struct BlockingTransport(Arc<std::sync::Barrier>);
        impl IntegrationTransport for BlockingTransport {
            fn read_sync(&self, _: Service) -> Result<Vec<FetchedItem>, String> {
                Ok(vec![])
            }
        }
        impl WriteExecutor for BlockingTransport {
            fn execute(&self, _: Service, _: &str, _: Value) -> Result<Value, String> {
                self.0.wait();
                self.0.wait();
                Ok(serde_json::json!({"private": "must not reach model"}))
            }
        }
        for change in ["disconnect", "reconnect", "runtime replacement"] {
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut rt = ConnectorRuntime::new(BlockingTransport(barrier.clone()), Wave::One, true);
            rt.set_plan(shogun_agents::entitlement::entitlements(
                shogun_agents::entitlement::Plan::Pro,
                0,
            ));
            rt.mark_connected(Service::GoogleCalendar, 1000);
            let runtime = Arc::new(Mutex::new(rt));
            let sink = RecordingSink::default();
            std::thread::scope(|scope| {
                let reader = scope.spawn(|| {
                    RuntimeReadRunner::new(&runtime, &sink).run(
                        Service::GoogleCalendar,
                        "read_sync",
                        &serde_json::json!({}),
                        10_000,
                    )
                });
                barrier.wait();
                let mut current = runtime
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if change == "runtime replacement" {
                    let mut replacement =
                        ConnectorRuntime::new(BlockingTransport(barrier.clone()), Wave::One, true);
                    replacement.set_plan(shogun_agents::entitlement::entitlements(
                        shogun_agents::entitlement::Plan::Pro,
                        0,
                    ));
                    replacement.mark_connected(Service::GoogleCalendar, 1000);
                    *current = replacement;
                } else {
                    current.disconnect(Service::GoogleCalendar, false);
                    if change == "reconnect" {
                        current.mark_connected(Service::GoogleCalendar, 1000);
                    }
                }
                drop(current);
                barrier.wait();
                assert!(matches!(reader.join(), Ok(Err(ToolRunError::Failed(_)))));
            });
            assert_eq!(
                sink.records().len(),
                1,
                "completed egress remains traceable"
            );
        }
        Ok(())
    }

    struct FakeTransport {
        result: Result<Value, String>,
    }
    impl IntegrationTransport for FakeTransport {
        fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
            Ok(vec![])
        }
    }
    impl WriteExecutor for FakeTransport {
        fn execute(&self, _service: Service, _op: &str, _args: Value) -> Result<Value, String> {
            self.result.clone()
        }
    }

    fn runtime_with(
        result: Result<Value, String>,
        _legacy_consent: bool,
    ) -> Arc<Mutex<ConnectorRuntime<FakeTransport>>> {
        let mut rt = ConnectorRuntime::new(FakeTransport { result }, Wave::One, true);
        // A new runtime starts on the not-started trial, which refuses reads and writes. These
        // tests are about the loop seam, not plan gating, so give them a plan that may read.
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        rt.use_direct_gmail();
        rt.set_plan(shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Pro,
            0,
        ));
        rt.mark_connected(Service::Gmail, 1_000);
        rt.mark_connected(Service::GoogleCalendar, 1_000);
        Arc::new(Mutex::new(rt))
    }

    #[test]
    fn a_read_returns_json_and_records_the_route() {
        let rt = runtime_with(Ok(serde_json::json!({ "items": [1, 2] })), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink);

        let out = runner
            .run(
                Service::GoogleCalendar,
                "read_sync",
                &serde_json::json!({}),
                10_000,
            )
            .unwrap();
        assert!(out.contains("items"));
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].route, Route::Mcp);
        assert_eq!(recs[0].purpose, PURPOSE_MCP_TOOL_READ);
        assert!(!recs[0].third_party);
        // Empty chunk: the record proves the read, never carries its content.
        assert_eq!(recs[0].chunk_bytes, 0);
    }

    #[test]
    fn a_gmail_read_is_recorded_as_direct_provider() {
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink);
        runner
            .run(Service::Gmail, "read_sync", &serde_json::json!({}), 10_000)
            .unwrap();
        let recs = sink.records();
        assert_eq!(recs[0].route, Route::Mcp);
        assert_eq!(recs[0].purpose, PURPOSE_GMAIL_TOOL_READ);
        assert!(
            !recs[0].third_party,
            "a direct Gmail read must not carry the third-party badge"
        );
    }

    #[test]
    fn a_refused_read_records_nothing() {
        // A disconnected account refuses the Gmail read before egress, so no trace row.
        let rt = runtime_with(Ok(serde_json::json!({})), false);
        rt.lock().unwrap().disconnect(Service::Gmail, false);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink);
        let out = runner.run(Service::Gmail, "read_sync", &serde_json::json!({}), 10_000);
        assert!(matches!(out, Err(ToolRunError::Failed(_))));
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_send_op_cannot_pass_through_the_runner() {
        // Invariant 4 at this layer too: even called directly with a send op, the runtime's
        // read path refuses it before the transport, and nothing is traced as a read.
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink);
        let out = runner.run(Service::Gmail, "send", &serde_json::json!({}), 10_000);
        assert!(matches!(out, Err(ToolRunError::Failed(_))));
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_web_read_without_consent_fetches_nothing() {
        // Issue #555: even an allowed user link is refused when consent is explicitly off,
        // before any request or trace. The host machine's saved/default policy is irrelevant.
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink)
            .with_web_consent(false)
            .with_user_links(["https://example.com/".to_string()]);
        let out = runner.run(
            Service::Web,
            "page",
            &serde_json::json!({ "url": "https://example.com/" }),
            10_000,
        );
        assert!(matches!(out, Err(ToolRunError::Failed(why)) if why == "web fetch is not switched on"));
        assert!(sink.records().is_empty());
        let out = runner.run(Service::Web, "open_url", &serde_json::json!({}), 10_000);
        assert!(matches!(out, Err(ToolRunError::Failed(_))), "open_url is not a read");
    }

    fn web_read(runner: &RuntimeReadRunner<'_, FakeTransport, RecordingSink>, url: &str) -> String {
        match runner.run(Service::Web, "page", &serde_json::json!({ "url": url }), 10_000) {
            Err(ToolRunError::Failed(why)) => why,
            other => panic!("expected a refusal for {url}, got {other:?}"),
        }
    }

    #[test]
    fn the_allow_list_ignores_the_case_of_the_host_but_not_of_the_path() {
        // The normaliser is shogun-mcp's (`tool_catalog::comparable_link`), shared with the
        // loop's level decision; these pin the behaviour the runner relies on.
        assert_eq!(comparable_link("HTTPS://Example.COM/Docs/#top"), "https://example.com/Docs");
        assert_eq!(comparable_link("https://example.com/"), comparable_link("https://EXAMPLE.com"));
        assert_ne!(comparable_link("https://example.com/Docs"), comparable_link("https://example.com/docs"));
        assert_eq!(comparable_link("https://Example.com?Q=A"), "https://example.com?Q=A");
    }

    #[test]
    fn the_runner_and_the_loop_agree_a_user_link_may_lose_its_query_but_not_gain_one() {
        // The loop's `web_page_read_level` lets the model drop a user link's query, and the
        // runner accepts the same, so a read the loop ran untapped is not refused here.
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink)
            .with_web_consent(false)
            .with_user_links(["https://a.example/p?x=1".to_string()]);
        // The link as pasted, and without its query: past the link check (consent is the next gate).
        for url in ["https://a.example/p?x=1", "https://a.example/p"] {
            let why = web_read(&runner, url);
            assert!(why.contains("not switched on"), "{url}: {why}");
            assert_eq!(
                shogun_mcp::tool_catalog::web_page_read_level(url, &["https://a.example/p?x=1".to_string()]),
                None
            );
        }
        // A changed or added query is the model's, not the user's: refused, nothing traced.
        for url in ["https://a.example/p?x=2", "https://a.example/p?x=1&d=secret"] {
            let why = web_read(&runner, url);
            assert!(why.contains("not one the user gave"), "{url}: {why}");
            assert!(shogun_mcp::tool_catalog::web_page_read_level(url, &["https://a.example/p?x=1".to_string()]).is_some());
        }
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_read_page_only_adds_links_on_its_own_host_and_not_many() {
        let text = "see https://docs.example.com/next and https://evil.example/x?d=secret \
                    and https://DOCS.example.com/other";
        assert_eq!(
            links_a_page_may_add("https://docs.example.com/start", text),
            ["https://docs.example.com/next", "https://DOCS.example.com/other"]
        );
        let many: String = (0..100).map(|i| format!("https://docs.example.com/p{i} ")).collect();
        assert_eq!(
            links_a_page_may_add("https://docs.example.com/start", &many).len(),
            MAX_LINKS_FROM_A_PAGE
        );
    }

    #[test]
    fn a_link_the_user_never_gave_is_not_fetched() {
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink)
            .with_user_links(["https://docs.example/guide".to_string()]);
        let why = web_read(&runner, "https://attacker.example/c?d=my+commitments");
        assert!(why.contains("not one the user gave"), "{why}");
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_link_the_user_gave_passes_the_link_check() {
        // Past the link check the next gate is consent (explicitly off): proof the link
        // itself was accepted, in any of the spellings the model might use.
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink)
            .with_web_consent(false)
            .with_user_links(["https://docs.example/guide/".to_string()]);
        for url in ["https://docs.example/guide", "https://docs.example/guide/", " https://docs.example/guide#setup "] {
            let why = web_read(&runner, url);
            assert!(why.contains("not switched on"), "{url}: {why}");
        }
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_private_address_is_never_read_even_when_the_user_gave_it() {
        let rt = runtime_with(Ok(serde_json::json!({})), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink)
            .with_user_links(["https://192.168.1.1/admin".to_string(), "https://127.1/".to_string()]);
        for url in ["https://192.168.1.1/admin", "https://127.1/"] {
            let why = web_read(&runner, url);
            assert!(why.contains("private network"), "{url}: {why}");
        }
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_timed_out_transport_maps_to_the_loop_timeout() {
        let rt = runtime_with(Err("mcp request failed: operation timed out".into()), true);
        let sink = RecordingSink::default();
        let runner = RuntimeReadRunner::new(&rt, &sink);
        let out = runner.run(
            Service::GoogleCalendar,
            "read_sync",
            &serde_json::json!({}),
            10_000,
        );
        assert_eq!(out, Err(ToolRunError::Timeout));
        assert_eq!(sink.records().len(), 1);
    }
}
