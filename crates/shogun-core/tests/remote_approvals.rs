//! E15 remote approvals through the real `shogun-api --multi` server: an agent's L3 send is
//! queued with an approval URL on the gateway (`/approve/<token>`, the REST pending envelope
//! carries it, E4 follow-up), the gateway reads a redacted summary with the token and decides it
//! once; a replayed, expired, forged or mismatched token is refused, an agent's own bearer never
//! decides, and every step leaves an audit record (E18) with labels only.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::{json, Value};
use shogun_cli::http::request;
use shogun_core::daemon::Clock;
use shogun_core::tenancy::{EnvKeyProvider, PrincipalStore, TokenPrincipalMap};
use shogun_mcp::remote_approvals::{ApprovalSigner, RemoteApprovals};
use shogun_mcp::server::{bind_local, serve_on, AppState};
use shogun_memory::DbKey;

const TOKEN_A: &str = "token-alice-0001";
const TOKEN_B: &str = "token-bob-00001";
const ADMIN: &str = "admin-credential-0123456789";
const KEY: &[u8] = b"gateway-and-sidecar-shared-key-0123456789";
const BASE: &str = "https://gw.example";

struct TempRoot(PathBuf);

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

fn now_ms() -> u64 {
    u64::try_from((wall_clock())()).unwrap()
}

