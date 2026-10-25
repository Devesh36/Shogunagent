//! Multi Round-Trip Requests and URL-mode elicitation on the MCP face (E4-T2, MCP 2026-07-28
//! SEP-2322): how the L2 / L3 tiers look to an agent.
//!
//! - **L2** (a one-tap confirmation): the first `tools/call` of an L2 tool answers a result with
//!   `resultType: "input_required"` and one `inputRequests` entry, a form with a single
//!   `confirm` boolean ([`input_request`]). The client shows it to the human and retries the
//!   same call with `params.inputResponses` carrying the answer ([`confirmation`]). An accepted
//!   answer runs the tool; a declined or cancelled one runs nothing and says so.
//! - **L3** (a dedicated human approval): the tool runs as far as the approval queue and answers
//!   the pending envelope (`{"pending": true, "approval_id": …}`); [`shape_approval`] turns that
//!   into `resultType: "approval_required"` with an `approval` block (id, level, the
//!   `actions.status` poll) and, when an approval base URL is configured, a URL-mode
//!   `inputRequests` entry the human opens outside the MCP client. The queue, the id and the
//!   poll are the ones every face already uses ([`crate::rest::act`] / `poll_approval`); nothing
//!   here grants anything.
//!
//! Both shapes are applied on this face only: REST and the CLI keep their `202 pending`
//! envelope, which carries the same id.

use serde_json::{json, Value};
use shogun_agents::permission::Level;

use crate::memory_api::{tool_level, ApiLevel, Tool};

/// The `inputRequests[].id` of the L2 confirmation form.
pub const INPUT_REQUEST_ID: &str = "l2_confirm";
/// The `inputRequests[].id` of the L3 approval URL.
pub const APPROVAL_REQUEST_ID: &str = "l3_approval";
/// `resultType` of a call that needs the caller's confirmation first.
pub const RESULT_INPUT_REQUIRED: &str = "input_required";
/// `resultType` of a call that is waiting for the human's approval.
pub const RESULT_APPROVAL_REQUIRED: &str = "approval_required";

/// Whether `tool` asks for the MRTR confirmation before it runs: exactly the L2 writes. L1 runs,
/// L3 and per-action tools go to the approval queue, reads read.
pub fn needs_confirmation(tool: Tool) -> bool {
    tool_level(tool) == ApiLevel::Write(Level::L2)
}

/// What the retry's `inputResponses` said about the confirmation form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// No response for the form yet: ask.
    Missing,
    /// The human confirmed: run.
    Accepted,
    /// The human declined or cancelled: run nothing.
    Declined,
}

/// Read the confirmation out of `params.inputResponses`: an array of
/// `{ "id": "l2_confirm", "action": "accept" | "decline" | "cancel", "content": { "confirm": true } }`
/// (the elicitation result shape). An entry without an `id` counts when it is the only one. An
/// `accept` whose `content.confirm` is not `true` is a decline: nothing runs on a blank form.
pub fn confirmation(responses: Option<&Value>) -> Confirmation {
    let Some(responses) = responses.and_then(Value::as_array) else {
        return Confirmation::Missing;
    };
    let entry = responses.iter().find(|entry| match entry.get("id").and_then(Value::as_str) {
        Some(id) => id == INPUT_REQUEST_ID,
        None => responses.len() == 1,
    });
    let Some(entry) = entry else {
        return Confirmation::Missing;
    };
    match entry.get("action").and_then(Value::as_str).unwrap_or("accept") {
        "accept" if entry["content"]["confirm"] == Value::Bool(true) => Confirmation::Accepted,
        _ => Confirmation::Declined,
    }
}

/// The form an L2 tool asks the human to confirm: one boolean, with the tool and its arguments
/// named so the client can render what will run. Argument values are the caller's own text,
/// echoed to the same caller; nothing from memory is in it.
pub fn input_request(tool: Tool, args: &Value) -> Value {
    let name = tool.wire_name();
    json!({
        "id": INPUT_REQUEST_ID,
        "mode": "form",
        "message": format!("{name} is a one-tap confirmation (L2). Confirm to run it with these arguments; decline to run nothing."),
        "tool": name,
        "arguments": args,
        "requestedSchema": {
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "title": format!("Run {name}"),
                    "description": "true runs the tool once with the arguments shown; false runs nothing.",
                }
            },
            "required": ["confirm"],
        },
    })
}

