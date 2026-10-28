//! Reproducible walkthrough through the real ingest/review/recall/handoff pipeline.
//! The Batch answers are synthetic fixtures, not live model output. No external calls.
//! Run: cargo run --profile fast --locked -p shogun-core --features daemon-server --example maya_handoff

use std::sync::Arc;

use serde_json::{json, Value};
use shogun_core::daemon::Db;
use shogun_core::db_backend::DbBackend;
use shogun_core::llm::anthropic::{BatchHandle, BatchItem, BatchLane, BatchResult};
use shogun_core::llm::traceability::{Route, TraceRecord, TraceabilitySink};
use shogun_core::llm::LlmError;
use shogun_core::tenancy::dream_tick::{tick_principal_batch, TickOutcome};
use shogun_mcp::handoff_api;
use shogun_mcp::ingest_api::{ingest_request, recall_request};
use shogun_mcp::memory_api::Tool;

const HOUR: i64 = 3_600_000;
const NOW: i64 = 1_793_188_800_000;
const CONTEXT: &str = "Maya Chen is the product lead at Kestrel Labs. Kestrel is our launch project. We work together on the Kestrel launch copy over Slack.";
const PROMISE: &str = "I'll send Maya Chen the Kestrel launch copy by Friday.";
const REVIEW: &str = "The Kestrel launch copy is waiting for Maya Chen's review.";

/// Only the model boundary is replaced. Anchors are resolved by the production parser;
/// evidence checks, confidence rules, storage and handoff remain production code.
struct Replay {
    db: Db,
    events: Vec<i64>,
}

impl Replay {
    fn answer(&self, item: &BatchItem) -> Value {
        if item.purpose == shogun_memory::loops::PURPOSE {
            return json!({"open": [
                {"id":null,"kind":"commitment","horizon":"short","source_event_id":self.events[1],
                 "source_quote":PROMISE,"confidence":0.94,"text":"Send Maya Chen the Kestrel launch copy by Friday"},
                {"id":null,"kind":"loop","horizon":"short","source_event_id":self.events[2],
                 "source_quote":REVIEW,"confidence":0.94,"text":"Wait for Maya Chen to review the Kestrel launch copy"}
            ], "resolved":[], "duplicates":[]});
        }
        let mut sessions: Vec<Value> = Vec::new();
        for line in item.chunk.lines() {
            if let Some(id) = line.strip_prefix("Session: ") {
                sessions
                    .push(json!({"id":id.trim(),"commitments":[],"open_loops":[],"entities":[]}));
                continue;
            }
            let Some((number, text)) = line.strip_prefix('[').and_then(|s| s.split_once("] "))
            else {
                continue;
            };
            let (Ok(number), Some(session)) = (number.parse::<usize>(), sessions.last_mut()) else {
                continue;
            };
            let anchor = json!({"line":number,"anchor":text.chars().take(40).collect::<String>(),"anchor_len":text.chars().count()});
            let mut add = |key: &str, fields: Value| {
                let mut value = anchor.clone();
                if let (Some(object), Some(fields)) = (value.as_object_mut(), fields.as_object()) {
                    object.extend(fields.clone());
                }
                if let Some(rows) = session[key].as_array_mut() {
                    rows.push(value);
                }
            };
            if text.contains("Maya Chen") {
                add(
                    "entities",
                    json!({"kind":"person","name":"Maya Chen","context":"Kestrel launch copy",
                    "person_class":"contact","description":"Maya Chen is the product lead at Kestrel Labs and works with the user on launch copy.","channel":"Slack"}),
                );
            }
            if text.contains("Kestrel") {
                add(
                    "entities",
                    json!({"kind":"project","name":"Kestrel","context":"Kestrel launch copy",
                    "project_class":"user_project","description":"Kestrel is the user's launch project with Maya Chen."}),
                );
            }
            if text.contains(PROMISE) {
                add(
                    "commitments",
                    json!({"direction":"mine","description":"Send Maya Chen the Kestrel launch copy by Friday"}),
                );
            }
            if text.contains(REVIEW) {
                add(
                    "open_loops",
                    json!({"kind":"waiting_on_them","description":"Wait for Maya Chen to review the Kestrel launch copy"}),
                );
            }
        }
        json!({"sessions":sessions})
    }
}

