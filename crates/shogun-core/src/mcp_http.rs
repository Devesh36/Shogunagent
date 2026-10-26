//! The concrete HTTPS clients for first-layer connectors (feature `net`).
//!
//! These live in shogun-core because it is the single crate allowlisted to hold an HTTP client
//! (FR-TR-03 / invariant 3: external egress goes through one place). shogun-integrations stays
//! HTTP-client-free — it defines the seams ([`shogun_integrations::McpRpc`],
//! [`shogun_integrations::oauth::TokenExchange`]) and the pure logic; this module supplies the
//! reqwest implementations the daemon wires in.
//!
//! Two blocking clients (the seams are synchronous):
//! - [`HttpMcpRpc`] — MCP `tools/call` over the Streamable-HTTP transport (JSON-RPC 2.0, Bearer).
//! - [`HttpTokenExchange`] — the OAuth token endpoint form POST.
//!
//! Errors are content-free (a request/response body can echo a token or user data, so it is never
//! surfaced or logged).

use serde_json::{json, Value};
use shogun_integrations::endpoints;

/// Hard ceiling on a complete first-layer MCP operation (including handshake and teardown),
/// or one OAuth HTTP round trip. Matches the tool loop's per-tool budget
/// (`shogun_mcp::tool_loop::TOOL_TIMEOUT_MS`) so a `ToolRunError::Timeout` is actually producible,
/// and bounds the sync poller's worst case: a stalled server cannot hang a blocking call
/// indefinitely. A timeout error's text contains "timed out".
pub const HTTP_TIMEOUT_MS: u64 = 10_000;
use shogun_integrations::oauth::{ExchangeAuth, ExchangeBody, ExchangeMethod, ExchangeRequest, TokenExchange};
use shogun_integrations::rpc::{McpRpc, TokenProvider};
use shogun_mcp::scope::Service;

/// A blocking MCP Streamable-HTTP client. Each operation gets its own initialized session,
/// so token rotation/disconnect never leaves a cached session authenticated as the old user.
/// ponytail: one handshake per operation; cache sessions only if measured latency requires it.
pub struct HttpMcpRpc<P: TokenProvider> {
    client: reqwest::blocking::Client,
    tokens: P,
    /// Google tool schemas from `tools/list`, per service. Reads only: writes always bind
    /// against a fresh schema. Any failed call clears the entry so a schema change is re-discovered.
    google_tools: std::sync::Mutex<std::collections::HashMap<&'static str, std::sync::Arc<Vec<Value>>>>,
}

const MCP_VERSION: &str = "2025-11-25";
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

impl<P: TokenProvider> HttpMcpRpc<P> {
    /// Build the HTTPS client with a token source. Redirects are refused at the provider boundary.
    pub fn new(tokens: P) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("shogun/1.0")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(HTTP_TIMEOUT_MS))
            .build()
            .map_err(|_| "mcp http client init failed".to_string())?;
        Ok(Self { client, tokens, google_tools: Default::default() })
    }

    /// Read-only proof that authentication, initialization and advertised tools work. Call before
    /// marking a connection successful; this does not execute tools or read workspace content.
    pub fn list_tools(&self, service: Service) -> Result<Vec<Value>, String> {
        self.with_session(service, |session| {
            let tools = all_tools(session)?;
            // A Connect probe's discovery serves the first Google read too.
            if matches!(service, Service::GoogleCalendar | Service::GoogleDrive) {
                if let Ok(mut m) = self.google_tools.lock() {
                    m.insert(service.source_str(), std::sync::Arc::new(tools.clone()));
                }
            }
            Ok(tools)
        })
    }

    fn with_session<T>(
        &self,
        service: Service,
        operation: impl FnOnce(&Session<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let endpoint = endpoints::endpoint(service)
            .ok_or_else(|| format!("{} has no official MCP endpoint", service.source_str()))?;
        // Token refresh has its own timeout; start the MCP budget only once we hold a token.
        let token = self.tokens.access_token(service)?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(HTTP_TIMEOUT_MS);
        self.at_endpoint(endpoint.url, &token, deadline, operation)
    }

    fn at_endpoint<T>(
        &self,
        endpoint: &str,
        token: &str,
        deadline: std::time::Instant,
        operation: impl FnOnce(&Session<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut session = Session {
            client: &self.client,
            endpoint,
            token,
            id: None,
            version: MCP_VERSION.to_string(),
            deadline,
            next_id: std::cell::Cell::new(2),
        };
        let response = session.post(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": MCP_VERSION, "capabilities": {},
                "clientInfo": { "name": "shogun", "version": "1.0" } }
        }))?;
        session.id = response.headers().get("mcp-session-id").cloned();
        let initialized = read_rpc_response(response, 1)?;
        let version = initialized
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or("mcp initialize had no protocol version")?;
        if !["2025-03-26", "2025-06-18", MCP_VERSION].contains(&version) {
            return Err("mcp protocol version is unsupported".into());
        }
        if !initialized
            .get("capabilities")
            .is_some_and(|c| c.get("tools").is_some())
        {
            return Err("mcp server does not support tools".into());
        }
        session.version = version.to_string();
        let notification =
            session.post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        if notification.status() != reqwest::StatusCode::ACCEPTED {
            return Err("mcp initialized notification was not accepted".into());
        }
        operation(&session)
    }
}

