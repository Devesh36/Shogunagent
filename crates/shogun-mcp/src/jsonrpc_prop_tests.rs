//! Property tests for the MCP JSON-RPC parser (plan E38): the stdio line handler
//! ([`crate::mcp::McpServer::handle_line`]) and the Streamable HTTP transport
//! ([`crate::mcp_http::handle`]) over seeded random input, in the ordinary `cargo test`. The
//! nightly-only libFuzzer target (`fuzz/fuzz_targets/mcp_jsonrpc.rs`) drives the same two entry
//! points with coverage guidance.
//!
//! Only read tools and unknown names are generated for `tools/call`, so no case writes a settings
//! file or the approval queue.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shogun_agents::approval::ApprovalQueue;

use crate::backend::StubBackend;
use crate::mcp::McpServer;
use crate::mcp_http::{handle, HttpRequest, MAX_METERING_HEADER_CHARS};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const METHODS: &[&str] = &[
    "initialize", "ping", "tools/list", "tools/call", "server/discover", "resources/list",
    "notifications/initialized", "notifications/cancelled", "prompts/list", "", "tools/call ", "TOOLS/CALL",
    "tools/list\n", "é", "x\r\ny: z",
];
const TOOLS: &[&str] = &[
    "memory.search", "memory.recall", "state.commitments.list", "state.people.get", "profile.whoami",
    "memory.get_context_pack", "nope.tool", "", "memory.search\n", "😀",
];

fn scalar(rng: &mut Rng) -> Value {
    match rng.below(7) {
        0 => Value::Null,
        1 => json!(rng.below(2) == 0),
        2 => json!(rng.next() as i64),
        3 => json!(1.5e300),
        4 => json!(""),
        5 => json!(rng.pick(&["x", "Aiko", "\u{0}", "😀", "\n"])),
        _ => json!(-1),
    }
}

fn any(rng: &mut Rng, depth: u32) -> Value {
    if depth > 3 {
        return scalar(rng);
    }
    match rng.below(4) {
        0 => Value::Array((0..rng.below(4)).map(|_| any(rng, depth + 1)).collect()),
        1 => {
            let mut map = serde_json::Map::new();
            for _ in 0..rng.below(4) {
                map.insert(rng.pick(&["query", "id", "limit", "name", "x"]).to_string(), any(rng, depth + 1));
            }
            Value::Object(map)
        }
        _ => scalar(rng),
    }
}

/// One JSON-RPC message, mostly well-formed, often subtly not.
fn message(rng: &mut Rng) -> Value {
    if rng.below(10) == 0 {
        return any(rng, 0);
    }
    let mut map = serde_json::Map::new();
    if rng.below(8) != 0 {
        map.insert("jsonrpc".into(), json!(rng.pick(&["2.0", "1.0", ""])));
    }
    match rng.below(6) {
        0 => {}
        1 => {
            map.insert("id".into(), any(rng, 2));
        }
        _ => {
            map.insert("id".into(), json!(rng.below(1000)));
        }
    }
    let method = rng.pick(METHODS);
    if rng.below(12) != 0 {
        map.insert("method".into(), if rng.below(15) == 0 { scalar(rng) } else { json!(method) });
    }
    if rng.below(3) != 0 {
        let params = if method == "tools/call" && rng.below(5) != 0 {
            json!({ "name": rng.pick(TOOLS), "arguments": any(rng, 1) })
        } else {
            any(rng, 1)
        };
        map.insert("params".into(), params);
    }
    Value::Object(map)
}

fn line(rng: &mut Rng) -> String {
    match rng.below(12) {
        0 => {
            let text = message(rng).to_string();
            let cut = rng.below(text.len() as u64 + 1) as usize;
            text.get(..cut).unwrap_or("").to_string()
        }
        1 => "[".repeat(rng.below(300) as usize),
        2 => Value::Array((0..rng.below(5)).map(|_| message(rng)).collect()).to_string(),
        3 => String::from_utf8_lossy(&(0..rng.below(40)).map(|_| rng.below(256) as u8).collect::<Vec<_>>()).into_owned(),
        _ => message(rng).to_string(),
    }
}

fn server() -> McpServer<StubBackend> {
    McpServer::new(
        StubBackend,
        Arc::new(Mutex::new(ApprovalQueue::new())),
        || 1000,
        || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
    )
}

