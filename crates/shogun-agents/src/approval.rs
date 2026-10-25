//! The L3 explicit-approval flow (pure core; §6.6.1, FR-AG-03/04). Every external send waits
//! here for a deliberate human confirmation before it runs.
//!
//! Structural guarantees:
//! - An approval request holds a [`SendAction`], not a generic `Action`. Since only sends are L3
//!   (invariant 4) and every send is L3, the queue is L3-by-construction — there is no path to
//!   enqueue a non-send, and none to downgrade one (FR-AG-02: no dynamic L3→L2).
//! - Confirmation requires the dedicated button ([`ConfirmIntent::DedicatedButton`]); the Enter
//!   key alone never confirms (FR-AG-03) — a distinct intent that the state machine refuses.
//! - The same flow serves human-UI and AI-API origins (FR-AG-04 / invariant 6). An API-origin
//!   request stays [`ApprovalState::Pending`] (the API call returns pending) until the user
//!   confirms or the 10-minute timeout rejects it.
//!
//! The preview carries the *full* content (FR-AG-03: full text, not a summary) plus destination,
//! route, and key kind, so the confirm UI can show everything required. The text lives only in the
//! in-memory request; traceability persists a digest, never this body.

use crate::permission::{CaptureExclusionChange, PausedCapture, SendAction, VisualRecallWidening};

/// The route a send takes (FR-AG-03: 経路表示). Composio entries also get a "third-party" badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Direct to the service's official remote MCP.
    DirectMcp,
    /// Via Composio (second layer, §6.10) — surfaced as "third-party".
    ViaComposio,
}

impl Route {
    /// Whether a confirmed approval carrying this route may execute. Every provider write,
    /// Gmail included, goes over the user's own direct connection (#713), so only
    /// [`Route::DirectMcp`] runs. A [`Route::ViaComposio`] preview was queued for the retired
    /// mail connection: it stays listed so the user can reject or regenerate it, but confirming
    /// it must never send under a route other than the one the card showed.
    pub fn executable(self) -> bool {
        matches!(self, Route::DirectMcp)
    }
}

/// The credential that will execute the send (FR-AG-03: 使用キー種別), using the FR-TR-01 trace
/// vocabulary so the confirm preview and the traceability record can never disagree. Derived from
/// the route in [`Preview::for_send`] — a send never picks its own label (#273).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// The user's own OAuth token to the service's official remote MCP (first layer).
    OauthUser,
    /// The Composio API key (second layer, third party).
    Composio,
    /// The user's own AI credentials (their plan or their key) for the model provider — the
    /// credential a shared writing example travels under (issue #654).
    AiProvider,
    /// No credential at all: confirming changes a setting on this Mac and nothing leaves it
    /// (issue #657, resuming capture of excluded apps or websites).
    Local,
}

/// The surface a pending approval was requested from. The flow is
/// identical for every origin (FR-AG-04 / invariant 6) — the origin never changes permissions,
/// confirmation rules, or expiry. It is carried so the single shared queue's listing can label
/// each entry (UI / API / MCP) and so an API/MCP caller's waiting semantics (holding a pending
/// result) are visible to the confirm UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOrigin {
    /// Enqueued by the human UI (Notch / settings / an agent the user launched from the UI).
    Ui,
    /// Proposed by the chat assistant.
    Assistant,
    /// Enqueued through the REST/CLI Memory API face.
    Api,
    /// Enqueued through the MCP (stdio) face.
    Mcp,
}

impl ApprovalOrigin {
    /// Stable wire label for list output ("ui" / "assistant" / "api" / "mcp").
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalOrigin::Ui => "ui",
            ApprovalOrigin::Assistant => "assistant",
            ApprovalOrigin::Api => "api",
            ApprovalOrigin::Mcp => "mcp",
        }
    }

    /// Parse the wire label written by [`Self::as_str`].
    pub fn parse(s: &str) -> Option<ApprovalOrigin> {
        match s {
            "ui" => Some(ApprovalOrigin::Ui),
            "assistant" => Some(ApprovalOrigin::Assistant),
            "api" => Some(ApprovalOrigin::Api),
            "mcp" => Some(ApprovalOrigin::Mcp),
            _ => None,
        }
    }
}

/// Provider thread identity and the last message used to prepare a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyThread {
    pub thread_id: String,
    pub latest_message_id: String,
}

impl ReplyThread {
    /// A failed/offline read preserves the existing explicit-confirm behavior.
    pub fn changed(&self, latest: Result<&str, &str>, acknowledged: bool) -> bool {
        !acknowledged && latest.is_ok_and(|id| id != self.latest_message_id)
    }
}

/// The full preview shown at L3 confirmation (FR-AG-03). `full_body` is the complete content, never
/// a summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub op_type: &'static str,
    pub destination: String,
    pub full_body: String,
    pub route: Route,
    pub key_kind: KeyKind,
    pub reply_thread: Option<ReplyThread>,
}

impl Preview {
    /// Build the preview for a send, deriving the operation type and destination from the action —
    /// and the key kind from the route — so they cannot disagree with what will actually run.
    pub fn for_send(action: &SendAction, full_body: impl Into<String>, route: Route) -> Self {
        let (op_type, destination) = describe(action);
        Self {
            op_type,
            destination,
            full_body: full_body.into(),
            reply_thread: None,
            route,
            key_kind: match (action, route) {
                // Writing examples go to the model provider under the user's own AI credentials,
                // not under a connected service account.
                // So do delegated prompts (issue #110): they run on the user's own AI plan.
                (SendAction::ShareWritingStyle | SendAction::UseSubscriptionAgent { .. }, _) => {
                    KeyKind::AiProvider
                }
                // A capture-exclusion change is applied locally; no key executes anything.
                // So is a Visual recall change (issue #782): the frames stay on this Mac.
                (
                    SendAction::ResumeCapture { .. }
                    | SendAction::UnpauseCapture { .. }
                    | SendAction::EnableDictionaryAutoAdd
                    | SendAction::WidenVisualRecall { .. },
                    _,
                ) => KeyKind::Local,
                (_, Route::DirectMcp) => KeyKind::OauthUser,
                (_, Route::ViaComposio) => KeyKind::Composio,
            },
        }
    }
}