/// The MRTR gate of one call, after auth and argument checks: `None` lets the tool run (not an
/// L2 tool, or the retry carries an accepted confirmation); `Some` is the `tools/call` result
/// to answer instead — the `input_required` form, or the "ran nothing" answer of a decline.
pub fn input_required(tool: Tool, args: &Value, responses: Option<&Value>) -> Option<Value> {
    if !needs_confirmation(tool) {
        return None;
    }
    match confirmation(responses) {
        Confirmation::Accepted => None,
        Confirmation::Missing => {
            let text = json!({
                "input_required": true,
                "tool": tool.wire_name(),
                "level": "L2",
                "message": "Confirm with the human, then resend the same call with inputResponses.",
            });
            Some(json!({
                "resultType": RESULT_INPUT_REQUIRED,
                "inputRequests": [input_request(tool, args)],
                "content": [{ "type": "text", "text": text.to_string() }],
                "isError": false,
            }))
        }
        Confirmation::Declined => {
            let text = json!({
                "ran": false,
                "declined": true,
                "tool": tool.wire_name(),
                "level": "L2",
                "message": "The confirmation was declined; nothing ran.",
            });
            Some(json!({
                "content": [{ "type": "text", "text": text.to_string() }],
                "isError": false,
            }))
        }
    }
}

/// The URL a human opens to approve `approval_id`: `<base>/approvals/<id>`.
pub fn approval_url(base_url: Option<&str>, approval_id: u64) -> Option<String> {
    let base = base_url?.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    Some(format!("{base}/approvals/{approval_id}"))
}

/// Reshape a `tools/call` response whose text is the pending envelope into
/// `resultType: "approval_required"` (URL-mode elicitation, E4-T2). Any other response is
/// returned unchanged. The envelope's own fields stay in the text block (the id, the level, the
/// origin), joined by the URL when one is configured, so an older client that reads only the text
/// still finds the id to poll.
pub fn shape_approval(response: String, base_url: Option<&str>) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(&response) else {
        return response;
    };
    let Some(text) = value["result"]["content"][0]["text"].as_str() else {
        return response;
    };
    let Ok(mut payload) = serde_json::from_str::<Value>(text) else {
        return response;
    };
    if payload["pending"] != Value::Bool(true) {
        return response;
    }
    let Some(approval_id) = payload["approval_id"].as_u64().filter(|id| *id > 0) else {
        return response;
    };
    let level = payload["level"].as_str().unwrap_or("L3").to_string();
    let origin = payload["origin"].as_str().map(str::to_string);
    let url = approval_url(base_url, approval_id);
    let message = match &url {
        Some(_) => "Hand the URL to the human; it approves or rejects this request outside the MCP client. Poll actions.status until it leaves pending. Never report the action as done before it says sent.",
        None => "The human approves this request in the ShogunAI app. Poll actions.status until it leaves pending. Never report the action as done before it says sent.",
    };
    if let Some(url) = &url {
        payload["url"] = Value::String(url.clone());
    }
    payload["result_type"] = Value::String(RESULT_APPROVAL_REQUIRED.into());
    payload["next_action"] = Value::String(if url.is_some() { "approve_at_url".into() } else { "poll_status".into() });
    let mut approval = json!({
        "approval_id": approval_id,
        "level": level,
        "url": url,
        "poll": { "tool": Tool::ActionsStatus.wire_name(), "arguments": { "approval_id": approval_id } },
        "message": message,
    });
    if let Some(origin) = origin {
        approval["origin"] = Value::String(origin);
    }
    let Some(result) = value["result"].as_object_mut() else {
        return response;
    };
    result.insert("resultType".into(), Value::String(RESULT_APPROVAL_REQUIRED.into()));
    result.insert("approval".into(), approval);
    if let Some(url) = &url {
        result.insert(
            "inputRequests".into(),
            json!([{
                "id": APPROVAL_REQUEST_ID,
                "mode": "url",
                "url": url,
                "message": message,
            }]),
        );
    }
    result.insert("isError".into(), Value::Bool(false));
    result["content"][0]["text"] = Value::String(payload.to_string());
    value.to_string()
}

