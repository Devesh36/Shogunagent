//! E46 payment mandates through the real `shogun-api --multi` server and executor: an agent asks
//! for a mandate (L3, approval URL), a human approves it, the executor writes it into the
//! principal's store; a payment inside it settles through the (stub) rail with the grant as its
//! provenance and is never paid twice for one key; one outside it waits for a per-payment human
//! approval and then settles; a revoked mandate pre-approves nothing; another principal sees none
//! of it; and the traceability log names the rail, never the payee.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::{json, Value};
use shogun_cli::http::request;
use shogun_core::approval_exec::{ConnectorTransports, SendExecExecutor, SinkFor, StoreFor};
use shogun_core::daemon::Clock;
use shogun_core::tenancy::{EnvKeyProvider, PrincipalId, PrincipalStore, TokenPrincipalMap};
use shogun_mcp::remote_approvals::{ApprovalSigner, RemoteApprovals};
use shogun_mcp::server::{bind_local, serve_on};
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

fn boot(tag: &str) -> (u16, TempRoot, Arc<PrincipalStore>) {
    shogun_core::payment_rail::install(Arc::new(shogun_core::payment_rail::StubRail));
    let dir = std::env::temp_dir().join(format!("shogun_payments_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = TempRoot(dir.canonicalize().unwrap());
    let clock = wall_clock();
    let keys = EnvKeyProvider::with_default(DbKey::new([0x5a; 32]));
    let store = Arc::new(PrincipalStore::new(root.0.clone(), Arc::new(keys), clock.clone(), 8));
    let resolver = TokenPrincipalMap::parse(&format!("alice:{TOKEN_A}:claude-code,bob:{TOKEN_B}")).unwrap();
    let (sink_store, pay_store, app_store) = (store.clone(), store.clone(), store.clone());
    let sinks: SinkFor = Arc::new(move |principal: Option<&str>| {
        let open = sink_store.open(&PrincipalId::parse(principal?).ok()?).ok()?;
        Some(Box::new(open.db().traceability_sink()) as Box<dyn shogun_core::llm::traceability::TraceabilitySink>)
    });
    let stores: StoreFor = Arc::new(move |principal: Option<&str>| {
        let open = pay_store.open(&PrincipalId::parse(principal?).ok()?).ok()?;
        Some(open.db().clone())
    });
    let tokens: Arc<dyn shogun_integrations::TokenStore> = Arc::new(shogun_integrations::MemorySecretStore::new());
    let executor = Arc::new(SendExecExecutor::new(Arc::new(ConnectorTransports::new(tokens)), sinks).with_payments(stores));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(app_store, Arc::new(resolver), clock)
                .with_admin_token(Some(ADMIN))
                .with_remote_approvals(RemoteApprovals::default().with_base_url(Some(BASE.into())).with_signer(ApprovalSigner::new(KEY)))
                .with_approval_executor(executor);
            let listener = bind_local(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (port, root, store)
}

fn post(port: u16, token: &str, path: &str, body: Value) -> (u16, Value) {
    let r = request(port, "POST", path, Some(token), Some(&body.to_string())).unwrap();
    (r.status, serde_json::from_str(&r.body).unwrap_or_else(|_| panic!("not JSON: {}", r.body)))
}

fn admin_decide(port: u16, id: u64, decision: &str) -> (u16, Value) {
    let r = request(
        port,
        "POST",
        &format!("/v1/approvals/{id}/decide"),
        Some(ADMIN),
        Some(&json!({ "decision": decision, "decided_by": "human:owner" }).to_string()),
    )
    .unwrap();
    (r.status, serde_json::from_str(&r.body).unwrap())
}

fn spend(port: u16, amount: u64, key: &str) -> (u16, Value) {
    post(
        port,
        TOKEN_A,
        "/v1/payments/spend",
        json!({ "payee": "cloud:hetzner", "currency": "USD", "amount_minor": amount, "memo": "build minutes for invoice 42", "idempotency_key": key }),
    )
}

#[test]
fn a_mandate_is_granted_by_a_human_and_pre_approves_payments_within_its_bounds() {
    let (port, _root, store) = boot("main");

    // The agent asks; nothing is granted until a human approves at the URL.
    let (status, asked) = post(
        port,
        TOKEN_A,
        "/v1/payments/mandates/request",
        json!({ "payee_class": "cloud", "currency": "USD", "max_per_payment_minor": 5000, "total_cap_minor": 8000,
                "expires_in_days": 30, "purpose": "Build minutes for the release", "idempotency_key": "mandate-1" }),
    );
    assert_eq!(status, 202, "{asked}");
    assert_eq!(asked["next_action"], "approve_at_url", "{asked}");
    assert!(asked["url"].as_str().unwrap().starts_with(&format!("{BASE}/approve/")));
    let mandate_id = asked["mandate_id"].as_str().unwrap().to_string();
    let grant_id = asked["approval_id"].as_u64().unwrap();
    let r = request(port, "GET", "/v1/payments/mandates", Some(TOKEN_A), None).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&r.body).unwrap()["mandates"], json!([]), "{}", r.body);
    // Before the grant, a spend asks for a per-payment approval.
    let (status, body) = spend(port, 100, "early");
    assert_eq!((status, body["reason"].as_str()), (202, Some("no_mandate")), "{body}");

    // An agent's bearer never decides; a human does, and the executor writes the mandate.
    let agent = request(port, "POST", &format!("/v1/approvals/{grant_id}/decide"), Some(TOKEN_A), Some(r#"{"decision":"approve","decided_by":"agent:claude-code"}"#)).unwrap();
    assert_eq!(agent.status, 403);
    let (status, decided) = admin_decide(port, grant_id, "approve");
    assert_eq!((status, decided["status"].as_str()), (200, Some("sent")), "{decided}");
    let r = request(port, "GET", "/v1/payments/mandates", Some(TOKEN_A), None).unwrap();
    let listed: Value = serde_json::from_str(&r.body).unwrap();
    let m = &listed["mandates"][0];
    assert_eq!((m["mandate_id"].as_str(), m["status"].as_str()), (Some(mandate_id.as_str()), Some("active")), "{listed}");
    assert_eq!(m["granted"]["approval_id"], grant_id);
    assert_eq!(m["granted"]["by"], "human:owner");

    // Inside the bounds: settled through the rail, the grant decision as provenance.
    let (status, paid) = spend(port, 5000, "pay-1");
    assert_eq!(status, 200, "{paid}");
    assert_eq!((paid["status"].as_str(), paid["authorization"].as_str()), (Some("settled"), Some("mandate")));
    assert_eq!(paid["spend"]["approval_id"], grant_id);
    assert_eq!(paid["spend"]["decided_by"], "human:owner");
    assert!(paid["spend"]["rail_ref"].as_str().unwrap().starts_with("stub_psp_"));
    assert!(!paid.to_string().contains("invoice 42"), "the memo is never stored: {paid}");
    // A retry replays; nothing is paid twice.
    let (status, again) = spend(port, 5000, "pay-1");
    assert_eq!((status, again["replayed"].as_bool()), (200, Some(true)));

    // Over the remaining cap: a per-payment approval; approved, it settles under that decision.
    let (status, over) = spend(port, 4000, "pay-2");
    assert_eq!(status, 202, "{over}");
    assert_eq!((over["authorization"].as_str(), over["reason"].as_str()), (Some("approval"), Some("over_total_cap")));
    assert!(over["url"].is_string(), "{over}");
    let (status, decided) = admin_decide(port, over["approval_id"].as_u64().unwrap(), "approve");
    assert_eq!((status, decided["status"].as_str()), (200, Some("sent")), "{decided}");
    let (_, replay) = spend(port, 4000, "pay-2");
    assert_eq!(replay["spend"]["status"], "settled", "{replay}");
    assert_eq!(replay["spend"]["authorization"], "approval");
    assert_eq!(replay["spend"]["decided_by"], "human:owner");

    // Every payment left a traceability row naming the rail, never the payee.
    let open = store.open(&PrincipalId::parse("alice").unwrap()).unwrap();
    let rows: Vec<(String, String)> =
        open.db().try_trace_page(200, 0).unwrap().into_iter().map(|(_, row)| (row.destination, row.purpose)).collect();
    assert!(rows.iter().filter(|(dest, purpose)| dest == "payment_rail" && purpose == "payments.pay").count() >= 2, "{rows:?}");
    assert!(!rows.iter().any(|(dest, _)| dest.contains("hetzner")));

    // Another principal sees none of it.
    let r = request(port, "GET", "/v1/payments/mandates", Some(TOKEN_B), None).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&r.body).unwrap()["mandates"], json!([]), "{}", r.body);

    // Revoked: the next payment asks a human again.
    let (status, revoked) = post(port, TOKEN_A, "/v1/payments/mandates/revoke", json!({ "mandate_id": mandate_id }));
    assert_eq!((status, revoked["revoked"].as_bool()), (200, Some(true)), "{revoked}");
    let (status, after) = spend(port, 10, "pay-3");
    assert_eq!((status, after["reason"].as_str()), (202, Some("no_mandate")), "{after}");
}

#[test]
fn a_rejected_per_payment_approval_marks_the_spend_rejected_and_pays_nothing() {
    let (port, _root, store) = boot("reject");

    // No mandate: the payment waits for a human.
    let (status, asked) = spend(port, 1200, "rej-1");
    assert_eq!((status, asked["authorization"].as_str()), (202, Some("approval")), "{asked}");
    let approval_id = asked["approval_id"].as_u64().unwrap();
    let spend_ref = asked["spend_ref"].as_str().unwrap().to_string();

    // The human rejects it: actions.status and payments.* agree, with who and which approval.
    let (status, decided) = admin_decide(port, approval_id, "reject");
    assert_eq!((status, decided["status"].as_str()), (200, Some("rejected")), "{decided}");
    let polled = request(port, "GET", &format!("/v1/actions/status/{approval_id}"), Some(TOKEN_A), None).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&polled.body).unwrap()["status"], "rejected", "{}", polled.body);
    let (status, replay) = spend(port, 1200, "rej-1");
    assert_eq!((status, replay["status"].as_str(), replay["replayed"].as_bool()), (200, Some("rejected"), Some(true)), "{replay}");
    assert_eq!(replay["spend"]["spend_ref"], spend_ref.as_str());
    assert_eq!(replay["spend"]["approval_id"], approval_id);
    assert_eq!(replay["spend"]["decided_by"], "human:owner");
    assert_eq!(replay["next_action"], "ask_human");

    // A second decision changes nothing, and nothing ever reached the rail.
    let (status, _) = admin_decide(port, approval_id, "approve");
    assert_eq!(status, 409);
    let (_, replay) = spend(port, 1200, "rej-1");
    assert_eq!(replay["status"], "rejected", "{replay}");
    let open = store.open(&PrincipalId::parse("alice").unwrap()).unwrap();
    let rows = open.db().try_trace_page(200, 0).unwrap();
    assert!(!rows.iter().any(|(_, row)| row.destination == "payment_rail"), "nothing was sent to the rail");
}