/// Human-readable op type + full destination for a send (FR-AG-03: 完全表記).
fn describe(action: &SendAction) -> (&'static str, String) {
    match action {
        SendAction::SendEmail { to } => ("Send email", to.clone()),
        SendAction::PostMessage { channel } => ("Post message", channel.clone()),
        SendAction::AddReaction { target } => ("Add reaction", target.clone()),
        SendAction::CreateCalendarEvent { title } => ("Create calendar event", title.clone()),
        // Named as a change rather than an update: the attendees are the ones who find out, and
        // "Update event" reads like an edit to a private note.
        SendAction::UpdateCalendarEvent { title } => ("Change calendar event", title.clone()),
        SendAction::PostComment { target } => ("Post comment", target.clone()),
        SendAction::CreateDocument { title } => ("Create document", title.clone()),
        SendAction::UpdateDocument { title } => ("Change document", title.clone()),
        SendAction::ChangeIssueStatus { target } => ("Change issue status", target.clone()),
        SendAction::ShareWritingStyle => ("Share your writing style", WRITING_STYLE_DESTINATION.to_string()),
        SendAction::EnableDictionaryAutoAdd => ("Auto-add corrected words", "this Mac".to_string()),
        SendAction::ResumeCapture { .. } => ("Resume capture", CAPTURE_DESTINATION.to_string()),
        SendAction::UnpauseCapture { .. } => ("End capture pause", CAPTURE_DESTINATION.to_string()),
        SendAction::UseSubscriptionAgent { delegate } => ("Use your AI subscription", delegate.clone()),
        SendAction::WidenVisualRecall { change } => (
            if change.enable { "Turn on Visual recall" } else { "Keep saved screens longer" },
            CAPTURE_DESTINATION.to_string(),
        ),
        SendAction::ArchiveMailThread { thread_key, unarchive } => (
            if *unarchive { MAIL_UNARCHIVE_OP } else { MAIL_ARCHIVE_OP },
            thread_key.clone(),
        ),
    }
}

/// The op type of a queued Gmail archive request (issue #779).
pub const MAIL_ARCHIVE_OP: &str = "Archive in Gmail";
/// The op type of a queued request to put a Gmail thread back in the Inbox (issue #779).
pub const MAIL_UNARCHIVE_OP: &str = "Move back to Inbox";

