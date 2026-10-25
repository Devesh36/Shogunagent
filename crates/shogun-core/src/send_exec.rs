//! Post-approval L3 send execution with mandatory traceability (§6.14 / invariant 3).
//!
//! This is the write-path counterpart to [`crate::daemon::Db::ingest_integration`] (the read path):
//! once the [`ApprovalQueue`](shogun_agents::approval::ApprovalQueue) hands back a
//! [`ConfirmedSend`], the send actually leaves the device here — and **before it does, a
//! traceability record is written**. The recording is structural, not a matter of discipline:
//!
//! - The record is built from the [`ConfirmedSend`]'s own action and preview, so the traced
//!   route, service and digested body can never disagree with what was sent.
//! - The record names the **service** (`gmail`, `gcal`, …), never the preview's destination.
//!   The approval card shows the full recipient or title so the user knows what they confirm. The
//!   ledger lives for a year and only needs the service, action, time and route (issue #924).
//! - The record is written **for every attempt that reaches the wire**, before the transport is
//!   called (issue #782). A send that times out or gets a 5xx may still have been delivered —
//!   the body left the device either way — so the ledger cannot wait for success. Only a refusal
//!   that happens before anything is sent ([`SendTransport::precheck`]: a local-setting approval,
//!   an email offered to the first layer, a service the gate refuses) leaves no row.
//! - The converse holds too: **no row, no send.** When the ledger cannot take the row (disk full,
//!   a poisoned lock) the transport is never called and the outcome says so in a fixed,
//!   content-free sentence ([`TRACE_UNAVAILABLE_REFUSAL`]). Writing the row best-effort and
//!   sending anyway would be fail-open.
//! - A Composio failure that falls back to saving a draft ([`RoutedSendTransport`]) sends the body
//!   a second time; the draft writes its own row ([`save_gmail_draft`]), so two egresses are two
//!   rows.
//! - [`TraceRecord`] has no text field, so the sent body is digested and dropped; the body never
//!   reaches storage (G8).
//!
//! The transport is a seam ([`SendTransport`]): the real remote-MCP / Composio client needs OAuth
//! tokens (Category C) and lands later, but the whole record-on-send guarantee is exercised here on
//! Linux with a fake transport.

use shogun_agents::approval::{ConfirmedSend, Route as ApprovalRoute};
use shogun_agents::permission::SendAction;

use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};

/// Read only the reply thread marker through the existing connector authorization and trace.
pub fn latest_reply_message<T, S>(
    runtime: &mut shogun_integrations::ConnectorRuntime<T>,
    sink: &S,
    action: &SendAction,
    thread: &shogun_agents::approval::ReplyThread,
) -> Result<String, String>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
    S: TraceabilitySink + ?Sized,
{
    use serde_json::json;
    use shogun_mcp::scope::Service;
    let (service, arguments) = match action {
        SendAction::SendEmail { .. } => (Service::Gmail,
            json!({"thread_id": thread.thread_id, "marker_only": true})),
        SendAction::PostMessage { channel } => (Service::Slack,
            json!({"channel_id": channel, "message_ts": thread.thread_id})),
        _ => return Err("not_reply".into()),
    };
    runtime.precheck_read(service, "read_on_demand")?;
    sink.try_record(TraceRecord::for_chunk(Route::Mcp, "reply_thread_check",
        service.source_str(), "", false)).map_err(|_| "trace_unavailable")?;
    let result = runtime.execute_read_owned(service, "read_on_demand", arguments)?;
    let payload = shogun_integrations::result::payload(&result).unwrap_or(result);
    latest_reply_marker(&payload, service == Service::Slack)
}

fn latest_reply_marker(payload: &serde_json::Value, slack: bool) -> Result<String, String> {
    use serde_json::Value;
    if !slack {
        return payload.get("latest_message_id").and_then(Value::as_str)
            .filter(|s| !s.is_empty()).map(str::to_string).ok_or("thread_marker_unavailable".into());
    }
    let messages = payload.get("messages").and_then(Value::as_array)
        .ok_or("thread_marker_unavailable")?;
    // Slack's parent latest_reply covers replies beyond a paginated first page.
    if let Some(latest) = messages.iter().filter_map(|m| m.get("latest_reply").and_then(Value::as_str)).filter(|s| !s.is_empty()).max() {
        return Ok(latest.to_string());
    }
    if payload.get("has_more").and_then(Value::as_bool) == Some(true)
        || payload.pointer("/response_metadata/next_cursor").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        return Err("thread_marker_unavailable".into());
    }
    messages.iter().filter_map(|m| m.get("ts").and_then(Value::as_str)).max()
        .filter(|s| !s.is_empty()).map(str::to_string).ok_or("thread_marker_unavailable".into())
}

/// The seam that actually performs an external send. The real implementation is a remote-MCP /
/// Composio client (Category C — needs connected OAuth tokens); tests inject a fake. It is handed
/// the send action and the full body; it must return a non-sensitive error string on failure (no
/// body text in the error).
pub trait SendTransport {
    fn send(&self, action: &SendAction, body: &str) -> Result<(), String>;

    /// Reject a preview prepared for a different provider route before recording or sending.
    fn accepts_route(&self, _route: ApprovalRoute) -> bool { true }

    /// Refuse an action this transport would never put on the wire, without sending anything.
    /// [`execute_send`] calls this first and records traceability only when it passes, so a
    /// refusal leaves no row while every attempt that can reach the wire does (issue #782).
    /// [`Self::send`] must still re-check: this is the ledger's boundary, not the only gate.
    /// The default admits everything.
    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        let _ = action;
        Ok(())
    }
}

/// Why a transport refuses an approval that is a local setting (issue #654). The desktop applies
/// such an approval itself on confirm; reaching a transport means that step was skipped, and the
/// honest result is a failure, not a send to somewhere.
const LOCAL_SETTING_REFUSAL: &str = "a local setting approval has no transport";

/// The outcome of executing a confirmed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendExecOutcome {
    /// The send left the device; its traceability record was written before it did.
    Sent,
    /// The send did not complete. Carries the non-sensitive error string. If the transport was
    /// reached, the attempt is already on the ledger (the body may have left before the failure);
    /// a precheck refusal sent nothing and recorded nothing.
    Failed(String),
}