impl BatchLane for Replay {
    async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
        let mut replies = Vec::new();
        for item in items {
            // The consolidator uses the production per-store attempt ledger. No body is logged.
            self.db
                .traceability_sink()
                .try_record(TraceRecord::for_chunk(
                    Route::BatchRelay,
                    item.purpose.clone(),
                    "synthetic-maya-replay",
                    &item.chunk,
                    false,
                ))?;
            replies.push(BatchResult {
                custom_id: item.custom_id.clone(),
                text: Some(self.answer(item).to_string()),
                error: None,
            });
        }
        Ok(replies)
    }
    fn synchronous_review(&self) -> bool {
        true
    }
    async fn submit(&self, _: &[BatchItem], _: Option<&str>) -> Result<BatchHandle, LlmError> {
        Err(LlmError::Provider("replay uses synchronous review".into()))
    }
    async fn poll(&self, _: &str) -> Result<BatchHandle, LlmError> {
        Err(LlmError::Provider("replay has no batch handle".into()))
    }
    async fn results(&self, _: &str) -> Result<Vec<BatchResult>, LlmError> {
        Err(LlmError::Provider("replay has no batch handle".into()))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("SYNTHETIC REPLAY: real engine, scripted model answers, no model/network calls.");
    let db = Db::open_in_memory(Arc::new(|| NOW))?;
    let events: Vec<Value> = [CONTEXT, PROMISE, REVIEW]
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let ts = NOW - if index == 0 { 3 * HOUR } else { 2 * HOUR };
            json!({"content":content,"ts_ms":ts,"kind":"message","title":"Kestrel launch"})
        })
        .collect();
    let request = ingest_request(&json!({
        "source":"maya-walkthrough", "actor":"agent:researcher", "thread_key":"kestrel-launch",
        "idempotency_key":"example-maya-handoff-v1", "events":events
    }))?;
    let ingested = db.agent_ingest(&request)?;
    assert_eq!(ingested.inserted, 3);
    assert!(db.agent_ingest(&request)?.replayed);
    let query = recall_request(&json!({"query":"Kestrel Maya","budget_tokens":4000}))?;
    let before = db.agent_recall(&query)?;
    assert!(
        before.items.is_empty(),
        "local low-confidence candidates must not become facts"
    );
    println!("1. Stored three source events; duplicate ingestion replayed safely. Before review: no eligible state items.");

    let lane = Replay {
        db: db.clone(),
        events: ingested.event_ids,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (outcome, loops) = runtime.block_on(tick_principal_batch(&db, &lane, NOW));
    assert_eq!(outcome, TickOutcome::Ran { complete: true });
    assert!(loops.is_some(), "the review must reach the consolidator");
    let recalled = db.agent_recall(&query)?;
    for kind in ["person", "project", "commitment", "open_loop"] {
        assert!(
            recalled.items.iter().any(|i| i.kind == kind),
            "missing {kind}: {:?}",
            recalled.items
        );
    }
    for item in &recalled.items {
        assert!(item.event_id.is_some() && item.quote.as_ref().is_some_and(|q| !q.is_empty()));
        println!(
            "2. {} [{}]: {} | source: {}",
            item.kind,
            item.band,
            item.text,
            item.quote.as_deref().unwrap_or_default()
        );
    }
    let person = recalled
        .items
        .iter()
        .find(|i| i.kind == "person")
        .ok_or("missing person")?;
    let neighbors = db.knowledge_neighbors(&format!("person-{}", person.id))
        .map_err(|error| format!("read evidence links: {error:?}"))?;
    assert!(
        neighbors.iter().any(|n| n.name == "Kestrel"),
        "shared source should link Maya and Kestrel"
    );
    println!("3. Maya and Kestrel are linked through shared source evidence (co-occurrence, not an inferred ownership edge).");

    let backend = DbBackend::new(db.clone());
    let created = handoff_api::serve(
        Tool::HandoffCreate,
        &json!({
            "task":"Draft the Kestrel launch copy for Maya's review", "query":"Kestrel Maya",
            "from_agent":"agent:researcher", "to_agent":"agent:writer", "budget_tokens":4000
        }),
        &backend,
        NOW,
    )
    .map_err(|e| format!("handoff create: {e:?}"))?;
    let created: Value = serde_json::from_str(&created)?;
    let handoff = &created["result"]["handoff"];
    assert!(handoff["entities"].as_array().is_some_and(|a| a.len() >= 2));
    assert!(handoff["facts"].as_array().is_some_and(|a| a.len() >= 2));
    let args = json!({"handoff":handoff,"actor":"agent:writer","idempotency_key":"maya-accept-v1"});
    let accepted = handoff_api::serve(Tool::HandoffAccept, &args, &backend, NOW)
        .map_err(|e| format!("handoff accept: {e:?}"))?;
    let accepted: Value = serde_json::from_str(&accepted)?;
    assert_eq!(accepted["result"]["namespace"], "agent_scratch");
    let scratch_ids = accepted["result"]["event_ids"]
        .as_array()
        .ok_or("missing scratch event ids")?;
    assert!(!scratch_ids.is_empty());
    let scratch = db.agent_recall(&recall_request(
        &json!({"query":"Kestrel", "include_scratch":true,"budget_tokens":4000}),
    )?)?;
    assert!(scratch
        .evidence
        .iter()
        .any(|e| scratch_ids.contains(&json!(e.event_id)) && e.excerpt.contains("Kestrel")));
    let world = db.agent_recall(&query)?;
    assert!(
        world
            .evidence
            .iter()
            .all(|e| !scratch_ids.contains(&json!(e.event_id))),
        "scratch must not leak into world-only recall"
    );
    let replayed: Value = serde_json::from_str(
        &handoff_api::serve(Tool::HandoffAccept, &args, &backend, NOW)
            .map_err(|e| format!("handoff replay: {e:?}"))?,
    )?;
    assert_eq!(replayed["result"]["replayed"], true);
    assert_eq!(replayed["result"]["inserted"], 0);
    println!("4. Created a handoff with facts and entities; accepted into writer scratch. No send or publication occurred.");
    println!("{}", serde_json::to_string_pretty(&created)?);
    Ok(())
}