/// Add the read cache hints (2026-07-28 `ttlMs` / `cacheScope`, E4-T1) to a successful read's
/// result `_meta`. Errors and refusals carry none.
pub fn with_read_hints(response: String, ttl_ms: u64, cache_scope: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(&response) else {
        return response;
    };
    if value.get("error").is_some() || value["result"]["isError"] == Value::Bool(true) {
        return response;
    }
    let Some(result) = value["result"].as_object_mut() else {
        return response;
    };
    result.insert("_meta".into(), json!({ "ttlMs": ttl_ms, "cacheScope": cache_scope }));
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_l2_writes_ask_for_confirmation() {
        for &tool in crate::memory_api::ALL_TOOLS {
            assert_eq!(
                needs_confirmation(tool),
                tool_level(tool) == ApiLevel::Write(Level::L2),
                "{}",
                tool.wire_name()
            );
        }
        assert!(!needs_confirmation(Tool::MemoryAppendNote));
        assert!(!needs_confirmation(Tool::ActionsExecute));
        assert!(!needs_confirmation(Tool::MemoryRecall));
    }

    #[test]
    fn confirmation_reads_the_elicitation_result_shape() {
        assert_eq!(confirmation(None), Confirmation::Missing);
        assert_eq!(confirmation(Some(&json!([]))), Confirmation::Missing);
        assert_eq!(confirmation(Some(&json!([{ "id": "other", "action": "accept", "content": { "confirm": true } }]))), Confirmation::Missing);
        assert_eq!(
            confirmation(Some(&json!([{ "id": INPUT_REQUEST_ID, "action": "accept", "content": { "confirm": true } }]))),
            Confirmation::Accepted
        );
        assert_eq!(confirmation(Some(&json!([{ "action": "accept", "content": { "confirm": true } }]))), Confirmation::Accepted);
        assert_eq!(confirmation(Some(&json!([{ "id": INPUT_REQUEST_ID, "action": "accept", "content": {} }]))), Confirmation::Declined);
        assert_eq!(confirmation(Some(&json!([{ "id": INPUT_REQUEST_ID, "action": "decline" }]))), Confirmation::Declined);
        assert_eq!(confirmation(Some(&json!([{ "id": INPUT_REQUEST_ID, "action": "cancel" }]))), Confirmation::Declined);
    }

    #[test]
    fn the_form_names_the_tool_and_asks_one_boolean() {
        let form = input_request(Tool::MemoryAppendNote, &json!({ "text": "x" }));
        assert_eq!(form["id"], INPUT_REQUEST_ID);
        assert_eq!(form["mode"], "form");
        assert_eq!(form["requestedSchema"]["required"], json!(["confirm"]));
        assert_eq!(form["requestedSchema"]["properties"]["confirm"]["type"], "boolean");
        assert_eq!(form["arguments"], json!({ "text": "x" }));
    }

    #[test]
    fn a_pending_envelope_becomes_approval_required_with_a_url_when_configured() {
        let pending = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "content": [{ "type": "text", "text": r#"{"pending":true,"approval_id":9,"level":"L3","origin":"mcp"}"# }], "isError": false }
        })
        .to_string();
        let shaped: Value = serde_json::from_str(&shape_approval(pending.clone(), Some("https://approve.example/"))).unwrap();
        assert_eq!(shaped["result"]["resultType"], RESULT_APPROVAL_REQUIRED);
        assert_eq!(shaped["result"]["approval"]["approval_id"], 9);
        assert_eq!(shaped["result"]["approval"]["url"], "https://approve.example/approvals/9");
        assert_eq!(shaped["result"]["approval"]["origin"], "mcp");
        assert_eq!(shaped["result"]["approval"]["poll"]["tool"], "actions.status");
        assert_eq!(shaped["result"]["inputRequests"][0]["mode"], "url");
        assert_eq!(shaped["result"]["inputRequests"][0]["url"], "https://approve.example/approvals/9");
        let text: Value = serde_json::from_str(shaped["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["approval_id"], 9, "the id stays in the text for older clients");
        assert_eq!(text["url"], "https://approve.example/approvals/9");
        assert_eq!(text["next_action"], "approve_at_url");

        let without: Value = serde_json::from_str(&shape_approval(pending, None)).unwrap();
        assert_eq!(without["result"]["resultType"], RESULT_APPROVAL_REQUIRED);
        assert_eq!(without["result"]["approval"]["url"], Value::Null);
        assert!(without["result"].get("inputRequests").is_none(), "no URL, no URL-mode request");
        let text: Value = serde_json::from_str(without["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["next_action"], "poll_status");
    }

    #[test]
    fn other_responses_pass_through_untouched() {
        for response in [
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": r#"{"accepted":true}"# }], "isError": false } }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "x" } }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "not json" }], "isError": false } }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": r#"{"pending":false,"approval_id":2}"# }], "isError": false } }).to_string(),
        ] {
            assert_eq!(shape_approval(response.clone(), Some("https://a.example")), response);
        }
    }

    #[test]
    fn read_hints_are_added_to_successful_results_only() {
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [], "isError": false } }).to_string();
        let hinted: Value = serde_json::from_str(&with_read_hints(ok, 15_000, "private")).unwrap();
        assert_eq!(hinted["result"]["_meta"], json!({ "ttlMs": 15_000, "cacheScope": "private" }));
        let failed = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [], "isError": true } }).to_string();
        assert_eq!(with_read_hints(failed.clone(), 15_000, "private"), failed);
        let error = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32000, "message": "x" } }).to_string();
        assert_eq!(with_read_hints(error.clone(), 15_000, "private"), error);
    }
}