/// Map the approval-layer route onto the traceability route. A direct send goes over the service's
/// official MCP; a Composio-relayed send is the third-party route (surfaced as such in the viewer).
fn trace_route(route: ApprovalRoute) -> Route {
    match route {
        ApprovalRoute::DirectMcp => Route::Mcp,
        ApprovalRoute::ViaComposio => Route::Composio,
    }
}

/// A stable machine purpose string for a send, stored in the traceability log (FR-TR-01).
fn purpose_for(action: &SendAction) -> &'static str {
    match action {
        SendAction::SendEmail { .. } => "integration.send_email",
        SendAction::PostMessage { .. } => "integration.post_message",
        SendAction::AddReaction { .. } => "integration.add_reaction",
        SendAction::CreateCalendarEvent { .. } => "integration.create_calendar_event",
        SendAction::UpdateCalendarEvent { .. } => "integration.update_calendar_event",
        SendAction::PostComment { .. } => "integration.post_comment",
        SendAction::CreateDocument { .. } => "integration.create_document",
        SendAction::UpdateDocument { .. } => "integration.update_document",
        SendAction::ChangeIssueStatus { .. } => "integration.change_issue_status",
        // Never traced as a send: no transport accepts it (see `SendRoute::LocalSetting`).
        SendAction::ShareWritingStyle => "settings.writing_voice_enable",
        SendAction::EnableDictionaryAutoAdd => "settings.dictionary_autoadd_enable",
        SendAction::ResumeCapture { .. } => "settings.capture_exclusions_resume",
        SendAction::UseSubscriptionAgent { .. } => "settings.agent_source_subscription",
        SendAction::WidenVisualRecall { .. } => "settings.visual_recall_widen",
        SendAction::ArchiveMailThread { .. } => "mail.archive_request",
        SendAction::UnpauseCapture { .. } => "settings.capture_unpause",
    }
}

/// The traceability destination of a send: the service it acts on, the same id the read path
/// records (`Service::source_str`). Never the recipient, channel or title from the preview: those
/// are the user's contacts and work, and the ledger keeps rows for a year (issue #924).
fn trace_destination(action: &SendAction) -> &'static str {
    shogun_integrations::send_bridge::service_for(action)
        // A local setting has no service; no transport accepts one, so it is never traced.
        .map_or("local", shogun_mcp::scope::Service::source_str)
}

/// Build the traceability record for a confirmed send — pure, digests the body and drops it. The
/// route and third-party flag come from the confirmed preview, the service and purpose from its
/// action, so the trace describes exactly what will be sent without copying who receives it.
pub fn trace_for_send(confirmed: &ConfirmedSend) -> TraceRecord {
    let preview = &confirmed.preview;
    let route = trace_route(preview.route);
    TraceRecord::for_chunk(
        route,
        purpose_for(&confirmed.action),
        trace_destination(&confirmed.action),
        &preview.full_body,
        preview.route == ApprovalRoute::ViaComposio,
    )
}

/// What the user sees when the ledger row for a send could not be written. Fixed and content-free:
/// no body, no recipient, no reason from the sink.
pub const TRACE_UNAVAILABLE_REFUSAL: &str = "Traceability log unavailable; nothing was sent.";

/// Gmail was disconnected or reconnected between Confirm and the write. Nothing was sent; the
/// approval has to be proposed again (the desktop says so as `mail_connection_changed`).
pub const GMAIL_CONNECTION_CHANGED_REFUSAL: &str = "Gmail connection changed; create a new approval.";

/// Execute a confirmed L3 send: precheck, record traceability to `sink`, then attempt the
/// transport. The row is written before the request leaves, so a send that fails after reaching
/// the provider (a timeout, a 5xx after delivery) is still on the ledger (issue #782); a precheck
/// refusal sends nothing and records nothing. The row is written with
/// [`TraceabilitySink::try_record`], and when the ledger refuses it the transport is **not**
/// called: an untraced send is the failure invariant 3 exists to rule out, so the honest outcome
/// is [`TRACE_UNAVAILABLE_REFUSAL`] and nothing on the wire (the same
/// fail-closed rule every LLM lane follows). This is the single point through which a confirmed
/// send reaches the wire, so every send that may have left the device leaves a trace
/// (invariant 3 / FR-TR-03).
pub fn execute_send<T: SendTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    confirmed: &ConfirmedSend,
    transport: &T,
    sink: &S,
) -> SendExecOutcome {
    if !transport.accepts_route(confirmed.preview.route) {
        return SendExecOutcome::Failed("Approval connection changed; create a new proposal.".into());
    }
    if let Err(e) = transport.precheck(&confirmed.action) {
        return SendExecOutcome::Failed(e);
    }
    if sink.try_record(trace_for_send(confirmed)).is_err() {
        return SendExecOutcome::Failed(TRACE_UNAVAILABLE_REFUSAL.into());
    }
    match transport.send(&confirmed.action, &confirmed.preview.full_body) {
        Ok(()) => SendExecOutcome::Sent,
        Err(e) => SendExecOutcome::Failed(e),
    }
}

/// The traceability purpose of a Gmail draft saved as the FR-C2-05 fallback.
pub const DRAFT_FALLBACK_PURPOSE: &str = "draft_create";

/// The traceability row for a Gmail draft saved through Composio: second layer, third party, the
/// mail body digested and dropped (G8).
pub fn trace_for_draft(mail_body: &str) -> TraceRecord {
    TraceRecord::for_chunk(
        Route::Mcp,
        DRAFT_FALLBACK_PURPOSE,
        "gmail",
        mail_body,
        false,
    )
}

/// Save a confirmed email as a Gmail draft (FR-C2-05) through the connector runtime's
/// `draft_create_update` op, recording traceability **before** the write is sent (issue #782).
/// The service gate is checked first, so a draft the gate refuses (not connected, no consent)
/// sends nothing and records nothing; once it passes, the row is written even if the write then
/// fails, because the body may already have reached Gmail.
pub fn save_gmail_draft<T, S>(
    runtime: &std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>,
    sink: &S,
    action: &SendAction,
    body: &str,
) -> Result<(), String>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
    S: TraceabilitySink + ?Sized,
{
    save_gmail_draft_at_revision(runtime, sink, action, body, None)
}

