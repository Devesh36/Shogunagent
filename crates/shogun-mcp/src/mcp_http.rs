//! The Streamable HTTP transport of the MCP face (MCP 2026-07-28, E4-T1), as pure request →
//! reply logic: no socket, no runtime, so the whole contract is unit-testable here and the axum
//! route in [`crate::server`] is only the adapter.
//!
//! The transport is **stateless**: no `initialize` handshake, no `Mcp-Session-Id`, one JSON-RPC
//! request per `POST /mcp`, any request on any instance. A `GET` (the server-initiated stream of
//! the 2025 transport) and a `DELETE` (ending a session) answer 405.
//!
//! Metering from headers, never bodies (CLAUDE.md invariants 1 / 10, `apps/api/src/metering.ts`):
//! every `POST` must carry `Mcp-Method` (the JSON-RPC method) and, for `tools/call`, `Mcp-Name`
//! (the tool); each must equal what the body says, or the request is refused before the handler
//! sees it, so a client cannot bill a `memory.ingest` as a `tools/list`. Every reply carries
//! `Mcp-Method` and, when applicable, `Mcp-Name`; a `memory.ingest` reply also carries
//! `Mcp-Quantity` (the items it received) so the gateway settles the actual count.
//!
//! Streaming: nothing on this face streams today (every handler answers one message), so a
//! client that accepts only `text/event-stream` gets that one message as a single SSE event and
//! the stream ends; `application/json` is the default.

use serde_json::{json, Value};

use crate::backend::MemoryBackend;
use crate::mcp::McpServer;

/// The transport's path.
pub const PATH: &str = "/mcp";
/// The request header naming the JSON-RPC method (required on `POST`).
pub const HEADER_METHOD: &str = "mcp-method";
/// The request header naming the tool / resource / prompt (required for `tools/call`).
pub const HEADER_NAME: &str = "mcp-name";
/// The reply header carrying the number of items a `memory.ingest` received.
pub const HEADER_QUANTITY: &str = "mcp-quantity";
/// The reply header naming the protocol version the transport speaks.
pub const HEADER_PROTOCOL_VERSION: &str = "mcp-protocol-version";
/// The longest `POST /mcp` body (8 MiB: a full `memory.ingest` with room to spare).
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// What the transport needs from a handler: one message in, at most one message out, under the
/// request's own bearer.
pub trait McpHandler: Send + Sync {
    fn handle_line_as(&self, line: &str, token: Option<&str>) -> Option<String>;
}

impl<B: MemoryBackend> McpHandler for McpServer<B> {
    fn handle_line_as(&self, line: &str, token: Option<&str>) -> Option<String> {
        McpServer::handle_line_as(self, line, token)
    }
}

/// One HTTP request to the transport, already lifted off the socket.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest<'a> {
    /// The HTTP method (`POST`, `GET`, …).
    pub method: &'a str,
    pub body: &'a str,
    /// `Mcp-Method`.
    pub mcp_method: Option<&'a str>,
    /// `Mcp-Name`.
    pub mcp_name: Option<&'a str>,
    /// `Accept`.
    pub accept: Option<&'a str>,
    /// The `Bearer` token.
    pub bearer: Option<&'a str>,
}

/// The reply: status, media type, body and the metering headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    /// `Mcp-Method` of the reply: the JSON-RPC method, or the HTTP method of a refused request.
    pub mcp_method: String,
    /// `Mcp-Name` of the reply, when the request named a tool.
    pub mcp_name: Option<String>,
    /// `Mcp-Quantity` of the reply: the items a `memory.ingest` received.
    pub mcp_quantity: Option<u64>,
    /// `Allow` of a 405.
    pub allow: Option<&'static str>,
}

const JSON: &str = "application/json";
const SSE: &str = "text/event-stream";