fn temp_root(tag: &str) -> TempRoot {
    let dir = std::env::temp_dir().join(format!("shogun_remote_approvals_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempRoot(dir.canonicalize().unwrap())
}

/// Start one server object over the principal files under `root` (a second call over the same
/// root is a restart: fresh queue, fresh in-process ledger, the same stores).
fn serve(root: PathBuf, configure: impl FnOnce(AppState) -> AppState + Send + 'static) -> u16 {
    let keys = EnvKeyProvider::with_default(DbKey::new([0x5a; 32]));
    let store = Arc::new(PrincipalStore::new(root, Arc::new(keys), wall_clock(), 8));
    let resolver = TokenPrincipalMap::parse(&format!("alice:{TOKEN_A}:claude-code,bob:{TOKEN_B}")).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = configure(
                shogun_core::tenancy::multi::app_state(store, Arc::new(resolver), wall_clock())
                    .with_admin_token(Some(ADMIN))
                    .with_remote_approvals(
                        RemoteApprovals::default().with_base_url(Some(BASE.into())).with_signer(ApprovalSigner::new(KEY)),
                    ),
            );
            let listener = bind_local(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    port
}

fn boot() -> (u16, TempRoot) {
    let root = temp_root("main");
    (serve(root.0.clone(), |state| state), root)
}

/// One request with extra headers; returns (status, raw headers, body).
fn raw(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, head.to_ascii_lowercase(), body.to_string())
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not JSON: {body}"))
}

fn send(port: u16, to: &str) -> (u64, String, String) {
    let body = json!({ "kind": "send_email", "to": to, "subject": "Q3", "body": "Forecast attached. password: hunter2hunter2" }).to_string();
    let bearer = format!("Bearer {TOKEN_A}");
    let (status, head, body) = raw(port, "POST", "/v1/actions/execute", &[("Authorization", &bearer)], &body);
    assert_eq!(status, 202, "{body}");
    let v = json_of(&body);
    assert_eq!(v["pending"], true, "{body}");
    assert_eq!(v["next_action"], "approve_at_url", "{body}");
    let url = v["url"].as_str().unwrap().to_string();
    let token = url.strip_prefix(&format!("{BASE}/approve/")).unwrap_or_else(|| panic!("{url}")).to_string();
    // The gateway learns of the approval from a header, never the body.
    assert!(head.contains(&format!("x-shogun-approval-token: {}", token.to_ascii_lowercase())), "{head}");
    (v["approval_id"].as_u64().unwrap(), token, url)
}

fn decide(port: u16, id: u64, token: &str, nonce: &str, decision: &str) -> (u16, String) {
    let body = json!({ "decision": decision, "decided_by": "web:approval-page", "nonce": nonce }).to_string();
    let (status, _, body) = raw(port, "POST", &format!("/v1/approvals/{id}/decide"), &[("X-Shogun-Approval-Token", token)], &body);
    (status, body)
}

#[test]
fn a_gateway_decides_once_with_a_signed_token_and_an_agent_never_does() {
    let (port, _root) = boot();
    let signer = ApprovalSigner::new(KEY).unwrap();
    let (id, token, _url) = send(port, "aiko@example.com");
    let claims = signer.verify(&token, now_ms()).unwrap();
    assert_eq!((claims.approval_id, claims.principal.as_deref()), (id, Some("alice")));

    // The gateway reads the redacted summary with the token: kind, tier, recipient class and a
    // masked preview, never the recipient or the secret in the body.
    let (status, _, summary) = raw(port, "GET", &format!("/v1/approvals/{id}"), &[("X-Shogun-Approval-Token", &token)], "");
    assert_eq!(status, 200, "{summary}");
    let s = &json_of(&summary)["approval"];
    assert_eq!(s["status"], "pending");
    assert_eq!(s["level"], "L3");
    assert_eq!(s["recipient_class"], "email_recipient");
    assert!(s["expires_at"].is_string() && s["created_at"].is_string(), "{summary}");
    assert!(!summary.contains("aiko@example.com") && !summary.contains("hunter2hunter2"), "{summary}");

    // The agents' reads: Alice sees her approval, Bob sees none of it.
    let mine = request(port, "GET", "/v1/approvals", Some(TOKEN_A), None).unwrap();
    assert_eq!(json_of(&mine.body)["approvals"][0]["approval_id"], id, "{}", mine.body);
    let theirs = request(port, "GET", "/v1/approvals", Some(TOKEN_B), None).unwrap();
    assert_eq!(json_of(&theirs.body)["approvals"], json!([]), "{}", theirs.body);
    assert_eq!(request(port, "GET", &format!("/v1/approvals/{id}"), Some(TOKEN_B), None).unwrap().status, 404);

    // An agent's own bearer never decides, even naming the right nonce.
    let agent = request(
        port,
        "POST",
        &format!("/v1/approvals/{id}/decide"),
        Some(TOKEN_A),
        Some(&json!({ "decision": "approve", "decided_by": "agent:claude-code", "nonce": claims.nonce }).to_string()),
    )
    .unwrap();
    assert_eq!(agent.status, 403, "{}", agent.body);
    assert!(agent.body.contains("human_only"), "{}", agent.body);

    // Forged, mismatched and expired tokens are refused before anything changes.
    let forged = ApprovalSigner::new(b"some-other-key-that-is-long-enough!!").unwrap().mint(id, Some("alice"), now_ms() + 60_000).unwrap();
    assert_eq!(decide(port, id, &forged.0, &forged.1.nonce, "approve").0, 403);
    assert_eq!(decide(port, id, &token, "not-the-nonce", "approve").0, 403);
    let expired = signer.mint(id, Some("alice"), 1).unwrap();
    let (status, body) = decide(port, id, &expired.0, &expired.1.nonce, "approve");
    assert_eq!(status, 410, "{body}");
    assert!(body.contains("token_expired"), "{body}");
    let bob_token = signer.mint(id, Some("bob"), now_ms() + 60_000).unwrap();
    assert_eq!(decide(port, id, &bob_token.0, &bob_token.1.nonce, "approve").0, 404, "a token for another principal");

    // The real token decides once…
    let (status, body) = decide(port, id, &token, &claims.nonce, "approve");
    assert_eq!(status, 200, "{body}");
    let decided = json_of(&body);
    assert_eq!((decided["status"].as_str(), decided["decided_by"].as_str()), (Some("approved"), Some("web:approval-page")));
    // …and a replay is refused.
    let (status, body) = decide(port, id, &token, &claims.nonce, "approve");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("token_replayed"), "{body}");

    // The poll and the summary report the decision and who made it.
    let poll = request(port, "GET", &format!("/v1/actions/status/{id}"), Some(TOKEN_A), None).unwrap();
    assert!(poll.body.contains("\"approved\""), "{}", poll.body);
    let after = request(port, "GET", &format!("/v1/approvals/{id}"), Some(TOKEN_A), None).unwrap();
    assert_eq!(json_of(&after.body)["approval"]["decision"]["decided_by"], "web:approval-page", "{}", after.body);

    // The operator decides from the CLI with the admin credential (a second approval, rejected).
    let (second, _, _) = send(port, "kenji@example.com");
    let operator = request(
        port,
        "POST",
        &format!("/v1/approvals/{second}/decide"),
        Some(ADMIN),
        Some(&json!({ "decision": "reject", "decided_by": "human:operator" }).to_string()),
    )
    .unwrap();
    assert_eq!(operator.status, 200, "{}", operator.body);
    assert_eq!(json_of(&operator.body)["status"], "rejected");

    // Alice's audit log: the two queued sends and the two decisions, labels only.
    let audit = request(port, "GET", "/v1/audit", Some(TOKEN_A), None).unwrap();
    let records = json_of(&audit.body)["records"].as_array().cloned().unwrap();
    let rows: Vec<(String, String, String)> = records
        .iter()
        .map(|r| (r["route"].as_str().unwrap().into(), r["outcome"].as_str().unwrap().into(), r["actor"].as_str().unwrap().into()))
        .collect();
    assert!(rows.contains(&("actions.execute".into(), "pending".into(), "agent:claude-code".into())), "{rows:?}");
    assert!(rows.contains(&("approvals.decide".into(), "approved".into(), "web:approval-page".into())), "{rows:?}");
    assert!(rows.contains(&("approvals.decide".into(), "rejected".into(), "human:operator".into())), "{rows:?}");
    assert!(!audit.body.contains("hunter2") && !audit.body.contains("example.com"), "{}", audit.body);
    // Bob's audit log holds none of it.
    let bob = request(port, "GET", "/v1/audit", Some(TOKEN_B), None).unwrap();
    assert_eq!(json_of(&bob.body)["records"], json!([]), "{}", bob.body);
}

/// V103: the spent nonce and the decision live in the principal's store, so a restarted server
/// (a new server object over the same files, with an empty queue and an empty in-process ledger)
/// still refuses the replayed token and still reports who decided.
#[test]
fn a_restart_forgets_neither_the_spent_nonce_nor_the_decision() {
    let root = temp_root("restart");
    let port = serve(root.0.clone(), |state| state);
    let signer = ApprovalSigner::new(KEY).unwrap();
    let (id, token, _url) = send(port, "aiko@example.com");
    let claims = signer.verify(&token, now_ms()).unwrap();
    let (status, body) = decide(port, id, &token, &claims.nonce, "approve");
    assert_eq!(status, 200, "{body}");
    // This build's REST face carries no executor in this configuration: the approval stays
    // approved and says so.
    assert_eq!(json_of(&body)["next_action"], "execute_unavailable", "{body}");

    let restarted = serve(root.0.clone(), |state| state);
    let (status, body) = decide(restarted, id, &token, &claims.nonce, "approve");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("token_replayed"), "{body}");
    // A reject replay of the same token is the same replay.
    assert_eq!(decide(restarted, id, &token, &claims.nonce, "reject").0, 409);

    // The decision survives: the summary and the poll read it from Alice's store.
    let after = request(restarted, "GET", &format!("/v1/approvals/{id}"), Some(TOKEN_A), None).unwrap();
    assert_eq!(after.status, 200, "{}", after.body);
    let approval = &json_of(&after.body)["approval"];
    assert_eq!((approval["status"].as_str(), approval["decision"]["decided_by"].as_str()), (Some("approved"), Some("web:approval-page")));
    let poll = request(restarted, "GET", &format!("/v1/actions/status/{id}"), Some(TOKEN_A), None).unwrap();
    let poll = json_of(&poll.body);
    assert_eq!((poll["status"].as_str(), poll["next_action"].as_str()), (Some("approved"), Some("execute_unavailable")));
    // Bob's store holds none of it.
    assert_eq!(request(restarted, "GET", &format!("/v1/approvals/{id}"), Some(TOKEN_B), None).unwrap().status, 404);
    let bob_poll = request(restarted, "GET", &format!("/v1/actions/status/{id}"), Some(TOKEN_B), None).unwrap();
    assert_eq!(json_of(&bob_poll.body)["status"], "unknown", "{}", bob_poll.body);
    // An approval queued after the restart gets a fresh id: a persisted decision never names it.
    let (fresh, _, _) = send(restarted, "kenji@example.com");
    assert!(fresh > id, "{fresh} <= {id}");
}
