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
use shogun_mcp::approval_exec::{ApprovalExecutor, ExecOutcome, ExecRequest};
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
    serve_with_clock(root, wall_clock(), configure)
}

fn serve_with_clock(root: PathBuf, clock: Clock, configure: impl FnOnce(AppState) -> AppState + Send + 'static) -> u16 {
    let keys = EnvKeyProvider::with_default(DbKey::new([0x5a; 32]));
    let store = Arc::new(PrincipalStore::new(root, Arc::new(keys), clock.clone(), 8));
    let resolver = TokenPrincipalMap::parse(&format!("alice:{TOKEN_A}:claude-code,bob:{TOKEN_B}")).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
        rt.block_on(async move {
            let state = configure(
                shogun_core::tenancy::multi::app_state(store, Arc::new(resolver), clock)
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

/// A fake executor: counts calls, remembers what it was handed, answers a fixed outcome. It
/// sleeps a little so racing decisions overlap it.
struct CountingExecutor {
    calls: std::sync::atomic::AtomicUsize,
    seen: std::sync::Mutex<Vec<(u64, Option<String>, String)>>,
    outcome: ExecOutcome,
}

impl CountingExecutor {
    fn new(outcome: ExecOutcome) -> Arc<Self> {
        Arc::new(Self { calls: Default::default(), seen: Default::default(), outcome })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ApprovalExecutor for CountingExecutor {
    fn execute(&self, request: &ExecRequest<'_>) -> ExecOutcome {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let to = match &request.confirmed.action {
            shogun_agents::permission::SendAction::SendEmail { to } => to.clone(),
            _ => String::new(),
        };
        self.seen.lock().unwrap().push((request.approval_id, request.principal.map(str::to_string), to));
        std::thread::sleep(Duration::from_millis(50));
        self.outcome.clone()
    }
}

fn admin_decide(port: u16, id: u64, decision: &str) -> (u16, String) {
    let r = request(
        port,
        "POST",
        &format!("/v1/approvals/{id}/decide"),
        Some(ADMIN),
        Some(&json!({ "decision": decision, "decided_by": "human:operator" }).to_string()),
    )
    .unwrap();
    (r.status, r.body)
}

fn audit_rows(port: u16, token: &str) -> Vec<(String, String)> {
    let audit = request(port, "GET", "/v1/audit", Some(token), None).unwrap();
    json_of(&audit.body)["records"]
        .as_array()
        .cloned()
        .unwrap()
        .iter()
        .map(|r| (r["route"].as_str().unwrap().to_string(), r["outcome"].as_str().unwrap().to_string()))
        .collect()
}

fn poll(port: u16, id: u64, token: &str) -> Value {
    json_of(&request(port, "GET", &format!("/v1/actions/status/{id}"), Some(token), None).unwrap().body)
}

/// E15 follow-up: an approved action runs exactly once, its outcome is recorded on the approval,
/// in the audit log and on `actions.status`; a rejected approval never runs.
#[test]
fn an_approved_action_runs_once_and_reports_its_outcome() {
    let root = temp_root("execute");
    let executor = CountingExecutor::new(ExecOutcome::Sent);
    let attached: Arc<dyn ApprovalExecutor> = executor.clone();
    let port = serve(root.0.clone(), move |state| state.with_approval_executor(attached));
    let signer = ApprovalSigner::new(KEY).unwrap();

    let (id, token, _) = send(port, "aiko@example.com");
    let claims = signer.verify(&token, now_ms()).unwrap();
    let (status, body) = decide(port, id, &token, &claims.nonce, "approve");
    assert_eq!(status, 200, "{body}");
    let decided = json_of(&body);
    assert_eq!((decided["status"].as_str(), decided["executed"].as_bool()), (Some("sent"), Some(true)), "{body}");
    assert!(decided["next_action"].is_null(), "{body}");
    assert_eq!(executor.calls(), 1);
    // The executor got exactly what the human approved, for the approving principal.
    assert_eq!(executor.seen.lock().unwrap()[0], (id, Some("alice".to_string()), "aiko@example.com".to_string()));

    // A second decision on the same id runs nothing: the replayed token and the operator alike.
    assert_eq!(decide(port, id, &token, &claims.nonce, "approve").0, 409);
    assert_eq!(admin_decide(port, id, "approve").0, 409);
    assert_eq!(executor.calls(), 1, "executed exactly once");

    let status = poll(port, id, TOKEN_A);
    assert_eq!(status["status"], "sent", "{status}");
    assert!(status.get("next_action").is_none(), "{status}");
    let summary = json_of(&request(port, "GET", &format!("/v1/approvals/{id}"), Some(TOKEN_A), None).unwrap().body);
    assert_eq!(summary["approval"]["status"], "sent", "{summary}");
    // Bob never sees Alice's outcome.
    assert_eq!(poll(port, id, TOKEN_B)["status"], "unknown");

    // A rejected approval never runs.
    let (rejected, _, _) = send(port, "kenji@example.com");
    let (status, body) = admin_decide(port, rejected, "reject");
    assert_eq!((status, json_of(&body)["status"].as_str()), (200, Some("rejected")), "{body}");
    assert_eq!(executor.calls(), 1);

    // The audit log names the decision and its execution, labels only.
    let rows = audit_rows(port, TOKEN_A);
    assert!(rows.contains(&("approvals.decide".into(), "approved".into())), "{rows:?}");
    assert!(rows.contains(&("approvals.execute".into(), "sent".into())), "{rows:?}");
    assert_eq!(rows.iter().filter(|(route, _)| route == "approvals.execute").count(), 1, "{rows:?}");

    // After a restart the outcome is still reported from Alice's store.
    let restarted = serve(root.0.clone(), |state| state);
    assert_eq!(poll(restarted, id, TOKEN_A)["status"], "sent");
}

/// A failed execution records its class (never the provider's words) and runs once too.
#[test]
fn a_failed_execution_records_its_class() {
    let root = temp_root("failed");
    let executor = CountingExecutor::new(ExecOutcome::Failed("not_connected"));
    let attached: Arc<dyn ApprovalExecutor> = executor.clone();
    let port = serve(root.0.clone(), move |state| state.with_approval_executor(attached));
    let (id, _, _) = send(port, "aiko@example.com");
    let (status, body) = admin_decide(port, id, "approve");
    assert_eq!(status, 200, "{body}");
    let decided = json_of(&body);
    assert_eq!((decided["status"].as_str(), decided["reason"].as_str()), (Some("send_failed"), Some("not_connected")), "{body}");
    let status = poll(port, id, TOKEN_A);
    assert_eq!((status["status"].as_str(), status["reason"].as_str()), (Some("send_failed"), Some("not_connected")), "{status}");
    assert!(audit_rows(port, TOKEN_A).contains(&("approvals.execute".into(), "failed:not_connected".into())));
    assert_eq!(executor.calls(), 1);
    // The class survives a restart (the queue forgets; the store does not).
    let restarted = serve(root.0.clone(), |state| state);
    let status = poll(restarted, id, TOKEN_A);
    assert_eq!((status["status"].as_str(), status["reason"].as_str()), (Some("send_failed"), Some("not_connected")), "{status}");
}

/// The real executor (`send_exec` over the principal's connector grants): with no traceability
/// sink, or no grant for the service, it fails before anything is sent.
#[test]
fn the_send_exec_executor_sends_nothing_it_cannot_trace_or_authorize() {
    use shogun_core::approval_exec::{ConnectorTransports, SendExecExecutor, SinkFor};
    let root = temp_root("sendexec");
    let tokens: Arc<dyn shogun_integrations::TokenStore> = Arc::new(shogun_integrations::MemorySecretStore::new());
    let sinks: SinkFor = Arc::new(|_| None);
    let executor: Arc<dyn ApprovalExecutor> = Arc::new(SendExecExecutor::new(Arc::new(ConnectorTransports::new(tokens)), sinks));
    let port = serve(root.0.clone(), move |state| state.with_approval_executor(executor));
    let (id, _, _) = send(port, "aiko@example.com");
    let (status, body) = admin_decide(port, id, "approve");
    assert_eq!(status, 200, "{body}");
    let decided = json_of(&body);
    assert_eq!((decided["status"].as_str(), decided["reason"].as_str()), (Some("send_failed"), Some("trace_unavailable")), "{body}");
    assert!(!body.contains("aiko"), "{body}");
}

/// An expired approval never runs, even when a human approves it late.
#[test]
fn an_expired_approval_never_runs() {
    let root = temp_root("expired");
    let offset = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let skew = offset.clone();
    let clock: Clock = Arc::new(move || (wall_clock())() + skew.load(std::sync::atomic::Ordering::SeqCst));
    let executor = CountingExecutor::new(ExecOutcome::Sent);
    let attached: Arc<dyn ApprovalExecutor> = executor.clone();
    let port = serve_with_clock(root.0.clone(), clock, move |state| state.with_approval_executor(attached));
    let (id, _, _) = send(port, "aiko@example.com");
    offset.store(11 * 60 * 1000, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = admin_decide(port, id, "approve");
    assert_eq!(status, 410, "{body}");
    assert_eq!(executor.calls(), 0);
    assert_eq!(poll(port, id, TOKEN_A)["status"], "timed_out");
}

/// Decisions racing on one approval: one wins, one execution.
#[test]
fn a_double_decide_race_executes_once() {
    let root = temp_root("race");
    let executor = CountingExecutor::new(ExecOutcome::Sent);
    let attached: Arc<dyn ApprovalExecutor> = executor.clone();
    let port = serve(root.0.clone(), move |state| state.with_approval_executor(attached));
    let signer = ApprovalSigner::new(KEY).unwrap();
    for round in 0..3 {
        let (id, token, _) = send(port, &format!("round{round}@example.com"));
        let claims = signer.verify(&token, now_ms()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let barrier = barrier.clone();
                let (token, nonce) = (token.clone(), claims.nonce.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    // Two present the signed token, two the operator credential.
                    if i % 2 == 0 {
                        decide(port, id, &token, &nonce, "approve").0
                    } else {
                        admin_decide(port, id, "approve").0
                    }
                })
            })
            .collect();
        let statuses: Vec<u16> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 1, "round {round}: {statuses:?}");
        assert!(statuses.iter().all(|s| matches!(s, 200 | 409)), "round {round}: {statuses:?}");
        assert_eq!(executor.calls(), round + 1, "round {round}: one execution per approval");
    }
}

/// E26: the operator exports a principal's audit log with a running SHA-256 chain, streamed, in
/// JSON Lines or CSV; `verify` recomputes it, and any edit, drop or truncation fails. Agents never
/// reach it.
#[test]
fn the_operator_exports_a_hash_chained_audit_log() {
    use shogun_mcp::audit_export::{verify, VerifyError, GENESIS};
    let root = temp_root("export");
    let port = serve(root.0.clone(), |state| state);
    // Some activity in Alice's log: two queued sends, one decided.
    let (first, _, _) = send(port, "aiko@example.com");
    send(port, "kenji@example.com");
    assert_eq!(admin_decide(port, first, "reject").0, 200);

    // Agents and anonymous callers never export.
    let agent = request(port, "GET", "/v1/audit/export?principal=alice", Some(TOKEN_A), None).unwrap();
    assert_eq!(agent.status, 403, "{}", agent.body);
    assert!(agent.body.contains("human_only"), "{}", agent.body);
    assert_eq!(request(port, "GET", "/v1/audit/export?principal=alice", None, None).unwrap().status, 401);
    // Multi-principal: the operator names the principal.
    assert_eq!(request(port, "GET", "/v1/audit/export", Some(ADMIN), None).unwrap().status, 400);
    assert_eq!(request(port, "GET", "/v1/audit/export?principal=alice&format=xml", Some(ADMIN), None).unwrap().status, 400);

    let jsonl = request(port, "GET", "/v1/audit/export?principal=alice", Some(ADMIN), None).unwrap();
    assert_eq!(jsonl.status, 200, "{}", jsonl.body);
    let report = verify(&jsonl.body).unwrap_or_else(|e| panic!("{e}: {}", jsonl.body));
    // Two sends, the decision, and the export itself (recorded before it streamed).
    assert!(report.records >= 4, "{report:?}");
    let lines: Vec<serde_json::Value> = jsonl.body.lines().map(json_of).collect();
    assert_eq!(lines[0]["prev_hash"], GENESIS);
    assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
    assert!(lines.iter().any(|l| l["route"] == "approvals.decide" && l["outcome"] == "rejected"));
    assert!(lines.iter().any(|l| l["route"] == "audit.export" && l["actor"] == "human:admin"));
    assert_eq!(lines.last().unwrap()["trailer"], true);
    assert_eq!(lines.last().unwrap()["head_hash"].as_str(), Some(report.head_hash.as_str()));
    // Labels only: no recipient, no body.
    assert!(!jsonl.body.contains("example.com") && !jsonl.body.contains("hunter2"), "{}", jsonl.body);

    // CSV of the same window chains the same records to the same head.
    let until = lines.iter().filter_map(|l| l["ts"].as_i64()).max().unwrap() + 1;
    let window = format!("principal=alice&until_ms={until}");
    std::thread::sleep(Duration::from_millis(5));
    let a = verify(&request(port, "GET", &format!("/v1/audit/export?{window}"), Some(ADMIN), None).unwrap().body).unwrap();
    let csv = request(port, "GET", &format!("/v1/audit/export?{window}&format=csv"), Some(ADMIN), None).unwrap();
    assert_eq!(csv.status, 200);
    assert!(csv.body.starts_with(shogun_mcp::audit_export::CSV_HEADER), "{}", csv.body);
    let b = verify(&csv.body).unwrap();
    // The window was fixed before either export: the second export's own record is after it.
    assert_eq!((a.records, a.head_hash.clone()), (b.records, b.head_hash.clone()), "deterministic over a fixed window");

    // Tampering is caught: an altered outcome, a dropped record, a truncated stream.
    assert!(matches!(verify(&jsonl.body.replacen("\"rejected\"", "\"approved\"", 1)), Err(VerifyError::BadHash { .. })));
    let mut dropped: Vec<&str> = jsonl.body.lines().collect();
    dropped.remove(1);
    assert!(verify(&dropped.join("\n")).is_err());
    let truncated: Vec<&str> = jsonl.body.lines().collect();
    assert_eq!(verify(&truncated[..truncated.len() - 1].join("\n")), Err(VerifyError::MissingTrailer));
    // Bob's log does not contain Alice's records.
    let bob = request(port, "GET", "/v1/audit/export?principal=bob", Some(ADMIN), None).unwrap();
    assert!(bob.status == 200 || bob.status == 404, "{}", bob.body);
    assert!(!bob.body.contains("approvals.decide"), "{}", bob.body);
}
