//! The MCP JSON-RPC parser: the stdio line handler and the Streamable HTTP transport, with the
//! first input byte choosing the `Mcp-Method` / `Mcp-Name` headers. Invariants checked on every
//! input: no panic; a reply is JSON; the reply's metering headers are sendable header values;
//! a request that ran was metered as exactly what ran.
#![no_main]

use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use shogun_agents::approval::ApprovalQueue;
use shogun_mcp::backend::StubBackend;
use shogun_mcp::mcp::McpServer;
use shogun_mcp::mcp_http::{handle, HttpRequest, MAX_METERING_HEADER_CHARS};

const METHODS: &[&str] = &["tools/call", "tools/list", "initialize", "ping", "notifications/initialized", "x\ny"];
const NAMES: &[&str] = &["memory.search", "state.commitments.list", "nope", ""];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else { return };
    let text = String::from_utf8_lossy(rest);
    let server = McpServer::new(
        StubBackend,
        Arc::new(Mutex::new(ApprovalQueue::new())),
        || 1000,
        || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
    );
    if let Some(reply) = server.handle_line(&text) {
        assert!(serde_json::from_str::<Value>(&reply).is_ok(), "reply is not JSON");
    }
    let method = METHODS.get(usize::from(selector % 8)).copied();
    let name = NAMES.get(usize::from((selector >> 3) % 6)).copied();
    let reply = handle(&server, &HttpRequest { method: "POST", body: &text, mcp_method: method, mcp_name: name, accept: None, bearer: None });
    for value in std::iter::once(reply.mcp_method.as_str()).chain(reply.mcp_name.as_deref()) {
        assert!(value.len() <= MAX_METERING_HEADER_CHARS && value.bytes().all(|b| b.is_ascii_graphic()));
    }
    if matches!(reply.status, 200 | 202) {
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        assert_eq!(method.map(str::trim), body.get("method").and_then(Value::as_str));
    }
});