/// Every advertised tool, following `nextCursor`. Bounded, and a looping server is an error rather
/// than a partial capability report.
fn all_tools(session: &Session<'_>) -> Result<Vec<Value>, String> {
    let mut tools = Vec::new();
    let mut cursor = None;
    for _ in 0..32 {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |c| json!({ "cursor": c }));
        let result = session.request("tools/list", params)?;
        let page = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or("mcp tools/list had no tools")?;
        tools.extend(page.iter().cloned());
        if tools.len() > 4096 {
            return Err("mcp tools/list exceeded tool limit".into());
        }
        let next = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if next.is_none() {
            return Ok(tools);
        }
        if next == cursor {
            return Err("mcp tools/list repeated cursor".into());
        }
        cursor = next;
    }
    Err("mcp tools/list exceeded page limit".into())
}

struct Session<'a> {
    client: &'a reqwest::blocking::Client,
    endpoint: &'a str,
    token: &'a str,
    id: Option<reqwest::header::HeaderValue>,
    version: String,
    deadline: std::time::Instant,
    next_id: std::cell::Cell<u64>,
}

impl Session<'_> {
    fn post(&self, body: &Value) -> Result<reqwest::blocking::Response, String> {
        let remaining = self
            .deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or("mcp request deadline expired before send")?;
        let mut request = self
            .client
            .post(self.endpoint)
            .timeout(remaining)
            .bearer_auth(self.token)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
        // Initialization has no negotiated version yet.
        if body.get("method").and_then(Value::as_str) != Some("initialize") {
            request = request.header("mcp-protocol-version", &self.version);
        }
        if let Some(id) = &self.id {
            request = request.header("mcp-session-id", id);
        }
        let tool_call = body.get("method").and_then(Value::as_str) == Some("tools/call");
        let response = request.json(body).send().map_err(|error| {
            let before_send = error.is_connect() || error.is_builder();
            let reason = http_error(error);
            if tool_call && !before_send {
                format!("delivery_uncertain: {reason}")
            } else {
                reason
            }
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let reason = if status.as_u16() == 403 && forbidden_is_auth(response) {
                "mcp http 403 invalid_token".to_string()
            } else {
                format!("mcp http {}", status.as_u16())
            };
            return Err(if tool_call && status.is_server_error() {
                format!("delivery_uncertain: {reason}")
            } else {
                reason
            });
        }
        Ok(response)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let response =
            self.post(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        read_rpc_response(response, id).map_err(|reason| {
            // Invalid requests were rejected; internal errors or lost replies can follow delivery.
            let rejected = matches!(
                reason.as_str(),
                "mcp json-rpc error -32700"
                    | "mcp json-rpc error -32600"
                    | "mcp json-rpc error -32601"
                    | "mcp json-rpc error -32602"
            );
            if method == "tools/call" && !rejected {
                format!("delivery_uncertain: {reason}")
            } else {
                reason
            }
        })
    }
}

/// How long a session teardown may take. It runs off the caller's path, so this bounds only the
/// background thread, never a read's latency.
const TEARDOWN_TIMEOUT_MS: u64 = 2_000;

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // Best-effort teardown, including failed initialization, on its own thread: the result
            // is already in hand, and a slow DELETE must not hold it back from the chat's first
            // token. No tool retry: a write may have reached the provider even if its response
            // was lost.
            let request = self
                .client
                .delete(self.endpoint)
                .timeout(std::time::Duration::from_millis(TEARDOWN_TIMEOUT_MS))
                .bearer_auth(self.token)
                .header("mcp-session-id", id)
                .header("mcp-protocol-version", &self.version);
            let _ = std::thread::Builder::new()
                .name("mcp-teardown".into())
                .spawn(move || {
                    let _ = request.send();
                });
        }
    }
}