/// A JSON-RPC 2.0 response object: `jsonrpc: "2.0"`, an `id`, and exactly one of result / error.
fn assert_response(value: &Value, seed: u64) {
    let object = value.as_object().unwrap_or_else(|| panic!("seed {seed}: a response that is not an object: {value}"));
    assert_eq!(object.get("jsonrpc"), Some(&json!("2.0")), "seed {seed}: {value}");
    assert!(object.contains_key("id"), "seed {seed}: no id: {value}");
    assert!(object.contains_key("result") != object.contains_key("error"), "seed {seed}: {value}");
    if let Some(error) = object.get("error") {
        assert!(error["code"].is_i64() && error["message"].is_string(), "seed {seed}: {value}");
    }
}

#[test]
fn the_line_handler_answers_every_request_with_a_well_formed_response() {
    let server = server();
    for seed in 0..1_500u64 {
        let mut rng = Rng(seed ^ 0x4A50);
        let input = line(&mut rng);
        let Some(reply) = server.handle_line(&input) else {
            // Silence is only for notifications: an object (or a batch of objects) with a method
            // and no id.
            let parsed: Value = serde_json::from_str(&input).unwrap_or_else(|_| panic!("seed {seed}: no answer to unparseable input"));
            let members = parsed.as_array().cloned().unwrap_or_else(|| vec![parsed.clone()]);
            assert!(
                members.iter().all(|m| m.get("id").is_none() && m.get("method").is_some_and(Value::is_string)),
                "seed {seed}: no answer to {input}"
            );
            continue;
        };
        let value: Value = serde_json::from_str(&reply).unwrap_or_else(|_| panic!("seed {seed}: the reply is not JSON"));
        match &value {
            Value::Array(items) => {
                assert!(!items.is_empty(), "seed {seed}: an empty batch reply");
                items.iter().for_each(|item| assert_response(item, seed));
            }
            other => assert_response(other, seed),
        }
    }
}

#[test]
fn the_http_transport_meters_what_it_runs_and_only_sends_valid_headers() {
    let server = server();
    for seed in 0..1_500u64 {
        let mut rng = Rng(seed ^ 0x77E7);
        let body = line(&mut rng);
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let body_method = parsed.as_ref().and_then(|v| v.get("method")).and_then(Value::as_str).map(str::to_string);
        let body_name = parsed.as_ref().and_then(|v| v.get("params")).and_then(|p| p.get("name")).and_then(Value::as_str).map(str::to_string);
        // The header usually tells the truth, sometimes lies, sometimes is missing.
        let header_method = match rng.below(5) {
            0 => None,
            1 => Some(rng.pick(METHODS).to_string()),
            _ => body_method.clone(),
        };
        let header_name = match rng.below(5) {
            0 => None,
            1 => Some(rng.pick(TOOLS).to_string()),
            _ => body_name.clone(),
        };
        let http_method = if rng.below(10) == 0 { rng.pick(&["GET", "DELETE", "PUT"]) } else { "POST" };
        let request = HttpRequest {
            method: http_method,
            body: &body,
            mcp_method: header_method.as_deref(),
            mcp_name: header_name.as_deref(),
            accept: Some(rng.pick(&["application/json", "text/event-stream", "*/*"])),
            bearer: None,
        };
        let reply = handle(&server, &request);
        assert!(matches!(reply.status, 200 | 202 | 400 | 405), "seed {seed}: status {}", reply.status);
        for value in std::iter::once(reply.mcp_method.as_str()).chain(reply.mcp_name.as_deref()) {
            assert!(
                !value.is_empty() && value.len() <= MAX_METERING_HEADER_CHARS && value.bytes().all(|b| b.is_ascii_graphic()),
                "seed {seed}: unsendable metering header {value:?}"
            );
        }
        if reply.status == 200 || reply.status == 202 {
            // The handler ran: the header the gateway billed on named exactly what ran.
            let ran = body_method.clone().unwrap_or_default();
            assert_eq!(header_method.as_deref().map(str::trim), Some(ran.as_str()), "seed {seed}");
            if ran == "tools/call" {
                if let Some(name) = &body_name {
                    assert_eq!(header_name.as_deref().map(str::trim), Some(name.as_str()), "seed {seed}");
                }
            }
        }
    }
}
