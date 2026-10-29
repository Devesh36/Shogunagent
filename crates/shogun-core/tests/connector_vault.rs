//! E50: the sidecar's connector routes (`POST /v1/internal/connectors/*`) on the real
//! `shogun-api --multi` server, with a fake vendor behind the token-exchange seam. What is proved:
//! the gateway needs the admin credential *and* the principal's own sidecar bearer; the principal is
//! the one that bearer names (a body naming another is refused); one principal never reads
//! another's grant; an agent bearer alone reaches nothing; the connect flow writes the tokenset into
//! the principal that started it and the token route then serves it; every answer is `no-store`.
//!
//! E50 follow-ups and E44 read-sync: a pending flow survives a sidecar restart (it is sealed in the
//! principal's store); connects, refreshes, revocations and disconnects are traceability rows in
//! the principal's own store with no token in them; `connections.list` and
//! `connections.disconnect` answer from the vault on the REST and MCP faces as on the internal
//! routes; a Gmail change notice through `memory.ingest_webhook` is queued and the background tick
//! fetches it into the principal's store, once.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use shogun_core::daemon::Clock;
use shogun_core::llm::Secret;
use shogun_core::tenancy::{ChainedResolver, DerivedBearers, EnvKeyProvider, PrincipalId, PrincipalStore, TokenPrincipalMap};
use shogun_integrations::hosted_connect::{HostedClients, HostedConnectors, OAuthClient, Vendor};
use shogun_integrations::read_sync::{FetchError, ProviderHttp};
use shogun_integrations::oauth::{TokenExchange, TokenSet};
use shogun_integrations::token::GrantStore;
use shogun_integrations::token_store::{MemorySecretStore, PrincipalGrants};
use shogun_mcp::scope::Service;
use shogun_mcp::server::{bind_exact, serve_on};
use shogun_memory::DbKey;

const SECRET: &str = "gateway-and-sidecar-derived-bearer-secret-e50x";
const ADMIN: &str = "operator-admin-credential-e50-0123";
const ACCESS: &str = "ya29.SENTINEL-alice-access-4711";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("shogun_connector_vault_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wall_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    })
}

/// The vendor's token endpoint: one authorization code, never a network call.
/// The token-endpoint forms the fake vendor received.
type Forms = Arc<Mutex<Vec<Vec<(String, String)>>>>;

#[derive(Clone, Default)]
struct FakeVendor {
    forms: Forms,
}

impl TokenExchange for FakeVendor {
    fn post_form(&self, endpoint: &str, form: &[(String, String)]) -> Result<String, String> {
        self.forms.lock().unwrap().push(form.to_vec());
        if endpoint == "https://oauth2.googleapis.com/revoke" {
            return Ok("{}".into());
        }
        if form.iter().any(|(k, v)| k == "code" && v == "vendor-code-1") {
            return Ok(r#"{"access_token":"ya29.connected-by-flow","expires_in":3599,"refresh_token":"1//flow-refresh","scope":"https://www.googleapis.com/auth/calendar.events"}"#.into());
        }
        Err("token endpoint http 400 Bad Request".into())
    }
}

fn bearer(principal: &str, key: &str) -> String {
    DerivedBearers::new(Secret::new(SECRET)).unwrap().derive(&PrincipalId::parse(principal).unwrap(), key).unwrap()
}

fn boot(root: PathBuf, tokens: Arc<MemorySecretStore>, vendor: FakeVendor) -> u16 {
    boot_with(root, tokens, vendor).0
}

type Vault = Arc<HostedConnectors<FakeVendor>>;

/// The `shogun-api --multi` composition: the vault audits into each principal's own store, and
/// each principal's backend serves `connections.*` from it.
fn boot_with(root: PathBuf, tokens: Arc<MemorySecretStore>, vendor: FakeVendor) -> (u16, Arc<PrincipalStore>, Vault) {
    let audit_store: Arc<std::sync::OnceLock<std::sync::Weak<PrincipalStore>>> = Arc::default();
    let resolver = ChainedResolver {
        listed: TokenPrincipalMap::parse("carol:token-listed-carol-1").unwrap(),
        derived: Some(DerivedBearers::new(Secret::new(SECRET)).unwrap()),
    };
    let clients = HostedClients::default()
        .with_redirect_uri("https://api.example.test/v1/connectors/callback")
        .with_client(Vendor::Google, OAuthClient::new("123-abc.apps.googleusercontent.com", Some(Secret::new("web-secret"))));
    let vault = Arc::new(
        HostedConnectors::new(tokens, clients, move || Ok(vendor.clone()))
            .with_audit(shogun_core::tenancy::multi::connector_audit_sink(audit_store.clone())),
    );
    let dyn_vault: Arc<dyn shogun_mcp::connector_vault::ConnectorVault> = vault.clone();
    let store = Arc::new(
        PrincipalStore::new(root, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x5e; 32]))), wall_clock(), 8).with_connectors(Some(dyn_vault)),
    );
    let _ = audit_store.set(Arc::downgrade(&store));
    let (served_store, served_vault) = (store.clone(), vault.clone());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(served_store, Arc::new(resolver), wall_clock())
                .with_admin_token(Some(ADMIN))
                .with_connector_vault(served_vault);
            let listener = bind_exact(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (port, store, vault)
}

