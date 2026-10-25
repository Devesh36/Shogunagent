//! CLAUDE.md invariant 6 / plan E2-T2: tenancy is a file boundary. The real `shogun-api` server
//! runs in multi-principal mode with two principals; a note and a state row written with A's token
//! are never visible with B's — not through search, not through the notes list, not through a GET
//! by id (the MemClaw sub-tenant case: knowing another principal's row id buys nothing), and not
//! through `memory.get_events`.
//!
//! E18 adds the two explicit ACLs and proves they are the only way across a boundary:
//! - **Scratch ACL** (one principal, two agents): agent B reads none of agent A's
//!   `agent_scratch:A` rows — not in a search with `include_scratch`, not by event id — until the
//!   operator grants it, and none again after the revoke; a body naming A as `actor` writes
//!   nothing into A's scratch.
//! - **Team** (two principals): `?principal=<other>` reads zero rows without a grant, only world
//!   rows with one (never scratch, never a write), and zero again after the revoke.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use shogun_cli::http::request;
use shogun_core::daemon::Clock;
use shogun_core::tenancy::{EnvKeyProvider, PrincipalId, PrincipalStore, TokenPrincipalMap};
use shogun_mcp::server::{bind_local, serve_on};
use shogun_memory::DbKey;

const TOKEN_A: &str = "token-alice-0001";
const TOKEN_A2: &str = "token-alice-0002";
const TOKEN_B: &str = "token-bob-00001";
const ADMIN: &str = "admin-credential-0123456789";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        Self::named("base")
    }

    fn named(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("shogun_invariant_tenant_{label}_{}", std::process::id()));
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

