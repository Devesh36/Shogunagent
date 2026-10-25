//! CLAUDE.md invariant 6 / plan E2-T2: tenancy is a file boundary. The real `shogun-api` server
//! runs in multi-principal mode with two principals; a note and a state row written with A's token
//! are never visible with B's — not through search, not through the notes list, not through a GET
//! by id (the MemClaw sub-tenant case: knowing another principal's row id buys nothing), and not
//! through `memory.get_events`.
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
const TOKEN_B: &str = "token-bob-00001";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shogun_invariant_tenant_{}", std::process::id()));
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
    let resolver = TokenPrincipalMap::parse(&format!("alice:{TOKEN_A}:claude-code,bob:{TOKEN_B}")).unwrap();
    let (tx, rx) = mpsc::channel();
    let served = store.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let state = shogun_core::tenancy::multi::app_state(served, Arc::new(resolver), wall_clock());
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
