//! Translation and reply drafts on the API faces (issue #279, invariant 6): `translate.text`,
//! `translate.reply_draft` and `translate.status`.
//!
//! The notch panel translates a selection and drafts a reply in the user's language pair; an AI
//! client can now ask for the same two things. The faces cannot run them: the Agent lane's
//! credential (a subscription CLI's sign-in or an API key in the Keychain) is only reachable from
//! the desktop, and this process never reads the Keychain. So a request is queued, the desktop
//! runs it with the same core prompt and output gates as the panel, and the face waits for the
//! answer — or, past its wait, hands back a job id to poll with `translate.status`.
//!
//! Levels: both requests are **L1**. They produce text and nothing else — no send, no insert, no
//! stored draft (FR-TL-14 / FR-TL-20); the text goes to the model the user chose, on their own
//! credential, exactly as a chat turn does. Being writes, they are refused to a read-only token,
//! which therefore cannot spend the user's model access. `translate.status` is a read.
//!
//! Plan: the Memory API (Pro) plus agent execution, the gate every Agent-lane tool carries.
//!
//! This module owns the wire shapes (operation parsing, refusal and failure messages, HTTP
//! statuses). The queue and the wait live with the backend (`shogun_core::db_backend`), which
//! can reach the database.

use serde_json::{json, Value};

use crate::memory_api::Tool;

/// Where a request came from, as the queue records it.
pub const ORIGIN_MCP: &str = "mcp";
/// REST — and the CLI, which speaks REST.
pub const ORIGIN_API: &str = "api";

/// The job kinds (the same strings `shogun_core::translate_job` uses).
pub const KIND_TRANSLATE: &str = "translate";
pub const KIND_REPLY_DRAFT: &str = "reply_draft";

/// One call on a translate tool, as every face hands it to the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateOperation {
    /// `translate.text` / `translate.reply_draft`: the caller's body, validated by the backend.
    Request {
        kind: &'static str,
        body: String,
        origin: &'static str,
    },
    /// `translate.status`: one job by id.
    Status { id: i64 },
}

/// Whether a tool is answered by [`crate::backend::MemoryBackend::translate`].
pub fn is_translate_job_tool(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::TranslateText | Tool::TranslateReplyDraft | Tool::TranslateStatus
    )
}

/// Build the operation for `tool`. `id` is the status id (MCP `id` argument, REST path segment).
pub fn operation(
    tool: Tool,
    body: &str,
    id: Option<i64>,
    origin: &'static str,
) -> Result<TranslateOperation, &'static str> {
    match tool {
        Tool::TranslateText => Ok(TranslateOperation::Request {
            kind: KIND_TRANSLATE,
            body: body.to_string(),
            origin,
        }),
        Tool::TranslateReplyDraft => Ok(TranslateOperation::Request {
            kind: KIND_REPLY_DRAFT,
            body: body.to_string(),
            origin,
        }),
        Tool::TranslateStatus => match id {
            Some(id) if id > 0 => Ok(TranslateOperation::Status { id }),
            _ => Err("missing_id"),
        },
        _ => Err("not_a_translate_tool"),
    }
}

/// A plain-language sentence for each failure code. Never request text, never provider text.
pub fn failure_message(code: &str) -> &'static str {
    match code {
        "plan_required" => {
            "Translating and drafting over the API run on your own model access, which needs a paid plan or an active trial."
        }
        "no_agent" => {
            "ShogunAI has no model access to run this on. Choose a subscription or add a key in ShogunAI Settings > Connections."
        }
        "key_rejected" => "Your model key was rejected. Check it in ShogunAI Settings > Connections.",
        "rate_limited" => "Your model provider is rate limiting. Try again shortly.",
        "needs_update" => {
            "The assistant ShogunAI runs this on needs an update. Update it, then use Test connection in ShogunAI Settings > Intelligence."
        }
        "unusable_output" => {
            "The model's answer was not a usable translation or draft, so nothing is returned. Try again."
        }
        "interrupted" => "ShogunAI quit while this was running. Ask again.",
        "app_not_running" => {
            "The ShogunAI app has to be running to translate or draft: it holds your model access. Open it and ask again."
        }
        "not_found" => "No such request. Results are kept for 10 minutes and handed over once.",
        "missing_id" => "id is required.",
        "missing_text" => "Nothing to translate or reply to.",
        "text_too_long" => "Too long: translate up to 30,000 characters, reply to a thread of up to 12,000.",
        "unknown_language" => "Language must be en or ja.",
        "unknown_formality" => "formality must be one of match, casual, polite or formal.",
        "invalid_app" => "app must be a bundle id.",
        "invalid_request" => "The request must be a JSON object.",
        "unavailable" => "Translation is unavailable right now.",
        _ => "That didn't work. Try again.",
    }
}