/// The approval request for an MCP / REST / CLI `mail.triage.archive` or
/// `mail.triage.unarchive` (issue #779): the Gmail change the user's tap would allow, on which
/// thread, and how to take it back. The destination is the triage thread key; the body never
/// carries the thread's subject or sender, because the queue row is persisted (the desktop's
/// approval card looks the subject up from the memory store when it draws the card). The route
/// is the Gmail connection's (third party), so the card shows the key that will execute it.
pub fn mail_archive_request(thread_key: &str, unarchive: bool) -> (SendAction, Preview) {
    let what = if unarchive {
        "Move this Gmail thread back to your Inbox. ShogunAI adds it back to the Inbox in Gmail."
    } else {
        "Archive this Gmail thread. ShogunAI removes it from your Inbox in Gmail; nothing is deleted."
    };
    let body = format!(
        "{what}\n\nThread: {thread_key}\n\nAn app connected to ShogunAI asked for this. After you \
         approve, it runs at the next mail sync, and you can undo it from the Mail history for 7 \
         days."
    );
    let action = SendAction::ArchiveMailThread {
        thread_key: thread_key.to_string(),
        unarchive,
    };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

fn days_label(days: u32) -> String {
    if days == 1 {
        "1 day".to_string()
    } else {
        format!("{days} days")
    }
}

/// The approval request for a Visual recall change that keeps more of the screen (issue #782):
/// a `visual_recall.set_enabled` that turns it on, or a `visual_recall.set_retention` that keeps
/// saved screens longer. The full text says what confirming starts, how long frames are kept,
/// and that nothing leaves this Mac (FR-AG-03: the whole change, not a summary). Local: no route
/// out, no credential.
pub fn visual_recall_request(change: VisualRecallWidening) -> (SendAction, Preview) {
    let mut parts = Vec::new();
    if change.enable {
        parts.push(
            "Turn on Visual recall. ShogunAI will start saving images of the focused window and \
             reading their text, so you can search what was on your screen. Saved screens stay \
             encrypted on this Mac; nothing is uploaded."
                .to_string(),
        );
    }
    match change.retention_days {
        Some(days) => parts.push(format!(
            "Keep saved screens for {} instead of {}. They are deleted automatically when they \
             reach that age.",
            days_label(days),
            days_label(change.previous_retention_days),
        )),
        None => parts.push(format!(
            "Saved screens are kept for {}, then deleted automatically.",
            days_label(change.previous_retention_days),
        )),
    }
    parts.push(
        "You can shorten how long screens are kept, or turn Visual recall off, any time in \
         Settings. Turning it off deletes the screens it saved."
            .to_string(),
    );
    let body = parts.join("\n\n");
    let action = SendAction::WidenVisualRecall { change };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

/// This disclosure is versioned by the database consent gate, not by prior observation consent.
pub const DICTIONARY_AUTOADD_DISCLOSURE: &str = "Enable automatic dictionary additions. After you fix a name in a dictation, ShogunAI adds it to your voice dictionary on this Mac, with the misheard spelling as an alias. To tell names from other edits, the dictated text before and after your edits (never other text in the field or on screen) is checked by ShogunAI's AI service; nothing is stored there. Observation lasts at most 5 minutes after each verified insertion; correction records expire after 24 hours. A compact confirmation offers Undo, and an undone word is not relearned for 30 days. Turn this off in Voice settings.";

pub fn dictionary_autoadd_request() -> (SendAction, Preview) {
    let action = SendAction::EnableDictionaryAutoAdd;
    let preview = Preview::for_send(&action, DICTIONARY_AUTOADD_DISCLOSURE, Route::DirectMcp);
    (action, preview)
}

/// The approval request for running the Agent lane on a subscription delegate (issue #110): an
/// `agent.source.set` from MCP / REST / CLI that selects `delegate_id` (`label` and `plan` name it
/// on the card). The full text carries the same three-point disclosure the Settings card shows
/// before its "use my plan" button (FR-AG-03: the whole disclosure, not a summary), because
/// confirming it is the user's opt-in. Nothing is sent at confirm time; later prompts travel under
/// the user's own plan.
pub fn subscription_agent_request(delegate_id: &str, label: &str, plan: &str) -> (SendAction, Preview) {
    let body = format!(
        "Run chat and drafts on your {plan} plan through {label} on this Mac, instead of an API key.\n\n\
         Before using your plan:\n\
         - Your prompt leaves this device through that assistant, on your own plan.\n\
         - A consumer plan's data handling is set by that vendor, not by ShogunAI.\n\
         - ShogunAI never reads or stores that account's sign-in. It stays with the tool you installed.\n\n\
         Approving records your consent. Stop using your plan, or switch back to an API key, any time in Settings."
    );
    let action = SendAction::UseSubscriptionAgent { delegate: delegate_id.to_string() };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

/// Where a capture-exclusion change applies (issue #657): the exclusion lists on this Mac.
pub const CAPTURE_DESTINATION: &str = "this Mac";

/// The approval request for a `capture_exclusions.set` that removes exclusions (issue #657). The
/// full text names every app and website that would start being captured, then the complete lists
/// the user's exclusions become (FR-AG-03: the whole change, not a summary). Local: no route out,
/// no credential.
pub fn capture_resume_request(change: CaptureExclusionChange) -> (SendAction, Preview) {
    let list = |items: &[String]| {
        if items.is_empty() {
            "(none)".to_string()
        } else {
            items.join(", ")
        }
    };
    let body = format!(
        "Stop excluding these from capture. ShogunAI will start reading their on-screen text again.\n\
         Apps: {}\n\
         Websites: {}\n\n\
         Your exclusion lists become:\n\
         Apps: {}\n\
         Websites: {}\n\n\
         You can exclude them again any time in Settings.",
        list(&change.resumed_apps),
        list(&change.resumed_websites),
        list(&change.apps),
        list(&change.websites),
    );
    let action = SendAction::ResumeCapture { change };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

/// The full text an approval to end the capture pause shows (issue #897).
pub const CAPTURE_UNPAUSE_DISCLOSURE: &str = "End your capture pause. ShogunAI will start reading the \
text of the window you are working in again, as it did before you paused. Excluded apps and \
websites stay excluded. An app connected to ShogunAI asked for this. You can pause again any time \
from the notch or Settings.";

/// The approval request for a `capture.set_paused` that would end the user's pause (issue #897).
/// `pause` is the pause in force now; confirming ends only that one. Local: no route out, no
/// credential.
pub fn capture_unpause_request(pause: PausedCapture) -> (SendAction, Preview) {
    let action = SendAction::UnpauseCapture { pause };
    let preview = Preview::for_send(&action, CAPTURE_UNPAUSE_DISCLOSURE, Route::DirectMcp);
    (action, preview)
}

/// Where a shared writing example goes (issue #654): the provider the user selected for rewrites.
pub const WRITING_STYLE_DESTINATION: &str = "your model provider";

/// The full text a writing-style approval shows (FR-AG-03: the whole disclosure, not a summary).
/// It says what confirming starts, and how to stop it.
pub const WRITING_STYLE_DISCLOSURE: &str = "Turn on writing-style sharing. From now on, each Option \
rewrite and Scribe text edit sends up to four of your own examples from the same app to your model \
provider, so drafts sound like you. Examples are still learned and kept only on this Mac either \
way. Turn sharing off or clear what was learned any time in Settings > Privacy & Security.";

/// The approval request for turning writing-style sharing on (issue #654). Direct to the
/// provider the user already chose; never a third party.
pub fn writing_style_request() -> (SendAction, Preview) {
    let action = SendAction::ShareWritingStyle;
    let preview = Preview::for_send(&action, WRITING_STYLE_DISCLOSURE, Route::DirectMcp);
    (action, preview)
}

/// A handle for a pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApprovalId(pub u64);

/// Why an approval was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCause {
    /// The user explicitly rejected.
    UserRejected,
    /// The 10-minute window elapsed (FR-AG-04).
    TimedOut,
    /// The requester cancelled it.
    Cancelled,
}

/// The state of an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Confirmed,
    Rejected(RejectCause),
}

/// Durable, body-free outcome exposed to every API face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Rejected,
    TimedOut,
    Sent,
    SendFailed,
    DraftSaved,
    /// Crash/restart interrupted an in-flight send. Delivery is unknown — do not resubmit
    /// from this status alone (the destination may already have the message).
    OutcomeUnknown,
    /// A human approved it on a remote surface (ShogunAgent E15: the gateway's approval page, its
    /// Slack app, or the operator's CLI). Confirmed with the dedicated button's authority, and
    /// left for the executor that owns the action; nothing has been sent by the approval itself.
    Approved,
}

/// Body-free terminal ledger record. Previews remain only while an item is pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRecord {
    pub id: ApprovalId,
    pub status: ApprovalStatus,
    pub resolved_ms: u64,
    /// Machine reason for a failed send. Allowlisted; never a provider body (#399).
    pub reason: Option<String>,
}

/// How the user attempted to confirm. Only [`ConfirmIntent::DedicatedButton`] confirms; the Enter
/// key alone must not (FR-AG-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmIntent {
    DedicatedButton,
    EnterKey,
}

/// A confirmed send, ready to execute. Carries the action *and* its [`Preview`] so the executor
/// has the egress details it needs to record traceability (route, destination, and the full body
/// to digest) without re-deriving them — the trace and the send can never disagree (invariant 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedSend {
    pub action: SendAction,
    pub preview: Preview,
}

/// The result of a confirm/reject/poll interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
// ponytail: a Decision lives for one confirm call; boxing ConfirmedSend would touch every match.
#[allow(clippy::large_enum_variant)]
pub enum Decision {
    /// Confirmed and removed from the queue — the caller may now execute the send. Carries the
    /// action and its preview (see [`ConfirmedSend`]).
    Confirmed(ConfirmedSend),
    /// Rejected (with cause) and removed.
    Rejected(RejectCause),
    /// Still awaiting confirmation.
    StillPending,
    /// The Enter key alone was used — no state change; the dedicated button is required.
    RequiresDedicatedButton,
    /// No pending request with that id.
    Unknown,
}