/// Keep a draft-only save or recovery draft on the connection the user confirmed.
/// A disconnect/reconnect must not move an in-flight approval onto a different grant.
pub fn save_gmail_draft_at_revision<T, S>(
    runtime: &std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>,
    sink: &S,
    action: &SendAction,
    body: &str,
    expected_revision: Option<u64>,
) -> Result<(), String>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
    S: TraceabilitySink + ?Sized,
{
    let SendAction::SendEmail { to } = action else {
        return Err("draft fallback only applies to email".into());
    };
    let (subject, mail_body) = shogun_mcp::composio::parse_gmail_full_body(body);
    // Internal aliases carry the same confirmed recipient, subject and body to the provider.
    let args = serde_json::json!({ "to": to, "subject": subject, "body": mail_body });
    let mut rt = runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
    if expected_revision.is_some_and(|expected| rt.connection_revision(shogun_mcp::scope::Service::Gmail) != expected) {
        return Err(GMAIL_CONNECTION_CHANGED_REFUSAL.into());
    }
    rt.precheck_write(shogun_mcp::scope::Service::Gmail, DRAFT_OP)?;
    sink.try_record(trace_for_draft(&mail_body))
        .map_err(|_| "Gmail draft could not be recorded.".to_string())?;
    rt.execute_write_owned(shogun_mcp::scope::Service::Gmail, DRAFT_OP, args).map(|_| ())
}

/// The Gmail scope op a draft is saved under.
const DRAFT_OP: &str = "draft_create_update";

/// The first-layer [`SendTransport`]: routes a confirmed send through the connector runtime.
/// Routing is `shogun_integrations::send_bridge` (which service + scope op performs this
/// action); execution is [`ConnectorRuntime::execute_write`], which re-applies the service gate —
/// so even a confirmed send is refused if the service is unreleased / disconnected (double gate).
/// Gmail follows this same first-layer seam through the direct REST adapter.
pub struct FirstLayerSendTransport<'a, T> {
    runtime: &'a std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>,
    gmail_revision: Option<u64>,
}

impl<'a, T> FirstLayerSendTransport<'a, T> {
    pub fn new(runtime: &'a std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>) -> Self {
        Self { runtime, gmail_revision: None }
    }

    pub fn with_gmail_revision(mut self, revision: u64) -> Self {
        self.gmail_revision = Some(revision);
        self
    }
}

impl<T> SendTransport for FirstLayerSendTransport<'_, T>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
{
    fn accepts_route(&self, route: ApprovalRoute) -> bool { route == ApprovalRoute::DirectMcp }

    fn send(&self, action: &SendAction, body: &str) -> Result<(), String> {
        use shogun_integrations::send_bridge::{args_for_send, route_send, SendRoute};
        match route_send(action) {
            SendRoute::Composio => Err(FIRST_LAYER_EMAIL_REFUSAL.to_string()),
            SendRoute::LocalSetting => Err(LOCAL_SETTING_REFUSAL.to_string()),
            SendRoute::FirstLayer { service, op } => {
                let args = args_for_send(action, body);
                let mut rt = self.runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
                if service == shogun_mcp::scope::Service::Gmail
                    && self.gmail_revision.is_some_and(|expected| rt.connection_revision(service) != expected)
                {
                    return Err(GMAIL_CONNECTION_CHANGED_REFUSAL.into());
                }
                rt.execute_write_owned(service, op, args).map(|_| ())
            }
        }
    }

    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        use shogun_integrations::send_bridge::{route_send, SendRoute};
        // A card an older build queued may name a target no provider call can use; refuse it
        // here, before the ledger row, rather than after it at the provider.
        if let Some(reason) = shogun_agents::permission::send_target_refusal(action) {
            return Err(reason.to_string());
        }
        match route_send(action) {
            SendRoute::Composio => Err(FIRST_LAYER_EMAIL_REFUSAL.to_string()),
            SendRoute::LocalSetting => Err(LOCAL_SETTING_REFUSAL.to_string()),
            SendRoute::FirstLayer { service, op } => {
                let rt = self.runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
                if service == shogun_mcp::scope::Service::Gmail
                    && self.gmail_revision.is_some_and(|expected| rt.connection_revision(service) != expected)
                {
                    return Err(GMAIL_CONNECTION_CHANGED_REFUSAL.into());
                }
                rt.precheck_write(service, op)
            }
        }
    }
}

/// The legacy broker route cannot be executed by the direct transport.
const FIRST_LAYER_EMAIL_REFUSAL: &str = "legacy broker route is unavailable on the direct transport";

/// A [`SendTransport`] that performs a Gmail send through Composio (second layer, §6.10 / FR-C2-01).
/// Pure over the [`ComposioApi`](shogun_integrations::composio::ComposioApi) seam — the reqwest
/// client is [`crate::composio_send::HttpComposioApi`] (feature `net`). Handles **only** the email
/// send (v1's single second-layer op); any other action is refused (it is the first layer's).
pub struct ComposioSendTransport<A: shogun_integrations::composio::ComposioApi> {
    api: A,
    /// This device's Composio user identifier for the connected Gmail account.
    user_id: String,
}

impl<A: shogun_integrations::composio::ComposioApi> ComposioSendTransport<A> {
    pub fn new(api: A, user_id: impl Into<String>) -> Self {
        Self { api, user_id: user_id.into() }
    }
}

impl<A: shogun_integrations::composio::ComposioApi> SendTransport for ComposioSendTransport<A> {
    fn send(&self, action: &SendAction, body: &str) -> Result<(), String> {
        use shogun_integrations::composio::{gmail_send_arguments, parse_execute_response, GMAIL_SEND_EMAIL};
        let SendAction::SendEmail { to } = action else {
            return Err("ComposioSendTransport only performs Gmail send (second layer)".to_string());
        };
        // `body` is the confirmed full preview ("Subject: …\n\n…"); split it back for Composio.
        let (subject, mail_body) = shogun_mcp::composio::parse_gmail_full_body(body);
        let args = gmail_send_arguments(to, &subject, &mail_body);
        let resp = self.api.execute(GMAIL_SEND_EMAIL, &self.user_id, args)?;
        parse_execute_response(&resp)
    }

    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        match action {
            SendAction::SendEmail { .. } => Ok(()),
            _ => Err("ComposioSendTransport only performs Gmail send (second layer)".to_string()),
        }
    }
}

