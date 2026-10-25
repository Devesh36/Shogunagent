//! E34 / threat model R4: `shogun-api --multi` accepts the gateway's derived `sgw1` bearers
//! alongside the listed tokens, with no principal token in its environment. A derived bearer
//! opens the principal it names (any principal, no provisioning), writes as `agent:<key_id>`, and
//! is accepted by both faces (REST through the token registry, `/mcp` through the resolver); a
//! bearer the secret did not sign is refused on both, and the file boundary holds.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use shogun_cli::http::request;
use shogun_core::daemon::Clock;
use shogun_core::llm::Secret;
use shogun_core::tenancy::{ChainedResolver, DerivedBearers, EnvKeyProvider, PrincipalId, PrincipalStore, TokenPrincipalMap};
use shogun_mcp::server::{bind_exact, serve_on};
use shogun_memory::DbKey;

const SECRET: &str = "gateway-and-sidecar-derived-bearer-secret-0123";
const LISTED: &str = "token-listed-bob-1";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shogun_derived_bearer_{}", std::process::id()));
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

fn boot(root: PathBuf) -> (u16, Arc<PrincipalStore>) {
    let store = Arc::new(PrincipalStore::new(
        root,
        Arc::new(EnvKeyProvider::with_default(DbKey::new([0x3c; 32]))),
        wall_clock(),
        8,
    ));
    let resolver = ChainedResolver {
        listed: TokenPrincipalMap::parse(&format!("bob:{LISTED}")).unwrap(),
        derived: Some(DerivedBearers::new(Secret::new(SECRET)).unwrap()),
    };
    let (tx, rx) = mpsc::channel();
    let served = store.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(served, Arc::new(resolver), wall_clock());
            let listener = bind_exact(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (port, store)
}

fn bearer(principal: &str, key: &str) -> String {
    DerivedBearers::new(Secret::new(SECRET))
        .unwrap()
        .derive(&PrincipalId::parse(principal).unwrap(), key)
        .unwrap()
}

/// One `POST /mcp` `tools/call`; returns (status, body).
fn mcp_call(port: u16, token: &str, name: &str, arguments: serde_json::Value) -> (u16, serde_json::Value) {
    use std::io::{Read, Write};
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nMcp-Method: tools/call\r\nMcp-Name: {name}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
}

#[test]
fn derived_bearers_open_their_principal_and_write_as_the_key() {
    let root = TempRoot::new();
    let (port, store) = boot(root.0.clone());
    let alice = bearer("prn_alice", "key_01");

    // REST: the token registry admits the derived bearer (it is not listed anywhere).
    let note = request(port, "POST", "/v1/memory/notes", Some(&alice), Some("okapi migration notes")).unwrap();
    assert_eq!(note.status, 202, "{}", note.body);
    let note_id = serde_json::from_str::<serde_json::Value>(&note.body).unwrap()["note_id"].as_i64().unwrap();

    // MCP: the same bearer, the same principal; another key of the same principal reads it too.
    let (status, found) = mcp_call(port, &bearer("prn_alice", "key_02"), "memory.search", serde_json::json!({ "query": "okapi" }));
    assert_eq!(status, 200, "{found}");
    assert!(found.to_string().contains("okapi migration"), "{found}");

    // Another principal's derived bearer, and the listed token, see none of it.
    let (status, theirs) = mcp_call(port, &bearer("prn_carol", "key_01"), "memory.search", serde_json::json!({ "query": "okapi" }));
    assert_eq!(status, 200);
    assert!(!theirs.to_string().contains("okapi"), "{theirs}");
    let listed = request(port, "GET", "/v1/memory/search?q=okapi", Some(LISTED), None).unwrap();
    assert_eq!(listed.status, 200, "the listed token still works: {}", listed.body);
    assert!(!listed.body.contains("okapi"), "{}", listed.body);

    // A bearer the secret did not sign is refused on both faces, and opens no store.
    let forged = format!("{}x", &alice[..alice.len() - 1]);
    let forged = if forged == alice { format!("{}y", &alice[..alice.len() - 1]) } else { forged };
    let other_secret = DerivedBearers::new(Secret::new("a-different-secret-of-sufficient-length-99"))
        .unwrap()
        .derive(&PrincipalId::parse("prn_mallory").unwrap(), "key_01")
        .unwrap();
    for bad in [forged.as_str(), other_secret.as_str()] {
        assert_eq!(request(port, "GET", "/v1/state/commitments", Some(bad), None).unwrap().status, 401, "{bad}");
        let (status, refused) = mcp_call(port, bad, "memory.search", serde_json::json!({ "query": "okapi" }));
        assert_eq!(status, 401, "{refused}");
        assert_eq!(refused["error"]["code"], shogun_mcp::mcp::TOKEN_INVALID_CODE);
    }
    assert!(!store.exists(&PrincipalId::parse("prn_mallory").unwrap()));

    // The note names the key that wrote it.
    store.close_all();
    let db = shogun_memory::open_encrypted_existing_read_only(
        store.path_for(&PrincipalId::parse("prn_alice").unwrap()),
        &DbKey::new([0x3c; 32]),
    )
    .unwrap();
    let actor: Option<String> = db
        .query_row("SELECT actor FROM user_memory_notes WHERE id = ?1", [note_id], |r| r.get(0))
        .unwrap();
    assert_eq!(actor.as_deref(), Some("agent:key_01"));
}