/// The 10-minute L3 approval timeout (FR-AG-04).
pub const APPROVAL_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// Why an approved action did not go out, as a fixed machine class: the only failure reasons an
/// executed approval records (on the queue, in the principal's ledger and in the audit log).
/// Never a provider message, a recipient or a body.
pub const EXECUTION_FAILURE_CLASSES: &[&str] = &[
    // The principal has no usable credential for the action's service (never connected,
    // disconnected, or the stored grant expired).
    "not_connected",
    // The service gate refused the write (plan, wave, consent).
    "not_authorized",
    // The connection changed between the decision and the write.
    "connection_changed",
    // The traceability row could not be written, so nothing was sent.
    "trace_unavailable",
    // The action names a target no provider call can use.
    "invalid_target",
    // The action is a local setting the desktop applies; the sidecar has no transport for it.
    "not_executable",
    // The provider was reached and refused or failed.
    "provider_error",
    // The provider did not answer in time; delivery may still have happened.
    "timeout",
    // The principal's store could not record the decision, so nothing was run.
    "ledger_unavailable",
];

/// A pending L3 request.
#[derive(Debug, Clone)]
struct PendingApproval {
    id: ApprovalId,
    action: SendAction,
    preview: Preview,
    origin: ApprovalOrigin,
    created_ms: u64,
}

/// Persistable pending approval, used only by the local protected L3 store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRecord {
    pub id: ApprovalId,
    pub action: SendAction,
    pub preview: Preview,
    pub origin: ApprovalOrigin,
    pub created_ms: u64,
}

/// The L3 approval queue. Holds pending sends until confirmed, rejected, or timed out.
#[derive(Default)]
pub struct ApprovalQueue {
    pending: Vec<PendingApproval>,
    terminal: Vec<TerminalRecord>,
    in_flight: Vec<ApprovalId>,
    next_id: u64,
}

