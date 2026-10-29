//! Lexical relevance contracts for the public recall path. Synthetic state and source
//! fixtures exercise retrieval only; no embedder or extraction model is involved.
#![cfg(feature = "db")]
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use shogun_core::daemon::Db;
use shogun_mcp::ingest_api::{recall_request, RecallOutcome, RecallRequest};
use shogun_memory::event_log::NewEvent;
use shogun_memory::state::{
    CommitmentDirection, CommitmentStatus, NewCommitment, NewOpenLoop, OpenLoopKind, Provenance,
};

const NOW: i64 = 1_000_000;

fn source(db: &Db, text: &str) -> i64 {
    db.capture(&NewEvent {
        ts: NOW - 1,
        source: "capture",
        kind: "text",
        app_bundle_id: None,
        window_title: Some("Synthetic source"),
        content: text,
        content_hash: text,
        dwell_ms: 0,
        display_id: None,
        window_bounds: None,
    })
    .expect("capture source")
    .0
}

fn commitment(db: &Db, text: &str) -> (i64, i64) {
    let event_id = source(db, text);
    let id = db
        .insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: text,
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: NOW,
            },
            &[Provenance::new(event_id)],
        )
        .expect("insert supported commitment");
    (id, event_id)
}

fn open_loop(db: &Db, text: &str) -> (i64, i64) {
    let event_id = source(db, text);
    let id = db
        .insert_open_loop(
            &NewOpenLoop {
                kind: OpenLoopKind::WaitingOnThem,
                description: text,
                counterparty_id: None,
                project_id: None,
                opened_at: NOW,
                confidence: 0.9,
                now: NOW,
            },
            &[Provenance::new(event_id)],
        )
        .expect("insert supported open loop");
    (id, event_id)
}

fn recall(db: &Db, query: &str) -> RecallOutcome {
    db.agent_recall(
        &recall_request(&json!({ "query": query, "budget_tokens": 4000 }))
            .expect("valid recall request"),
    )
    .expect("recall")
}

#[test]
fn unknown_natural_questions_do_not_match_typed_state_through_function_words() {
    let db = Db::open_in_memory(Arc::new(|| NOW)).expect("store");
    commitment(&db, "send the launch deck to Bob");
    open_loop(&db, "waiting for the venue confirmation from Alice");

    for query in [
        "What is the weather in Tokyo?",
        "Where is my scuba certificate?",
        "Who has the telescope?",
        "Please tell me about the volcano",
        "the and for from what where",
    ] {
        let outcome = recall(&db, query);
        assert!(
            outcome.items.is_empty(),
            "unrelated query {query:?} must not return authoritative typed items: {:?}",
            outcome.items
        );
        assert!(
            outcome.facts.is_empty() && outcome.possibly.is_empty() && outcome.evidence.is_empty(),
            "unrelated query {query:?} must not retrieve these sources: {outcome:?}"
        );
    }
}

#[test]
fn topical_natural_questions_keep_typed_ids_confidence_and_source_attribution() {
    let db = Db::open_in_memory(Arc::new(|| NOW)).expect("store");
    let (deck, deck_source) = commitment(&db, "send the launch deck to Bob");
    let (venue, venue_source) = open_loop(&db, "waiting for the venue confirmation from Alice");

    for (query, kind, id, event_id) in [
        ("launch deck", "commitment", deck, deck_source),
        (
            "When should I send the launch deck?",
            "commitment",
            deck,
            deck_source,
        ),
        (
            "What is the venue confirmation?",
            "open_loop",
            venue,
            venue_source,
        ),
    ] {
        let outcome = recall(&db, query);
        assert_eq!(outcome.items.len(), 1, "{query:?}: {:?}", outcome.items);
        let item = &outcome.items[0];
        assert_eq!((item.kind, item.id), (kind, id));
        assert_eq!(item.band, "high");
        assert_eq!(item.event_id, Some(event_id));
    }
}

#[test]
fn short_identifiers_and_japanese_subjects_remain_searchable() {
    let db = Db::open_in_memory(Arc::new(|| NOW)).expect("store");
    let (review, _) = commitment(&db, "review the PR");
    let (meeting, _) = open_loop(&db, "会議の日程を確認する");

    for (query, kind, id) in [
        ("PR", "commitment", review),
        ("会議", "open_loop", meeting),
        ("会議はいつですか", "open_loop", meeting),
    ] {
        let outcome = recall(&db, query);
        assert_eq!(outcome.items.len(), 1, "{query:?}: {:?}", outcome.items);
        assert_eq!((outcome.items[0].kind, outcome.items[0].id), (kind, id));
    }
}

#[test]
fn empty_queries_keep_the_existing_request_and_typed_recall_behavior() {
    let db = Db::open_in_memory(Arc::new(|| NOW)).expect("store");
    commitment(&db, "send the launch deck to Bob");

    for query in ["", "   "] {
        assert!(recall_request(&json!({ "query": query })).is_err());
        // Internal callers can construct the request directly; empty text must still
        // produce no typed items rather than matching every row.
        let outcome = db
            .agent_recall(&RecallRequest {
                query: query.into(),
                budget_tokens: 4000,
                include_scratch: false,
            })
            .expect("internal empty recall");
        assert!(outcome.items.is_empty());
    }
}