impl<P: TokenProvider> McpRpc for HttpMcpRpc<P> {
    fn call_tool(&self, service: Service, tool: &str, arguments: Value) -> Result<Value, String> {
        let result = self.call_tool_inner(service, tool, arguments);
        if result.is_err() {
            if let Ok(mut m) = self.google_tools.lock() { m.remove(service.source_str()); }
        }
        result
    }
}

impl<P: TokenProvider> HttpMcpRpc<P> {
    fn call_tool_inner(&self, service: Service, tool: &str, arguments: Value) -> Result<Value, String> {
        self.with_session(service, |session| {
            let arguments = if matches!(service, Service::GoogleCalendar | Service::GoogleDrive) {
                let is_read = ["read_sync", "read_on_demand"].iter().any(|op| shogun_integrations::toolmap::tool_for(service, op) == Some(tool));
                let cached = if is_read { self.google_tools.lock().ok().and_then(|m| m.get(service.source_str()).cloned()) } else { None };
                let tools = if let Some(tools) = cached { tools } else {
                    let tools = std::sync::Arc::new(all_tools(session)?);
                    if let Ok(mut m) = self.google_tools.lock() { m.insert(service.source_str(), tools.clone()); }
                    tools
                };
                let tools: &[Value] = &tools;
                shogun_integrations::provider_read::bind_google_arguments(tool, &arguments, tools)?
            } else if service == Service::Granola {
                // Granola's public docs omit input schemas. Fresh tools/list is the wire contract;
                // refuse unknown fields or schema drift before sending any meeting data request.
                let tools = all_tools(session)?;
                shogun_integrations::provider_read::granola_arguments(tool, &arguments, &tools)?
            } else if service == Service::Slack && tool == "slack_search_public" {
                let query = arguments.get("query").and_then(Value::as_str).filter(|query| !query.trim().is_empty() && query.len() <= 2000).ok_or("Slack search requires a short non-empty query")?;
                json!({"query":query,"limit":arguments.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1,20),"only_my_channels":true,"content_types":"messages"})
            } else if service == Service::Linear && tool == "list_issues" && arguments.get("limit").is_none() {
                let mut args = arguments.clone();
                args["limit"] = json!(25);
                args
            } else { arguments };
            let tool = if service == Service::Notion && tool == "notion-search" {
                let query = arguments.get("query").and_then(Value::as_str).ok_or("Notion search requires a query")?;
                if query.trim().is_empty() || query.len() > 2000 || query.split_whitespace().count() > 50 {
                    return Err("Notion search requires a short non-empty query".into());
                }
                let access = session.request("tools/call", json!({"name":"notion-get-tool-access","arguments":{}}))?;
                shogun_integrations::provider_read::notion_search_tool(&shogun_integrations::result::payload(&access)?)?
            } else { tool };
            if matches!((service, tool), (Service::GitHub, "add_issue_comment") | (Service::Slack, "slack_send_message" | "slack_search_channels" | "slack_search_users")) {
                let tools = all_tools(session)?;
                shogun_integrations::provider_read::validate_arguments(tool, &arguments, &tools)?;
            }
            let result = session.request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )?;
            if let Some(reason) = shogun_integrations::result::tool_error_reason(&result) {
                return Err(reason.into());
            }
            if !result.get("content").is_some_and(Value::is_array) {
                return Err("delivery_uncertain: mcp tool result had no content array".into());
            }
            Ok(result)
        })
    }
}