/// Saves a device-visible Gmail draft when a Composio send fails (FR-C2-05). Wired by the daemon to
/// [`save_gmail_draft`], which records the draft's own traceability row before it is sent.
pub type DraftFallback = Box<dyn Fn(&SendAction, &str) -> Result<(), String> + Send + Sync>;

/// Legacy compatibility transport; the desktop never constructs it after 2026-10-07.
/// A [`SendTransport`] that dispatches by route ([`shogun_integrations::send_bridge::route_send`]):
/// the email send goes through Composio (second layer, §6.10); everything else goes through the
/// first layer. On a Composio failure it applies the FR-C2-05 fallback — save a Gmail draft via the
/// injected `draft_fallback` — and still reports failure (the send may not have happened). The
/// send attempt was already traced by [`execute_send`]; the draft is a second egress and traces
/// itself. This is the one transport the daemon hands to `execute_send`.
pub struct RoutedSendTransport<C: SendTransport, F: SendTransport> {
    composio: C,
    first_layer: F,
    draft_fallback: DraftFallback,
}

impl<C: SendTransport, F: SendTransport> RoutedSendTransport<C, F> {
    pub fn new(composio: C, first_layer: F, draft_fallback: DraftFallback) -> Self {
        Self { composio, first_layer, draft_fallback }
    }
}

fn legacy_composio_route(action: &SendAction) -> shogun_integrations::send_bridge::SendRoute {
    if matches!(action, SendAction::SendEmail { .. }) { shogun_integrations::send_bridge::SendRoute::Composio }
    else { shogun_integrations::send_bridge::route_send(action) }
}

