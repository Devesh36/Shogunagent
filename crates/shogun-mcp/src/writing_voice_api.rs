//! `writing_voice.set` over the API faces (issue #654).
//!
//! The tool does three things, and they are not the same level:
//! - `enable` starts sending up to four of the user's own examples to their model provider with
//!   every rewrite. That opens an egress, so it is **L3**: an MCP / REST / CLI caller can only
//!   *request* it. The request lands in the shared L3 approval queue as
//!   [`SendAction::ShareWritingStyle`](shogun_agents::permission::SendAction::ShareWritingStyle)
//!   and the caller gets the same pending envelope as an `actions.execute` send. The setting
//!   changes only when the user confirms it in the app. The `consent` flag is still required, but
//!   it is the caller's own claim, so it can never stand in for that confirmation.
//! - `disable` and `clear` only reduce what is shared or kept. They stay direct, at **L1**.
//!
//! One tool whose effective level depends on `action` (like `actions.execute`) instead of three
//! tools: the REST path, the CLI verb and every existing caller keep working, and `tools/list`
//! states the split in the description. The backend (`shogun-core`'s `DbBackend`) refuses
//! `enable` outright, so a face that forgot to route it here fails closed instead of enabling.
//! The desktop Settings toggle does not come through here: the user is the actor there.

use shogun_agents::approval::{writing_style_request, ApprovalOrigin, ApprovalQueue};
use shogun_agents::permission::Level;

use crate::rest::ApprovalSurface;

/// What a `writing_voice.set` body asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Start sharing examples with the provider. Needs in-app approval (L3).
    Enable,
    /// Stop sharing. Direct (L1).
    Disable,
    /// Delete the learned examples. Direct (L1).
    Clear,
    /// Delete one learned style note (#1066). Direct (L1): it only forgets.
    NoteDelete(i64),
    /// Confirm / reject one style note (#1066). Direct (L1): it only tunes what is shown.
    NoteConfirm(i64),
    NoteReject(i64),
}

impl Change {
    /// The level this change actually runs at: only `enable` opens an egress.
    pub fn level(self) -> Level {
        match self {
            Change::Enable => Level::L3,
            Change::Disable | Change::Clear | Change::NoteDelete(_) | Change::NoteConfirm(_) | Change::NoteReject(_) => Level::L1,
        }
    }
}

/// Why a body is refused (every face answers 400 / invalid params).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Not JSON, or `action` is missing or unknown.
    BadRequest,
    /// `enable` without `consent: true`.
    ConsentRequired,
}

impl Invalid {
    pub fn code(self) -> &'static str {
        match self {
            Invalid::BadRequest => "invalid_writing_voice_request",
            Invalid::ConsentRequired => "consent_required",
        }
    }
}

/// The `action` a body names, before any consent check. Used to decide the route: an `enable`
/// always goes to the approval queue, even when it would be refused there, so it can never reach
/// the backend.
pub fn requested(body: &str) -> Option<Change> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    match value.get("action")?.as_str()? {
        "enable" => Some(Change::Enable),
        "disable" => Some(Change::Disable),
        "clear" => Some(Change::Clear),
        action @ ("note_delete" | "note_confirm" | "note_reject") => {
            let id = value.get("id")?.as_i64().filter(|id| *id > 0)?;
            Some(match action {
                "note_delete" => Change::NoteDelete(id),
                "note_confirm" => Change::NoteConfirm(id),
                _ => Change::NoteReject(id),
            })
        }
        _ => None,
    }
}

/// Parse and validate a body. `enable` needs `consent: true` (the caller's attestation that the
/// user saw the disclosure), which is necessary but never sufficient.
pub fn parse(body: &str) -> Result<Change, Invalid> {
    let change = requested(body).ok_or(Invalid::BadRequest)?;
    if change == Change::Enable {
        let consent = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("consent").and_then(serde_json::Value::as_bool));
        if consent != Some(true) {
            return Err(Invalid::ConsentRequired);
        }
    }
    Ok(change)
}

/// Whether this body must go to the approval queue rather than the backend.
pub fn needs_approval(body: Option<&str>) -> bool {
    body.and_then(requested) == Some(Change::Enable)
}

