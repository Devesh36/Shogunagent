use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue, Preview, Route};
use shogun_agents::permission::{Action, Level, LocalAction, SendAction};

use super::{json_escape, level_label};

enum ActionSpec {
    Local(LocalAction),
    Send(SendAction, Preview),
}

/// Parse the `actions.execute` JSON body into an action. Only string-parameterised actions are
/// expressible over the API (`SaveDraft`/`UpdateState` carry `'static` targets and are launched
/// from the UI, not the wire). Unknown / malformed bodies return `None` (→ 400).
fn parse_action(body: &str) -> Option<ActionSpec> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let kind = v.get("kind")?.as_str()?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);

    // For a send, the L3 preview shows the full content (FR-AG-03). Gmail sends directly to Google.
    let send = |action: SendAction, full: String, route: Route| {
        let mut preview = Preview::for_send(&action, full, route);
        preview.reply_thread = crate::reply_thread::parse(&action, &v).ok()?;
        Some(ActionSpec::Send(action, preview))
    };

    Some(match kind {
        "local_search" => ActionSpec::Local(LocalAction::LocalSearch {
            query: field("query")?,
        }),
        "open_app" => ActionSpec::Local(LocalAction::OpenApp {
            bundle_id: field("bundle_id")?,
        }),
        "reveal_file" => ActionSpec::Local(LocalAction::RevealFile {
            path: field("path")?,
        }),
        "show_notification" => ActionSpec::Local(LocalAction::ShowNotification {
            text: field("text")?,
        }),
        "copy_to_clipboard" => ActionSpec::Local(LocalAction::CopyToClipboard {
            text: field("text")?,
        }),
        "send_email" => {
            let to = field("to")?;
            let full = format!(
                "Subject: {}\n\n{}",
                field("subject").unwrap_or_default(),
                field("body").unwrap_or_default()
            );
            send(SendAction::SendEmail { to }, full, Route::DirectMcp)?
        }
        "post_message" => {
            let channel = field("channel")?;
            send(
                SendAction::PostMessage { channel },
                field("body").unwrap_or_default(),
                Route::DirectMcp,
            )?
        }
        "post_comment" => {
            let target = field("target")?;
            send(
                SendAction::PostComment { target },
                field("body").unwrap_or_default(),
                Route::DirectMcp,
            )?
        }
        _ => return None,
    })
}