/// Inspect a bounded rejection locally; never return provider text or change permission-only 403s.
pub(crate) fn forbidden_is_auth(response: reqwest::blocking::Response) -> bool {
    forbidden_kind(response) == Forbidden::Auth
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Forbidden {
    Auth,
    /// Google returns quota and rate limits as 403 with these `reason` values.
    RateLimit,
    Permission,
}

pub(crate) fn forbidden_kind(response: reqwest::blocking::Response) -> Forbidden {
    use std::io::Read;
    if response.headers().get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(shogun_integrations::runtime::is_auth_failure)
    {
        return Forbidden::Auth;
    }
    let mut body = String::new();
    if response.take(16 * 1024).read_to_string(&mut body).is_err() {
        return Forbidden::Permission;
    }
    if shogun_integrations::runtime::is_auth_failure(&body) {
        return Forbidden::Auth;
    }
    let body = body.to_ascii_lowercase();
    if ["ratelimitexceeded", "dailylimitexceeded", "quotaexceeded"]
        .iter()
        .any(|reason| body.contains(reason))
    {
        Forbidden::RateLimit
    } else {
        Forbidden::Permission
    }
}

fn http_error(error: reqwest::Error) -> String {
    if error.is_connect() || error.is_builder() {
        "mcp connection failed before send"
    } else if error.is_timeout() {
        "mcp request timed out"
    } else {
        "mcp request failed"
    }
    .to_string()
}

fn rpc_result(envelope: Value, id: u64) -> Result<Value, String> {
    if envelope.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || envelope.get("id") != Some(&json!(id))
    {
        return Err("mcp response did not match request".into());
    }
    if let Some(error) = envelope.get("error") {
        if error.get("code").and_then(Value::as_i64) == Some(401)
            || shogun_integrations::runtime::is_auth_failure(&error.to_string())
        {
            return Err("mcp unauthorized".into());
        }
        return Err(format!(
            "mcp json-rpc error {}",
            error.get("code").and_then(Value::as_i64).unwrap_or(0)
        ));
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| "mcp response had no result".into())
}

fn read_rpc_response(response: reqwest::blocking::Response, id: u64) -> Result<Value, String> {
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    let sse = match mime {
        "application/json" => false,
        "text/event-stream" => true,
        _ => return Err("mcp response had unsupported content type".into()),
    };
    parse_response(response, sse, id)
}

fn parse_response(reader: impl std::io::Read, sse: bool, id: u64) -> Result<Value, String> {
    use std::io::{BufRead, Read};
    let mut limited = reader.take(MAX_RESPONSE_BYTES + 1);
    if !sse {
        let mut body = Vec::new();
        limited
            .read_to_end(&mut body)
            .map_err(|_| "mcp response read failed")?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("mcp response exceeded size limit".into());
        }
        let envelope =
            serde_json::from_slice(&body).map_err(|_| "mcp response was not valid json")?;
        return rpc_result(envelope, id);
    }
    let mut reader = std::io::BufReader::new(limited);
    let mut data = String::new();
    let mut consumed = 0;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|_| "mcp stream read failed")?;
        consumed += bytes as u64;
        if consumed > MAX_RESPONSE_BYTES {
            return Err("mcp response exceeded size limit".into());
        }
        if bytes == 0 {
            return Err("mcp stream ended without response".into());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                let envelope: Value =
                    serde_json::from_str(&data).map_err(|_| "mcp event was not valid json")?;
                data.clear();
                if envelope.get("method").is_none() && envelope.get("id") == Some(&json!(id)) {
                    return rpc_result(envelope, id);
                }
                if envelope.get("method").is_none() && envelope.get("id").is_some() {
                    return Err("mcp response did not match request".into());
                }
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }
}

/// A blocking `reqwest` OAuth token exchange (implements [`shogun_integrations::oauth::TokenExchange`]).
pub struct HttpTokenExchange {
    client: reqwest::blocking::Client,
}