struct Reply {
    status: u16,
    head: String,
    body: serde_json::Value,
}

fn call(port: u16, method: &str, path: &str, admin: Option<&str>, principal: Option<&str>, body: &str) -> Reply {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(admin) = admin {
        req.push_str(&format!("Authorization: Bearer {admin}\r\n"));
    }
    if let Some(principal) = principal {
        req.push_str(&format!("X-Shogun-Principal-Authorization: Bearer {principal}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    Reply { status, head: head.to_ascii_lowercase(), body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null) }
}

const TOKEN: &str = "/v1/internal/connectors/token";

#[test]
fn the_gateway_gets_the_calling_principals_token_and_no_one_elses() {
    let root = TempRoot::new("token");
    let tokens = Arc::new(MemorySecretStore::new());
    let far = wall_clock()() + 3_600_000;
    PrincipalGrants::new(tokens.as_ref(), "prn_alice")
        .save(Service::Gmail, &TokenSet { access_token: ACCESS.into(), refresh_token: Some("r".into()), expires_at_ms: far, account_label: None })
        .unwrap();
    let port = boot(root.0.clone(), tokens.clone(), FakeVendor::default());
    let alice = bearer("prn_alice", "key_01");
    let bob = bearer("prn_bob", "key_01");
    let slot = r#"{"slot":"gmail-tokenset"}"#;

    let ok = call(port, "POST", TOKEN, Some(ADMIN), Some(&alice), slot);
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(ok.body, serde_json::json!({ "access_token": ACCESS, "token_type": "Bearer", "expires_at": far }));
    assert!(ok.head.contains("cache-control: no-store"), "{}", ok.head);

    // Bob's own bearer reads Bob's (absent) grant, never Alice's.
    let theirs = call(port, "POST", TOKEN, Some(ADMIN), Some(&bob), slot);
    assert_eq!(theirs.status, 404);
    assert_eq!(theirs.body["error"], "connector_not_connected");
    assert!(!theirs.body.to_string().contains(ACCESS));

    // The body never names the principal.
    let named = call(port, "POST", TOKEN, Some(ADMIN), Some(&bob), r#"{"slot":"gmail-tokenset","principal":"prn_alice"}"#);
    assert_eq!(named.status, 400);

    // An agent bearer in place of the admin credential, no admin credential, no principal, a
    // forged principal bearer: refused, and nothing is served.
    let agent = call(port, "POST", TOKEN, Some(&alice), Some(&alice), slot);
    assert_eq!(agent.status, 403, "{}", agent.body);
    assert_eq!(agent.body["error"], "admin_required");
    assert_eq!(call(port, "POST", TOKEN, None, Some(&alice), slot).status, 401);
    assert_eq!(call(port, "POST", TOKEN, Some("not-the-admin-credential-0000"), Some(&alice), slot).status, 401);
    let missing = call(port, "POST", TOKEN, Some(ADMIN), None, slot);
    assert_eq!((missing.status, missing.body["error"].as_str()), (401, Some("principal_required")));
    let forged = format!("{}A", &alice[..alice.len() - 1]);
    let forged = if forged == alice { format!("{}B", &alice[..alice.len() - 1]) } else { forged };
    assert_eq!(call(port, "POST", TOKEN, Some(ADMIN), Some(&forged), slot).status, 401);
    assert_eq!(call(port, "GET", TOKEN, Some(ADMIN), Some(&alice), "").status, 405);
    for refused in [&agent, &missing, &named] {
        assert!(!refused.body.to_string().contains(ACCESS));
        assert!(refused.head.contains("cache-control: no-store"));
    }
}

#[test]
fn the_connect_flow_writes_into_the_principal_that_started_it() {
    let root = TempRoot::new("connect");
    let tokens = Arc::new(MemorySecretStore::new());
    let vendor = FakeVendor::default();
    let port = boot(root.0.clone(), tokens.clone(), vendor.clone());
    let alice = bearer("prn_carla", "key_07");
    let other = bearer("prn_dan", "key_07");
    let state = "gateway-sealed-state-0123456789abcdef";

    let started = call(port, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&alice), &format!(r#"{{"provider":"calendar","state":"{state}"}}"#));
    assert_eq!(started.status, 200, "{}", started.body);
    let url = started.body["authorize_url"].as_str().unwrap();
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
    assert!(url.contains("code_challenge_method=S256") && !url.contains("web-secret"));

    // Another principal's bearer cannot finish it: its own store holds no such flow.
    let finish = |who: &str| call(port, "POST", "/v1/internal/connectors/connect/finish", Some(ADMIN), Some(who), &format!(r#"{{"state":"{state}","code":"vendor-code-1"}}"#));
    assert_eq!(finish(&other).status, 404);
    assert!(vendor.forms.lock().unwrap().is_empty(), "no vendor call for another principal");
    let done = finish(&alice);
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.body, serde_json::json!({ "connected": true, "provider": "calendar", "slot": "gcal-tokenset" }));
    // The code and the token never come back to the gateway; the token route serves the grant.
    assert!(!done.body.to_string().contains("ya29") && !done.body.to_string().contains("vendor-code-1"));
    assert_eq!(finish(&alice).status, 404, "single use");
    let served = call(port, "POST", TOKEN, Some(ADMIN), Some(&alice), r#"{"slot":"gcal-tokenset"}"#);
    assert_eq!(served.body["access_token"], "ya29.connected-by-flow");
    assert!(PrincipalGrants::new(tokens.as_ref(), "prn_dan").load(Service::GoogleCalendar).unwrap().is_none());
    // The exchange carried the PKCE verifier and the hosted callback.
    let forms = vendor.forms.lock().unwrap();
    let last = forms.last().unwrap();
    assert!(last.iter().any(|(k, _)| k == "code_verifier"));
    assert!(last.contains(&("redirect_uri".into(), "https://api.example.test/v1/connectors/callback".into())));
}

fn rest(port: u16, method: &str, path: &str, token: &str, body: &str) -> Reply {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    Reply { status, head: head.to_ascii_lowercase(), body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null) }
}

fn mcp_tool(port: u16, token: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nMcp-Method: tools/call\r\nMcp-Name: {name}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let reply: serde_json::Value = serde_json::from_str(text.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    let block = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{reply}"));
    serde_json::from_str(block).unwrap()
}

/// The connector rows of a principal's traceability log: (purpose, destination, bytes).
fn audit_rows(store: &PrincipalStore, principal: &str) -> Vec<(String, String, i64)> {
    let handle = store.open(&PrincipalId::parse(principal).unwrap()).unwrap();
    handle
        .db()
        .trace_rows(&Default::default())
        .into_iter()
        .filter(|r| r.purpose.starts_with("connector."))
        .map(|r| (r.purpose, r.destination, r.chunk_bytes))
        .collect()
}

fn service_row(list: &serde_json::Value, service: &str) -> serde_json::Value {
    list["services"].as_array().unwrap().iter().find(|r| r["service"] == service).unwrap().clone()
}

#[test]
fn connects_and_disconnects_are_audited_and_listed_on_every_face() {
    let root = TempRoot::new("faces");
    let tokens = Arc::new(MemorySecretStore::new());
    let vendor = FakeVendor::default();
    let (port, store, _) = boot_with(root.0.clone(), tokens.clone(), vendor.clone());
    let erin = bearer("prn_erin", "key_02");
    let frank = bearer("prn_frank", "key_02");
    let state = "gateway-sealed-state-erin-0123456789";
    call(port, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&erin), &format!(r#"{{"provider":"calendar","state":"{state}"}}"#));
    let done = call(port, "POST", "/v1/internal/connectors/connect/finish", Some(ADMIN), Some(&erin), &format!(r#"{{"state":"{state}","code":"vendor-code-1"}}"#));
    assert_eq!(done.status, 200, "{}", done.body);

    // Status: the internal route, REST and MCP agree; the agent's own bearer reads its own.
    let status = call(port, "POST", "/v1/internal/connectors/status", Some(ADMIN), Some(&erin), "{}");
    assert_eq!(status.status, 200, "{}", status.body);
    let gcal = status.body["connectors"].as_array().unwrap().iter().find(|r| r["service"] == "gcal").unwrap().clone();
    assert_eq!((gcal["state"].as_str(), gcal["provider"].as_str()), (Some("connected"), Some("calendar")));
    let listed = rest(port, "GET", "/v1/connections", &erin, "");
    assert_eq!(listed.status, 200, "{}", listed.body);
    let listed = if listed.body.get("result").is_some() { listed.body["result"].clone() } else { listed.body.clone() };
    assert_eq!(listed["hosted"], true, "{listed}");
    assert_eq!(service_row(&listed, "gcal")["state"], "connected");
    assert_eq!(service_row(&listed, "gmail")["state"], "disconnected");
    assert_eq!(service_row(&listed, "gdrive")["state"], "coming_soon");
    let over_mcp = mcp_tool(port, &erin, "connections.list", serde_json::json!({}));
    let over_mcp = if over_mcp.get("result").is_some() { over_mcp["result"].clone() } else { over_mcp };
    assert_eq!(service_row(&over_mcp, "gcal")["state"], "connected");
    // Another principal sees nothing of it.
    let theirs = rest(port, "GET", "/v1/connections", &frank, "");
    let theirs = if theirs.body.get("result").is_some() { theirs.body["result"].clone() } else { theirs.body.clone() };
    assert_eq!(service_row(&theirs, "gcal")["state"], "disconnected");
    assert!(!listed.to_string().contains("ya29") && !over_mcp.to_string().contains("ya29"));

    // Disconnect from the REST face: revoked at Google, deleted from the store.
    let gone = rest(port, "POST", "/v1/connections/disconnect", &erin, r#"{"service":"gcal"}"#);
    assert!(gone.status == 200 || gone.status == 202, "{} {}", gone.status, gone.body);
    assert!(PrincipalGrants::new(tokens.as_ref(), "prn_erin").load(Service::GoogleCalendar).unwrap().is_none());
    let revoke = vendor.forms.lock().unwrap().last().unwrap().clone();
    assert_eq!(revoke, vec![("token".to_string(), "1//flow-refresh".to_string())]);
    // E51: the answer keeps the standard write receipt and says what the vendor did.
    assert_eq!(gone.body["accepted"], true, "{}", gone.body);
    assert_eq!((gone.body["tool"].as_str(), gone.body["level"].as_str()), (Some("connections.disconnect"), Some("L1")), "{}", gone.body);
    assert_eq!(
        (gone.body["disconnected"].as_bool(), gone.body["provider"].as_str(), gone.body["slot"].as_str(), gone.body["removed"].as_bool(), gone.body["vendor_revoke"].as_str()),
        (Some(true), Some("calendar"), Some("gcal-tokenset"), Some(true), Some("revoked")),
        "{}",
        gone.body
    );
    let after = mcp_tool(port, &erin, "connections.list", serde_json::json!({}));
    let after = if after.get("result").is_some() { after["result"].clone() } else { after };
    assert_eq!(service_row(&after, "gcal")["state"], "disconnected");

    // The same on the MCP face: connect again, disconnect over MCP, the same receipt fields.
    let state_again = "gateway-sealed-state-erin-again-0123";
    call(port, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&erin), &format!(r#"{{"provider":"calendar","state":"{state_again}"}}"#));
    let done = call(port, "POST", "/v1/internal/connectors/connect/finish", Some(ADMIN), Some(&erin), &format!(r#"{{"state":"{state_again}","code":"vendor-code-1"}}"#));
    assert_eq!(done.status, 200, "{}", done.body);
    let over_mcp = mcp_tool(port, &erin, "connections.disconnect", serde_json::json!({ "service": "gcal" }));
    assert_eq!(over_mcp["accepted"], true, "{over_mcp}");
    for key in ["disconnected", "provider", "slot", "removed", "vendor_revoke"] {
        assert_eq!(over_mcp[key], gone.body[key], "{key}: {over_mcp} vs {}", gone.body);
    }
    let twice = mcp_tool(port, &erin, "connections.disconnect", serde_json::json!({ "service": "gcal" }));
    assert_eq!((twice["removed"].as_bool(), twice["vendor_revoke"].as_str()), (Some(false), Some("nothing_stored")), "{twice}");
    assert!(!gone.body.to_string().contains("ya29") && !over_mcp.to_string().contains("1//flow-refresh"));
    // The internal route disconnects too (idempotent: nothing stored any more).
    let again = call(port, "POST", "/v1/internal/connectors/disconnect", Some(ADMIN), Some(&erin), r#"{"provider":"calendar"}"#);
    assert_eq!((again.status, again.body["removed"].as_bool(), again.body["vendor_revoke"].as_str()), (200, Some(false), Some("nothing_stored")));

    // The audit rows are in Erin's own store, name the action and the vendor host, carry no bytes
    // and no value; Frank's store has none.
    let rows = audit_rows(&store, "prn_erin");
    for expected in ["connector.connect.calendar.ok", "connector.revoke.calendar.ok", "connector.disconnect.calendar.ok"] {
        assert!(rows.iter().any(|(p, _, _)| p == expected), "{expected} in {rows:?}");
    }
    assert!(rows.iter().any(|(p, d, _)| p == "connector.connect.calendar.ok" && d == "oauth2.googleapis.com"));
    assert!(rows.iter().all(|(_, _, bytes)| *bytes == 0));
    assert!(!format!("{rows:?}").contains("ya29") && !format!("{rows:?}").contains("1//flow-refresh") && !format!("{rows:?}").contains("vendor-code-1"));
    assert!(audit_rows(&store, "prn_frank").is_empty());
}

#[test]
fn a_pending_flow_survives_a_sidecar_restart() {
    let root = TempRoot::new("restart");
    let tokens = Arc::new(MemorySecretStore::new());
    let gina = bearer("prn_gina", "key_03");
    let state = "gateway-sealed-state-gina-0123456789";
    let first = boot(root.0.clone(), tokens.clone(), FakeVendor::default());
    let started = call(first, "POST", "/v1/internal/connectors/connect/start", Some(ADMIN), Some(&gina), &format!(r#"{{"provider":"calendar","state":"{state}"}}"#));
    assert_eq!(started.status, 200);
    // A new sidecar process over the same principal store finishes it.
    let second = boot(root.0.clone(), tokens.clone(), FakeVendor::default());
    let done = call(second, "POST", "/v1/internal/connectors/connect/finish", Some(ADMIN), Some(&gina), &format!(r#"{{"state":"{state}","code":"vendor-code-1"}}"#));
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(PrincipalGrants::new(tokens.as_ref(), "prn_gina").load(Service::GoogleCalendar).unwrap().is_some());
}

/// Gmail behind the read-sync seam: two messages, one with an attachment that is never read.
struct FakeGmail {
    calls: Mutex<Vec<String>>,
}

impl ProviderHttp for FakeGmail {
    fn get(&self, url: &str, bearer: &Secret, _headers: &[(&'static str, &'static str)]) -> Result<String, FetchError> {
        self.calls.lock().unwrap().push(url.to_string());
        if *bearer != Secret::new("ya29.hana-gmail-token") {
            return Err(FetchError::Unauthorized);
        }
        let base = url.split('?').next().unwrap_or(url);
        Ok(match base.rsplit('/').next().unwrap_or("") {
            "messages" => r#"{"messages":[{"id":"m1"}]}"#.to_string(),
            "m1" => serde_json::json!({
                "id": "m1", "threadId": "t1", "historyId": "77", "labelIds": ["INBOX"],
                "payload": { "headers": [{ "name": "Subject", "value": "Contract draft" }, { "name": "From", "value": "legal@example.com" }],
                    "parts": [{ "mimeType": "text/plain", "body": { "data": "UGxlYXNlIHJldmlldyBieSBUaHVyc2RheS4" } },
                              { "mimeType": "application/pdf", "filename": "contract.pdf", "body": { "attachmentId": "a1" } }] }
            })
            .to_string(),
            _ => return Err(FetchError::NotFound),
        })
    }
}

#[test]
fn a_gmail_notice_is_queued_and_the_tick_fetches_it_into_the_principals_store_once() {
    let root = TempRoot::new("readsync");
    let tokens = Arc::new(MemorySecretStore::new());
    let (port, store, vault) = boot_with(root.0.clone(), tokens.clone(), FakeVendor::default());
    let hana = bearer("prn_hana", "key_04");
    let far = wall_clock()() + 3_600_000;
    PrincipalGrants::new(tokens.as_ref(), "prn_hana")
        .save(Service::Gmail, &TokenSet { access_token: "ya29.hana-gmail-token".into(), refresh_token: Some("r".into()), expires_at_ms: far, account_label: None })
        .unwrap();
    let notice = r#"{"provider":"gmail","delivery_id":"psm-1","idempotency_key":"hook:gmail:psm-1","changed":{"history_id":"77"}}"#;
    let queued = rest(port, "POST", "/v1/memory/ingest-webhook", &hana, notice);
    assert_eq!(queued.status, 200, "{}", queued.body);
    assert_eq!(queued.body["result"]["outcome"], "changed");
    assert_eq!(queued.body["result"]["fetch"], "queued");
    assert_eq!(queued.body["usage"]["quantity"], 0);

    let principals = vec![PrincipalId::parse("prn_hana").unwrap(), PrincipalId::parse("prn_ivan").unwrap()];
    let http = FakeGmail { calls: Mutex::new(Vec::new()) };
    let now = wall_clock()();
    let tick = shogun_core::tenancy::connector_sync::drain_tick(&store, &vault, &http, &principals, now);
    let hana_line = tick[0].outcome.clone().unwrap().unwrap();
    assert_eq!((hana_line.jobs, hana_line.objects, hana_line.stored), (1, 1, 1), "{hana_line:?}");
    assert_eq!(tick[1].outcome, None, "nothing queued for Ivan: no store opened, no call");
    let calls = http.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|u| !u.contains("attachments")));
    let summary = shogun_core::tenancy::connector_sync::summary(&tick);
    assert!(summary.contains("stored=1"), "{summary}");
    // The fetch is on Hana's traceability log; the stored event is searchable in her store only.
    assert!(audit_rows(&store, "prn_hana").iter().any(|(p, d, _)| p == "connector.sync.gmail.ok" && d == "gmail.googleapis.com"));
    let found = mcp_tool(port, &hana, "memory.search", serde_json::json!({ "query": "review by Thursday" }));
    assert!(found.to_string().contains("Contract draft") || found.to_string().contains("Thursday"), "{found}");
    let ivan = bearer("prn_ivan", "key_04");
    let not_found = mcp_tool(port, &ivan, "memory.search", serde_json::json!({ "query": "review by Thursday" }));
    assert!(!not_found.to_string().contains("Thursday"), "{not_found}");

    // The same notice again: the same message at the same version is not stored twice.
    rest(port, "POST", "/v1/memory/ingest-webhook", &hana, &notice.replace("psm-1", "psm-2"));
    let tick = shogun_core::tenancy::connector_sync::drain_tick(&store, &vault, &http, &principals, now + 1);
    let again = tick[0].outcome.clone().unwrap().unwrap();
    assert_eq!((again.objects, again.stored), (1, 0));
}

/// Synthetic versioned document provider. No network or token exchange is performed.
struct FakeNotionVersions {
    version: std::sync::atomic::AtomicUsize,
    fail_next: std::sync::atomic::AtomicBool,
}

impl FakeNotionVersions {
    fn page_and_blocks(&self) -> (serde_json::Value, serde_json::Value) {
        let version = self.version.load(std::sync::atomic::Ordering::SeqCst);
        let day = if version == 1 { 20 } else { 21 };
        let body = format!(
            "Deadline: 2030-04-{day}.\n{}",
            "Synthetic launch planning context: the team reviews the draft and records the next steps. ".repeat(60)
        );
        (
            serde_json::json!({
                "id": "synthetic-launch-page", "last_edited_time": format!("2026-10-29T1{version}:00:00Z"),
                "url": "https://example.test/synthetic-launch-page",
                "properties": {"title": {"type": "title", "title": [{"plain_text": "Synthetic launch plan"}]}},
            }),
            serde_json::json!({"results": [{"type": "paragraph", "paragraph": {"rich_text": [{"plain_text": body}]}}]}),
        )
    }

    fn request(&self) -> shogun_mcp::ingest_api::IngestRequest {
        let (page, blocks) = self.page_and_blocks();
        let object = shogun_integrations::read_sync::notion_page(&page, &blocks).unwrap();
        shogun_integrations::read_sync::ingest_request(shogun_integrations::read_sync::SyncProvider::Notion, &object)
    }
}

impl ProviderHttp for FakeNotionVersions {
    fn get(&self, url: &str, bearer: &Secret, headers: &[(&'static str, &'static str)]) -> Result<String, FetchError> {
        assert_eq!(*bearer, Secret::new("synthetic-notion-read-token"));
        assert_eq!(headers, &[("Notion-Version", shogun_integrations::read_sync::NOTION_VERSION)]);
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(FetchError::Transient);
        }
        let (page, blocks) = self.page_and_blocks();
        match url.split('?').next().unwrap_or(url) {
            "https://api.notion.com/v1/pages/synthetic-launch-page" => Ok(page.to_string()),
            "https://api.notion.com/v1/blocks/synthetic-launch-page/children" => Ok(blocks.to_string()),
            _ => Err(FetchError::NotFound),
        }
    }
}

#[test]
fn versioned_notion_snapshot_retains_small_factual_edits_and_replays_after_restart() {
    use shogun_integrations::read_sync::{enqueue, pending, RETRY_AFTER_MS};
    use shogun_mcp::backend::MemoryBackend;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    println!("SYNTHETIC CONNECTOR REPLAY: fake Notion responses, real principal stores; no network or model calls.");
    let root = TempRoot::new("versioned_notion");
    let now = 1_800_000_000_000;
    let tokens = Arc::new(MemorySecretStore::new());
    let vault = HostedConnectors::new(tokens.clone(), HostedClients::default(), || Ok(FakeVendor::default()));
    let store = PrincipalStore::new(&root.0, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x67; 32]))), Arc::new(move || now), 4);
    let alice = PrincipalId::parse("snapshot_alice").unwrap();
    let bob = PrincipalId::parse("snapshot_bob").unwrap();
    PrincipalGrants::new(tokens.as_ref(), alice.as_str()).save(Service::Notion, &TokenSet {
        access_token: "synthetic-notion-read-token".into(), refresh_token: None,
        expires_at_ms: now + 3_600_000, account_label: None,
    }).unwrap();
    let http = FakeNotionVersions { version: AtomicUsize::new(1), fail_next: AtomicBool::new(false) };
    let notice = serde_json::json!({"entity_type": "page", "entity_id": "synthetic-launch-page"});
    let queue = |at| enqueue(&vault, alice.as_str(), "notion", &notice, at).unwrap();
    let tick = |at| shogun_core::tenancy::connector_sync::drain_tick(&store, &vault, &http, std::slice::from_ref(&alice), at)[0]
        .outcome.clone().unwrap().unwrap();

    queue(now);
    assert_eq!(tick(now).stored, 1);
    let first_request = http.request();
    let first = store.open(&alice).unwrap().memory_backend().ingest_versioned_snapshot(&first_request).unwrap();
    assert!(first.replayed);
    assert_eq!(first.event_ids.len(), 1);

    http.version.store(2, Ordering::SeqCst);
    let revised_request = http.request();
    assert_ne!(first_request.idempotency_key, revised_request.idempotency_key);
    assert!(shogun_core::capture::dedup::similarity(&first_request.items[0].content, &revised_request.items[0].content)
        >= shogun_core::capture::dedup::NEAR_DUP_THRESHOLD, "fixture must expose near-duplicate loss");
    // A failed fetch leaves the prior snapshot and retries the queued notice without a receipt.
    http.fail_next.store(true, Ordering::SeqCst);
    queue(now + 1);
    assert_eq!(tick(now + 1).retried, 1);
    assert_eq!(pending(&vault, alice.as_str()).unwrap().len(), 1);
    assert_eq!(store.open(&alice).unwrap().db().try_events_in_range(0, i64::MAX).unwrap().len(), 1);
    assert_eq!(tick(now + RETRY_AFTER_MS + 1).stored, 1, "a changed version must retain its corrected body");

    let handle = store.open(&alice).unwrap();
    let backend = handle.memory_backend();
    let revised = MemoryBackend::ingest_versioned_snapshot(&backend, &revised_request).unwrap();
    assert!(revised.replayed);
    assert_eq!(revised.inserted, 0);
    assert_ne!(revised.event_ids, first.event_ids);
    let events = handle.db().events_json(&[first.event_ids[0], revised.event_ids[0]], 16_000).unwrap();
    assert!(events["events"][0]["text"].as_str().unwrap().contains("Deadline: 2030-04-20."));
    assert!(events["events"][1]["text"].as_str().unwrap().contains("Deadline: 2030-04-21."));
    for (request, ids) in [(&first_request, &first.event_ids), (&revised_request, &revised.event_ids)] {
        let ts = request.items[0].ts_ms.unwrap();
        let dated = handle.db().try_events_in_range(ts, ts + 1).unwrap();
        assert_eq!(dated.len(), 1);
        assert_eq!(dated[0].id, ids[0]);
        assert_eq!(dated[0].content, request.items[0].content, "preserve the entire dated snapshot and provider footer");
    }

    // A new provider timestamp remains dated evidence even when the body is unchanged.
    http.version.store(3, Ordering::SeqCst);
    let unchanged_request = http.request();
    assert_ne!(unchanged_request.idempotency_key, revised_request.idempotency_key);
    let unchanged = backend.ingest_versioned_snapshot(&unchanged_request).unwrap();
    assert_eq!((unchanged.inserted, unchanged.deduplicated, unchanged.near_duplicates), (1, 0, 0));
    assert_ne!(unchanged.event_ids, revised.event_ids);
    let other = store.open(&bob).unwrap();
    assert!(other.db().try_events_in_range(0, i64::MAX).unwrap().is_empty());
    assert!(other.db().events_json(&revised.event_ids, 16_000).unwrap()["events"].as_array().unwrap().is_empty());
    // The same connector-looking source/metadata on generic ingest must not select the
    // trusted policy. Bob's separate receipts also cannot replay Alice's accepted keys.
    let generic_first = other.memory_backend().ingest(&first_request).unwrap();
    assert!(!generic_first.replayed);
    assert_eq!(generic_first.inserted, 1);
    let generic_revision = other.memory_backend().ingest(&revised_request).unwrap();
    assert_eq!((generic_revision.inserted, generic_revision.deduplicated, generic_revision.near_duplicates), (0, 1, 1));
    assert_eq!(generic_revision.event_ids, generic_first.event_ids);

    drop(other);
    drop(backend);
    drop(handle);
    store.close_all();
    assert_eq!(store.live_handles(), 0);
    let handle = store.open(&alice).unwrap();
    for (request, ids) in [(&first_request, &first.event_ids), (&revised_request, &revised.event_ids), (&unchanged_request, &unchanged.event_ids)] {
        let replay = handle.memory_backend().ingest_versioned_snapshot(request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.inserted, 0);
        assert_eq!(&replay.event_ids, ids);
    }
    assert_eq!(handle.db().try_events_in_range(0, i64::MAX).unwrap().len(), 3);
}

#[test]
fn refetched_snapshot_repairs_a_lossy_pre_upgrade_receipt() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    println!("SYNTHETIC UPGRADE REPLAY: refetch repairs an old lossy receipt; no automatic backfill or network calls.");
    let root = TempRoot::new("snapshot_receipt_upgrade");
    let store = PrincipalStore::new(&root.0, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x68; 32]))), Arc::new(|| 1_800_000_000_000), 2);
    let principal = PrincipalId::parse("snapshot_upgrade").unwrap();
    let http = FakeNotionVersions { version: AtomicUsize::new(1), fail_next: AtomicBool::new(false) };
    let first_request = http.request();
    http.version.store(2, Ordering::SeqCst);
    let revised_request = http.request();

    // Reproduce the old read-sync dispatch, including its successful but lossy receipt.
    let handle = store.open(&principal).unwrap();
    let backend = handle.memory_backend();
    let original = backend.ingest(&first_request).unwrap();
    let lost_revision = backend.ingest(&revised_request).unwrap();
    assert_eq!((lost_revision.inserted, lost_revision.near_duplicates), (0, 1));
    assert_eq!(lost_revision.event_ids, original.event_ids);
    drop(backend);
    drop(handle);
    store.close_all();

    // Merely reopening does not backfill anything. Repair happens when that version is fetched.
    let handle = store.open(&principal).unwrap();
    assert_eq!(handle.db().try_events_in_range(0, i64::MAX).unwrap().len(), 1);
    let backend = handle.memory_backend();
    let repaired = backend.ingest_versioned_snapshot(&revised_request).unwrap();
    assert_eq!(repaired.inserted, 1, "an old lossy receipt must not block a refetched correction");
    assert!(!repaired.replayed);
    assert_ne!(repaired.event_ids, original.event_ids);
    let exact_original = backend.ingest_versioned_snapshot(&first_request).unwrap();
    assert_eq!((exact_original.inserted, exact_original.deduplicated), (0, 1));
    assert_eq!(exact_original.event_ids, original.event_ids);
    let ordinary_replay = backend.ingest(&revised_request).unwrap();
    assert!(ordinary_replay.replayed);
    assert_eq!(ordinary_replay.event_ids, original.event_ids, "ordinary receipts are retained, not overwritten");
    drop(backend);
    drop(handle);
    store.close_all();

    let handle = store.open(&principal).unwrap();
    let replay = handle.memory_backend().ingest_versioned_snapshot(&revised_request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.event_ids, repaired.event_ids);
    assert_eq!(handle.db().try_events_in_range(0, i64::MAX).unwrap().len(), 2);
    let events = handle.db().events_json(&[original.event_ids[0], repaired.event_ids[0]], 16_000).unwrap();
    assert_eq!(events["events"][0]["text"].as_str(), Some(first_request.items[0].content.as_str()));
    assert_eq!(events["events"][1]["text"].as_str(), Some(revised_request.items[0].content.as_str()));
}

#[test]
fn reverting_snapshot_and_unchanged_new_versions_retain_fresh_dated_evidence() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    println!("SYNTHETIC VERSION REVERSION: deadline 21 -> 22 -> 21; no network or model calls.");
    let root = TempRoot::new("snapshot_reversion");
    let store = PrincipalStore::new(&root.0, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x69; 32]))), Arc::new(|| 1_800_000_000_000), 2);
    let principal = PrincipalId::parse("snapshot_reversion").unwrap();
    let http = FakeNotionVersions { version: AtomicUsize::new(1), fail_next: AtomicBool::new(false) };
    let handle = store.open(&principal).unwrap();
    let backend = handle.memory_backend();
    let mut requests = Vec::new();
    let mut event_ids = Vec::new();
    for (version, day) in [(1, 21), (2, 22), (3, 21), (4, 21)] {
        http.version.store(version, Ordering::SeqCst);
        let mut request = http.request();
        let fixture_day = if version == 1 { 20 } else { 21 };
        request.items[0].content = request.items[0].content.replace(
            &format!("Deadline: 2030-04-{fixture_day}."), &format!("Deadline: 2030-04-{day}."),
        );
        let outcome = backend.ingest_versioned_snapshot(&request).unwrap();
        assert!(!outcome.replayed);
        assert_eq!(outcome.inserted, 1, "new timestamp for version {version} must retain fresh evidence, including unchanged or historical wording");
        assert_eq!(outcome.deduplicated, 0);
        assert!(!event_ids.contains(&outcome.event_ids[0]));
        let ts = request.items[0].ts_ms.unwrap();
        let dated = handle.db().try_events_in_range(ts, ts + 1).unwrap();
        assert_eq!(dated.len(), 1);
        assert_eq!(dated[0].id, outcome.event_ids[0]);
        assert_eq!(dated[0].content, request.items[0].content);
        let replay = backend.ingest_versioned_snapshot(&request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.event_ids, outcome.event_ids);
        event_ids.push(outcome.event_ids[0]);
        requests.push(request);
    }
    assert_eq!(requests[0].items[0].content, requests[2].items[0].content);
    assert_ne!(event_ids[0], event_ids[2], "a historical matching body is distinct fresh evidence");
    drop(backend);
    drop(handle);
    store.close_all();
    let handle = store.open(&principal).unwrap();
    for (request, event_id) in requests.iter().zip(&event_ids) {
        let replay = handle.memory_backend().ingest_versioned_snapshot(request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.inserted, 0);
        assert_eq!(replay.event_ids, vec![*event_id]);
    }
    assert_eq!(handle.db().try_events_in_range(0, i64::MAX).unwrap().len(), 4);
}

#[test]
fn late_changed_snapshot_cannot_replace_newer_unchanged_evidence() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    println!("SYNTHETIC OUT-OF-ORDER REPLAY: A11 -> unchanged A13 -> late B12; no network or model calls.");
    let root = TempRoot::new("snapshot_out_of_order");
    let store = PrincipalStore::new(&root.0, Arc::new(EnvKeyProvider::with_default(DbKey::new([0x70; 32]))), Arc::new(|| 1_800_000_000_000), 2);
    let principal = PrincipalId::parse("snapshot_out_of_order").unwrap();
    let http = FakeNotionVersions { version: AtomicUsize::new(1), fail_next: AtomicBool::new(false) };
    let handle = store.open(&principal).unwrap();
    let mut accepted = Vec::new();
    for (version, day) in [(1, 21), (3, 21), (2, 22)] {
        http.version.store(version, Ordering::SeqCst);
        let mut request = http.request();
        let fixture_day = if version == 1 { 20 } else { 21 };
        request.items[0].content = request.items[0].content.replace(
            &format!("Deadline: 2030-04-{fixture_day}."), &format!("Deadline: 2030-04-{day}."),
        );
        let outcome = handle.memory_backend().ingest_versioned_snapshot(&request).unwrap();
        assert_eq!(outcome.inserted, 1, "each distinct provider timestamp remains dated evidence");
        accepted.push((request, outcome.event_ids));
    }
    let assert_latest = |db: &shogun_core::daemon::Db| {
        let events = db.try_events_in_range(0, i64::MAX).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].id, accepted[1].1[0], "late B12 cannot supersede unchanged A13");
        assert_eq!(events[2].content, accepted[1].0.items[0].content);
        assert_eq!(events[1].id, accepted[2].1[0], "the late revision retains its original provider time");
    };
    assert_latest(handle.db());
    drop(handle);
    store.close_all();
    let handle = store.open(&principal).unwrap();
    for (request, ids) in &accepted {
        let replay = handle.memory_backend().ingest_versioned_snapshot(request).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.inserted, 0);
        assert_eq!(&replay.event_ids, ids);
    }
    assert_latest(handle.db());
}
