//! Synthetic GitHub MCP pages through the real sync runtime and encrypted store.
//! No network, OAuth, extraction model, or external write is used.
#![cfg(feature = "db")]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shogun_core::daemon::Db;
use shogun_integrations::{ConnectorRuntime, IngestSink, McpRpc, RemoteMcpTransport};
use shogun_mcp::scope::{Service, Wave};
use shogun_memory::DbKey;

const ISSUE_URL: &str = "https://github.com/example/harbor/issues/42";
const BODY: &str = "The Harbor export intermittently times out.";
const VERSIONS: [(&str, &str, &str); 3] = [
    ("open", "", "2026-10-27T09:00:00Z"),
    ("closed", "completed", "2026-10-27T10:00:00Z"),
    ("open", "reopened", "2026-10-27T11:00:00Z"),
];

#[derive(Clone)]
struct FakeGitHub(Arc<Mutex<Value>>);

impl FakeGitHub {
    fn set_revision(&self, index: usize) {
        let (state, reason, updated) = VERSIONS[index];
        *self.0.lock().expect("fixture page") = json!({
            "total_count": 1,
            "items": [{
                "id": 90042,
                "number": 42,
                "title": "Harbor export timeout",
                "body": BODY,
                "state": state,
                "state_reason": (!reason.is_empty()).then_some(reason),
                "updated_at": updated,
                "html_url": ISSUE_URL
            }]
        });
    }
}

impl McpRpc for FakeGitHub {
    fn call_tool(&self, service: Service, tool: &str, arguments: Value) -> Result<Value, String> {
        assert_eq!(service, Service::GitHub);
        match tool {
            "get_me" => Ok(json!({"structuredContent": {"login": "fixture-reader"}})),
            "search_issues" => {
                assert!(arguments["query"]
                    .as_str()
                    .expect("query")
                    .starts_with("involves:fixture-reader updated:>="));
                assert_eq!(arguments["page"], 1);
                // Exercise the JSON-in-text envelope used by remote MCP as well.
                Ok(
                    json!({"content": [{"type": "text", "text": self.0.lock().expect("fixture page").to_string()}]}),
                )
            }
            _ => panic!("unexpected provider tool: {tool}"),
        }
    }
}

struct TempStore(PathBuf);

impl TempStore {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("shogun-github-sync-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).expect("temporary directory");
        Self(root)
    }

    fn path(&self) -> PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime(provider: FakeGitHub, now: i64) -> ConnectorRuntime<RemoteMcpTransport<FakeGitHub>> {
    let mut runtime = ConnectorRuntime::new(RemoteMcpTransport::new(provider), Wave::Three, true);
    runtime.set_plan(shogun_agents::entitlement::entitlements(
        shogun_agents::entitlement::Plan::Pro,
        0,
    ));
    runtime.mark_connected(Service::GitHub, now);
    runtime
}

#[test]
fn github_status_revisions_survive_replay_restart_and_empty_provider_pages() {
    let store = TempStore::new();
    let key = DbKey::new([0x47; 32]);
    let timestamps = VERSIONS.map(|(_, _, updated)| {
        shogun_integrations::result::rfc3339_to_ms(updated).expect("fixture timestamp")
    });
    let now = timestamps[2] + 1_000;
    let db = Db::open_encrypted(store.path(), &key, Arc::new(move || now)).expect("store");
    let provider = FakeGitHub(Arc::new(Mutex::new(Value::Null)));
    let mut runtime = runtime(provider.clone(), now);
    let mut event_ids = Vec::new();

    for (revision, timestamp) in timestamps.iter().enumerate() {
        provider.set_revision(revision);
        let report = runtime
            .sync_service(Service::GitHub, now, &db)
            .expect("sync revision");
        assert_eq!(
            (report.fetched, report.inserted),
            (1, 1),
            "revision {revision} must become evidence"
        );
        let hits = db.search_source("Harbor", "github", 10);
        assert_eq!(hits.len(), revision + 1);
        let current = hits
            .iter()
            .find(|hit| hit.ts == *timestamp)
            .expect("dated revision");
        event_ids.push(current.event_id);

        let replay = runtime
            .sync_service(Service::GitHub, now, &db)
            .expect("replay same page");
        assert_eq!((replay.fetched, replay.inserted), (1, 0));
        assert_eq!(db.search_source("Harbor", "github", 10).len(), revision + 1);
    }
    let hits = db.search_source("Harbor", "github", 10);
    for (revision, &(state, reason, updated)) in VERSIONS.iter().enumerate() {
        let current = hits
            .iter()
            .find(|hit| hit.event_id == event_ids[revision])
            .expect("retained snapshot");
        assert!(
            current.content.contains(&format!("State: {state}")),
            "{}",
            current.content
        );
        if !reason.is_empty() {
            assert!(current.content.contains(reason));
        }
        assert!(current.content.contains(updated));
        assert!(current.content.contains(ISSUE_URL));
        assert!(current.content.contains(BODY));
    }
    assert_eq!(
        db.search("latest Harbor", 10)
            .first()
            .map(|hit| hit.event_id),
        event_ids.last().copied()
    );
    assert!(event_ids.windows(2).all(|ids| ids[0] != ids[1]));
    assert_eq!(db.thread_event_texts("github:90042").len(), 3);
    let cursor = db.cursor(Service::GitHub).expect("stored cursor");
    assert_eq!(cursor.watermark_ms, timestamps[2]);
    assert!(cursor.page_token.is_none());
    drop(db);

    // Inspect only this synthetic store's provenance; the provider identity remains stable
    // across revisions rather than becoming a new thread for every snapshot.
    let conn = shogun_memory::open_encrypted(store.path(), &key).expect("inspect store");
    let mut statement = conn
        .prepare(
            "SELECT actor, namespace, thread_key FROM event_log WHERE source='github' ORDER BY id",
        )
        .expect("provenance query");
    let provenance = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("provenance rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("provenance values");
    assert_eq!(
        provenance,
        vec![
            (
                "connector:github".into(),
                "world".into(),
                "github:90042".into()
            );
            3
        ]
    );
    drop(statement);
    drop(conn);

    let db = Db::open_encrypted(store.path(), &key, Arc::new(move || now)).expect("reopen store");
    assert_eq!(db.cursor(Service::GitHub), Some(cursor));
    let replay = runtime
        .sync_service(Service::GitHub, now, &db)
        .expect("replay after restart");
    assert_eq!(replay.inserted, 0);
    let mut retained_ids = db
        .search_source("Harbor", "github", 10)
        .iter()
        .map(|hit| hit.event_id)
        .collect::<Vec<_>>();
    retained_ids.sort_unstable();
    assert_eq!(retained_ids, event_ids);

    // Absence from a search page is not a remote deletion instruction. History stays.
    *provider.0.lock().expect("empty page") = json!({"items": [], "total_count": 0});
    let empty = runtime
        .sync_service(Service::GitHub, now, &db)
        .expect("empty provider page");
    assert_eq!((empty.fetched, empty.inserted), (0, 0));
    assert_eq!(db.thread_event_texts("github:90042").len(), 3);
    let other = Db::open_in_memory(Arc::new(move || now)).expect("other principal");
    assert!(other.search_source("Harbor", "github", 10).is_empty());
}