pub const TERMINAL_RETENTION: usize = 256;

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            terminal: Vec::new(),
            in_flight: Vec::new(),
            next_id: 1,
        }
    }

    /// Start a restored queue at a nonzero id; exhausted ids are never reused.
    pub fn with_next_id(next_id: u64) -> Self {
        Self {
            pending: Vec::new(),
            terminal: Vec::new(),
            in_flight: Vec::new(),
            next_id: next_id.max(1),
        }
    }

    pub fn export(&self) -> (u64, Vec<PendingRecord>) {
        (
            self.next_id,
            self.pending
                .iter()
                .map(|p| PendingRecord {
                    id: p.id,
                    action: p.action.clone(),
                    preview: p.preview.clone(),
                    origin: p.origin,
                    created_ms: p.created_ms,
                })
                .collect(),
        )
    }

    pub fn import_with_terminal(
        next_id: u64,
        records: Vec<PendingRecord>,
        terminal: Vec<TerminalRecord>,
        in_flight: Vec<ApprovalId>,
    ) -> Self {
        let mut queue = Self::with_next_id(next_id);
        queue.pending = records
            .into_iter()
            .map(|r| PendingApproval {
                id: r.id,
                action: r.action,
                preview: r.preview,
                origin: r.origin,
                created_ms: r.created_ms,
            })
            .collect();
        queue.terminal = terminal;
        queue.in_flight = in_flight;
        for id in queue
            .pending
            .iter()
            .map(|p| p.id)
            .chain(queue.terminal.iter().map(|r| r.id))
            .chain(queue.in_flight.iter().copied())
        {
            if id.0 >= queue.next_id {
                queue.next_id = id.0.saturating_add(1);
            }
        }
        queue.trim_terminal();
        queue
    }

    pub fn terminal_records(&self) -> &[TerminalRecord] {
        &self.terminal
    }
    pub fn in_flight_ids(&self) -> &[ApprovalId] {
        &self.in_flight
    }

    fn alloc_id(&mut self) -> Result<ApprovalId, &'static str> {
        if self.next_id == u64::MAX {
            return Err("approval id space exhausted");
        }
        let id = ApprovalId(self.next_id);
        self.next_id += 1;
        Ok(id)
    }

    /// Enqueue a send for L3 approval. Returns the handle; the request starts [`ApprovalState::Pending`]
    /// (an AI-API caller holds this pending result until resolved — FR-AG-04). A send that could
    /// never run as addressed is refused here, for every producer
    /// ([`crate::permission::send_target_refusal`]).
    pub fn try_request(
        &mut self,
        action: SendAction,
        preview: Preview,
        origin: ApprovalOrigin,
        now_ms: u64,
    ) -> Result<ApprovalId, &'static str> {
        if let Some(reason) = crate::permission::send_target_refusal(&action) {
            return Err(reason);
        }
        let id = self.alloc_id()?;
        self.pending.push(PendingApproval {
            id,
            action,
            preview,
            origin,
            created_ms: now_ms,
        });
        Ok(id)
    }

    /// Attach source metadata without changing the approved action or body.
    pub fn set_reply_thread(&mut self, id: ApprovalId, thread: ReplyThread) {
        if let Some(pending) = self.pending.iter_mut().find(|p| p.id == id) {
            pending.preview.reply_thread = Some(thread);
        }
    }

    fn position(&self, id: ApprovalId) -> Option<usize> {
        self.pending.iter().position(|p| p.id == id)
    }

    fn is_expired(p: &PendingApproval, now_ms: u64) -> bool {
        now_ms.saturating_sub(p.created_ms) > APPROVAL_TIMEOUT_MS
    }

    /// Attempt to confirm. Only the dedicated button confirms (FR-AG-03); an expired request is
    /// rejected as timed-out instead of running.
    pub fn confirm(&mut self, id: ApprovalId, intent: ConfirmIntent, now_ms: u64) -> Decision {
        if intent != ConfirmIntent::DedicatedButton {
            // Enter-alone (or any non-button intent) never confirms; the request stays pending.
            return match self.position(id) {
                Some(_) => Decision::RequiresDedicatedButton,
                None => Decision::Unknown,
            };
        }
        let Some(idx) = self.position(id) else {
            return Decision::Unknown;
        };
        if Self::is_expired(&self.pending[idx], now_ms) {
            self.pending.remove(idx);
            self.record_terminal(id, ApprovalStatus::TimedOut, now_ms, None);
            return Decision::Rejected(RejectCause::TimedOut);
        }
        let p = self.pending.remove(idx);
        self.in_flight.push(id);
        Decision::Confirmed(ConfirmedSend {
            action: p.action,
            preview: p.preview,
        })
    }

    /// Explicitly reject a pending request.
    pub fn reject(&mut self, id: ApprovalId, cause: RejectCause, now_ms: u64) -> Decision {
        match self.position(id) {
            Some(idx) => {
                self.pending.remove(idx);
                self.record_terminal(
                    id,
                    if cause == RejectCause::TimedOut {
                        ApprovalStatus::TimedOut
                    } else {
                        ApprovalStatus::Rejected
                    },
                    now_ms,
                    None,
                );
                Decision::Rejected(cause)
            }
            None => Decision::Unknown,
        }
    }

    /// Poll a request's current state (for an AI-API caller awaiting resolution).
    pub fn poll(&self, id: ApprovalId) -> Decision {
        match self.position(id) {
            Some(_) => Decision::StillPending,
            None => Decision::Unknown,
        }
    }

    pub fn status(&self, id: ApprovalId) -> Option<ApprovalStatus> {
        if self.position(id).is_some() || self.in_flight.contains(&id) {
            return Some(ApprovalStatus::Pending);
        }
        self.terminal
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.status)
    }

    /// Allowlisted failure reason for API/CLI parity (`actions.status`). Never a provider body.
    pub fn failure_reason(&self, id: ApprovalId) -> Option<&str> {
        self.terminal
            .iter()
            .find(|record| record.id == id)
            .and_then(|record| record.reason.as_deref())
    }

    /// Record a provider-stopped draft without creating a pending or confirmed send.
    /// The returned id can be polled through actions.status; it can never be approved.
    pub fn record_policy_stop(&mut self, now_ms: u64) -> Result<ApprovalId, &'static str> {
        let id = self.alloc_id()?;
        self.record_terminal(
            id,
            ApprovalStatus::SendFailed,
            now_ms,
            Some("provider_policy_stop"),
        );
        Ok(id)
    }

    /// Record outcome only after a dedicated confirmation moved an item in flight.
    pub fn mark_status(
        &mut self,
        id: ApprovalId,
        status: ApprovalStatus,
        resolved_ms: u64,
    ) -> bool {
        self.mark_status_with_reason(id, status, resolved_ms, None)
    }

    /// Same as [`Self::mark_status`], with an optional machine reason (`provider_policy_stop`).
    pub fn mark_status_with_reason(
        &mut self,
        id: ApprovalId,
        status: ApprovalStatus,
        resolved_ms: u64,
        reason: Option<&str>,
    ) -> bool {
        if status == ApprovalStatus::Pending || !self.in_flight.contains(&id) {
            return false;
        }
        self.in_flight.retain(|candidate| *candidate != id);
        self.record_terminal(id, status, resolved_ms, reason);
        true
    }

    /// Settle a remotely [`ApprovalStatus::Approved`] item once its executor has run it (ShogunAgent
    /// E15 follow-up): `Approved` → `Sent` / `SendFailed` / `OutcomeUnknown`, at most once. Any
    /// other current status (pending, rejected, timed out, already settled) is left alone and
    /// answers `false`. `reason` is kept only when it is an allowlisted machine class
    /// ([`EXECUTION_FAILURE_CLASSES`]).
    pub fn settle_approved(
        &mut self,
        id: ApprovalId,
        status: ApprovalStatus,
        resolved_ms: u64,
        reason: Option<&str>,
    ) -> bool {
        if !matches!(status, ApprovalStatus::Sent | ApprovalStatus::SendFailed | ApprovalStatus::OutcomeUnknown) {
            return false;
        }
        let approved = self
            .terminal
            .iter()
            .any(|record| record.id == id && record.status == ApprovalStatus::Approved);
        if !approved {
            return false;
        }
        self.record_terminal(id, status, resolved_ms, reason);
        true
    }

    /// Resolve crash-recovered in-flight items conservatively; never claim a send succeeded
    /// and never label unknown delivery as [`ApprovalStatus::SendFailed`] (that invites a resend).
    pub fn recover_in_flight(&mut self, now_ms: u64) -> Vec<ApprovalId> {
        let ids = std::mem::take(&mut self.in_flight);
        for id in &ids {
            self.record_terminal(*id, ApprovalStatus::OutcomeUnknown, now_ms, None);
        }
        ids
    }

    /// Reject every request past the timeout (FR-AG-04). Returns the timed-out ids. The daemon
    /// calls this on a timer tick.
    pub fn expire_due(&mut self, now_ms: u64) -> Vec<ApprovalId> {
        let (expired, live): (Vec<PendingApproval>, Vec<PendingApproval>) = self
            .pending
            .drain(..)
            .partition(|p| Self::is_expired(p, now_ms));
        self.pending = live;
        let ids: Vec<_> = expired.iter().map(|p| p.id).collect();
        for id in &ids {
            self.record_terminal(*id, ApprovalStatus::TimedOut, now_ms, None);
        }
        ids
    }

    /// The preview for a pending request (what the confirm UI renders).
    pub fn preview(&self, id: ApprovalId) -> Option<&Preview> {
        self.pending.iter().find(|p| p.id == id).map(|p| &p.preview)
    }

    /// The origin of a pending request (which surface enqueued it — shown in the list UI).
    pub fn origin(&self, id: ApprovalId) -> Option<ApprovalOrigin> {
        self.pending.iter().find(|p| p.id == id).map(|p| p.origin)
    }

    /// The action of a pending request (read-only; confirmation still goes through
    /// [`Self::confirm`]). Lets approval-time hooks derive per-action metadata (e.g. the L5
    /// feedback scope) without dequeuing.
    pub fn action(&self, id: ApprovalId) -> Option<&SendAction> {
        self.pending.iter().find(|p| p.id == id).map(|p| &p.action)
    }

    /// Deadline displayed by the approval card; confirmation still enforces it.
    pub fn expires_at_ms(&self, id: ApprovalId) -> Option<u64> {
        self.pending.iter().find(|p| p.id == id)
            .map(|p| p.created_ms.saturating_add(APPROVAL_TIMEOUT_MS))
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// The ids of all pending approvals, oldest first — for a UI that lists the L3 confirm queue
    /// (each id resolves to its [`Preview`] via [`Self::preview`]).
    pub fn pending_ids(&self) -> Vec<ApprovalId> {
        self.pending.iter().map(|p| p.id).collect()
    }

    fn record_terminal(
        &mut self,
        id: ApprovalId,
        status: ApprovalStatus,
        resolved_ms: u64,
        reason: Option<&str>,
    ) {
        self.terminal.retain(|record| record.id != id);
        self.terminal.push(TerminalRecord {
            id,
            status,
            resolved_ms,
            reason: match reason {
                Some("provider_policy_stop") => Some("provider_policy_stop".to_string()),
                Some(class) if EXECUTION_FAILURE_CLASSES.contains(&class) => Some(class.to_string()),
                _ => None,
            },
        });
        self.trim_terminal();
    }

    fn trim_terminal(&mut self) {
        if self.terminal.len() > TERMINAL_RETENTION {
            self.terminal
                .drain(..self.terminal.len() - TERMINAL_RETENTION);
        }
    }
}

