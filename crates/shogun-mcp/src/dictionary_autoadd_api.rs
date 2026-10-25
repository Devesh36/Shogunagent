//! Shared approval boundary for enabling local automatic dictionary additions.

use crate::rest::ApprovalSurface;
use shogun_agents::approval::{dictionary_autoadd_request, ApprovalOrigin, ApprovalQueue};

pub fn needs_approval(body: Option<&str>) -> bool {
    body.and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .is_some_and(|v| v.get("action").and_then(serde_json::Value::as_str) == Some("enable"))
}

pub fn request_enable(
    body: Option<&str>,
    now_ms: i64,
    approvals: &mut ApprovalQueue,
    origin: ApprovalOrigin,
    surface: ApprovalSurface,
) -> (u16, String) {
    let Some(body) = body else {
        return (400, r#"{"error":"missing_body"}"#.into());
    };
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => {
            return (
                400,
                r#"{"error":"invalid_dictionary_autoadd_action"}"#.into(),
            )
        }
    };
    let Some(fields) = value.as_object() else {
        return (
            400,
            r#"{"error":"invalid_dictionary_autoadd_action"}"#.into(),
        );
    };
    if fields.keys().any(|key| key != "action" && key != "consent")
        || value.get("action").and_then(serde_json::Value::as_str) != Some("enable")
    {
        return (
            400,
            r#"{"error":"invalid_dictionary_autoadd_action"}"#.into(),
        );
    }
    if value.get("consent").and_then(serde_json::Value::as_bool) != Some(true) {
        return (400, r#"{"error":"consent_required"}"#.into());
    }
    let (action, preview) = dictionary_autoadd_request();
    crate::rest::enqueue(action, preview, now_ms, approvals, origin, surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_agents::approval::{ApprovalId, ConfirmIntent, Decision};
    use shogun_agents::permission::SendAction;

    #[test]
    fn only_enable_queues_and_consent_never_self_authorizes() {
        assert!(needs_approval(Some(r#"{"action":"enable"}"#)));
        assert!(!needs_approval(Some(r#"{"action":"disable"}"#)));
        let mut queue = ApprovalQueue::new();
        for body in [
            r#"{"action":"enable"}"#,
            r#"{"action":"enable","consent":false}"#,
            r#"{"action":"enable","consent":true,"extra":1}"#,
        ] {
            assert_eq!(
                request_enable(
                    Some(body),
                    1,
                    &mut queue,
                    ApprovalOrigin::Api,
                    ApprovalSurface::Present
                )
                .0,
                400
            );
        }
        assert_eq!(queue.pending_len(), 0);
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
        let id = ApprovalId(value["approval_id"].as_u64().unwrap());
        assert_eq!(queue.action(id), Some(&SendAction::EnableDictionaryAutoAdd));
        assert_eq!(
            queue.confirm(id, ConfirmIntent::EnterKey, 11),
            Decision::RequiresDedicatedButton
        );
        assert!(
            matches!(queue.confirm(id, ConfirmIntent::DedicatedButton, 12),
            Decision::Confirmed(confirmed) if confirmed.action == SendAction::EnableDictionaryAutoAdd)
        );
    }

    #[test]
    fn absent_approval_surface_refuses_enable() {
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
}