/// An `error` payload with its code and sentence (plus the job id when there was one).
pub fn refusal_json(code: &str, job_id: Option<i64>) -> String {
    let mut value = json!({ "error": code, "message": failure_message(code) });
    if let Some(id) = job_id {
        value["job_id"] = json!(id);
        value["status"] = json!("failed");
    }
    value.to_string()
}

/// A finished job as every face returns it.
pub fn done_json(job_id: i64, result: Value) -> String {
    json!({ "status": "done", "job_id": job_id, "result": result }).to_string()
}

/// A job the face stopped waiting for: poll `translate.status` with this id.
pub fn pending_json(job_id: i64, status: &str) -> String {
    json!({
        "status": status,
        "job_id": job_id,
        "poll": "translate.status",
        "message": "Still running in ShogunAI. Ask translate.status with this job_id; the result is kept for 10 minutes.",
    })
    .to_string()
}

/// The HTTP status for a translate answer.
pub fn http_status(json: &str) -> u16 {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return 500;
    };
    match value.get("error").and_then(Value::as_str) {
        None => match value.get("status").and_then(Value::as_str) {
            Some("pending" | "running") => 202,
            _ => 200,
        },
        Some("not_found") => 404,
        Some("plan_required") => 403,
        Some("no_agent" | "needs_update") => 409,
        Some("rate_limited") => 429,
        Some("app_not_running" | "unavailable") => 503,
        Some("key_rejected" | "provider_error" | "unusable_output" | "interrupted") => 502,
        Some(_) => 400,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_carry_the_face_origin_and_status_needs_an_id() {
        assert_eq!(
            operation(Tool::TranslateText, "{}", None, ORIGIN_MCP),
            Ok(TranslateOperation::Request {
                kind: "translate",
                body: "{}".into(),
                origin: "mcp"
            })
        );
        assert_eq!(
            operation(Tool::TranslateReplyDraft, "{}", None, ORIGIN_API).map(|op| match op {
                TranslateOperation::Request { kind, .. } => kind,
                TranslateOperation::Status { .. } => "status",
            }),
            Ok("reply_draft")
        );
        assert_eq!(
            operation(Tool::TranslateStatus, "", Some(4), ORIGIN_API),
            Ok(TranslateOperation::Status { id: 4 })
        );
        assert_eq!(
            operation(Tool::TranslateStatus, "", Some(0), ORIGIN_API),
            Err("missing_id")
        );
        assert_eq!(
            operation(Tool::TranslateStatus, "", None, ORIGIN_API),
            Err("missing_id")
        );
        assert_eq!(
            operation(Tool::MemorySearch, "", None, ORIGIN_API),
            Err("not_a_translate_tool")
        );
        assert!(is_translate_job_tool(Tool::TranslateStatus));
        assert!(!is_translate_job_tool(Tool::TranslateRegisterGet));
    }

    #[test]
    fn statuses_follow_the_answer() {
        assert_eq!(http_status(&done_json(1, json!({ "text": "x" }))), 200);
        assert_eq!(http_status(&pending_json(1, "running")), 202);
        assert_eq!(http_status(&refusal_json("not_found", None)), 404);
        assert_eq!(http_status(&refusal_json("plan_required", Some(2))), 403);
        assert_eq!(http_status(&refusal_json("app_not_running", Some(2))), 503);
        assert_eq!(http_status(&refusal_json("needs_update", Some(2))), 409);
        assert_eq!(http_status(&refusal_json("unusable_output", Some(2))), 502);
        assert_eq!(http_status(&refusal_json("missing_text", None)), 400);
        assert_eq!(http_status("nope"), 500);
    }

    #[test]
    fn every_code_has_a_sentence_without_placeholders() {
        for code in [
            "plan_required",
            "no_agent",
            "key_rejected",
            "rate_limited",
            "needs_update",
            "unusable_output",
            "interrupted",
            "app_not_running",
            "not_found",
            "missing_id",
            "missing_text",
            "text_too_long",
            "unknown_language",
            "unknown_formality",
            "invalid_app",
            "invalid_request",
            "unavailable",
            "provider_error",
        ] {
            let message = failure_message(code);
            assert!(!message.is_empty() && !message.contains('{'), "{code}");
        }
        let failed: Value = serde_json::from_str(&refusal_json("no_agent", Some(9))).unwrap();
        assert_eq!(
            (failed["status"].as_str(), failed["job_id"].as_i64()),
            (Some("failed"), Some(9))
        );
    }
}