/// Test-only infallible enqueue. Production paths use [`ApprovalQueue::try_request`]: a panic on
/// id exhaustion would poison the shared queue mutex (`unwrap`/`expect` are test-only anyway).
#[cfg(test)]
impl ApprovalQueue {
    pub fn request(
        &mut self,
        action: SendAction,
        preview: Preview,
        origin: ApprovalOrigin,
        now_ms: u64,
    ) -> ApprovalId {
        self.try_request(action, preview, origin, now_ms)
            .expect("approval id space exhausted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> SendAction {
        SendAction::SendEmail {
            to: "alice@example.com".into(),
        }
    }

    #[test]
    fn reply_marker_requires_an_explicit_override_only_when_the_thread_changed() {
        let marker = ReplyThread { thread_id: "t1".into(), latest_message_id: "m1".into() };
        assert!(!marker.changed(Ok("m1"), false));
        assert!(marker.changed(Ok("m2"), false));
        assert!(!marker.changed(Ok("m2"), true));
        assert!(!marker.changed(Err("offline"), false));
    }

    fn preview() -> Preview {
        Preview::for_send(&email(), "Hi Alice, shipping Friday.", Route::ViaComposio)
    }

    #[test]
    fn an_approved_item_settles_once_and_keeps_only_allowlisted_reasons() {
        let mut queue = ApprovalQueue::new();
        let id = queue.request(email(), preview(), ApprovalOrigin::Api, 0);
        // Pending: nothing to settle.
        assert!(!queue.settle_approved(id, ApprovalStatus::Sent, 1, None));
        assert!(matches!(queue.confirm(id, ConfirmIntent::DedicatedButton, 1), Decision::Confirmed(_)));
        assert!(queue.mark_status(id, ApprovalStatus::Approved, 1));
        assert!(!queue.settle_approved(id, ApprovalStatus::Approved, 2, None), "only an outcome settles");
        assert!(queue.settle_approved(id, ApprovalStatus::SendFailed, 2, Some("not_connected")));
        assert_eq!(queue.status(id), Some(ApprovalStatus::SendFailed));
        assert_eq!(queue.failure_reason(id), Some("not_connected"));
        assert!(!queue.settle_approved(id, ApprovalStatus::Sent, 3, None), "settled once");
        // A rejected item never settles as sent.
        let rejected = queue.request(email(), preview(), ApprovalOrigin::Api, 0);
        queue.reject(rejected, RejectCause::UserRejected, 1);
        assert!(!queue.settle_approved(rejected, ApprovalStatus::Sent, 2, None));
        // A free-text reason is dropped.
        let other = queue.request(email(), preview(), ApprovalOrigin::Api, 0);
        assert!(matches!(queue.confirm(other, ConfirmIntent::DedicatedButton, 1), Decision::Confirmed(_)));
        queue.mark_status(other, ApprovalStatus::Approved, 1);
        assert!(queue.settle_approved(other, ApprovalStatus::SendFailed, 2, Some("smtp said: mailbox full")));
        assert_eq!(queue.failure_reason(other), None);
    }

    #[test]
    fn assistant_origin_and_expiry_are_exposed() {
        assert_eq!(ApprovalOrigin::parse(ApprovalOrigin::Assistant.as_str()), Some(ApprovalOrigin::Assistant));
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Assistant, 123);
        assert_eq!(q.expires_at_ms(id), Some(123 + APPROVAL_TIMEOUT_MS));
        assert_eq!(q.origin(id), Some(ApprovalOrigin::Assistant));
    }

    #[test]
    fn pending_ids_lists_requests_oldest_first() {
        let mut q = ApprovalQueue::new();
        let a = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        let b = q.request(email(), preview(), ApprovalOrigin::Ui, 1);
        assert_eq!(q.pending_ids(), vec![a, b]);
        q.confirm(a, ConfirmIntent::DedicatedButton, 2);
        assert_eq!(q.pending_ids(), vec![b]);
    }

    #[test]
    fn preview_is_derived_from_the_action_and_is_full_text() {
        let p = Preview::for_send(&email(), "full body here", Route::ViaComposio);
        assert_eq!(p.op_type, "Send email");
        assert_eq!(p.destination, "alice@example.com");
        assert_eq!(p.full_body, "full body here"); // not summarized
        assert_eq!(p.route, Route::ViaComposio);
        // #273: the key kind follows the route — a Composio send is executed by the Composio key.
        assert_eq!(p.key_kind, KeyKind::Composio);
        assert_eq!(
            Preview::for_send(&email(), "b", Route::DirectMcp).key_kind,
            KeyKind::OauthUser
        );
    }

    #[test]
    fn writing_style_preview_names_the_provider_and_the_ai_credentials() {
        let (action, p) = writing_style_request();
        assert_eq!(action, SendAction::ShareWritingStyle);
        assert_eq!(p.op_type, "Share your writing style");
        assert_eq!(p.destination, WRITING_STYLE_DESTINATION);
        assert_eq!(p.full_body, WRITING_STYLE_DISCLOSURE);
        assert!(p.full_body.contains("up to four"));
        // Never labelled as a connected service account or a third party.
        assert_eq!(p.route, Route::DirectMcp);
        assert_eq!(p.key_kind, KeyKind::AiProvider);
        // And it rides the one L3 queue like any other send.
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Mcp, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
    }