impl HttpTokenExchange {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("shogun/1.0")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(HTTP_TIMEOUT_MS))
            .build()
            .map_err(|e| format!("http client init failed: {e}"))?;
        Ok(Self { client })
    }
}

impl TokenExchange for HttpTokenExchange {
    fn post_form(&self, token_endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
        let resp = self
            .client
            .post(token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .map_err(|e| format!("token exchange failed: {}", redact(&e.to_string())))?;
        read_token_response(resp)
    }

    /// Notion's token endpoint (Basic client auth, JSON body), Linear's revocation (bearer) and
    /// GitHub's grant deletion (`DELETE`, Basic) — E51. Same client, limits and error words as
    /// [`Self::post_form`]; credentials go only into the `Authorization` header.
    fn send(&self, request: &ExchangeRequest<'_>) -> Result<String, String> {
        let mut builder = match request.method {
            ExchangeMethod::Post => self.client.post(request.endpoint),
            ExchangeMethod::Delete => self.client.delete(request.endpoint),
        }
        .header(reqwest::header::ACCEPT, "application/json");
        for (name, value) in request.headers {
            builder = builder.header(*name, *value);
        }
        builder = match &request.auth {
            ExchangeAuth::None => builder,
            ExchangeAuth::Basic { user, password } => builder.basic_auth(user, Some(password)),
            ExchangeAuth::Bearer(token) => builder.bearer_auth(token),
        };
        builder = match &request.body {
            ExchangeBody::Form(form) => builder.form(form),
            ExchangeBody::Json(value) => builder.json(value),
            ExchangeBody::Empty => builder,
        };
        let resp = builder
            .send()
            .map_err(|e| format!("token exchange failed: {}", redact(&e.to_string())))?;
        read_token_response(resp)
    }
}

/// A token endpoint's answer: the body (≤ 64 KiB, UTF-8) of a 2xx, else the status only.
fn read_token_response(resp: reqwest::blocking::Response) -> Result<String, String> {
    use std::io::Read;
    let status = resp.status();
    let mut body = Vec::new();
    resp.take(65_537).read_to_end(&mut body).map_err(|_| "token response read failed")?;
    if body.len() > 65_536 {
        return Err("token response exceeded size limit".into());
    }
    let body = String::from_utf8(body).map_err(|_| "token response was not UTF-8")?;
    if !status.is_success() {
        // Status only — the body can echo the code/verifier, so it is not surfaced.
        return Err(format!("token endpoint http {status}"));
    }
    Ok(body)
}

/// Strip anything after a `?` from an error string before returning it for logging (defense in
/// depth — reqwest errors can embed the URL with a query).
fn redact(msg: &str) -> String {
    match msg.find('?') {
        Some(i) => format!("{}…", &msg[..i]),
        None => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_auth_errors_are_content_free_and_permission_errors_stay_distinct() {
        for error in [
            json!({"code": 401, "message": "PRIVATE"}),
            json!({"code": -32000, "message": "invalid_token PRIVATE"}),
        ] {
            assert_eq!(rpc_result(json!({"jsonrpc": "2.0", "id": 1, "error": error}), 1), Err("mcp unauthorized".into()));
        }
        assert_eq!(rpc_result(json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": 403, "message": "Forbidden: insufficient_scope PRIVATE"
        }}), 1), Err("mcp json-rpc error 403".into()));
    }

    #[test]
    fn json_and_sse_match_requests_and_redact_errors() {
        let envelope = json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [] } });
        assert_eq!(
            parse_response(envelope.to_string().as_bytes(), false, 2).unwrap(),
            json!({ "tools": [] })
        );
        let stream = format!(": heartbeat\r\n\r\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\r\n\r\ndata: {envelope}\r\n\r\n");
        assert_eq!(
            parse_response(stream.as_bytes(), true, 2).unwrap(),
            json!({ "tools": [] })
        );
        assert!(parse_response(envelope.to_string().as_bytes(), false, 1).is_err());
        let error = json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32602, "message": "SECRET", "data": "private@example.com" } });
        assert_eq!(
            parse_response(error.to_string().as_bytes(), false, 2).unwrap_err(),
            "mcp json-rpc error -32602"
        );
        assert!(parse_response("data: {}\n\n".as_bytes(), true, 2).is_err());
        assert!(parse_response(
            vec![b'x'; MAX_RESPONSE_BYTES as usize + 1].as_slice(),
            false,
            2
        )
        .unwrap_err()
        .contains("size limit"));
    }

    #[test]
    fn only_ambiguous_tool_calls_mark_delivery_uncertain() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        for (method, status, body, uncertain) in [
            ("tools/call", "400 Bad Request", "{}", false),
            ("tools/call", "401 Unauthorized", "{}", false),
            ("tools/call", "429 Too Many Requests", "{}", false),
            ("tools/call", "503 Unavailable", "{}", true),
            ("initialize", "503 Unavailable", "{}", false),
            ("tools/call", "200 OK", "", true),
            ("tools/call", "200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602}}"#, false),
            ("tools/call", "200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603}}"#, true),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let client = reqwest::blocking::Client::new();
            let session = Session {
                client: &client,
                endpoint: &endpoint,
                token: "fixture-token",
                id: None,
                version: "2025-06-18".into(),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
                next_id: std::cell::Cell::new(1),
            };
            let error = session.request(method, json!({})).unwrap_err();
            assert_eq!(error.starts_with("delivery_uncertain:"), uncertain, "{method}: {error}");
            server.join().unwrap();
        }
    }

    #[test]
    fn live_http_sequence_initializes_carries_session_and_tears_down() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..5 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut input = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let headers = headers.to_ascii_lowercase();
                assert!(headers.contains("authorization: bearer test-token"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|n| n.trim().parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                let reply = match index {
                    0 => {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["method"], "initialize");
                        assert!(!headers.contains("mcp-session-id"));
                        json!({ "jsonrpc": "2.0", "id": 1, "result": {
                            "protocolVersion": "2025-06-18", "capabilities": { "tools": {} },
                            "serverInfo": { "name": "test", "version": "1" }
                        } })
                        .to_string()
                    }
                    1 => {
                        assert!(headers.contains("mcp-session-id: session-123"));
                        assert!(headers.contains("mcp-protocol-version: 2025-06-18"));
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["method"], "notifications/initialized");
                        String::new()
                    }
                    2 => {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["method"], "tools/list");
                        assert_eq!(request["id"], 2);
                        json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [] } }).to_string()
                    }
                    3 => {
                        assert!(headers.contains("mcp-session-id: session-123"));
                        assert!(headers.contains("mcp-protocol-version: 2025-06-18"));
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(request["method"], "tools/call");
                        assert_eq!(request["id"], 3);
                        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[]}}\n\n"
                            .into()
                    }
                    _ => {
                        assert!(headers.starts_with("delete /mcp"));
                        assert!(headers.contains("mcp-session-id: session-123"));
                        String::new()
                    }
                };
                let status = if index == 1 || index == 4 {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                let mime = if index == 3 {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let session = if index == 0 {
                    "Mcp-Session-Id: session-123\r\n"
                } else {
                    ""
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        });
        let rpc =
            HttpMcpRpc::new(shogun_integrations::rpc::StaticTokenProvider::new("unused")).unwrap();
        let started = std::time::Instant::now();
        let result = rpc
            .at_endpoint(
                &endpoint,
                "test-token",
                started + std::time::Duration::from_millis(HTTP_TIMEOUT_MS),
                |session| {
                    session.request("tools/list", json!({}))?;
                    session.request("tools/call", json!({ "name": "test", "arguments": {} }))
                },
            )
            .unwrap();
        assert_eq!(result, json!({ "content": [] }));
        eprintln!(
            "MCP_LOOPBACK_MS={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        server.join().unwrap();
    }

    #[test]
    fn redact_strips_query_string() {
        assert_eq!(
            redact("https://x.example/mcp/v1?access_token=SECRET"),
            "https://x.example/mcp/v1…"
        );
        assert_eq!(redact("plain error"), "plain error");
    }

    /// Pins the strings `shogun_integrations::token::is_transient_token_failure` relies on: a
    /// refresh that cannot reach the token endpoint is transient, a rejected grant is not.
    #[test]
    fn unreachable_token_endpoint_is_a_transient_refresh_failure() {
        use shogun_integrations::token::{is_transient_token_failure, TokenError};
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .expect("test port");
        // The listener is dropped: nothing accepts on that port any more.
        let error = HttpTokenExchange::new()
            .expect("client")
            .post_form(&format!("http://127.0.0.1:{port}/token"), &[])
            .expect_err("connection refused");
        assert!(is_transient_token_failure(&format!("{:?}", TokenError::Refresh(error))));
        assert!(!is_transient_token_failure(&format!(
            "{:?}",
            TokenError::Refresh("token endpoint http 400 Bad Request".into())
        )));
    }

    /// E51: `send` puts the client credentials into Basic auth (Notion's token endpoint, GitHub's
    /// grant deletion) or the token into a bearer (Linear's revocation), the body as JSON, the
    /// method as asked, and answers a 204 with an empty body and a refusal with its status only.
    #[test]
    fn send_speaks_basic_auth_json_and_delete_and_keeps_bodies_out_of_errors() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, reply) in [("200 OK", r#"{"access_token":"ok"}"#), ("204 No Content", ""), ("401 Unauthorized", r#"{"error":"secret-echo"}"#)] {
                let (mut socket, _) = listener.accept().expect("accept");
                let mut reader = std::io::BufReader::new(socket.try_clone().expect("clone"));
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).expect("line") > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                        length = value.trim().parse::<usize>().expect("length");
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("body");
                seen.push((head, String::from_utf8(body).expect("utf8")));
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).expect("write");
            }
            seen
        });
        let exchange = HttpTokenExchange::new().expect("client");
        let token_body = json!({ "grant_type": "authorization_code", "code": "c-1" });
        let answer = exchange
            .send(&ExchangeRequest {
                method: ExchangeMethod::Post,
                endpoint: &format!("{base}/v1/oauth/token"),
                auth: ExchangeAuth::Basic { user: "client-id", password: "client-secret" },
                body: ExchangeBody::Json(&token_body),
                headers: &[],
            })
            .expect("200");
        assert_eq!(answer, r#"{"access_token":"ok"}"#);
        let grant = json!({ "access_token": "ghu_x" });
        let deleted = exchange
            .send(&ExchangeRequest {
                method: ExchangeMethod::Delete,
                endpoint: &format!("{base}/applications/client-id/grant"),
                auth: ExchangeAuth::Basic { user: "client-id", password: "client-secret" },
                body: ExchangeBody::Json(&grant),
                headers: &[("x-github-api-version", "2022-11-28")],
            })
            .expect("204");
        assert_eq!(deleted, "");
        let refused = exchange
            .send(&ExchangeRequest { method: ExchangeMethod::Post, endpoint: &format!("{base}/oauth/revoke"), auth: ExchangeAuth::Bearer("lin_access"), body: ExchangeBody::Empty, headers: &[] })
            .expect_err("401");
        assert_eq!(refused, "token endpoint http 401 Unauthorized", "the status only, never the body");
        let seen = server.join().expect("server");
        use base64::Engine as _;
        let basic = format!("authorization: basic {}", base64::engine::general_purpose::STANDARD.encode("client-id:client-secret"));
        assert!(seen[0].0.starts_with("POST /v1/oauth/token "));
        assert!(seen[0].0.to_ascii_lowercase().contains(&basic.to_ascii_lowercase()));
        assert!(seen[0].0.to_ascii_lowercase().contains("content-type: application/json"));
        assert_eq!(serde_json::from_str::<Value>(&seen[0].1).expect("json"), token_body);
        assert!(seen[1].0.starts_with("DELETE /applications/client-id/grant "));
        assert!(seen[1].0.to_ascii_lowercase().contains("x-github-api-version: 2022-11-28"));
        assert_eq!(serde_json::from_str::<Value>(&seen[1].1).expect("json"), grant);
        assert!(seen[2].0.to_ascii_lowercase().contains("authorization: bearer lin_access"));
        assert!(seen[2].1.is_empty());
    }
}