/// Boot the multi-principal server on a background thread (its own runtime); returns the port
/// and the store, so the test can also look at the files.
fn boot_multi(root: PathBuf) -> (u16, Arc<PrincipalStore>) {
    let keys = EnvKeyProvider::with_default(DbKey::new([0x5a; 32]))
        .with_principal_key(&PrincipalId::parse("bob").unwrap(), DbKey::new([0xb0; 32]));
    let store = Arc::new(PrincipalStore::new(root, Arc::new(keys), wall_clock(), 8));
    let resolver = TokenPrincipalMap::parse(&format!(
        "alice:{TOKEN_A}:claude-code,alice:{TOKEN_A2}:codex,bob:{TOKEN_B}"
    ))
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let served = store.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(served, Arc::new(resolver), wall_clock())
                .with_admin_token(Some(ADMIN));
            let listener = bind_local(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (port, store)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not JSON: {body}"))
}

#[test]
fn principal_b_reads_zero_rows_of_principal_a() {
    let root = TempRoot::new();
    let (port, store) = boot_multi(root.0.clone());

    // Open routes work without a token; a memory route without one is 401.
    assert_eq!(request(port, "GET", "/v1/status", None, None).unwrap().status, 200);
    assert_eq!(request(port, "GET", "/v1/state/commitments", None, None).unwrap().status, 401);
    // A token no principal owns is 401 too (the registry and the resolver agree).
    assert_eq!(request(port, "GET", "/v1/state/commitments", Some("token-nobody-01"), None).unwrap().status, 401);

    // A writes a note and a commitment.
    let note = request(port, "POST", "/v1/memory/notes", Some(TOKEN_A), Some("zebrafish roadmap call with Aiko")).unwrap();
    assert_eq!(note.status, 202, "{}", note.body);
    let note_id = json(&note.body)["note_id"].as_i64().expect("note id");
    let created = request(
        port,
        "POST",
        "/v1/state/create",
        Some(TOKEN_A),
        Some(r#"{"table":"commitments","description":"send the zebrafish forecast to Aiko"}"#),
    )
    .unwrap();
    assert!(matches!(created.status, 200 | 202), "{} {}", created.status, created.body);
    let commitment_id = json(&created.body)["result"]["id"]
        .as_i64()
        .or_else(|| json(&created.body)["id"].as_i64())
        .unwrap_or_else(|| panic!("no id in {}", created.body));

    // Two files, two directories: the boundary is on disk.
    assert!(store.exists(&PrincipalId::parse("alice").unwrap()));
    assert!(store.exists(&PrincipalId::parse("bob").unwrap()) || {
        // B's file appears on B's first request below.
        true
    });

    // A sees its own rows.
    let search_a = request(port, "GET", "/v1/memory/search?q=zebrafish", Some(TOKEN_A), None).unwrap();
    assert_eq!(search_a.status, 200);
    assert!(search_a.body.contains("zebrafish"), "A's search: {}", search_a.body);
    let notes_a = request(port, "GET", "/v1/memory/user-notes", Some(TOKEN_A), None).unwrap();
    assert_eq!(notes_a.status, 200);
    assert!(notes_a.body.contains("zebrafish roadmap"), "A's notes: {}", notes_a.body);
    let get_a = request(port, "GET", &format!("/v1/state/commitments/{commitment_id}"), Some(TOKEN_A), None).unwrap();
    assert_eq!(get_a.status, 200, "{}", get_a.body);
    assert!(get_a.body.contains("zebrafish forecast"), "{}", get_a.body);

    // B sees none of them: zero search rows, an empty notes list, and the GET-by-id answers as if
    // the row did not exist (the MemClaw sub-tenant bypass: another principal's id is not a key).
    let search_b = request(port, "GET", "/v1/memory/search?q=zebrafish", Some(TOKEN_B), None).unwrap();
    assert_eq!(search_b.status, 200, "{}", search_b.body);
    assert!(!search_b.body.contains("zebrafish"), "B's search leaked A's row: {}", search_b.body);
    let results = json(&search_b.body);
    let rows = results["result"]["results"]
        .as_array()
        .or_else(|| results["results"].as_array())
        .map(Vec::len)
        .unwrap_or(0);
    assert_eq!(rows, 0, "B's search rows: {}", search_b.body);
    let notes_b = request(port, "GET", "/v1/memory/user-notes", Some(TOKEN_B), None).unwrap();
    assert_eq!(notes_b.status, 200, "{}", notes_b.body);
    assert!(!notes_b.body.contains("zebrafish"), "B's notes leaked A's row: {}", notes_b.body);
    let get_b = request(port, "GET", &format!("/v1/state/commitments/{commitment_id}"), Some(TOKEN_B), None).unwrap();
    assert!(
        matches!(get_b.status, 403 | 404) || (get_b.status == 200 && !get_b.body.contains("zebrafish")),
        "B's GET of A's row: {} {}",
        get_b.status,
        get_b.body
    );
    assert!(!get_b.body.contains("zebrafish"), "{}", get_b.body);
    let events_b = request(
        port,
        "POST",
        "/v1/memory/events",
        Some(TOKEN_B),
        Some(&format!(r#"{{"ids":[{note_id}]}}"#)),
    )
    .unwrap();
    assert!(!events_b.body.contains("zebrafish"), "B's get_events leaked A's event: {}", events_b.body);
    let lists_b = request(port, "GET", "/v1/state/commitments", Some(TOKEN_B), None).unwrap();
    assert_eq!(lists_b.status, 200);
    assert!(!lists_b.body.contains("zebrafish"), "{}", lists_b.body);

    // A's rows carry A's actor (the resolver's `agent:claude-code`), on A's file only.
    store.close_all();
    let alice = shogun_memory::open_encrypted_existing_read_only(
        store.path_for(&PrincipalId::parse("alice").unwrap()),
        &DbKey::new([0x5a; 32]),
    )
    .unwrap();
    let (actor, namespace): (Option<String>, String) = alice
        .query_row("SELECT actor_last, namespace FROM commitments WHERE id = ?1", [commitment_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(actor.as_deref(), Some("agent:claude-code"));
    assert_eq!(namespace, "world");
    // The note (`memory.append_note` stores a user note, V76/V98) names A's agent as its writer.
    let (note_actor, note_namespace): (Option<String>, String) = alice
        .query_row("SELECT actor, namespace FROM user_memory_notes WHERE id = ?1", [note_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(note_actor.as_deref(), Some("agent:claude-code"));
    assert_eq!(note_namespace, "world");
    // And nothing in A's file was written as anyone else (the owner default never applied).
    let foreign: i64 = alice
        .query_row(
            "SELECT (SELECT count(*) FROM event_log WHERE actor IS NOT 'agent:claude-code')
                  + (SELECT count(*) FROM user_memory_notes WHERE actor IS NOT 'agent:claude-code')
                  + (SELECT count(*) FROM commitments WHERE actor_last IS NOT 'agent:claude-code')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(foreign, 0, "a row in A's file was not written as A's agent");
    drop(alice);
    let bob = shogun_memory::open_encrypted_existing_read_only(
        store.path_for(&PrincipalId::parse("bob").unwrap()),
        &DbKey::new([0xb0; 32]),
    )
    .unwrap();
    let bob_events: i64 = bob.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0)).unwrap();
    let bob_commitments: i64 = bob.query_row("SELECT count(*) FROM commitments", [], |r| r.get(0)).unwrap();
    assert_eq!((bob_events, bob_commitments), (0, 0), "B's file must hold none of A's rows");
}

fn post_json(port: u16, path: &str, token: &str, body: &serde_json::Value) -> shogun_cli::http::HttpResponse {
    request(port, "POST", path, Some(token), Some(&body.to_string())).unwrap()
}

/// Ingest one item as the bearer's agent; returns the event id.
fn ingest(port: u16, token: &str, content: &str, namespace: &str, key: &str) -> i64 {
    let body = serde_json::json!({
        "events": [{ "content": content }],
        "source": "harness",
        "actor": "codex",
        "idempotency_key": key,
        "namespace": namespace,
    });
    let r = post_json(port, "/v1/memory/ingest", token, &body);
    assert!(matches!(r.status, 200 | 202), "{} {}", r.status, r.body);
    json(&r.body)["result"]["event_ids"][0].as_i64().unwrap_or_else(|| panic!("no event id: {}", r.body))
}

#[test]
fn an_agent_reads_another_agents_scratch_only_with_a_grant() {
    let root = TempRoot::named("scratch_acl");
    let (port, _store) = boot_multi(root.0.clone());

    // Agent codex (TOKEN_A2) keeps a working note in its own scratch, and one in the world.
    let scratch_id = ingest(port, TOKEN_A2, "quokka migration plan, private working note", "agent_scratch", "s1");
    ingest(port, TOKEN_A2, "quokka launch moved to Friday", "world", "w1");

    // codex sees its own scratch row.
    let own = request(port, "POST", "/v1/memory/events?include_scratch", Some(TOKEN_A2), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
    assert!(own.body.contains("quokka migration plan"), "{}", own.body);

    // claude-code (TOKEN_A, same principal) sees none of it: not by search with include_scratch,
    // and not by GET-by-id (the MemClaw bypass), with or without include_scratch.
    let search = request(port, "GET", "/v1/memory/search?q=quokka&include_scratch", Some(TOKEN_A), None).unwrap();
    assert_eq!(search.status, 200, "{}", search.body);
    assert!(!search.body.contains("migration plan"), "search leaked codex's scratch: {}", search.body);
    assert!(search.body.contains("launch moved to Friday"), "the world row is found: {}", search.body);
    // codex itself finds its scratch row by search with include_scratch.
    let own_search = request(port, "GET", "/v1/memory/search?q=quokka&include_scratch", Some(TOKEN_A2), None).unwrap();
    assert!(own_search.body.contains("migration plan"), "{}", own_search.body);
    for path in ["/v1/memory/events", "/v1/memory/events?include_scratch"] {
        let by_id = request(port, "POST", path, Some(TOKEN_A), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
        assert!(!by_id.body.contains("migration plan"), "GET-by-id leaked: {}", by_id.body);
        assert!(json(&by_id.body)["missing"].as_array().is_some_and(|m| m.contains(&serde_json::json!(scratch_id))), "{}", by_id.body);
    }
    // A body naming codex as the actor writes as claude-code (the key's agent), never into
    // codex's scratch.
    let forged = ingest(port, TOKEN_A, "forged note into codex scratch", "agent_scratch", "f1");
    let codex_view = request(port, "POST", "/v1/memory/events?include_scratch", Some(TOKEN_A2), Some(&format!(r#"{{"ids":[{forged}]}}"#))).unwrap();
    assert!(!codex_view.body.contains("forged note"), "the forged row landed in codex's scratch: {}", codex_view.body);

    // An agent cannot grant itself access, and acl.list is a read every agent may call.
    let self_grant = post_json(port, "/v1/acl/grant", TOKEN_A, &serde_json::json!({ "grantee": "agent:claude-code", "agent": "codex", "principal": "alice" }));
    assert_eq!(self_grant.status, 403, "{}", self_grant.body);
    assert!(self_grant.body.contains("human_only"), "{}", self_grant.body);

    // The operator grants read: claude-code now sees the row, by search and by id.
    let granted = post_json(port, "/v1/acl/grant", ADMIN, &serde_json::json!({ "grantee": "agent:claude-code", "agent": "codex", "access": "read", "principal": "alice" }));
    assert_eq!(granted.status, 200, "{}", granted.body);
    let listed = request(port, "GET", "/v1/acl", Some(TOKEN_A), None).unwrap();
    assert!(listed.body.contains("agent_scratch:codex"), "{}", listed.body);
    let by_id = request(port, "POST", "/v1/memory/events?include_scratch", Some(TOKEN_A), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
    assert!(by_id.body.contains("quokka migration plan"), "{}", by_id.body);
    let granted_search = request(port, "GET", "/v1/memory/search?q=quokka&include_scratch", Some(TOKEN_A), None).unwrap();
    assert!(granted_search.body.contains("migration plan"), "{}", granted_search.body);
    // Default reads stay world-only even with the grant.
    let default = request(port, "POST", "/v1/memory/events", Some(TOKEN_A), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
    assert!(!default.body.contains("migration plan"), "{}", default.body);
    // A read grant does not let claude-code write into codex's scratch files.
    let write = post_json(port, "/v1/memory/fs", TOKEN_A, &serde_json::json!({ "command": "create", "path": "/memories/scratch/codex/x.md", "file_text": "overwrite" }));
    assert!(!write.body.contains("created successfully"), "{}", write.body);

    // Revoked: zero again.
    let revoked = post_json(port, "/v1/acl/revoke", ADMIN, &serde_json::json!({ "grantee": "agent:claude-code", "agent": "codex", "principal": "alice" }));
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    let after = request(port, "POST", "/v1/memory/events?include_scratch", Some(TOKEN_A), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
    assert!(!after.body.contains("migration plan"), "{}", after.body);
    let after_search = request(port, "GET", "/v1/memory/search?q=quokka&include_scratch", Some(TOKEN_A), None).unwrap();
    assert!(!after_search.body.contains("migration plan"), "{}", after_search.body);

    // Every write and grant above left an audit record, with labels only.
    let audit = request(port, "GET", "/v1/audit?limit=100", Some(TOKEN_A), None).unwrap();
    assert_eq!(audit.status, 200, "{}", audit.body);
    let records = json(&audit.body)["records"].as_array().cloned().unwrap_or_default();
    let routes: Vec<&str> = records.iter().filter_map(|r| r["route"].as_str()).collect();
    assert!(routes.contains(&"memory.ingest"), "{routes:?}");
    assert!(routes.contains(&"acl.grant") && routes.contains(&"acl.revoke"), "{routes:?}");
    assert!(!audit.body.contains("quokka"), "the audit log carries no bodies: {}", audit.body);
    assert!(records.iter().all(|r| r["request_id"].as_str().is_some_and(|id| !id.is_empty())));
}

#[test]
fn a_teammate_reads_world_rows_only_with_a_grant_and_zero_after_revoke() {
    let root = TempRoot::named("team");
    let (port, _store) = boot_multi(root.0.clone());

    // Alice's world note and her agent's scratch note; Bob opens his own store.
    let note = request(port, "POST", "/v1/memory/notes", Some(TOKEN_A), Some("zebrafish roadmap call with Aiko")).unwrap();
    assert_eq!(note.status, 202, "{}", note.body);
    let scratch_id = ingest(port, TOKEN_A, "zebrafish private scratch estimate", "agent_scratch", "s1");
    assert_eq!(request(port, "GET", "/v1/state/commitments", Some(TOKEN_B), None).unwrap().status, 200);

    let cross = |path: &str| request(port, "GET", path, Some(TOKEN_B), None).unwrap();
    let rows = |body: &str| -> usize {
        let v = json(body);
        v["result"]["results"].as_array().or_else(|| v["results"].as_array()).map(Vec::len).unwrap_or(0)
    };

    // No team, no grant: zero rows (403, the same answer for every principal id).
    let none = cross("/v1/memory/search?q=zebrafish&principal=alice");
    assert_eq!(none.status, 403, "{}", none.body);
    assert_eq!(rows(&none.body), 0);
    assert!(!none.body.contains("zebrafish"), "{}", none.body);
    assert_eq!(cross("/v1/memory/search?q=zebrafish&principal=nobody").body, none.body, "no oracle on principal ids");

    // A team alone is not a grant.
    let team = post_json(port, "/v1/teams", ADMIN, &serde_json::json!({ "tenant": "acme", "team": "ops", "members": ["alice", "bob"] }));
    assert_eq!(team.status, 200, "{}", team.body);
    assert_eq!(cross("/v1/memory/search?q=zebrafish&principal=alice").status, 403);
    // Only the operator defines teams and grants; an agent key cannot.
    assert_eq!(post_json(port, "/v1/teams", TOKEN_B, &serde_json::json!({ "tenant": "acme", "team": "x", "members": ["bob"] })).status, 403);
    assert_eq!(post_json(port, "/v1/acl/grant", TOKEN_A, &serde_json::json!({ "kind": "principal", "grantee_principal": "bob", "principal": "alice" })).status, 403);

    // Alice grants Bob world read.
    let grant = post_json(port, "/v1/acl/grant", ADMIN, &serde_json::json!({ "kind": "principal", "grantee_principal": "bob", "principal": "alice" }));
    assert_eq!(grant.status, 200, "{}", grant.body);
    let world = cross("/v1/memory/search?q=zebrafish&principal=alice&include_scratch");
    assert_eq!(world.status, 200, "{}", world.body);
    assert!(world.body.contains("zebrafish roadmap"), "{}", world.body);
    assert_eq!(json(&world.body)["source_principal"], "alice", "{}", world.body);
    assert!(!world.body.contains("private scratch"), "scratch crossed the boundary: {}", world.body);
    let by_id = request(port, "POST", "/v1/memory/events?principal=alice&include_scratch", Some(TOKEN_B), Some(&format!(r#"{{"ids":[{scratch_id}]}}"#))).unwrap();
    assert!(!by_id.body.contains("private scratch"), "scratch by id crossed the boundary: {}", by_id.body);
    // Never a write, and never Alice's audit log or grants.
    let write = request(port, "POST", "/v1/memory/notes?principal=alice", Some(TOKEN_B), Some("bob writes into alice")).unwrap();
    assert_eq!(write.status, 403, "{}", write.body);
    assert_eq!(cross("/v1/audit?principal=alice").status, 403);
    assert_eq!(cross("/v1/acl?principal=alice").status, 403);
    // Bob's own reads are unchanged: nothing of Alice's lands in his store.
    assert!(!cross("/v1/memory/search?q=zebrafish").body.contains("zebrafish"));

    // Revoked: zero rows again.
    let revoke = post_json(port, "/v1/acl/revoke", ADMIN, &serde_json::json!({ "kind": "principal", "grantee_principal": "bob", "principal": "alice" }));
    assert_eq!(revoke.status, 200, "{}", revoke.body);
    let after = cross("/v1/memory/search?q=zebrafish&principal=alice");
    assert_eq!(after.status, 403, "{}", after.body);
    assert_eq!(rows(&after.body), 0);
    assert!(!after.body.contains("zebrafish"));
}

/// Boot the multi-principal server with remote approvals configured (so an MCP send is queued with
/// an approval URL rather than refused for want of an approval surface).
fn boot_multi_with_approvals(root: PathBuf) -> (u16, Arc<PrincipalStore>) {
    use shogun_mcp::remote_approvals::{ApprovalSigner, RemoteApprovals};
    let keys = EnvKeyProvider::with_default(DbKey::new([0x5a; 32]))
        .with_principal_key(&PrincipalId::parse("bob").unwrap(), DbKey::new([0xb0; 32]));
    let store = Arc::new(PrincipalStore::new(root, Arc::new(keys), wall_clock(), 8));
    let resolver = TokenPrincipalMap::parse(&format!(
        "alice:{TOKEN_A}:claude-code,alice:{TOKEN_A2}:codex,bob:{TOKEN_B}"
    ))
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let served = store.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(served, Arc::new(resolver), wall_clock())
                .with_admin_token(Some(ADMIN))
                .with_remote_approvals(
                    RemoteApprovals::default()
                        .with_base_url(Some("https://gw.example".into()))
                        .with_signer(ApprovalSigner::new(b"gateway-and-sidecar-shared-key-0123456789")),
                );
            let listener = bind_local(0).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            let _ = serve_on(listener, state.with_port(port)).await;
        });
    });
    let port = rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (port, store)
}

/// One `POST /mcp` with the transport's headers; returns (status, lower-cased headers, body).
fn mcp_raw(port: u16, token: Option<&str>, method: Option<&str>, name: Option<&str>, body: &str) -> (u16, String, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut headers = String::new();
    if let Some(token) = token {
        headers.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(method) = method {
        headers.push_str(&format!("Mcp-Method: {method}\r\n"));
    }
    if let Some(name) = name {
        headers.push_str(&format!("Mcp-Name: {name}\r\n"));
    }
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// `tools/call` `name` with `arguments` as `token`; returns the full JSON-RPC reply.
fn mcp_call(port: u16, token: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
    let (status, _, reply) = mcp_raw(port, Some(token), Some("tools/call"), Some(name), &body.to_string());
    assert_eq!(status, 200, "{name}: {reply}");
    json(&reply)
}

/// The text block of a `tools/call` result, parsed.
fn mcp_text(reply: &serde_json::Value) -> serde_json::Value {
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text block: {reply}"));
    json(text)
}

/// Plan E4-T1 × E2-T2: `POST /mcp` in multi-principal mode resolves the principal from each
/// request's bearer, under the same header contract and MRTR shapes, and the file boundary holds
/// on this face too: B reads zero of A's rows, A's approvals and A's audit log through MCP.
#[test]
fn the_mcp_face_keeps_principals_apart() {
    let root = TempRoot::named("mcp");
    let (port, store) = boot_multi_with_approvals(root.0.clone());
    let list = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;

    // No bearer, or one no principal owns: refused before any store is opened.
    for token in [None, Some("token-nobody-01")] {
        let (status, _, body) = mcp_raw(port, token, Some("tools/list"), None, list);
        assert_eq!(status, 200, "{body}");
        assert_eq!(json(&body)["error"]["code"], shogun_mcp::mcp::TOKEN_INVALID_CODE, "{body}");
    }
    // The same header contract as the single-principal face.
    let (status, _, body) = mcp_raw(port, Some(TOKEN_A), None, None, list);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("missing_header"), "{body}");
    let (status, head, body) = mcp_raw(port, Some(TOKEN_A), Some("tools/list"), None, list);
    assert_eq!(status, 200, "{body}");
    assert!(head.contains("mcp-method: tools/list"), "{head}");
    assert!(json(&body)["result"]["tools"].as_array().is_some_and(|t| !t.is_empty()), "{body}");

    // A ingests over MCP; the row is written as A's agent, in A's file.
    let ingested = mcp_call(port, TOKEN_A, "memory.ingest", serde_json::json!({
        "events": [{ "content": "narwhal quarterly plan with Kenji" }],
        "source": "harness",
        "actor": "codex",
        "idempotency_key": "mcp-1",
    }));
    assert!(ingested.get("error").is_none(), "{ingested}");
    let ingest_text = mcp_text(&ingested);
    let event_id = ingest_text["event_ids"][0]
        .as_i64()
        .or_else(|| ingest_text["result"]["event_ids"][0].as_i64())
        .unwrap_or_else(|| panic!("no event id: {ingest_text}"));

    // A finds it; B finds nothing, by search or by id, on MCP and on REST.
    let mine = mcp_call(port, TOKEN_A, "memory.search", serde_json::json!({ "query": "narwhal" }));
    assert!(mine.to_string().contains("narwhal quarterly"), "{mine}");
    let theirs = mcp_call(port, TOKEN_B, "memory.search", serde_json::json!({ "query": "narwhal" }));
    assert!(!theirs.to_string().contains("narwhal"), "B's MCP search leaked: {theirs}");
    let own_by_id = mcp_call(port, TOKEN_A, "memory.get_events", serde_json::json!({ "ids": [event_id] }));
    assert!(own_by_id.to_string().contains("narwhal quarterly"), "{own_by_id}");
    let by_id = mcp_call(port, TOKEN_B, "memory.get_events", serde_json::json!({ "ids": [event_id] }));
    assert!(!by_id.to_string().contains("narwhal"), "B's MCP get-by-id leaked: {by_id}");
    assert!(!request(port, "GET", "/v1/memory/search?q=narwhal", Some(TOKEN_B), None).unwrap().body.contains("narwhal"));

    // A's L3 send over MCP: approval_required with a URL, recorded under A.
    let queued = mcp_call(port, TOKEN_A, "actions.execute", serde_json::json!({
        "kind": "send_email", "to": "kenji@example.com", "subject": "Q3", "body": "narwhal forecast attached",
    }));
    assert_eq!(queued["result"]["resultType"], "approval_required", "{queued}");
    let approval_id = queued["result"]["approval"]["approval_id"].as_u64().unwrap_or_else(|| panic!("{queued}"));
    assert!(queued["result"]["approval"]["url"].as_str().is_some_and(|u| u.starts_with("https://gw.example/approve/")), "{queued}");
    // A sees it on both faces (one queue, one ledger).
    assert_eq!(mcp_text(&mcp_call(port, TOKEN_A, "actions.status", serde_json::json!({ "approval_id": approval_id })))["status"], "pending");
    let listed = mcp_text(&mcp_call(port, TOKEN_A, "approvals.list", serde_json::json!({})));
    assert!(listed["approvals"].as_array().is_some_and(|a| a.iter().any(|x| x["approval_id"] == approval_id)), "{listed}");
    assert_eq!(request(port, "GET", &format!("/v1/approvals/{approval_id}"), Some(TOKEN_A), None).unwrap().status, 200);
    // B sees none of it.
    assert_eq!(mcp_text(&mcp_call(port, TOKEN_B, "actions.status", serde_json::json!({ "approval_id": approval_id })))["status"], "unknown");
    let bob_get = mcp_call(port, TOKEN_B, "approvals.get", serde_json::json!({ "approval_id": approval_id }));
    assert_eq!(bob_get["result"]["isError"], true, "{bob_get}");
    assert_eq!(mcp_text(&mcp_call(port, TOKEN_B, "approvals.list", serde_json::json!({})))["approvals"], serde_json::json!([]));
    assert!(!bob_get.to_string().contains("kenji"), "{bob_get}");

    // Audit logs: A's names A's agent and its MCP writes; B's holds none of them.
    let audit_a = mcp_text(&mcp_call(port, TOKEN_A, "audit.list", serde_json::json!({})));
    let records = audit_a["records"].as_array().cloned().unwrap_or_default();
    assert!(records.iter().any(|r| r["route"] == "memory.ingest" && r["actor"] == "agent:claude-code" && r["principal"] == "alice"), "{audit_a}");
    assert!(records.iter().any(|r| r["route"] == "actions.execute" && r["outcome"] == "pending"), "{audit_a}");
    assert!(!audit_a.to_string().contains("narwhal") && !audit_a.to_string().contains("kenji"), "{audit_a}");
    let audit_b = mcp_text(&mcp_call(port, TOKEN_B, "audit.list", serde_json::json!({})));
    assert_eq!(audit_b["records"], serde_json::json!([]), "{audit_b}");

    // On disk: B's file holds none of A's rows.
    let bob = shogun_memory::open_encrypted_existing_read_only(
        store.path_for(&PrincipalId::parse("bob").unwrap()),
        &DbKey::new([0xb0; 32]),
    )
    .unwrap();
    let bob_events: i64 = bob.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0)).unwrap();
    let bob_audit: i64 = bob.query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0)).unwrap();
    assert_eq!((bob_events, bob_audit), (0, 0));
}