    #[test]
    fn subscription_agent_preview_carries_the_full_disclosure_and_the_ai_credential() {
        let (action, p) = subscription_agent_request("claude-code", "Claude Code", "Claude Pro / Max");
        assert_eq!(action, SendAction::UseSubscriptionAgent { delegate: "claude-code".into() });
        assert_eq!(action.kind(), "use_subscription_agent");
        assert_eq!(p.op_type, "Use your AI subscription");
        assert_eq!(p.destination, "claude-code");
        // The same three points the Settings card discloses before its opt-in button.
        for line in [
            "Your prompt leaves this device through that assistant, on your own plan.",
            "A consumer plan's data handling is set by that vendor, not by ShogunAI.",
            "ShogunAI never reads or stores that account's sign-in.",
            "Claude Pro / Max plan through Claude Code",
        ] {
            assert!(p.full_body.contains(line), "{}", p.full_body);
        }
        assert_eq!(p.key_kind, KeyKind::AiProvider);
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Mcp, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.confirm(id, ConfirmIntent::EnterKey, 1), Decision::RequiresDedicatedButton);
    }

    #[test]
    fn mail_archive_previews_name_the_thread_and_the_gmail_change_and_wait_for_the_button() {
        // Issue #779: an API archive request is shown as the Gmail change it is, on the Gmail
        // connection's key, and only the dedicated button confirms it.
        let (action, p) = mail_archive_request("gmail:18f3", false);
        assert_eq!(action, SendAction::ArchiveMailThread { thread_key: "gmail:18f3".into(), unarchive: false });
        assert_eq!(p.op_type, MAIL_ARCHIVE_OP);
        assert_eq!(p.destination, "gmail:18f3");
        assert!(p.full_body.contains("Thread: gmail:18f3"), "{}", p.full_body);
        assert!(p.full_body.contains("nothing is deleted"), "{}", p.full_body);
        assert_eq!(p.route, Route::DirectMcp);
        assert_eq!(p.key_kind, KeyKind::OauthUser);

        let (action, p) = mail_archive_request("gmail:18f3", true);
        assert_eq!(p.op_type, MAIL_UNARCHIVE_OP);
        assert!(p.full_body.contains("back to your Inbox"), "{}", p.full_body);
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Mcp, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.confirm(id, ConfirmIntent::EnterKey, 1), Decision::RequiresDedicatedButton);
        assert!(matches!(q.confirm(id, ConfirmIntent::DedicatedButton, 2), Decision::Confirmed(_)));
    }

    #[test]
    fn visual_recall_previews_say_what_widens_and_are_local() {
        let enable = VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 3 };
        let (action, p) = visual_recall_request(enable.clone());
        assert_eq!(action, SendAction::WidenVisualRecall { change: enable });
        assert_eq!(p.op_type, "Turn on Visual recall");
        assert_eq!(p.destination, CAPTURE_DESTINATION);
        assert!(p.full_body.contains("Turn on Visual recall."), "{}", p.full_body);
        assert!(p.full_body.contains("kept for 3 days"), "{}", p.full_body);
        assert!(p.full_body.contains("nothing is uploaded"), "{}", p.full_body);
        assert_eq!(p.key_kind, KeyKind::Local);
        assert_eq!(p.route, Route::DirectMcp);

        let longer = VisualRecallWidening { enable: false, retention_days: Some(3650), previous_retention_days: 1 };
        let (action, p) = visual_recall_request(longer);
        assert_eq!(p.op_type, "Keep saved screens longer");
        assert!(p.full_body.contains("for 3650 days instead of 1 day."), "{}", p.full_body);
        assert!(!p.full_body.contains("Turn on Visual recall."), "{}", p.full_body);
        assert_eq!(p.key_kind, KeyKind::Local);
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Api, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.confirm(id, ConfirmIntent::EnterKey, 1), Decision::RequiresDedicatedButton);
    }

    #[test]
    fn capture_unpause_preview_says_what_resumes_and_is_local() {
        let pause = PausedCapture { resume_at_ms: Some(60_000) };
        let (action, p) = capture_unpause_request(pause);
        assert_eq!(action, SendAction::UnpauseCapture { pause });
        assert_eq!(action.kind(), "unpause_capture");
        assert_eq!(p.op_type, "End capture pause");
        assert_eq!(p.destination, CAPTURE_DESTINATION);
        assert!(p.full_body.contains("start reading"), "{}", p.full_body);
        assert!(p.full_body.contains("stay excluded"), "{}", p.full_body);
        assert_eq!(p.key_kind, KeyKind::Local);
        assert_eq!(p.route, Route::DirectMcp);
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Api, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.confirm(id, ConfirmIntent::EnterKey, 1), Decision::RequiresDedicatedButton);
    }

    #[test]
    fn capture_resume_preview_lists_what_resumes_and_is_local() {
        let change = CaptureExclusionChange {
            apps: vec!["com.example.Keep".into()],
            websites: vec!["new.example".into()],
            resumed_apps: vec!["com.example.Secret".into()],
            resumed_websites: vec!["private.example".into()],
        };
        let (action, p) = capture_resume_request(change.clone());
        assert_eq!(action, SendAction::ResumeCapture { change });
        assert_eq!(p.op_type, "Resume capture");
        assert_eq!(p.destination, CAPTURE_DESTINATION);
        assert!(p.full_body.contains("Apps: com.example.Secret\n"), "{}", p.full_body);
        assert!(p.full_body.contains("Websites: private.example\n"), "{}", p.full_body);
        assert!(p.full_body.contains("Apps: com.example.Keep\n"), "{}", p.full_body);
        assert!(p.full_body.contains("Websites: new.example\n"), "{}", p.full_body);
        // Never labelled as a connected account, a third party, or an AI credential.
        assert_eq!(p.key_kind, KeyKind::Local);
        assert_eq!(p.route, Route::DirectMcp);
        let mut q = ApprovalQueue::new();
        let id = q.request(action, p, ApprovalOrigin::Api, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
    }

    #[test]
    fn request_starts_pending() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Api, 0);
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.pending_len(), 1);
        assert_eq!(q.origin(id), Some(ApprovalOrigin::Api));
    }

    #[test]
    fn enter_key_alone_does_not_confirm() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        // Enter → refused, still pending.
        assert_eq!(
            q.confirm(id, ConfirmIntent::EnterKey, 1000),
            Decision::RequiresDedicatedButton
        );
        assert_eq!(q.poll(id), Decision::StillPending);
        assert_eq!(q.pending_len(), 1);
    }

    #[test]
    fn dedicated_button_confirms_and_yields_the_send() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        let decision = q.confirm(id, ConfirmIntent::DedicatedButton, 1000);
        // the confirmed decision carries both the action and its preview (for traceability)
        assert_eq!(
            decision,
            Decision::Confirmed(ConfirmedSend {
                action: email(),
                preview: preview()
            })
        );
        // removed from the queue after resolution
        assert_eq!(q.pending_len(), 0);
        assert_eq!(q.poll(id), Decision::Unknown);
    }

    #[test]
    fn confirm_after_timeout_is_rejected_not_run() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Api, 0);
        // 10min + 1ms later
        let decision = q.confirm(id, ConfirmIntent::DedicatedButton, APPROVAL_TIMEOUT_MS + 1);
        assert_eq!(decision, Decision::Rejected(RejectCause::TimedOut));
        assert_eq!(q.pending_len(), 0);
    }

    #[test]
    fn expire_due_rejects_only_stale_requests() {
        let mut q = ApprovalQueue::new();
        let a = q.request(email(), preview(), ApprovalOrigin::Api, 0);
        let b = q.request(email(), preview(), ApprovalOrigin::Ui, APPROVAL_TIMEOUT_MS); // newer
                                                                                        // at now = TIMEOUT+1: a is stale, b is fresh
        let expired = q.expire_due(APPROVAL_TIMEOUT_MS + 1);
        assert_eq!(expired, vec![a]);
        assert_eq!(q.pending_len(), 1);
        // b still confirmable
        assert!(matches!(
            q.confirm(b, ConfirmIntent::DedicatedButton, APPROVAL_TIMEOUT_MS + 2),
            Decision::Confirmed(ConfirmedSend { action, .. }) if action == email()
        ));
    }

    #[test]
    fn user_reject_removes_the_request() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        assert_eq!(
            q.reject(id, RejectCause::UserRejected, 1_234),
            Decision::Rejected(RejectCause::UserRejected)
        );
        assert_eq!(q.pending_len(), 0);
        // The terminal ledger records when the rejection happened, not epoch 0.
        let record = q.terminal_records().iter().find(|r| r.id == id).unwrap();
        assert_eq!(record.status, ApprovalStatus::Rejected);
        assert_eq!(record.resolved_ms, 1_234);
    }

    #[test]
    fn confirm_or_poll_unknown_id_is_unknown() {
        let mut q = ApprovalQueue::new();
        assert_eq!(
            q.confirm(ApprovalId(42), ConfirmIntent::DedicatedButton, 0),
            Decision::Unknown
        );
        assert_eq!(q.poll(ApprovalId(42)), Decision::Unknown);
    }

    #[test]
    fn preview_is_available_for_the_confirm_ui() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        let p = q.preview(id).unwrap();
        assert_eq!(p.destination, "alice@example.com");
        assert_eq!(p.route, Route::ViaComposio);
    }

    // Structural note: `request` takes a `SendAction`, so only L3 sends can enter the approval
    // queue. There is no constructor that accepts a `LocalAction`/L1/L2 action — a non-send simply
    // cannot be enqueued here, which is the type-level half of FR-AG-02 (no dynamic downgrade).
    #[test]
    fn every_send_variant_can_be_previewed() {
        for a in [
            SendAction::SendEmail {
                to: "a@b.com".into(),
            },
            SendAction::PostMessage {
                channel: "#eng".into(),
            },
            SendAction::CreateCalendarEvent {
                title: "Sync".into(),
            },
            SendAction::PostComment {
                target: "org/repo#1".into(),
            },
        ] {
            let p = Preview::for_send(&a, "body", Route::DirectMcp);
            assert!(!p.op_type.is_empty());
            assert!(!p.destination.is_empty());
        }
    }

    #[test]
    fn recover_in_flight_marks_outcome_unknown_not_send_failed() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        assert!(matches!(
            q.confirm(id, ConfirmIntent::DedicatedButton, 1),
            Decision::Confirmed(_)
        ));
        assert_eq!(q.status(id), Some(ApprovalStatus::Pending));
        assert_eq!(q.recover_in_flight(2), vec![id]);
        assert_eq!(q.status(id), Some(ApprovalStatus::OutcomeUnknown));
        assert!(
            !q.mark_status(id, ApprovalStatus::Sent, 3),
            "a later mark must not overwrite unknown delivery"
        );
    }

    #[test]
    fn confirm_time_failure_closes_in_flight_as_send_failed() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        assert!(matches!(
            q.confirm(id, ConfirmIntent::DedicatedButton, 1),
            Decision::Confirmed(_)
        ));
        assert!(q.mark_status(id, ApprovalStatus::SendFailed, 2));
        assert_eq!(q.status(id), Some(ApprovalStatus::SendFailed));
    }

    #[test]
    fn policy_stop_reason_is_allowlisted_and_never_a_provider_body() {
        let mut q = ApprovalQueue::new();
        let id = q.request(email(), preview(), ApprovalOrigin::Ui, 0);
        assert!(matches!(
            q.confirm(id, ConfirmIntent::DedicatedButton, 1),
            Decision::Confirmed(_)
        ));
        assert!(q.mark_status_with_reason(
            id,
            ApprovalStatus::SendFailed,
            2,
            Some("provider_policy_stop"),
        ));
        assert_eq!(q.status(id), Some(ApprovalStatus::SendFailed));
        assert_eq!(q.failure_reason(id), Some("provider_policy_stop"));

        let other = q.request(email(), preview(), ApprovalOrigin::Ui, 3);
        assert!(matches!(
            q.confirm(other, ConfirmIntent::DedicatedButton, 4),
            Decision::Confirmed(_)
        ));
        assert!(q.mark_status_with_reason(
            other,
            ApprovalStatus::SendFailed,
            5,
            Some("the model said: refuse this user prompt"),
        ));
        assert_eq!(q.failure_reason(other), None);
    }
}