impl<C: SendTransport, F: SendTransport> SendTransport for RoutedSendTransport<C, F> {
    fn send(&self, action: &SendAction, body: &str) -> Result<(), String> {
        use shogun_integrations::send_bridge::SendRoute;
        match legacy_composio_route(action) {
            SendRoute::Composio => match self.composio.send(action, body) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // FR-C2-05: the send failed → save a draft, but never report it as sent.
                    match (self.draft_fallback)(action, body) {
                        Ok(()) => Err(format!("composio send failed ({e}); Gmail draft saved")),
                        Err(de) => {
                            Err(format!("composio send failed ({e}); draft fallback also failed ({de})"))
                        }
                    }
                }
            },
            SendRoute::FirstLayer { .. } => self.first_layer.send(action, body),
            SendRoute::LocalSetting => Err(LOCAL_SETTING_REFUSAL.to_string()),
        }
    }

    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        use shogun_integrations::send_bridge::SendRoute;
        match legacy_composio_route(action) {
            SendRoute::Composio => self.composio.precheck(action),
            SendRoute::FirstLayer { .. } => self.first_layer.precheck(action),
            SendRoute::LocalSetting => Err(LOCAL_SETTING_REFUSAL.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::{digest, RecordingSink};
    use shogun_agents::approval::{Preview, Route as ApprovalRoute};

    fn email_send() -> SendAction {
        SendAction::SendEmail { to: "alice@example.com".into() }
    }

    fn confirmed(route: ApprovalRoute, body: &str) -> ConfirmedSend {
        let action = email_send();
        let preview = Preview::for_send(&action, body, route);
        ConfirmedSend { action, preview }
    }

    #[test]
    fn reply_marker_reads_use_the_latest_reply_and_refuse_incomplete_pages() {
        use serde_json::json;
        assert_eq!(latest_reply_marker(&json!({"latest_message_id":"m2"}), false).expect("test"), "m2");
        assert_eq!(latest_reply_marker(&json!({"messages":[{"ts":"100.000001","latest_reply":"102.000001"}],"has_more":true}), true).expect("test"), "102.000001");
        assert_eq!(latest_reply_marker(&json!({"messages":[{"ts":"102.000001"},{"ts":"101.000001"}]}), true).expect("test"), "102.000001");
        assert!(latest_reply_marker(&json!({"messages":[{"ts":"100.000001"}],"has_more":true}), true).is_err());
        assert!(latest_reply_marker(&json!({"messages":[]}), true).is_err());
    }

    /// A transport that succeeds, or fails with a fixed error.
    struct Fake {
        ok: bool,
    }
    impl SendTransport for Fake {
        fn send(&self, _action: &SendAction, _body: &str) -> Result<(), String> {
            if self.ok {
                Ok(())
            } else {
                Err("not connected".into())
            }
        }
    }

    #[test]
    fn successful_send_records_exactly_one_trace_with_digest_only() {
        let sink = RecordingSink::new();
        let body = "Hi Alice — shipping the roadmap Friday.";
        let cs = confirmed(ApprovalRoute::DirectMcp, body);

        let outcome = execute_send(&cs, &Fake { ok: true }, &sink);
        assert_eq!(outcome, SendExecOutcome::Sent);

        let recs = sink.records();
        assert_eq!(recs.len(), 1, "a send that reached the wire writes exactly one trace");
        let rec = &recs[0];
        assert_eq!(rec.route, Route::Mcp);
        assert_eq!(rec.purpose, "integration.send_email");
        assert_eq!(rec.destination, "gmail", "the service, never the recipient (issue #924)");
        assert_eq!(rec.chunk_bytes, body.len());
        assert_eq!(rec.chunk_xxh64, digest(body));
        assert!(!rec.third_party, "a direct MCP send is not third-party");
        // the body text never appears in the record
        assert!(!format!("{rec:?}").contains("roadmap"), "sent body must never appear in the trace");
        assert!(!format!("{rec:?}").contains("alice"), "the recipient must never appear in the trace");
    }

    #[test]
    fn composio_send_is_traced_third_party() {
        let sink = RecordingSink::new();
        let cs = confirmed(ApprovalRoute::ViaComposio, "body");
        assert_eq!(execute_send(&cs, &Fake { ok: true }, &sink), SendExecOutcome::Sent);
        let recs = sink.records();
        assert_eq!(recs[0].route, Route::Composio);
        assert!(recs[0].third_party, "a Composio-relayed send is disclosed third-party (FR-C2-04)");
    }

    #[test]
    fn a_failed_send_is_still_traced_because_the_body_may_have_left() {
        // Issue #782: a timeout or 5xx can follow a delivered send. The row is written before
        // the transport runs, so a failure cannot erase the egress from the ledger.
        let sink = RecordingSink::new();
        let body = "body that may have left";
        let cs = confirmed(ApprovalRoute::DirectMcp, body);
        let outcome = execute_send(&cs, &Fake { ok: false }, &sink);
        assert_eq!(outcome, SendExecOutcome::Failed("not connected".into()));
        let recs = sink.records();
        assert_eq!(recs.len(), 1, "the attempt is on the ledger");
        assert_eq!(recs[0].purpose, "integration.send_email");
        assert_eq!(recs[0].chunk_xxh64, digest(body));
        assert!(!format!("{:?}", recs[0]).contains("may have left"), "content-free row");
    }

    #[test]
    fn the_row_is_written_before_the_transport_runs() {
        /// Fails the send if the sink has not been written yet.
        struct OrderCheck<'a>(&'a RecordingSink);
        impl SendTransport for OrderCheck<'_> {
            fn send(&self, _action: &SendAction, _body: &str) -> Result<(), String> {
                if self.0.records().len() == 1 {
                    Ok(())
                } else {
                    Err("sent before it was recorded".into())
                }
            }
        }
        let sink = RecordingSink::new();
        let cs = confirmed(ApprovalRoute::ViaComposio, "Subject: s\n\nb");
        assert_eq!(execute_send(&cs, &OrderCheck(&sink), &sink), SendExecOutcome::Sent);
    }

    #[test]
    fn a_refused_trace_row_stops_the_send_before_the_transport() {
        use crate::llm::traceability::RefusingSink;
        /// Counts every call: with a refusing sink the count must stay at zero.
        struct NeverSent(std::sync::atomic::AtomicUsize);
        impl SendTransport for NeverSent {
            fn send(&self, _action: &SendAction, _body: &str) -> Result<(), String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }
        let sink = RefusingSink::new();
        let transport = NeverSent(std::sync::atomic::AtomicUsize::new(0));
        let body = "Hi Alice — the body that must not leave untraced";
        let cs = confirmed(ApprovalRoute::DirectMcp, body);
        let outcome = execute_send(&cs, &transport, &sink);
        assert_eq!(outcome, SendExecOutcome::Failed(TRACE_UNAVAILABLE_REFUSAL.into()));
        assert_eq!(transport.0.load(std::sync::atomic::Ordering::Relaxed), 0, "the transport was called without a ledger row");
        assert_eq!(sink.refused(), 1, "exactly one row was attempted");
        let shown = format!("{outcome:?}");
        assert!(!shown.contains("Alice") && !shown.contains("untraced"), "content-free refusal: {shown}");
        // The same send goes through once the ledger is back.
        let ok = RecordingSink::new();
        assert_eq!(execute_send(&cs, &transport, &ok), SendExecOutcome::Sent);
        assert_eq!(ok.records().len(), 1);
    }

    #[test]
    fn purpose_is_per_action() {
        let cal = ConfirmedSend {
            action: SendAction::CreateCalendarEvent { title: "Sync".into() },
            preview: Preview::for_send(
                &SendAction::CreateCalendarEvent { title: "Sync".into() },
                "Sync 3pm",
                ApprovalRoute::DirectMcp,
            ),
        };
        assert_eq!(trace_for_send(&cal).purpose, "integration.create_calendar_event");
    }

    #[test]
    fn the_trace_names_the_service_while_the_card_and_request_keep_the_target() {
        // Issue #924. Every target below is fictional and private: a recipient, a calendar or
        // document title, a channel, an issue. The approval card and the request keep each one in
        // full; the ledger row keeps only the service and the action.
        let cases = [
            (SendAction::SendEmail { to: "alice@example.com".into() }, "alice@example.com", "gmail", "integration.send_email"),
            (SendAction::CreateCalendarEvent { title: "Medical appointment".into() }, "Medical appointment", "gcal", "integration.create_calendar_event"),
            (SendAction::UpdateCalendarEvent { title: "Interview with Rival Co".into() }, "Interview with Rival Co", "gcal", "integration.update_calendar_event"),
            (SendAction::CreateDocument { title: "Divorce paperwork".into() }, "Divorce paperwork", "gdrive", "integration.create_document"),
            (SendAction::UpdateDocument { title: "Layoff plan Q4".into() }, "Layoff plan Q4", "notion", "integration.update_document"),
            (SendAction::PostMessage { channel: "@dana-private".into() }, "@dana-private", "slack", "integration.post_message"),
            (SendAction::AddReaction { target: "C42/1700000000.000100".into() }, "C42/1700000000.000100", "slack", "integration.add_reaction"),
            (SendAction::PostComment { target: "https://github.com/acme/secret-repo/issues/7".into() }, "https://github.com/acme/secret-repo/issues/7", "github", "integration.post_comment"),
            (SendAction::ChangeIssueStatus { target: "HR-12 Done".into() }, "HR-12 Done", "linear", "integration.change_issue_status"),
        ];
        for (action, target, service, purpose) in cases {
            let route = if service == "gmail" { ApprovalRoute::ViaComposio } else { ApprovalRoute::DirectMcp };
            let preview = Preview::for_send(&action, "body", route);
            assert_eq!(preview.destination, target, "the approval card keeps the full target");
            // GitHub's request carries the URL split into owner / repo / issue number.
            let marker = if service == "github" { "secret-repo" } else { target };
            let args = shogun_integrations::send_bridge::args_for_send(&action, "body").to_string();
            assert!(args.contains(marker), "the request keeps the full target: {args}");

            let sink = RecordingSink::new();
            let cs = ConfirmedSend { action, preview };
            assert_eq!(execute_send(&cs, &Fake { ok: true }, &sink), SendExecOutcome::Sent);
            let recs = sink.records();
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].destination, service);
            assert_eq!(recs[0].purpose, purpose);
            assert_eq!(recs[0].third_party, service == "gmail");
            assert_eq!(recs[0].chunk_xxh64, digest("body"));
            let row = format!("{:?}", recs[0]);
            assert!(!row.contains(target) && !row.contains(marker), "{target} leaked into the trace");
        }
    }

    #[test]
    fn v70_maps_every_send_purpose_to_the_service_new_rows_record() {
        // V70 is frozen once merged; its CASE must agree with `trace_for_send` for every send.
        let sql = include_str!(
            "../../shogun-memory/src/migrations/V70__traceability_send_destination_service.sql"
        );
        let v70: std::collections::BTreeMap<&str, &str> = sql
            .lines()
            .filter_map(|l| {
                let l = l.trim().strip_prefix("WHEN '")?;
                let (purpose, rest) = l.split_once("'")?;
                let service = rest.trim().strip_prefix("THEN '")?.strip_suffix('\'')?;
                Some((purpose, service))
            })
            .collect();
        let actions = [
            SendAction::SendEmail { to: "x".into() },
            SendAction::PostMessage { channel: "x".into() },
            SendAction::AddReaction { target: "x".into() },
            SendAction::CreateCalendarEvent { title: "x".into() },
            SendAction::UpdateCalendarEvent { title: "x".into() },
            SendAction::PostComment { target: "x".into() },
            SendAction::CreateDocument { title: "x".into() },
            SendAction::UpdateDocument { title: "x".into() },
            SendAction::ChangeIssueStatus { target: "x".into() },
        ];
        assert_eq!(v70.len(), actions.len());
        for action in actions {
            let preview = Preview::for_send(&action, "b", ApprovalRoute::DirectMcp);
            let rec = trace_for_send(&ConfirmedSend { action, preview });
            assert_eq!(v70.get(rec.purpose.as_str()), Some(&rec.destination.as_str()), "{}", rec.purpose);
        }
    }

    // FirstLayerSendTransport

    use shogun_integrations::{ConnectorRuntime, WriteExecutor};
    use shogun_mcp::scope::{Service, Wave};
    use shogun_mcp::sync::{FetchedItem, IntegrationTransport};
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// The write the runtime's own transport recorded (shared so a test can observe writes that go
    /// through the runtime, not a separate executor).
    type Executed = std::rc::Rc<RefCell<Option<(Service, String, serde_json::Value)>>>;

    /// A fake that satisfies both seams: no-op reads, and records the executed write.
    struct FakeMcp {
        executed: Executed,
    }
    impl IntegrationTransport for FakeMcp {
        fn read_sync(&self, _s: Service) -> Result<Vec<FetchedItem>, String> {
            Ok(vec![])
        }
    }
    impl WriteExecutor for FakeMcp {
        fn execute(&self, s: Service, op: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
            *self.executed.borrow_mut() = Some((s, op.to_string(), args));
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    /// Build a runtime whose own transport records writes, plus the handle to inspect them.
    fn runtime_at(wave: Wave, connect: &[Service]) -> (Mutex<ConnectorRuntime<FakeMcp>>, Executed) {
        let executed: Executed = std::rc::Rc::new(RefCell::new(None));
        let mut rt = ConnectorRuntime::new(FakeMcp { executed: executed.clone() }, wave, true);
        // A new runtime starts on the not-started trial, which refuses every write. These tests
        // are about the send path, not plan gating, so give them a plan that may write.
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        for &s in connect {
            rt.mark_connected(s, 1_000);
        }
        (Mutex::new(rt), executed)
    }

    #[test]
    fn confirmed_drive_send_executes_the_mapped_op_and_traces() {
        let (rt, executed) = runtime_at(Wave::One, &[Service::GoogleDrive]);
        let transport = FirstLayerSendTransport::new(&rt);

        let action = SendAction::CreateDocument { title: "Sync".into() };
        let preview = Preview::for_send(&action, "agenda body", ApprovalRoute::DirectMcp);
        let sink = RecordingSink::new();
        let out = execute_send(&ConfirmedSend { action, preview }, &transport, &sink);

        assert_eq!(out, SendExecOutcome::Sent);
        // The scope op (not the raw tool) was dispatched with the confirmed content.
        let (svc, op, args) = executed.borrow().clone().unwrap();
        assert_eq!((svc, op.as_str()), (Service::GoogleDrive, "file_create"));
        assert_eq!(args["name"], "Sync");
        assert_eq!(args["content"], "agenda body");
        // Exactly one trace, direct-MCP route.
        assert_eq!(sink.records().len(), 1);
    }

    #[test]
    fn a_send_no_provider_could_run_is_refused_before_the_ledger() {
        // A card an older build queued: an incomplete target or an unavailable write. Refused at
        // precheck, so nothing runs and no row claims an egress that never happened.
        let (rt, executed) = runtime_at(
            Wave::Three,
            &[Service::GoogleCalendar, Service::Notion, Service::GitHub, Service::Slack],
        );
        let transport = FirstLayerSendTransport::new(&rt);
        let sink = RecordingSink::new();
        for action in [
            SendAction::CreateCalendarEvent { title: "Sync".into() },
            SendAction::UpdateDocument { title: "Page".into() },
            SendAction::PostComment { target: "acme/repo#7".into() },
            SendAction::PostMessage { channel: "#general".into() },
        ] {
            let preview = Preview::for_send(&action, "body", ApprovalRoute::DirectMcp);
            let out = execute_send(&ConfirmedSend { action, preview }, &transport, &sink);
            assert!(matches!(out, SendExecOutcome::Failed(_)));
        }
        assert!(executed.borrow().is_none());
        assert!(sink.records().is_empty());
    }

    #[test]
    fn direct_email_send_requires_draft_unlock_and_refuses_stale_previews() {
        let (rt, executed) = runtime_at(Wave::One, &[Service::Gmail]);
        rt.lock().unwrap().use_direct_gmail();
        let transport = FirstLayerSendTransport::new(&rt);
        let cs = confirmed(ApprovalRoute::DirectMcp, "Subject: Plan\n\nShip Friday.");
        let sink = RecordingSink::new();
        assert!(matches!(execute_send(&cs, &transport, &sink), SendExecOutcome::Failed(_)));
        assert!(executed.borrow().is_none());
        assert!(sink.records().is_empty());
        rt.lock().unwrap().set_gmail_draft_stop(false);
        let old = confirmed(ApprovalRoute::ViaComposio, "body");
        assert!(matches!(execute_send(&old, &transport, &sink), SendExecOutcome::Failed(_)));
        assert!(executed.borrow().is_none());
        assert_eq!(execute_send(&cs, &transport, &sink), SendExecOutcome::Sent);
        assert_eq!(executed.borrow().as_ref().unwrap().1,"send");
        assert!(!sink.records()[0].third_party);
    }

    #[test]
    fn unreleased_wave_refuses_even_a_confirmed_send_double_gate() {
        // Slack post confirmed, but Slack is Wave 2 and only Wave 1 is released — the runtime's
        // gate refuses it even post-approval (double gate).
        let (rt, executed) = runtime_at(Wave::One, &[Service::Slack]);
        let transport = FirstLayerSendTransport::new(&rt);

        let action = SendAction::PostMessage { channel: "C0123ABCD".into() };
        let preview = Preview::for_send(&action, "hello", ApprovalRoute::DirectMcp);
        let sink = RecordingSink::new();
        let out = execute_send(&ConfirmedSend { action, preview }, &transport, &sink);

        assert!(matches!(out, SendExecOutcome::Failed(_)));
        assert!(executed.borrow().is_none());
        assert!(sink.records().is_empty());
    }

    // ComposioSendTransport + RoutedSendTransport

    use shogun_integrations::composio::ComposioApi;
    use serde_json::{json, Value};

    struct FakeComposio {
        last: RefCell<Option<(String, String, Value)>>,
        reply: Result<Value, String>,
    }
    impl ComposioApi for FakeComposio {
        fn execute(&self, tool: &str, user_id: &str, args: Value) -> Result<Value, String> {
            *self.last.borrow_mut() = Some((tool.to_string(), user_id.to_string(), args));
            self.reply.clone()
        }
    }

    #[test]
    fn composio_email_send_executes_gmail_send_email_and_traces() {
        let api = FakeComposio { last: RefCell::new(None), reply: Ok(json!({ "successful": true })) };
        let transport = ComposioSendTransport::new(api, "user-42");
        // The Composio preview carries the ViaComposio route (third-party badge in the trace).
        let cs = confirmed(ApprovalRoute::ViaComposio, "Subject: Ship date\n\nFriday.");
        let sink = RecordingSink::new();

        let out = execute_send(&cs, &transport, &sink);
        assert_eq!(out, SendExecOutcome::Sent);
        let (tool, user, args) = transport.api.last.borrow().clone().unwrap();
        assert_eq!((tool.as_str(), user.as_str()), ("GMAIL_SEND_EMAIL", "user-42"));
        assert_eq!(args["recipient_email"], "alice@example.com");
        assert_eq!(args["subject"], "Ship date");
        assert_eq!(args["body"], "Friday.");
        // Exactly one trace, carrying the third-party (Composio) badge.
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].third_party, "a Composio send must be traced third-party (FR-C2-04)");
    }

    #[test]
    fn composio_failure_saves_a_draft_and_traces_both_egresses_frc205() {
        let api = FakeComposio { last: RefCell::new(None), reply: Err("composio http 500".into()) };
        let composio = ComposioSendTransport::new(api, "u");
        // First-layer arm is unused for an email; a no-op fake satisfies the type.
        let (rt, _executed) = runtime_at(Wave::One, &[]);
        let first_layer = FirstLayerSendTransport::new(&rt);

        // The daemon wires the fallback to `save_gmail_draft` on the same ledger; this stand-in
        // writes the row that function writes, without a live runtime behind it.
        let sink = std::sync::Arc::new(RecordingSink::new());
        let draft_sink = std::sync::Arc::clone(&sink);
        let routed = RoutedSendTransport::new(
            composio,
            first_layer,
            Box::new(move |_a, body| {
                let (_subject, mail_body) = shogun_mcp::composio::parse_gmail_full_body(body);
                draft_sink.record(trace_for_draft(&mail_body));
                Ok(())
            }),
        );

        let cs = confirmed(ApprovalRoute::ViaComposio, "Subject: x\n\nbody");
        let out = execute_send(&cs, &routed, sink.as_ref());

        // FR-C2-05: failed send, draft saved, and never reported as sent.
        match out {
            SendExecOutcome::Failed(e) => assert!(e.contains("draft saved"), "got: {e}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        // Issue #782: the body went to Composio twice (the send that may have been delivered,
        // then the draft), so the ledger shows two third-party rows in that order.
        let recs = sink.records();
        let purposes: Vec<&str> = recs.iter().map(|r| r.purpose.as_str()).collect();
        assert_eq!(purposes, ["integration.send_email", DRAFT_FALLBACK_PURPOSE]);
        assert!(recs[0].third_party && recs[0].route == Route::Composio);
        assert!(!recs[1].third_party && recs[1].route == Route::Mcp);
    }

    /// A runtime transport whose writes always fail after "reaching" the provider.
    struct FailingWrites;
    impl IntegrationTransport for FailingWrites {
        fn read_sync(&self, _s: Service) -> Result<Vec<FetchedItem>, String> {
            Ok(vec![])
        }
    }
    impl WriteExecutor for FailingWrites {
        fn execute(&self, _s: Service, _op: &str, _args: serde_json::Value) -> Result<serde_json::Value, String> {
            Err("composio http 504".into())
        }
    }

    fn gmail_runtime<T>(transport: T, connected: bool) -> Mutex<ConnectorRuntime<T>> {
        let mut rt = ConnectorRuntime::new(transport, Wave::One, true);
        rt.set_plan(shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0));
        rt.set_composio_consent(true);
        if connected {
            rt.mark_connected(Service::Gmail, 1_000);
        }
        Mutex::new(rt)
    }

    #[test]
    fn reconnect_cancels_confirmed_gmail_send_and_recovery_draft() {
        let executed: Executed = std::rc::Rc::new(RefCell::new(None));
        let rt = gmail_runtime(FakeMcp { executed: executed.clone() }, true);
        let revision = {
            let mut state = rt.lock().unwrap();
            state.use_direct_gmail();
            state.set_gmail_draft_stop(false);
            state.connection_revision(Service::Gmail)
        };
        let transport = FirstLayerSendTransport::new(&rt).with_gmail_revision(revision);
        {
            let mut state = rt.lock().unwrap();
            state.disconnect(Service::Gmail, false);
            state.mark_connected(Service::Gmail, 2_000);
        }
        let sink = RecordingSink::new();
        assert!(matches!(execute_send(&confirmed(ApprovalRoute::DirectMcp, "Subject: Plan\n\nBody"), &transport, &sink), SendExecOutcome::Failed(_)));
        assert!(save_gmail_draft_at_revision(&rt, &sink, &email_send(), "Subject: Plan\n\nBody", Some(revision)).is_err());
        assert!(executed.borrow().is_none());
        assert!(sink.records().is_empty());
    }

    #[test]
    fn a_saved_draft_is_traced_as_one_direct_row() {
        let executed: Executed = std::rc::Rc::new(RefCell::new(None));
        let rt = gmail_runtime(FakeMcp { executed: executed.clone() }, true);
        let sink = RecordingSink::new();
        save_gmail_draft(&rt, &sink, &email_send(), "Subject: Plan\n\nShip Friday.").expect("draft saved");
        let (svc, op, args) = executed.borrow().clone().expect("the draft write ran");
        assert_eq!((svc, op.as_str()), (Service::Gmail, "draft_create_update"));
        assert_eq!(args["subject"], "Plan");
        let recs = sink.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].purpose, DRAFT_FALLBACK_PURPOSE);
        assert_eq!(recs[0].chunk_xxh64, digest("Ship Friday."));
        assert!(!recs[0].third_party);
        assert_eq!(recs[0].route,Route::Mcp);
    }

    #[test]
    fn a_failed_draft_write_is_still_traced() {
        let rt = gmail_runtime(FailingWrites, true);
        let sink = RecordingSink::new();
        assert!(save_gmail_draft(&rt, &sink, &email_send(), "Subject: s\n\nb").is_err());
        assert_eq!(sink.records().len(), 1, "the body may have reached Composio before the failure");
    }

    #[test]
    fn a_draft_the_gate_refuses_is_not_traced() {
        // Gmail not connected: the gate refuses before anything is sent.
        let rt = gmail_runtime(FailingWrites, false);
        let sink = RecordingSink::new();
        assert!(save_gmail_draft(&rt, &sink, &email_send(), "Subject: s\n\nb").is_err());
        let calendar = SendAction::CreateCalendarEvent { title: "x".into() };
        assert!(save_gmail_draft(&rt, &sink, &calendar, "b").is_err());
        assert!(sink.records().is_empty());
    }

    #[test]
    fn routed_sends_non_email_through_the_first_layer() {
        let api = FakeComposio { last: RefCell::new(None), reply: Ok(json!({ "successful": true })) };
        let composio = ComposioSendTransport::new(api, "u");
        let (rt, executed) = runtime_at(Wave::One, &[Service::GoogleDrive]);
        let first_layer = FirstLayerSendTransport::new(&rt);
        let routed = RoutedSendTransport::new(composio, first_layer, Box::new(|_a, _b| Ok(())));

        let action = SendAction::CreateDocument { title: "Sync".into() };
        let preview = Preview::for_send(&action, "agenda", ApprovalRoute::DirectMcp);
        let sink = RecordingSink::new();
        let out = execute_send(&ConfirmedSend { action, preview }, &routed, &sink);

        assert_eq!(out, SendExecOutcome::Sent);
        // The first layer executed the Drive op (the Composio arm is only for email).
        let (svc, op, _) = executed.borrow().clone().unwrap();
        assert_eq!((svc, op.as_str()), (Service::GoogleDrive, "file_create"));
    }

    #[test]
    fn local_setting_approvals_are_never_sent_by_any_transport() {
        // Issues #654 / #657 / #782: confirming one is the desktop's local setting change. If it ever
        // reaches a transport, it fails there, touches no connector or Composio, and traces nothing.
        let api = FakeComposio { last: RefCell::new(None), reply: Ok(json!({ "successful": true })) };
        let composio = ComposioSendTransport::new(api, "u");
        let (rt, executed) = runtime_at(Wave::One, &[Service::GoogleCalendar]);
        let routed = RoutedSendTransport::new(
            composio,
            FirstLayerSendTransport::new(&rt),
            Box::new(|_a, _b| panic!("no draft fallback for a setting")),
        );
        let sink = RecordingSink::new();
        let resume = shogun_agents::permission::CaptureExclusionChange {
            resumed_apps: vec!["com.example.Secret".into()],
            ..Default::default()
        };
        for (action, preview) in [
            shogun_agents::approval::writing_style_request(),
            shogun_agents::approval::capture_resume_request(resume),
            shogun_agents::approval::subscription_agent_request("codex", "Codex", "ChatGPT Plus / Pro"),
            shogun_agents::approval::visual_recall_request(shogun_agents::permission::VisualRecallWidening {
                enable: true,
                retention_days: None,
                previous_retention_days: 3,
            }),
            shogun_agents::approval::mail_archive_request("gmail:1", false),
        ] {
            let confirmed = ConfirmedSend { action, preview };
            for transport in [&routed as &dyn SendTransport, &FirstLayerSendTransport::new(&rt)] {
                assert_eq!(
                    execute_send(&confirmed, transport, &sink),
                    SendExecOutcome::Failed(LOCAL_SETTING_REFUSAL.into())
                );
            }
        }
        assert!(executed.borrow().is_none(), "no connector op ran");
        assert!(sink.records().is_empty(), "nothing egressed, so nothing is traced");
    }
}