/// Human Slack targets are resolved by the running desktop, where connector credentials live.
/// Both API faces enter here after their normal token/plan gates, before acquiring the queue.
pub fn slack_proposal(
    body: Option<&str>,
    backend: &(impl crate::backend::MemoryBackend + ?Sized),
    auth: crate::forms_api::FormsAuth,
    origin: &'static str,
) -> Option<(u16, String)> {
    let value: serde_json::Value = serde_json::from_str(body?).ok()?;
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("post_message") { return None; }
    let target = value.get("channel")?.as_str()?;
    if shogun_agents::permission::is_slack_conversation_id(target) { return None; }
    let text = match value.get("body") {
        None => "",
        Some(value) => match value.as_str() { Some(text) => text, None => return Some((400, r#"{"error":"bad_action_request"}"#.into())) },
    };
    let operation = crate::forms_api::FormOperation::ProposeSlackPost {
        target: target.into(), body: text.into(), origin: origin.into(),
    };
    if crate::forms_api::validate_operation(&operation).is_err() {
        return Some((400, r#"{"error":"bad_action_request"}"#.into()));
    }
    let answer = backend.forms(operation, auth)
        .unwrap_or_else(|_| r#"{"error":"slack_target_lookup_unavailable"}"#.into());
    // FR-API-04: an accepted proposal waits for the dedicated UI confirm (202 + approval id).
    let pending = serde_json::from_str::<serde_json::Value>(&answer)
        .ok()
        .and_then(|value| value.get("pending").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);
    let status = if pending { 202 } else { crate::forms_api::http_status(&answer) };
    Some((status, answer))
}

/// Whether this process has a surface that will ever drain the approval queue.
///
/// The desktop app has one (the Notch confirm UI); the standalone `shogun-api` / `shogun-mcp`
/// binaries do not — they build a queue at their composition root that nobody watches. Enqueuing
/// an L3 send there is not dangerous (invariant 4 still holds: nothing sends without a human),
/// but it *looks* like it worked and then expires in silence, which quietly breaks invariant 6's
/// promise that the API face behaves like the human one. Saying so is the honest outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalSurface {
    /// A confirm UI is running in this process.
    Present,
    /// Headless: refuse external sends rather than strand them.
    Absent,
}

/// Handle `actions.execute` (auth already enforced by [`route`]). A local action is authorized to
/// run (200); an external send is enqueued in the shared approval queue and returns pending +
/// approval id (202, FR-API-04) — it never runs here without a UI confirm. `origin` labels which
/// face enqueued it (REST/CLI = Api, stdio = Mcp) in the single shared queue.
/// `surface` says whether anything in this process can confirm; see [`ApprovalSurface`].
pub fn act(
    body: Option<&str>,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let Some(body) = body else {
        return (400, r#"{"error":"missing_body"}"#.to_string());
    };
    match parse_action(body) {
        None => (400, r#"{"error":"bad_action_request"}"#.to_string()),
        // A local action is answered by [`local_answer`] before the approval queue is reached
        // (issue #779). Reaching here means no backend was available to run it, so it did not
        // run: say so rather than report it executed.
        Some(ActionSpec::Local(action)) => local_unavailable(&action),
        Some(ActionSpec::Send(send, preview)) => {
            enqueue(send, preview, now_ms, approvals, origin, surface)
        }
    }
}

/// Answer an `actions.execute` body that names a local action, before the approval queue is
/// touched (issue #779). `None` for a send or an unreadable body, which take the queue path in
/// [`act`].
///
/// `local_search` runs the same search `memory.search` does and returns its results. The other
/// local kinds act on this Mac's screen (open an app, reveal a file, notify, copy), which only the
/// SHOGUN app can do: they are refused with `local_action_unavailable` and nothing runs, never
/// answered `{"executed":"local"}`. A local search is not refused with the L3
/// `desktop_unavailable` message when the app is closed.
pub fn local_answer(
    body: Option<&str>,
    backend: &(impl crate::backend::MemoryBackend + ?Sized),
) -> Option<(u16, String)> {
    use crate::backend::{
        search_request_from_wire, MemorySearchError, SearchBoundV1, SearchDepthWireV1,
        SearchSourceWireV1,
    };
    let Some(ActionSpec::Local(action)) = parse_action(body?) else {
        return None;
    };
    let LocalAction::LocalSearch { query } = &action else {
        return Some(local_unavailable(&action));
    };
    let request = match search_request_from_wire(
        query.clone(),
        SearchDepthWireV1::Absent,
        SearchBoundV1::Absent,
        SearchBoundV1::Absent,
        SearchSourceWireV1::Absent,
    ) {
        Ok(request) => request,
        Err(policy) => return Some((400, format!(r#"{{"error":"{}"}}"#, policy.as_str()))),
    };
    let level = level_label(Action::Local(action.clone()).required_level());
    Some(match backend.search(&request) {
        Ok(response) => match serde_json::to_string(&response) {
            Ok(results) => (
                200,
                format!(r#"{{"executed":"local","kind":"local_search","level":"{level}","search":{results}}}"#),
            ),
            Err(_) => (500, r#"{"error":"search_unavailable"}"#.to_string()),
        },
        Err(MemorySearchError::Policy(policy)) => {
            (400, format!(r#"{{"error":"{}"}}"#, policy.as_str()))
        }
        Err(MemorySearchError::Unavailable) => {
            (500, r#"{"error":"search_unavailable"}"#.to_string())
        }
    })
}

/// The refusal for a local action the API cannot carry out (issue #779): nothing ran.
fn local_unavailable(action: &LocalAction) -> (u16, String) {
    let kind = match action {
        LocalAction::LocalSearch { .. } => "local_search",
        LocalAction::OpenApp { .. } => "open_app",
        LocalAction::RevealFile { .. } => "reveal_file",
        LocalAction::ShowNotification { .. } => "show_notification",
        LocalAction::CopyToClipboard { .. } => "copy_to_clipboard",
        _ => "local",
    };
    (
        501,
        format!(
            r#"{{"error":"local_action_unavailable","kind":"{kind}","detail":"this action runs on this Mac from the ShogunAI app only; nothing was done"}}"#
        ),
    )
}

/// Put one L3 request into the shared approval queue and answer the caller with the pending
/// envelope (202 + approval id). Shared by `actions.execute` sends and by every other request that
/// needs the user's in-app confirmation (issue #654: `writing_voice.set` enable; issue #657: a
/// `capture_exclusions.set` that removes exclusions; issue #782: turning Visual recall on or
/// keeping saved screens longer), so they refuse, validate, and answer identically on every face.
pub(crate) fn enqueue(
    send: SendAction,
    preview: Preview,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    enqueue_at(send, preview, now_ms, approvals, origin, surface, Level::L3)
}

/// [`enqueue`], reporting `level` in the pending envelope. Every request still waits in the one
/// shared queue for the user's dedicated-button confirm; the label is the level of the tool the
/// caller called. Only an API caller's Gmail archive request (issue #779) reports less than L3:
/// `mail.triage.archive` / `unarchive` are L2 (a reversible change to the user's own mailbox,
/// FR-TG-21) and have answered `"level":"L2"` since they were added.
pub(crate) fn enqueue_at(
    send: SendAction,
    preview: Preview,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
    level: Level,
) -> (u16, String) {
    if surface == ApprovalSurface::Absent {
        return (
            501,
            r#"{"error":"no_approval_surface","detail":"external sends need the ShogunAI app running to confirm them"}"#
                .to_string(),
        );
    }
    // Refuse rows the persisted store would reject on load (empty/oversized destination,
    // oversized body, MAX_PENDING) — otherwise one bad enqueue writes a file every later
    // `load_queue` refuses, bricking the shared queue for every face.
    match crate::approval_store::validate_enqueue(approvals, &preview)
        .and_then(|()| crate::approval_store::validate_action(&send))
    {
        Err(crate::approval_store::EnqueueRefusal::Invalid(detail)) => {
            return (
                400,
                format!(
                    r#"{{"error":"bad_action_request","detail":"{}"}}"#,
                    json_escape(detail)
                ),
            );
        }
        Err(crate::approval_store::EnqueueRefusal::QueueFull) => {
            return (429, r#"{"error":"approval_queue_full"}"#.to_string());
        }
        Ok(()) => {}
    }
    let now = u64::try_from(now_ms).unwrap_or(0);
    match approvals.try_request(send, preview, origin, now) {
        Ok(id) => (
            202,
            format!(
                r#"{{"pending":true,"approval_id":{},"level":"{}","origin":"{}"}}"#,
                id.0,
                level_label(level),
                origin.as_str()
            ),
        ),
        // Id exhaustion: refuse rather than panic while the shared queue lock is held.
        Err(_) => (503, r#"{"error":"approval_queue_unavailable"}"#.to_string()),
    }
}

#[cfg(test)]
mod slack_tests {
    use super::*;
    use crate::backend::{MemoryBackend, ReadItem, ReadParams};
    use crate::forms_api::{FormOperation, FormsAuth};
    use crate::memory_api::Tool;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Desktop(Mutex<Vec<(FormOperation, FormsAuth)>>);
    impl MemoryBackend for Desktop {
        fn read(&self, _: Tool, _: &ReadParams) -> Vec<ReadItem> { vec![] }
        fn forms(&self, operation: FormOperation, auth: FormsAuth) -> Result<String, String> {
            self.0.lock().expect("test").push((operation, auth));
            Ok(r#"{"pending":true,"approval_id":42,"level":"L3"}"#.into())
        }
    }
    #[test]
    fn mcp_and_rest_cli_forward_same_proposal_without_trusting_an_id_or_label_from_the_caller() {
        let backend = Desktop::default();
        for origin in ["mcp", "api"] {
            let auth = FormsAuth { token: Some("test-token".into()), admitted_by_token: true, client: None };
            let (status, answer) = slack_proposal(
                Some(r##"{"kind":"post_message","channel":"#design","body":"Review this","resolved_id":"CATTACKER","label":"spoof"}"##),
                &backend, auth.clone(), origin).expect("test");
            assert_eq!(status,202);
            assert!(answer.contains("42"));
            let requests = backend.0.lock().expect("test");
            let (operation, forwarded_auth) = requests.last().expect("test");
            assert_eq!(forwarded_auth.token,auth.token);
            assert_eq!(*operation, FormOperation::ProposeSlackPost {
                target:"#design".into(), body:"Review this".into(), origin:origin.into(),
            });
        }
        assert!(slack_proposal(Some(r#"{"kind":"post_message","channel":"C0123ABCD","body":"raw"}"#),
            &backend, FormsAuth { token:None,admitted_by_token:false,client:None },"api").is_none());
        assert_eq!(backend.0.lock().expect("test").len(),2);
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_and_mcp_reply_proposals_keep_their_source_marker() {
        for origin in [ApprovalOrigin::Api, ApprovalOrigin::Mcp] {
            for input in [
                json!({"kind":"send_email", "to":"a@b.com", "body":"reply", "thread_id":"thread1", "latest_message_id":"msg1"}),
                json!({"kind":"post_message", "channel":"C0123ABCD", "body":"reply", "thread_ts":"100.000001", "latest_reply_ts":"101.000001"}),
            ] {
                let mut queue = ApprovalQueue::new();
                let (status, response) = act(Some(&input.to_string()), 1, &mut queue, origin, ApprovalSurface::Present);
                assert_eq!(status, 202);
                let response: serde_json::Value = serde_json::from_str(&response).expect("test");
                let id = shogun_agents::approval::ApprovalId(response["approval_id"].as_u64().expect("test"));
                assert!(queue.preview(id).expect("test").reply_thread.is_some());
                assert_eq!(queue.poll(id), shogun_agents::approval::Decision::StillPending);
            }
        }
    }
}