fn error_body(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn wants_sse_only(accept: Option<&str>) -> bool {
    let Some(accept) = accept else {
        return false;
    };
    let lower = accept.to_ascii_lowercase();
    lower.contains(SSE) && !lower.contains(JSON) && !lower.contains("*/*")
}

/// The `Mcp-Quantity` a `memory.ingest` reply settles (E36a conformance finding): what the core
/// actually took, not what the request carried. A refusal (a JSON-RPC error or an `isError`
/// result) took nothing, and an idempotent replay reports `usage.quantity: 0`, so neither is
/// settled as new events. A reply without a `usage.quantity` keeps the requested count.
fn settled_quantity(asked: u64, reply: &Value) -> u64 {
    if reply.get("error").is_some() || reply["result"]["isError"] == Value::Bool(true) {
        return 0;
    }
    reply["result"]["content"][0]["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|payload| payload["usage"]["quantity"].as_u64())
        .unwrap_or(asked)
}

/// Handle one request against `handler`.
/// The longest value a reply's `Mcp-Method` / `Mcp-Name` header carries.
pub const MAX_METERING_HEADER_CHARS: usize = 128;

/// What a reply's metering header says when the value it would echo cannot be a header value.
pub const INVALID_METERING_VALUE: &str = "invalid";

/// A value that is safe to put in a reply header: visible ASCII only (no space, no control
/// character, nothing above `~`) and at most [`MAX_METERING_HEADER_CHARS`]. Anything else becomes
/// [`INVALID_METERING_VALUE`].
///
/// E38 fuzzing found that a refused request echoed the body's `method` / `params.name` verbatim:
/// a method with a newline made the axum adapter's response builder fail (a 500 instead of the
/// 400 the transport chose), and an 8 MiB method was copied into a response header. The
/// metering names are tool and method identifiers; nothing legitimate is lost here.
pub fn metering_header_value(value: String) -> String {
    if value.is_empty() || value.len() > MAX_METERING_HEADER_CHARS || !value.bytes().all(|b| b.is_ascii_graphic()) {
        INVALID_METERING_VALUE.to_string()
    } else {
        value
    }
}

/// Handle one request against `handler`. The reply's metering headers are always valid header
/// values ([`metering_header_value`]).
pub fn handle(handler: &dyn McpHandler, request: &HttpRequest<'_>) -> HttpReply {
    let mut reply = handle_unchecked(handler, request);
    reply.mcp_method = metering_header_value(reply.mcp_method);
    reply.mcp_name = reply.mcp_name.map(metering_header_value);
    reply
}

fn handle_unchecked(handler: &dyn McpHandler, request: &HttpRequest<'_>) -> HttpReply {
    let refuse = |status: u16, body: String, method: String, name: Option<String>| HttpReply {
        status,
        content_type: JSON,
        body,
        mcp_method: method,
        mcp_name: name,
        mcp_quantity: None,
        allow: None,
    };
    if !request.method.eq_ignore_ascii_case("POST") {
        let message = if request.method.eq_ignore_ascii_case("GET") {
            "The transport is stateless: there is no server-initiated stream to open. POST one JSON-RPC request per call."
        } else {
            "The transport is stateless: there is no session to end or change. POST one JSON-RPC request per call."
        };
        return HttpReply {
            status: 405,
            content_type: JSON,
            body: json!({ "error": "method_not_allowed", "message": message }).to_string(),
            mcp_method: request.method.to_ascii_uppercase(),
            mcp_name: None,
            mcp_quantity: None,
            allow: Some("POST"),
        };
    }
    let header_method = request.mcp_method.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
    let header_name = request.mcp_name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    let fallback_method = header_method.clone().unwrap_or_else(|| "invalid".to_string());
    let message: Value = match serde_json::from_str(request.body) {
        Ok(value) => value,
        Err(_) => return refuse(400, error_body(Value::Null, -32700, "parse error"), fallback_method, header_name),
    };
    let Some(object) = message.as_object() else {
        let text = if message.is_array() {
            "invalid request: the stateless transport takes one JSON-RPC request per POST, not a batch"
        } else {
            "invalid request"
        };
        return refuse(400, error_body(Value::Null, -32600, text), fallback_method, header_name);
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let Some(body_method) = object.get("method").and_then(Value::as_str).map(str::to_string) else {
        return refuse(400, error_body(id, -32600, "invalid request: no method"), fallback_method, header_name);
    };
    let body_name = object
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // The metering contract: the headers say what the body says, or nothing runs.
    match &header_method {
        None => {
            return refuse(
                400,
                error_body(id, -32600, "missing_header: Mcp-Method must name the JSON-RPC method (MCP 2026-07-28)"),
                body_method,
                body_name,
            )
        }
        Some(header) if *header != body_method => {
            return refuse(
                400,
                error_body(id, -32600, "mcp_method_mismatch: Mcp-Method must equal the body's method"),
                body_method,
                body_name,
            )
        }
        Some(_) => {}
    }
    if body_method == "tools/call" {
        match (&header_name, &body_name) {
            (None, _) => {
                return refuse(
                    400,
                    error_body(id, -32600, "missing_header: Mcp-Name must name the tool of a tools/call (MCP 2026-07-28)"),
                    body_method,
                    body_name,
                )
            }
            (Some(header), Some(name)) if header != name => {
                return refuse(
                    400,
                    error_body(id, -32600, "mcp_name_mismatch: Mcp-Name must equal params.name"),
                    body_method,
                    body_name,
                )
            }
            _ => {}
        }
    }
    // `handoff.accept` (E35) appends through the same ingest path: it settles what it stored too.
    let quantity = (body_method == "tools/call" && matches!(body_name.as_deref(), Some("memory.ingest" | "handoff.accept"))).then(|| {
        let arguments = &object.get("params").and_then(|params| params.get("arguments"));
        ["events", "turns"]
            .iter()
            .filter_map(|key| arguments.and_then(|args| args.get(*key)).and_then(Value::as_array))
            .map(|items| items.len() as u64)
            .sum::<u64>()
            .max(1)
    });
    let line = message.to_string();
    let reply = handler.handle_line_as(&line, request.bearer);
    let name = body_name.filter(|_| body_method == "tools/call");
    let Some(reply) = reply else {
        // A notification (no id): accepted, nothing to answer.
        return HttpReply {
            status: 202,
            content_type: JSON,
            body: String::new(),
            mcp_method: body_method,
            mcp_name: name,
            mcp_quantity: quantity,
            allow: None,
        };
    };
    let parsed = serde_json::from_str::<Value>(&reply).ok();
    let status = match &parsed {
        Some(value) if value["error"]["code"].as_i64() == Some(-32700) => 400,
        _ => 200,
    };
    let quantity = quantity.map(|asked| parsed.as_ref().map_or(asked, |value| settled_quantity(asked, value)));
    // E36a conformance finding: this transport speaks 2026-07-28 (every reply says so in
    // `Mcp-Protocol-Version`), so an `initialize` that asks for it is answered with it rather than
    // negotiated down to the newest stdio version, which would contradict the header.
    let asked_stateless = body_method == "initialize"
        && object.get("params").and_then(|p| p.get("protocolVersion")).and_then(Value::as_str)
            == Some(crate::mcp::STATELESS_PROTOCOL_VERSION);
    let reply = match parsed {
        Some(mut value) if asked_stateless && value["result"].is_object() => {
            value["result"]["protocolVersion"] = Value::String(crate::mcp::STATELESS_PROTOCOL_VERSION.into());
            value.to_string()
        }
        _ => reply,
    };
    let (content_type, body) = if wants_sse_only(request.accept) {
        (SSE, format!("event: message\ndata: {reply}\n\n"))
    } else {
        (JSON, reply)
    };
    HttpReply {
        status,
        content_type,
        body,
        mcp_method: body_method,
        mcp_name: name,
        mcp_quantity: quantity,
        allow: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use shogun_agents::approval::ApprovalQueue;

    use crate::backend::{ReadItem, ReadParams, StubBackend};

    fn server() -> McpServer<StubBackend> {
        McpServer::new(
            StubBackend,
            Arc::new(Mutex::new(ApprovalQueue::new())),
            || 1000,
            || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
        )
    }

    fn post<'a>(body: &'a str, method: Option<&'a str>, name: Option<&'a str>) -> HttpRequest<'a> {
        HttpRequest { method: "POST", body, mcp_method: method, mcp_name: name, accept: None, bearer: None }
    }

    #[test]
    fn get_and_delete_are_405_because_there_is_no_session() {
        for method in ["GET", "DELETE", "PUT"] {
            let reply = handle(&server(), &HttpRequest { method, ..HttpRequest::default() });
            assert_eq!(reply.status, 405, "{method}");
            assert_eq!(reply.allow, Some("POST"));
            assert_eq!(reply.mcp_method, method);
            assert!(reply.body.contains("stateless"));
        }
    }

    #[test]
    fn a_request_needs_no_handshake_and_every_reply_carries_the_method_header() {
        let reply = handle(&server(), &post(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, Some("tools/list"), None));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "application/json");
        assert_eq!(reply.mcp_method, "tools/list");
        assert_eq!(reply.mcp_name, None);
        let value: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(value["result"]["ttlMs"], crate::mcp::LIST_TTL_MS);
        assert_eq!(value["result"]["cacheScope"], crate::mcp::CACHE_SCOPE);
        assert!(value["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "memory.recall"));

        let discover = handle(&server(), &post(r#"{"jsonrpc":"2.0","id":2,"method":"server/discover"}"#, Some("server/discover"), None));
        let value: Value = serde_json::from_str(&discover.body).unwrap();
        assert_eq!(value["result"]["protocolVersion"], crate::mcp::STATELESS_PROTOCOL_VERSION);
        assert_eq!(value["result"]["stateless"], true);
    }

    #[test]
    fn tools_call_carries_the_name_header_and_a_read_carries_hints() {
        let body = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"state.commitments.list","arguments":{}}}"#;
        let reply = handle(&server(), &post(body, Some("tools/call"), Some("state.commitments.list")));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.mcp_method, "tools/call");
        assert_eq!(reply.mcp_name.as_deref(), Some("state.commitments.list"));
        assert_eq!(reply.mcp_quantity, None);
        let value: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(value["result"]["_meta"]["ttlMs"], crate::mcp::READ_TTL_MS);
        assert_eq!(value["result"]["_meta"]["cacheScope"], crate::mcp::CACHE_SCOPE);
    }

    #[test]
    fn the_headers_must_be_present_and_agree_with_the_body() {
        let call = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory.ingest","arguments":{"events":[{"content":"a"},{"content":"b"}],"source":"s","actor":"a","idempotency_key":"k"}}}"#;
        let missing = handle(&server(), &post(call, None, Some("memory.ingest")));
        assert_eq!(missing.status, 400);
        assert!(missing.body.contains("missing_header"), "{}", missing.body);
        assert_eq!(missing.mcp_method, "tools/call", "the reply still names what the body asked");
        let mismatch = handle(&server(), &post(call, Some("tools/list"), Some("memory.ingest")));
        assert_eq!(mismatch.status, 400);
        assert!(mismatch.body.contains("mcp_method_mismatch"));
        let no_name = handle(&server(), &post(call, Some("tools/call"), None));
        assert_eq!(no_name.status, 400);
        assert!(no_name.body.contains("missing_header: Mcp-Name"));
        let wrong_name = handle(&server(), &post(call, Some("tools/call"), Some("memory.search")));
        assert_eq!(wrong_name.status, 400);
        assert!(wrong_name.body.contains("mcp_name_mismatch"));
        // Correct headers: the stub backend has no ingest, which is the handler's answer, not
        // the transport's; the quantity header says what the gateway should settle, and a
        // refusal settles nothing.
        let ok = handle(&server(), &post(call, Some("tools/call"), Some("memory.ingest")));
        assert_eq!(ok.status, 200);
        assert_eq!(ok.mcp_quantity, Some(0), "{}", ok.body);
        assert_eq!(ok.mcp_name.as_deref(), Some("memory.ingest"));
    }

    #[test]
    fn initialize_asking_for_the_stateless_version_gets_it() {
        let ask = |version: &str| {
            let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": version } }).to_string();
            let reply = handle(&server(), &post(&body, Some("initialize"), None));
            serde_json::from_str::<Value>(&reply.body).unwrap()["result"]["protocolVersion"].clone()
        };
        assert_eq!(ask(crate::mcp::STATELESS_PROTOCOL_VERSION), crate::mcp::STATELESS_PROTOCOL_VERSION);
        // Older clients still negotiate as before.
        assert_eq!(ask("2025-03-26"), "2025-03-26");
        assert_eq!(ask("1999-01-01"), crate::mcp::SUPPORTED_PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn the_quantity_header_settles_what_the_core_took() {
        struct Canned(String);
        impl McpHandler for Canned {
            fn handle_line_as(&self, _: &str, _: Option<&str>) -> Option<String> {
                Some(self.0.clone())
            }
        }
        let call = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory.ingest","arguments":{"events":[{"content":"a"},{"content":"b"}],"source":"s","actor":"a","idempotency_key":"k"}}}"#;
        let taken = |reply: String| handle(&Canned(reply), &post(call, Some("tools/call"), Some("memory.ingest"))).mcp_quantity;
        let text = |payload: &str| json!({ "jsonrpc": "2.0", "id": 4, "result": { "content": [{ "type": "text", "text": payload }], "isError": false } }).to_string();
        assert_eq!(taken(text(r#"{"usage":{"quantity":2,"unit":"events"}}"#)), Some(2));
        assert_eq!(taken(text(r#"{"usage":{"quantity":0,"unit":"events"},"result":{"replayed":true}}"#)), Some(0), "an idempotent replay adds nothing");
        assert_eq!(taken(text(r#"{"accepted":true}"#)), Some(2), "no usage: the requested count");
        assert_eq!(taken(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"missing_argument: idempotency_key is required"}}"#.into()), Some(0));
        assert_eq!(taken(r#"{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"{}"}],"isError":true}}"#.into()), Some(0));
    }

    #[test]
    fn batches_parse_errors_and_notifications() {
        let batch = handle(&server(), &post(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, Some("ping"), None));
        assert_eq!(batch.status, 400);
        assert!(batch.body.contains("not a batch"));
        let garbage = handle(&server(), &post("{not json", Some("ping"), None));
        assert_eq!(garbage.status, 400);
        assert!(garbage.body.contains("parse error"));
        assert_eq!(garbage.mcp_method, "ping");
        let notification = handle(&server(), &post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, Some("notifications/initialized"), None));
        assert_eq!(notification.status, 202);
        assert!(notification.body.is_empty());
        assert_eq!(notification.mcp_method, "notifications/initialized");
    }

    #[test]
    fn a_client_that_accepts_only_sse_gets_one_event() {
        let request = HttpRequest {
            method: "POST",
            body: r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
            mcp_method: Some("ping"),
            mcp_name: None,
            accept: Some("text/event-stream"),
            bearer: None,
        };
        let reply = handle(&server(), &request);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, "text/event-stream");
        assert!(reply.body.starts_with("event: message\ndata: {"));
        assert!(reply.body.ends_with("\n\n"));
        let both = HttpRequest { accept: Some("application/json, text/event-stream"), ..request };
        assert_eq!(handle(&server(), &both).content_type, "application/json");
    }

    #[test]
    fn the_bearer_is_authenticated_per_request_against_the_live_list() {
        struct Echo;
        impl MemoryBackend for Echo {
            fn read(&self, tool: crate::memory_api::Tool, _: &ReadParams) -> Vec<ReadItem> {
                vec![ReadItem::new(tool.wire_name(), 0.9)]
            }
        }
        let live = Arc::new(crate::live_tokens::LiveTokens::new(
            || {
                let mut registry = crate::memory_api::TokenRegistry::new();
                registry.issue("good-token");
                Ok(registry)
            },
            || None,
        ));
        let server = McpServer::new(
            Echo,
            Arc::new(Mutex::new(ApprovalQueue::new())),
            || 1000,
            || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
        )
        .with_live_session(live, None, true);
        let body = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"state.commitments.list","arguments":{}}}"#;
        let authed = HttpRequest { bearer: Some("good-token"), ..post(body, Some("tools/call"), Some("state.commitments.list")) };
        let value: Value = serde_json::from_str(&handle(&server, &authed).body).unwrap();
        assert!(value["result"]["content"][0]["text"].as_str().unwrap().contains("state.commitments.list"), "{value}");
        for bearer in [None, Some("wrong")] {
            let request = HttpRequest { bearer, ..post(body, Some("tools/call"), Some("state.commitments.list")) };
            let value: Value = serde_json::from_str(&handle(&server, &request).body).unwrap();
            assert_eq!(value["error"]["code"], crate::mcp::TOKEN_INVALID_CODE, "{bearer:?}: {value}");
        }
    }

    /// E38: a refused request whose body names a method or tool that cannot be a header value
    /// gets `invalid` in the reply header, not an echo the adapter cannot send.
    #[test]
    fn a_refusal_never_echoes_an_unsendable_method_or_name() {
        let newline = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list\r\nx-injected: 1","params":{"name":"a\nb"}}"#;
        let reply = handle(&server(), &post(newline, Some("tools/list"), None));
        assert_eq!(reply.status, 400);
        assert_eq!(reply.mcp_method, INVALID_METERING_VALUE);
        assert_eq!(reply.mcp_name.as_deref(), Some(INVALID_METERING_VALUE));
        let long = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{}"}}"#, "m".repeat(10_000));
        let reply = handle(&server(), &post(&long, Some("tools/list"), None));
        assert_eq!(reply.status, 400);
        assert_eq!(reply.mcp_method, INVALID_METERING_VALUE);
        // A normal method and tool pass through unchanged.
        let ok = handle(
            &server(),
            &post(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory.search","arguments":{"query":"x"}}}"#, Some("tools/call"), Some("memory.search")),
        );
        assert_eq!((ok.mcp_method.as_str(), ok.mcp_name.as_deref()), ("tools/call", Some("memory.search")));
    }
}