/// Queue an `enable` for the user's confirmation. Returns the HTTP status and JSON body; the MCP
/// face returns the same JSON as its text result. 202 + `{"pending":true,"approval_id":…}` on
/// success, exactly the `actions.execute` pending envelope. Nothing about the setting changes here.
pub fn request_enable(
    body: Option<&str>,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    match body.map(parse) {
        Some(Ok(Change::Enable)) => {
            let (action, preview) = writing_style_request();
            crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
        }
        // Only an enable is routed here; anything else is a caller error, never a direct write.
        Some(Ok(_)) => (
            400,
            format!(r#"{{"error":"{}"}}"#, Invalid::BadRequest.code()),
        ),
        Some(Err(invalid)) => (400, format!(r#"{{"error":"{}"}}"#, invalid.code())),
        None => (400, r#"{"error":"missing_body"}"#.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_agents::approval::{ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    #[test]
    fn only_enable_is_l3_and_only_enable_needs_approval() {
        assert_eq!(parse(r#"{"action":"enable","consent":true}"#), Ok(Change::Enable));
        assert_eq!(parse(r#"{"action":"disable"}"#), Ok(Change::Disable));
        assert_eq!(parse(r#"{"action":"clear"}"#), Ok(Change::Clear));
        assert_eq!(parse(r#"{"action":"note_delete","id":4}"#), Ok(Change::NoteDelete(4)));
        assert_eq!(parse(r#"{"action":"note_reject","id":4}"#), Ok(Change::NoteReject(4)));
        assert_eq!(parse(r#"{"action":"note_confirm"}"#), Err(Invalid::BadRequest));
        assert_eq!(Change::NoteDelete(4).level(), Level::L1);
        assert!(!needs_approval(Some(r#"{"action":"note_delete","id":4}"#)));
        assert_eq!(Change::Enable.level(), Level::L3);
        assert_eq!(Change::Disable.level(), Level::L1);
        assert_eq!(Change::Clear.level(), Level::L1);
        assert!(needs_approval(Some(r#"{"action":"enable","consent":true}"#)));
        // Even without consent: an enable never falls through to the backend.
        assert!(needs_approval(Some(r#"{"action":"enable"}"#)));
        assert!(!needs_approval(Some(r#"{"action":"disable"}"#)));
        assert!(!needs_approval(Some(r#"{"action":"clear"}"#)));
        assert!(!needs_approval(Some("not json")));
        assert!(!needs_approval(None));
    }

    #[test]
    fn consent_is_required_but_not_sufficient() {
        assert_eq!(parse(r#"{"action":"enable"}"#), Err(Invalid::ConsentRequired));
        assert_eq!(
            parse(r#"{"action":"enable","consent":"yes"}"#),
            Err(Invalid::ConsentRequired)
        );
        assert_eq!(parse(r#"{"action":"share"}"#), Err(Invalid::BadRequest));
        assert_eq!(parse("{}"), Err(Invalid::BadRequest));

        let mut queue = ApprovalQueue::new();
        let (status, body) = request_enable(
            Some(r#"{"action":"enable"}"#),
            1,
            &mut queue,
            ApprovalOrigin::Api,
            ApprovalSurface::Present,
        );
        assert_eq!((status, body.as_str()), (400, r#"{"error":"consent_required"}"#));
        assert_eq!(queue.pending_len(), 0);
    }

    #[test]
    fn an_enable_is_queued_pending_and_only_the_user_confirms_it() {
        let mut queue = ApprovalQueue::new();
        let (status, body) = request_enable(
            Some(r#"{"action":"enable","consent":true}"#),
            10,
            &mut queue,
            ApprovalOrigin::Mcp,
            ApprovalSurface::Present,
        );
        assert_eq!(status, 202);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["pending"], true);
        assert_eq!(value["level"], "L3");
        assert_eq!(value["origin"], "mcp");
        let id = shogun_agents::approval::ApprovalId(value["approval_id"].as_u64().unwrap());
        assert_eq!(queue.action(id), Some(&SendAction::ShareWritingStyle));
        assert_eq!(queue.origin(id), Some(ApprovalOrigin::Mcp));
        // Enter alone does not approve it; the dedicated button does.
        assert_eq!(
            queue.confirm(id, ConfirmIntent::EnterKey, 11),
            Decision::RequiresDedicatedButton
        );
        assert!(matches!(
            queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == SendAction::ShareWritingStyle
        ));
    }

    #[test]
    fn a_headless_process_refuses_rather_than_strands_the_request() {
        let mut queue = ApprovalQueue::new();
        let (status, body) = request_enable(
            Some(r#"{"action":"enable","consent":true}"#),
            1,
            &mut queue,
            ApprovalOrigin::Api,
            ApprovalSurface::Absent,
        );
        assert_eq!(status, 501);
        assert!(body.contains("no_approval_surface"));
        assert_eq!(queue.pending_len(), 0);
    }

    #[test]
    fn disable_and_clear_are_never_queued() {
        for body in [r#"{"action":"disable"}"#, r#"{"action":"clear"}"#] {
            let mut queue = ApprovalQueue::new();
            let (status, _) =
                request_enable(Some(body), 1, &mut queue, ApprovalOrigin::Api, ApprovalSurface::Present);
            assert_eq!(status, 400);
            assert_eq!(queue.pending_len(), 0);
        }
    }
}
