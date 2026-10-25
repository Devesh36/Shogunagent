use super::*;
use crate::llm::traceability::TraceabilitySink;
use crate::llm::traceability::{Route, TraceRecord};

#[test]
fn trace_read_failure_is_not_an_empty_receipt() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    assert!(db.try_trace_rows(&shogun_memory::traceability::Filter::default()).unwrap().is_empty());
    db.conn.lock().unwrap().execute_batch("DROP TABLE traceability_log").unwrap();
    assert!(db.try_trace_rows(&shogun_memory::traceability::Filter::default()).is_err());
}

fn clock(v: i64) -> Clock {
    Arc::new(move || v)
}

fn ev<'a>(content: &'a str, hash: &'a str, ts: i64) -> NewEvent<'a> {
    NewEvent {
        ts,
        source: "capture",
        kind: "text",
        app_bundle_id: Some("com.apple.Safari"),
        window_title: Some("t"),
        content,
        content_hash: hash,
        dwell_ms: 0,
        display_id: Some(1),
        window_bounds: None,
    }
}

fn hot_test_candidates(
    db: &Db,
    sources: &[&str],
    limit: usize,
) -> Vec<shogun_memory::hot::HotPreviewCandidate> {
    let snapshot = db
        .hot_context_candidates(sources, limit, shogun_memory::hot::HOT_CONTENT_MAX_BYTES)
        .unwrap();
    snapshot.candidates().to_vec()
}

#[test]
fn final_context_order_keeps_stronger_exact_fact_above_multi_representation_meeting() {
    use shogun_memory::search::{
        ComponentRanks, FederatedHit, FederatedSource, FederatedSourceId, FederatedSourceKind,
        MatchQualitySummary, TierMetadata,
    };
    use shogun_memory::search_documents::{SearchDocumentKind, SourceRef};

    let exact_source = FederatedSource {
        kind: FederatedSourceKind::Derived(SearchDocumentKind::Project),
        id: FederatedSourceId::Derived(SourceRef::Project(7)),
    };
    let exact = FederatedHit {
        source_kind: exact_source.kind.clone(),
        source_id: exact_source.id.clone(),
        sources: vec![exact_source],
        evidence_ids: Vec::new(),
        title: "Zephyr launch owner".to_owned(),
        excerpt: "Priya owns the Zephyr launch".to_owned(),
        confidence: 0.95,
        period_start_ms: None,
        period_end_ms: None,
        component_ranks: ComponentRanks {
            derived_fts: Some(1),
            ..ComponentRanks::default()
        },
        fused_score: 0.02,
        tiers: TierMetadata {
            canonical: true,
            ..TierMetadata::default()
        },
        answer_eligible: true,
        match_quality: MatchQualitySummary::default(),
    };
    let weak_source = FederatedSource {
        kind: FederatedSourceKind::Derived(SearchDocumentKind::MeetingRecap),
        id: FederatedSourceId::Derived(SourceRef::MeetingRecap(11)),
    };
    let weak_cluster = FederatedHit {
        source_kind: weak_source.kind.clone(),
        source_id: weak_source.id.clone(),
        sources: vec![weak_source],
        evidence_ids: Vec::new(),
        title: "Zephyr launch owner routine sync".to_owned(),
        excerpt: "Generic Zephyr launch owner status".to_owned(),
        confidence: 0.9,
        period_start_ms: None,
        period_end_ms: None,
        component_ranks: ComponentRanks {
            event_fts: Some(1),
            derived_fts: Some(1),
            meeting: Some(1),
            warm_knn: Some(1),
            ..ComponentRanks::default()
        },
        fused_score: 0.01,
        tiers: TierMetadata {
            canonical: true,
            warm: true,
            ..TierMetadata::default()
        },
        answer_eligible: true,
        match_quality: MatchQualitySummary::default(),
    };

    let ranked = rank_context_hits(
        &["zephyr".to_owned(), "launch".to_owned(), "owner".to_owned()],
        vec![weak_cluster, exact],
        2,
    );

    assert_eq!(
        ranked[0].source_id,
        FederatedSourceId::Derived(SourceRef::Project(7)),
        "final Fusion context order must preserve stronger fused relevance: {ranked:?}"
    );
}

// memory health (issue #121)
// The three outcomes a memory read can have — nothing stored, the query refused, the lock
// poisoned — are not one empty vector. These pin them apart, and pin the recovery.

/// Break the store the way a corrupt file or a bad migration would: the table the read needs
/// is gone, so SQLite refuses the statement while the connection itself is fine.
fn drop_table(db: &Db, table: &str) {
    let conn = db.conn.lock().unwrap();
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))
        .unwrap();
}

/// Poison the connection mutex the way a panic inside a locked section would.
fn poison_lock(db: &Db) {
    let conn = db.conn.clone();
    let _ = std::thread::spawn(move || {
        let _g = conn.lock().unwrap();
        panic!("poisoning the memory lock on purpose");
    })
    .join();
    assert!(
        db.conn.lock().is_err(),
        "the fixture must actually poison the lock"
    );
}

#[test]
fn an_empty_store_is_not_a_failure() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    assert_eq!(db.try_search("anything", 10).unwrap(), Vec::new());
    assert_eq!(db.try_commitment_rows().unwrap().len(), 0);
    let h = db.memory_health();
    assert!(!h.degraded, "no rows is an answer, not a fault");
    assert_eq!(h.faults_total, 0);
}

#[test]
fn absent_named_target_keeps_suggestions_but_is_not_answerable() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev(
        "routine project status reviewed",
        "negative-suggestion",
        9_000,
    ))
    .unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: "Quasar project status".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();
    assert!(!response.federated_hits.is_empty());
    assert_eq!(
        response.answerability.reason,
        AnswerabilityReasonV1::MissingAnchor
    );
}

#[test]
fn exact_named_fact_is_answerable() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev("Phoenix launch owner is Priya", "exact-answer", 9_000))
        .unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: "Phoenix launch owner Priya".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();
    assert_eq!(
        response.answerability.reason,
        AnswerabilityReasonV1::Answerable
    );
}

#[test]
fn substring_overlap_cannot_make_named_query_answerable() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev("Phoenix quarterly", "substring-answer", 9_000))
        .unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: "Phoenix art".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert!(!response.federated_hits.is_empty());
    assert_eq!(
        response.answerability.reason,
        AnswerabilityReasonV1::LowQuality
    );
}

#[test]
fn overlong_quoted_anchor_remains_searchable_but_insufficient() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev("routine status", "overlong-anchor", 9_000))
        .unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: format!("status \"{}\"", "界".repeat(200)),
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert!(!response.federated_hits.is_empty());
    assert_eq!(
        response.answerability.reason,
        AnswerabilityReasonV1::InsufficientSignal
    );
}

#[test]
fn generic_meeting_query_keeps_suggestions_with_conservative_verdict() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev(
        "meeting launch status reviewed",
        "generic-meeting",
        9_000,
    ))
    .unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: "what happened in the meeting".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();
    assert!(!response.federated_hits.is_empty());
    assert_eq!(
        response.answerability.reason,
        AnswerabilityReasonV1::InsufficientSignal
    );
}

#[test]
fn accented_named_fact_requires_exact_unicode_anchor_match() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev("Élodie owns the launch", "accent-answer", 9_000))
        .unwrap();
    let exact = db
        .search_request(&SearchRequestV1 {
            query: "Élodie launch".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();
    let absent = db
        .search_request(&SearchRequestV1 {
            query: "Elodie launch".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();
    assert_eq!(
        exact.answerability.reason,
        AnswerabilityReasonV1::Answerable
    );
    assert_eq!(
        absent.answerability.reason,
        AnswerabilityReasonV1::MissingAnchor
    );
}

#[test]
fn canonical_search_default_widens_only_lexically() {
    use crate::daemon::{SearchDepthV1, SearchLexicalScopeV1, SearchRequestV1};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let db = Db::open_in_memory(clock(now)).unwrap();
    db.capture(&ev(
        "the old vendor migration was cancelled",
        "old",
        now - 80 * DAY_MS,
    ))
    .unwrap();

    let response = db
        .search_request(&SearchRequestV1 {
            query: "vendor migration".into(),
            limit: 6,
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert_eq!(response.requested_depth, SearchDepthV1::Warm);
    assert_eq!(response.effective_depth, SearchDepthV1::Warm);
    assert_eq!(response.lexical_scope, SearchLexicalScopeV1::AllHistory);
    assert_eq!(response.cold_partitions_visited, 0);
    assert_eq!(response.cold_vectors_scanned, 0);
    assert!(response
        .hits
        .iter()
        .any(|hit| hit.content.contains("cancelled")));
}

#[test]
fn canonical_default_finds_old_unique_lexical_memory_despite_a_full_warm_result_cap() {
    use crate::daemon::{SearchLexicalScopeV1, SearchRequestV1};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let db = Db::open_in_memory(clock(now)).unwrap();
    for index in 0..8 {
        db.capture(&ev(
            &format!("warm vendor status distractor {index}"),
            &format!("warm-{index}"),
            now - index * 1_000,
        ))
        .unwrap();
    }
    let old_event = db
        .capture(&ev(
            "historical vendor status: zephyr renewal accepted",
            "old",
            now - 80 * DAY_MS,
        ))
        .unwrap()
        .0;

    let response = db
        .search_request(&SearchRequestV1 {
            query: "vendor status zephyr renewal".into(),
            limit: 8,
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert_eq!(response.lexical_scope, SearchLexicalScopeV1::AllHistory);
    assert_eq!(response.cold_partitions_visited, 0);
    assert_eq!(response.cold_vectors_scanned, 0);
    assert!(response.hits.iter().any(|hit| hit.event_id == old_event));
}

/// A common phrase matching hundreds of archived rows does not force a Warm-only, unscored
/// fallback: the lexical lane reads capped rowid postings over all history (`lexical.rs`) and
/// ranks them, and the recency prior puts the recent copies of an identical row first.
#[test]
fn canonical_default_ranks_high_cardinality_archive_and_prefers_recent_copies() {
    use crate::daemon::{SearchLexicalScopeV1, SearchRequestV1};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let db = Db::open_in_memory(clock(now)).unwrap();
    for index in 0..513 {
        db.capture(&ev(
            "project meeting status update review plan code work",
            &format!("old-common-{index}"),
            now - 80 * DAY_MS,
        ))
        .unwrap();
    }
    for index in 0..8 {
        db.capture(&ev(
            "project meeting status update review plan code work",
            &format!("warm-common-{index}"),
            now - (index + 1) * 1_000,
        ))
        .unwrap();
    }

    let response = db
        .search_request(&SearchRequestV1 {
            query: "project meeting status update review plan code work".into(),
            limit: 8,
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert_eq!(response.lexical_scope, SearchLexicalScopeV1::AllHistory);
    assert_eq!(response.cold_partitions_visited, 0);
    assert_eq!(response.cold_vectors_scanned, 0);
    assert_eq!(response.hits.len(), 8);
    assert!(response
        .hits
        .iter()
        .all(|hit| hit.content.contains("project meeting") && hit.ts > now - DAY_MS));
}

#[test]
fn federated_widen_without_range_searches_history_within_caps() {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let conn = shogun_memory::open_in_memory().unwrap();
    for index in 0..513 {
        shogun_memory::event_log::insert(
            &conn,
            &shogun_memory::event_log::NewEvent {
                ts: now - DAY_MS,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: Some("Common fallback"),
                content: "project meeting status update review plan code work",
                content_hash: &format!("no-range-common-{index}"),
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
    }

    let outcome = shogun_memory::search::search_federated(
        &conn,
        "project meeting status update review plan code work",
        None,
        now,
        &shogun_memory::search::FederatedSearchOptions {
            widen_event_lexical: true,
            ..shogun_memory::search::FederatedSearchOptions::default()
        },
        8,
    )
    .unwrap();

    // 513 matches are far under the per-term postings cap, so the archive read is complete.
    assert!(outcome.event_lexical_all_history);
    assert_eq!(outcome.cold.partitions_visited, 0);
    assert_eq!(outcome.cold.rows_scanned, 0);
    assert_eq!(outcome.raw_event_ranks.len(), 8);
}

#[test]
fn canonical_explicit_warm_range_remains_range_bounded() {
    use crate::daemon::{SearchDepthV1, SearchLexicalScopeV1, SearchRequestV1, SearchTimeRangeV1};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let db = Db::open_in_memory(clock(now)).unwrap();
    let old_event = db
        .capture(&ev(
            "historical vendor status: zephyr renewal accepted",
            "old-explicit-warm",
            now - 80 * DAY_MS,
        ))
        .unwrap()
        .0;
    db.capture(&ev(
        "warm vendor status distractor",
        "warm-explicit-warm",
        now - DAY_MS,
    ))
    .unwrap();

    let response = db
        .search_request(&SearchRequestV1 {
            query: "vendor status zephyr renewal".into(),
            limit: 8,
            depth: SearchDepthV1::Warm,
            range: Some(SearchTimeRangeV1 {
                from_ms: now - 30 * DAY_MS,
                to_ms: now,
            }),
            source_kind: None,
            include_scratch: false,
        })
        .unwrap();

    assert_eq!(response.lexical_scope, SearchLexicalScopeV1::Warm);
    assert_eq!(response.cold_partitions_visited, 0);
    assert_eq!(response.cold_vectors_scanned, 0);
    assert!(response.hits.iter().all(|hit| hit.event_id != old_event));
}

#[test]
fn canonical_search_range_is_half_open_for_federated_events() {
    use crate::daemon::{SearchRequestV1, SearchTimeRangeV1};

    let db = Db::open_in_memory(clock(100)).unwrap();
    db.capture(&ev("range sentinel", "before-end", 19)).unwrap();
    db.capture(&ev("range sentinel", "at-end", 20)).unwrap();

    let response = db
        .search_request(&SearchRequestV1 {
            query: "range sentinel".into(),
            limit: 8,
            range: Some(SearchTimeRangeV1 {
                from_ms: 10,
                to_ms: 20,
            }),
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert!(response
        .federated_hits
        .iter()
        .any(|hit| hit.period_start_ms == Some(19)));
    assert!(response
        .federated_hits
        .iter()
        .all(|hit| hit.period_start_ms != Some(20)));
}

#[test]
fn try_search_keeps_a_raw_event_when_its_meeting_cluster_is_primary() {
    use shogun_memory::meeting_index::index_session;
    use shogun_memory::meeting_recaps;
    use shogun_memory::session::{close, open, NewSession};
    use shogun_memory::transcript_segments::{append, NewSegment, Speaker};

    let db = Db::open_in_memory(clock(3_000)).unwrap();
    let (_meeting, event_id) = db
        .with_conn_mut("test.raw_compatibility.seed", |conn| {
            let meeting = open(
                conn,
                &NewSession {
                    kind: "meeting",
                    started_at: 1_000,
                    title: Some("Raw compatibility"),
                    app_bundle_id: None,
                    calendar_occurrence_id: None,
                    confidence: 0.9,
                    provenance: "{}",
                },
            )?;
            append(
                conn,
                &NewSegment {
                    session_id: meeting,
                    ts: 1_100,
                    speaker: Speaker::Other,
                    text: "clustered raw compatibility sentinel",
                    confidence: 0.9,
                },
                1_100,
            )?;
            close(conn, meeting, 2_000)?;
            let indexed = index_session(conn, meeting)?;
            meeting_recaps::save(
                conn,
                meeting,
                "clustered raw compatibility sentinel",
                "[]",
                "[]",
                None,
                "test",
                2_100,
            )?;
            Ok::<_, rusqlite::Error>((meeting, indexed.transcript_event_id.unwrap()))
        })
        .unwrap();

    let federated = db
        .search_request(&SearchRequestV1 {
            query: "clustered raw compatibility sentinel".into(),
            limit: 8,
            ..SearchRequestV1::default()
        })
        .unwrap();
    assert!(federated.federated_hits.iter().any(|hit| {
        hit.source_id != shogun_memory::search::FederatedSourceId::Event(event_id)
            && hit.sources.iter().any(|source| {
                source.id == shogun_memory::search::FederatedSourceId::Event(event_id)
            })
    }));

    let hits = db
        .try_search("clustered raw compatibility sentinel", 8)
        .unwrap();
    assert!(hits.iter().any(|hit| hit.event_id == event_id));
}

#[test]
fn default_search_widens_when_warm_derived_clusters_hide_an_old_raw_event() {
    use shogun_memory::state::{NewProject, ProjectStatus, Provenance};

    let now = shogun_memory::search::WARM_WINDOW_MS * 2;
    let db = Db::open_in_memory(clock(now)).unwrap();
    let old_event = db
        .capture(&ev("archive widening sentinel", "archive-widening-raw", 1))
        .unwrap()
        .0;

    for index in 0..4 {
        let evidence_content = format!("unrelated project evidence {index}");
        let evidence_hash = format!("archive-widening-derived-{index}");
        let evidence = db
            .capture(&ev(&evidence_content, &evidence_hash, now - 1))
            .unwrap()
            .0;
        let name = format!("Derived cluster {index}");
        db.insert_project(
            &NewProject {
                name: &name,
                status: ProjectStatus::Active,
                summary: Some("archive widening sentinel"),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: now - 1,
            },
            &[Provenance::new(evidence)],
        )
        .unwrap();
    }

    let response = db
        .search_request(&SearchRequestV1 {
            query: "archive widening sentinel".into(),
            limit: 8,
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert_eq!(response.lexical_scope, SearchLexicalScopeV1::AllHistory);
    assert!(response.hits.iter().any(|hit| hit.event_id == old_event));
}

#[test]
fn canonical_search_rejects_invalid_depth_and_range_combinations() {
    use crate::daemon::{
        SearchDepthV1, SearchErrorV1, SearchPolicyErrorV1, SearchRequestV1, SearchTimeRangeV1,
    };

    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let missing = db
        .search_request(&SearchRequestV1 {
            query: "anything".into(),
            limit: 8,
            depth: SearchDepthV1::All,
            range: None,
            source_kind: None,
            include_scratch: false,
        })
        .unwrap_err();
    assert_eq!(
        missing,
        SearchErrorV1::Policy(SearchPolicyErrorV1::RangeRequired)
    );

    let future = db
        .search_request(&SearchRequestV1 {
            query: "anything".into(),
            limit: 8,
            depth: SearchDepthV1::Warm,
            range: Some(SearchTimeRangeV1 {
                from_ms: 900,
                to_ms: 1_001,
            }),
            source_kind: None,
            include_scratch: false,
        })
        .unwrap_err();
    assert_eq!(
        future,
        SearchErrorV1::Policy(SearchPolicyErrorV1::InvalidRange)
    );
}

#[test]
fn canonical_search_auto_resolves_from_complete_range_not_hit_count() {
    use crate::daemon::{SearchDepthV1, SearchRequestV1, SearchTimeRangeV1};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let db = Db::open_in_memory(clock(now)).unwrap();
    let response = db
        .search_request(&SearchRequestV1 {
            query: "no match".into(),
            limit: 8,
            depth: SearchDepthV1::Auto,
            range: Some(SearchTimeRangeV1 {
                from_ms: now - DAY_MS,
                to_ms: now,
            }),
            source_kind: None,
            include_scratch: false,
        })
        .unwrap();

    assert_eq!(response.effective_depth, SearchDepthV1::Warm);
    assert_eq!(response.requested_range, Some(response.resolved_range));
    assert_eq!(response.cold_partitions_visited, 0);
}

#[test]
fn canonical_search_all_finds_demoted_semantic_only_memory() {
    use crate::daemon::{SearchDepthV1, SearchRequestV1, SearchTimeRangeV1};
    use shogun_memory::embed::{MockEmbedder, E5_SMALL_DIM};

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let now = 100 * DAY_MS;
    let old_ts = now - 80 * DAY_MS;
    let db = Db::open_in_memory(clock(now))
        .unwrap()
        .with_embedder(Arc::new(MockEmbedder::new(E5_SMALL_DIM)));
    let (old_id, _) = db
        .capture(&ev(
            "the budget review meeting is on friday",
            "cold",
            old_ts,
        ))
        .unwrap();
    assert_eq!(db.embed_pending(10), 1);
    assert!(db
        .with_conn_mut("test.cold.demote", |conn| {
            shogun_memory::cold::demote(conn, old_id, old_ts)
        })
        .unwrap());

    let lexical = db.try_search("standup", 8).unwrap();
    assert!(lexical.iter().all(|hit| hit.event_id != old_id));
    let response = db
        .search_request(&SearchRequestV1 {
            query: "review meeting standup".into(),
            limit: 8,
            depth: SearchDepthV1::All,
            range: Some(SearchTimeRangeV1 {
                from_ms: old_ts,
                to_ms: now,
            }),
            source_kind: None,
            include_scratch: false,
        })
        .unwrap();

    assert_eq!(response.effective_depth, SearchDepthV1::All);
    assert!(response.cold_partitions_visited >= 1);
    assert!(response.cold_vectors_scanned >= 1);
    assert!(response.hits.iter().any(|hit| hit.event_id == old_id));
}

#[test]
fn canonical_search_applies_cold_cap_only_when_vector_scan_can_run() {
    use crate::daemon::{
        SearchDepthV1, SearchErrorV1, SearchPolicyErrorV1, SearchRequestV1, SearchTimeRangeV1,
    };
    use shogun_memory::embed::{MockEmbedder, E5_SMALL_DIM};

    let now = 20 * shogun_memory::cold::PARTITION_MS;
    let mut db = Db::open_in_memory(clock(now))
        .unwrap()
        .with_embedder(Arc::new(MockEmbedder::new(E5_SMALL_DIM)));
    let mut archived = Vec::new();
    for partition in 1..=7i64 {
        let ts = partition * shogun_memory::cold::PARTITION_MS + 1;
        let text = format!("archived note {partition}");
        let hash = format!("cold-{partition}");
        let (id, _) = db.capture(&ev(&text, &hash, ts)).unwrap();
        archived.push((id, ts));
    }
    assert_eq!(db.embed_pending(20), archived.len());
    for (id, ts) in archived {
        assert!(db
            .with_conn_mut("test.cold.demote", |conn| {
                shogun_memory::cold::demote(conn, id, ts)
            })
            .unwrap());
    }

    let error = db
        .search_request(&SearchRequestV1 {
            query: "archived note".into(),
            limit: 8,
            depth: SearchDepthV1::All,
            range: Some(SearchTimeRangeV1 {
                from_ms: 0,
                to_ms: now,
            }),
            source_kind: None,
            include_scratch: false,
        })
        .unwrap_err();

    assert_eq!(
        error,
        SearchErrorV1::Policy(SearchPolicyErrorV1::ColdRangeTooBroad)
    );

    db.embedder = None;
    for depth in [SearchDepthV1::Auto, SearchDepthV1::All] {
        let lexical = db
            .search_request(&SearchRequestV1 {
                query: "archived note".into(),
                limit: 8,
                depth,
                range: Some(SearchTimeRangeV1 {
                    from_ms: 0,
                    to_ms: now,
                }),
                source_kind: None,
                include_scratch: false,
            })
            .unwrap();
        assert_eq!(lexical.hits.len(), 7);
        assert_eq!(lexical.cold_partitions_visited, 0);
        assert_eq!(lexical.cold_vectors_scanned, 0);
    }
}

#[test]
fn canonical_empty_query_does_no_database_work() {
    use crate::daemon::{SearchRequestV1, SearchResponseV1};

    let db = Db::open_in_memory(clock(1_000)).unwrap();
    drop_table(&db, "event_log");
    let SearchResponseV1 { hits, complete, .. } = db
        .search_request(&SearchRequestV1 {
            query: "   ".into(),
            ..SearchRequestV1::default()
        })
        .unwrap();

    assert!(hits.is_empty());
    assert!(complete);
}

#[test]
fn a_query_failure_is_reported_instead_of_looking_empty() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.capture(&ev("the budget review notes", "h1", 900))
        .unwrap();
    // Sanity: the same call answers before the break.
    assert!(!db.try_search("budget", 10).unwrap().is_empty());

    drop_table(&db, "event_log");
    let err = db.try_search("budget", 10).unwrap_err();
    assert_eq!(
        err,
        MemoryFault::Query,
        "a refused statement is not an empty result"
    );
    // The lossy wrapper still returns a vec, but the store is now visibly degraded — which is
    // the whole point: the caller that cannot act on the error is not the only one told.
    assert_eq!(db.search("budget", 10), Vec::new());
    let h = db.memory_health();
    assert!(h.degraded);
    assert_eq!(h.fault, Some(MemoryFault::Query));
    assert!(h.faults_total >= 2);
    assert_eq!(h.last_fault_ms, Some(1_000));
}

#[test]
fn a_poisoned_lock_is_recorded_but_the_store_keeps_serving() {
    // std mutex poisoning is permanent, but the SQLite connection under it is still valid —
    // refusing the guard would turn one panic anywhere into "capture stops until relaunch".
    // The fault must be visible (issue #121) AND the store must keep answering.
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.capture(&ev("the budget review notes", "h1", 900))
        .unwrap();
    poison_lock(&db);

    // Reads and writes recover and answer…
    assert!(!db.try_search("budget", 10).unwrap().is_empty());
    assert!(db.try_commitment_rows().unwrap().is_empty());
    assert!(db.capture(&ev("after the poison", "h2", 950)).is_some());

    // …and the poisoning was recorded, not swallowed (faults_total never resets).
    let h = db.memory_health();
    assert!(
        h.faults_total >= 1,
        "the poison must be visible in the health signal"
    );
    assert!(!h.degraded, "a served operation lifts the degraded state");
}

#[test]
fn state_reads_do_not_report_a_dead_store_as_nothing_owed() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    drop_table(&db, "commitments");
    // "You owe nothing today" is the answer this must never invent.
    assert_eq!(
        db.try_commitments_due(1_000).unwrap_err(),
        MemoryFault::Query
    );
    assert!(db.memory_health().degraded);
    // …and the fault is per operation, not a blanket verdict: an intact table still answers,
    // which is exactly what makes the recovery rule below meaningful.
    assert_eq!(db.try_people().unwrap(), Vec::new());
}

#[test]
fn the_degraded_state_lifts_once_the_store_answers_again() {
    // Recovery without a relaunch (the issue's acceptance criterion): a transient failure
    // must not leave the warning stuck on forever.
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    drop_table(&db, "people");
    assert!(db.try_people().is_err());
    assert!(db.memory_health().degraded);

    // A different read against an intact table is a successful operation.
    assert!(db.try_commitment_rows().is_ok());
    let h = db.memory_health();
    assert!(!h.degraded, "a success clears the degraded state");
    assert_eq!(h.faults_total, 1, "…without erasing that it happened");
}

#[test]
fn a_note_read_keeps_a_failure_apart_from_no_note() {
    // The notch editor loads the stored note before a keystroke may save it (#767). A failed read
    // that looked like "no note" would let the next save replace the whole note.
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let meeting = db.open_meeting(Some("Standup"), None, 0.9, "{}").unwrap();
    assert_eq!(db.try_meeting_note(meeting).unwrap(), None);
    assert!(db.save_meeting_note(meeting, "three paragraphs"));
    assert_eq!(
        db.try_meeting_note(meeting).unwrap().as_deref(),
        Some("three paragraphs")
    );
    db.conn
        .lock()
        .unwrap()
        .execute_batch("ALTER TABLE session_notes RENAME TO session_notes_gone")
        .unwrap();
    assert!(db.try_meeting_note(meeting).is_err());
    assert_eq!(db.meeting_note(meeting), None, "the recap path still reads it as nothing to add");
}

#[test]
fn an_ordinary_missing_row_is_not_a_fault() {
    // The failure mode this guards against is a warning that is always on: every `get` for a
    // row that simply does not exist would light "memory isn't responding" if a miss were
    // treated as a query error. The memory helpers map a missing row to `Ok(None)` — this
    // pins that they keep doing so through the health seam.
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    assert_eq!(db.person(404), None);
    assert_eq!(db.commitment(404), None);
    assert_eq!(db.open_loop(404), None);
    assert_eq!(db.meeting_note(404), None);
    assert_eq!(db.meeting_recap_full(404), None);
    assert_eq!(db.brief_for("2026-08-15"), None);
    assert_eq!(db.event_source(404), None);
    let h = db.memory_health();
    assert!(!h.degraded, "a miss is an answer");
    assert_eq!(h.faults_total, 0);
}

#[test]
fn a_capture_write_failure_marks_memory_degraded() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    drop_table(&db, "event_log");
    assert_eq!(
        db.try_capture(&ev("text", "h1", 1)).unwrap_err(),
        MemoryFault::Query
    );
    assert!(db.capture(&ev("text", "h2", 2)).is_none());
    assert!(
        db.memory_health().degraded,
        "capture that silently stops recording must show"
    );
}

#[test]
fn event_source_returns_origin_metadata_only() {
    let db = Db::open_in_memory(clock(10)).unwrap();
    let (id, _) = db.capture(&ev("some captured text", "h1", 1)).unwrap();
    assert_eq!(
        db.event_source(id),
        Some(("capture".to_string(), Some("com.apple.Safari".to_string())))
    );
    assert_eq!(
        db.event_source(id + 999),
        None,
        "unknown id is None, not an error"
    );
}

#[test]
fn the_wrap_window_starts_at_local_midnight_and_ends_after_tomorrow() {
    // On the Linux CI path local_day_bounds is UTC, so this pins the arithmetic; on macOS the
    // libc branch supplies the same shape with DST folded in.
    const DAY: i64 = 24 * 60 * 60 * 1_000;
    let now = 3 * DAY + 72_000_000; // 20:00 on day 3
    let (start, end) = local_wrap_window(now);
    assert!(start <= now && now < end);
    assert_eq!(
        start,
        local_day_bounds(now).today_start_ms,
        "the window opens at midnight"
    );
    assert_eq!(end - start, 2 * DAY, "today plus tomorrow");
}

#[test]
fn evening_wrap_aggregates_the_day_locally() {
    use shogun_memory::lessons::{FeedbackKind, LessonScope, NewFeedback};
    use shogun_memory::state::{
        CommitmentDirection, CommitmentStatus, NewCommitment, NewOpenLoop, OpenLoopKind, Provenance,
    };
    // A day: 00:00 = 0ms, "now" = 20:00 (72_000_000ms), tomorrow ends at 48h.
    let day_start = 0i64;
    let now = 72_000_000i64;
    let tomorrow_end = 172_800_000i64;
    let db = Db::open_in_memory(clock(now)).unwrap();
    let (e, _) = db.capture(&ev("evidence", "h1", 1)).unwrap();
    let prov = [Provenance::new(e)];

    // done today (outcome), still-open with today's activity, due-tomorrow, opened-today loop.
    let done_id = db
        .insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "finished today",
                due_at: Some(10),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 100,
            },
            &prov,
        )
        .unwrap();
    assert!(db.resolve_commitment(done_id));
    db.insert_commitment(
        &NewCommitment {
            direction: CommitmentDirection::Mine,
            counterparty_id: None,
            description: "due tomorrow",
            due_at: Some(now + 3_600_000),
            status: CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 200, // updated today → also "still open"
        },
        &prov,
    )
    .unwrap();
    db.insert_open_loop(
        &NewOpenLoop {
            kind: OpenLoopKind::ReplyNeeded,
            description: "new loose end",
            counterparty_id: None,
            project_id: None,
            opened_at: 500,
            confidence: 0.9,
            now: 500,
        },
        &prov,
    )
    .unwrap();
    // Today's decisions come from `feedback_events` — the table the lessons already learn
    // from. One adopted (edited, then approved) and one that is not a decision on a proposal
    // at all (`state_resolve`), so the counts have something to get wrong.
    {
        let conn = db.conn.lock().unwrap();
        let f = NewFeedback {
            ts_ms: 600,
            action_kind: Some("draft_reply"),
            scope_ref: None,
            before_text: Some("draft"),
            after_text: Some("edited draft"),
            ..Default::default()
        };
        shogun_memory::lessons::record_feedback(
            &conn,
            FeedbackKind::EditBeforeApprove,
            LessonScope::Global,
            &f,
        )
        .unwrap();
        shogun_memory::lessons::record_feedback(
            &conn,
            FeedbackKind::StateResolve,
            LessonScope::Global,
            &NewFeedback {
                before_text: None,
                after_text: None,
                ..f
            },
        )
        .unwrap();
    }

    let wrap = db.evening_wrap(
        vec![CalendarLine {
            start_ms: tomorrow_end - 1000,
            title: "standup".into(),
            all_day: false,
            updated: false,
        }],
        day_start,
        now,
        tomorrow_end,
    );

    assert_eq!(wrap.outcome.commitments_done, 1);
    // `state_resolve` is not a decision on one of SHOGUN's proposals, so it counts in neither.
    assert_eq!(wrap.outcome.actions_decided, 1);
    assert_eq!(wrap.outcome.actions_adopted, 1);
    // the resolved commitment is outcome, not "still open"; the live one shows under Tomorrow
    // only, and the loop opened today under Loose ends only.
    assert!(wrap.still_open.iter().all(|i| i.text != "due tomorrow" && i.text != "new loose end"));
    assert!(wrap.still_open.iter().all(|i| i.text != "finished today"));
    assert_eq!(wrap.tomorrow_commitments.len(), 1);
    assert_eq!(wrap.tomorrow_commitments[0].text, "due tomorrow");
    assert_eq!(wrap.tomorrow_calendar.len(), 1);
    assert_eq!(wrap.loose_ends.len(), 1);
    assert_eq!(wrap.loose_ends[0].text, "new loose end");
}

#[cfg(feature = "daemon-server")]
#[test]
fn confirmed_send_persists_a_trace_row_for_every_attempt() {
    use crate::send_exec::{SendExecOutcome, SendTransport};
    use shogun_agents::approval::{ConfirmedSend, Preview, Route as ApprovalRoute};
    use shogun_agents::permission::SendAction;

    struct Transport {
        ok: bool,
    }
    impl SendTransport for Transport {
        fn send(&self, _a: &SendAction, _body: &str) -> Result<(), String> {
            if self.ok {
                Ok(())
            } else {
                Err("not connected".into())
            }
        }
    }

    let db = Db::open_in_memory(clock(1)).unwrap();
    let action = SendAction::SendEmail {
        to: "alice@example.com".into(),
    };
    let body = "TOP SECRET send body";
    let cs = ConfirmedSend {
        action: action.clone(),
        preview: Preview::for_send(&action, body, ApprovalRoute::ViaComposio),
    };

    // Issue #782: a failed send is still an attempt — the body may have reached the provider
    // before the error — so it leaves a row.
    assert_eq!(
        db.execute_confirmed_send(&cs, &Transport { ok: false }),
        SendExecOutcome::Failed("not connected".into())
    );
    assert_eq!(
        db.trace_rows(&Filter::default()).len(),
        1,
        "a failed attempt is on the ledger"
    );

    // a successful send writes its own digest-only, third-party row
    assert_eq!(
        db.execute_confirmed_send(&cs, &Transport { ok: true }),
        SendExecOutcome::Sent
    );
    let rows = db.trace_rows(&Filter::default());
    assert_eq!(rows.len(), 2, "one row per egress attempt");
    for row in &rows {
        // Issue #924: the service, never the recipient the approval card showed.
        assert_eq!(row.destination, "gmail");
        assert!(row.third_party, "a Composio send is disclosed third-party");
        assert_eq!(row.chunk_bytes, body.len() as i64);
        assert!(
            !format!("{row:?}").contains("SECRET"),
            "sent body must never reach the trace row"
        );
        assert!(
            !format!("{row:?}").contains("alice"),
            "the recipient must never reach the trace row"
        );
    }
}

#[cfg(feature = "daemon-server")]
#[test]
fn persisted_send_rows_never_keep_event_or_document_titles() {
    use crate::send_exec::{SendExecOutcome, SendTransport};
    use shogun_agents::approval::{ConfirmedSend, Preview, Route as ApprovalRoute};
    use shogun_agents::permission::SendAction;
    use std::cell::RefCell;

    /// Records the action it was handed, so the test sees what the request carried.
    struct Transport(RefCell<Vec<SendAction>>);
    impl SendTransport for Transport {
        fn send(&self, a: &SendAction, _body: &str) -> Result<(), String> {
            self.0.borrow_mut().push(a.clone());
            Ok(())
        }
    }

    // Issue #924. Fictional private titles: the card and the request keep them, the row does not.
    let db = Db::open_in_memory(clock(1)).unwrap();
    let transport = Transport(RefCell::new(Vec::new()));
    let cases = [
        (SendAction::CreateCalendarEvent { title: "Medical appointment".into() }, "gcal"),
        (SendAction::UpdateCalendarEvent { title: "Custody hearing".into() }, "gcal"),
        (SendAction::CreateDocument { title: "Resignation letter".into() }, "gdrive"),
        (SendAction::UpdateDocument { title: "Salary review notes".into() }, "notion"),
    ];
    for (action, _) in &cases {
        let cs = ConfirmedSend {
            action: action.clone(),
            preview: Preview::for_send(action, "body", ApprovalRoute::DirectMcp),
        };
        assert_eq!(db.execute_confirmed_send(&cs, &transport), SendExecOutcome::Sent);
    }
    let sent = transport.0.borrow();
    assert_eq!(sent.len(), cases.len());
    let rows = db.trace_rows(&Filter::default());
    assert_eq!(rows.len(), cases.len());
    for (action, service) in &cases {
        let purpose = match action {
            SendAction::CreateCalendarEvent { .. } => "integration.create_calendar_event",
            SendAction::UpdateCalendarEvent { .. } => "integration.update_calendar_event",
            SendAction::CreateDocument { .. } => "integration.create_document",
            _ => "integration.update_document",
        };
        let row = rows.iter().find(|r| r.purpose == purpose).unwrap();
        assert_eq!(row.destination, *service, "{purpose}");
    }
    for ((action, _), sent) in cases.iter().zip(sent.iter()) {
        assert_eq!(sent, action, "the request keeps the full title");
    }
    let persisted = format!("{rows:?}");
    for title in ["Medical", "Custody", "Resignation", "Salary"] {
        assert!(!persisted.contains(title), "{title} leaked into the trace rows");
    }
}

#[test]
fn connector_reads_are_traced_per_request_even_when_they_fail() {
    use shogun_integrations::{ConnectorRuntime, IngestSink};
    use shogun_mcp::scope::{Service, Wave};
    use shogun_mcp::sync::{FetchedItem, IntegrationTransport};

    struct Failing;
    impl IntegrationTransport for Failing {
        fn read_sync(&self, _s: Service) -> Result<Vec<FetchedItem>, String> {
            Err("composio http 502".into())
        }
        fn fetch_on_demand(&self, _s: Service, _q: &str) -> Result<Vec<FetchedItem>, String> {
            Err("composio http 502".into())
        }
    }

    let db = Db::open_in_memory(clock(1)).unwrap();
    let mut rt = ConnectorRuntime::new(Failing, Wave::One, true);
    rt.set_plan(shogun_agents::entitlement::entitlements(
        shogun_agents::entitlement::Plan::Pro,
        0,
    ));
    // Without consent the gate refuses Gmail before anything is sent: no row.
    rt.mark_connected(Service::Gmail, 1);
    assert!(rt.sync_service(Service::Gmail, 2, &db).is_err());
    assert!(db.trace_rows(&Filter::default()).is_empty());

    // With consent the user id and query go to Composio even though nothing comes back.
    rt.set_composio_consent(true);
    assert!(rt.sync_service(Service::Gmail, 3, &db).is_err());
    assert!(rt.fetch_on_demand(Service::Gmail, "thread-secret-query", &db).is_err());
    let rows = db.trace_rows(&Filter::default());
    assert_eq!(rows.len(), 2, "one row per read request sent");
    for row in &rows {
        assert_eq!(row.purpose, "gmail_read");
        assert_eq!(row.destination, "gmail");
        assert!(
            !row.third_party,
            "direct Gmail does not cross an integration broker"
        );
        assert_eq!(
            row.chunk_bytes, 0,
            "content-free: THAT a read happened, never what"
        );
        assert!(!format!("{row:?}").contains("secret"));
    }
    // The hook itself is the sink seam the desktop poller relies on.
    db.record_fetch_attempt(Service::GoogleCalendar, shogun_integrations::FetchKind::SyncPage);
    let rows = db.trace_rows(&Filter::default());
    assert!(rows
        .iter()
        .any(|r| r.purpose == "first_layer_read" && !r.third_party));
}

#[test]
fn ingested_integration_items_are_searchable_and_source_tagged() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    let items = vec![
        IngestItem {
            source: "gmail",
            kind: "email",
            title: "Roadmap".into(),
            body: "Let's ship the quarterly deck on Friday".into(),
            ts_ms: 100,
            external_id: String::new(),
            calendar: None,
            mail: None,
        },
        IngestItem {
            source: "gcal",
            kind: "calendar_event",
            title: "Standup".into(),
            body: "Daily standup with the platform team".into(),
            ts_ms: 200,
            external_id: String::new(),
            calendar: None,
            mail: None,
        },
    ];
    let summary = db.ingest_integration(&items);
    assert_eq!(
        (summary.processed, summary.newly_inserted),
        (2, 2),
        "both items are fresh inserts"
    );

    // the synced email is now first-class memory: it comes back from hybrid search, tagged gmail…
    let hits = db.search("quarterly deck", 10);
    let email = hits
        .iter()
        .find(|h| h.content.contains("quarterly deck"))
        .expect("synced email searchable");
    assert_eq!(
        email.source, "gmail",
        "the hit carries its integration source (FR-INT-05)"
    );
    // …and the calendar event too.
    let hits = db.search("standup", 10);
    assert!(
        hits.iter().any(|h| h.source == "gcal"),
        "synced calendar event must be searchable"
    );
}

#[test]
fn dream_run_summary_reflects_events_state_and_chunks() {
    use crate::dreamcycle::plan::JobKind;
    use crate::dreamcycle::run::CycleReport;
    use crate::llm::traceability::{Route, TraceRecord};

    // clock at 1000 so run-start filtering (>= 1000) is meaningful.
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    // three events in the input window [10, 40)
    let mut first_event = 0;
    for (i, ts) in [10, 20, 30].into_iter().enumerate() {
        let (id, _) = db.capture(&ev("body", &format!("h{i}"), ts)).unwrap();
        if i == 0 {
            first_event = id;
        }
    }
    // a state change and two sent chunks happen "during the run" (updated_at/ts >= 1000 via clock)
    db.insert_open_loop(
        &shogun_memory::state::NewOpenLoop {
            kind: shogun_memory::state::OpenLoopKind::WaitingOnThem,
            description: "waiting on legal",
            counterparty_id: None,
            project_id: None,
            opened_at: 1_000,
            confidence: 0.9,
            now: 1_000,
        },
        &[shogun_memory::state::Provenance::new(first_event)],
    )
    .expect("open loop inserts with provenance");
    let sink = db.traceability_sink();
    sink.record(TraceRecord::for_chunk(
        Route::BatchApi,
        "consolidation",
        "api",
        "c1",
        false,
    ));
    sink.record(TraceRecord::for_chunk(
        Route::BatchApi,
        "consolidation",
        "api",
        "c2",
        false,
    ));

    let report = CycleReport {
        completed: vec![JobKind::Consolidation, JobKind::StateUpdate],
        failed: None,
    };
    let summary = db.summarize_dream_run(CycleKind::Full, &report, 10, 40, 1_000, 1_250);

    assert_eq!(
        summary.events_processed, 3,
        "three events in the input window"
    );
    assert!(
        summary.state_changes >= 1,
        "the open loop counts as a state change"
    );
    assert_eq!(
        summary.chunks_sent, 2,
        "two traceability rows written during the run"
    );
    assert_eq!(summary.jobs_completed, 2);
    assert_eq!(summary.duration_ms, 250);
    assert!(summary.completed_fully);
}

/// The reply context must carry the thread's own words, the state facts, and be measurable.
#[test]
fn a_reply_context_carries_the_thread_and_reports_its_build_cost() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Q3 pricing"),
        ..ev("Alice asked for the renewal terms", "h1", 100)
    })
    .unwrap();
    db.capture(&NewEvent {
        window_title: Some("Q3 pricing"),
        ..ev("we settled at 12k for the year", "h2", 200)
    })
    .unwrap();
    let key = shogun_memory::thread::thread_key(
        "capture",
        None,
        Some("com.apple.Safari"),
        Some("Q3 pricing"),
    )
    .unwrap();

    let ctx = db.build_reply_context(&key);
    assert_eq!(ctx.title.as_deref(), Some("Q3 pricing"));
    assert_eq!(
        ctx.turns.len(),
        2,
        "the whole conversation, not just the last line"
    );
    // Oldest first — a reply reads the thread in order.
    assert!(ctx.turns[0].excerpt.contains("renewal terms"));
    assert!(ctx.turns[1].excerpt.contains("12k"));
    // The SLO measurement ships with the data it describes.
    assert!(
        ctx.build_ms < 1_000,
        "assembly should be fast: {}ms",
        ctx.build_ms
    );
}

/// Repeated captures can show one underlying assertion; maintenance cannot promote it.
#[test]
fn local_maintenance_does_not_promote_repeated_captures() {
    let db = Db::open_in_memory(clock(1_000_000)).unwrap();
    let events: Vec<i64> = (0..4)
        .map(|i| {
            db.capture(&ev("I'll send the deck", &format!("h{i}"), 100 + i))
                .unwrap()
                .0
        })
        .collect();
    let prov: Vec<_> = events
        .iter()
        .map(|e| shogun_memory::state::Provenance::new(*e))
        .collect();
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send the deck",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.35, // what the local rules assign: Low, so invisible
            now: 100,
        },
        &prov,
    )
    .unwrap();

    // Precondition: the confidence gate excludes it, so the user sees nothing.
    assert!(
        !db.inline_memory(8)
            .iter()
            .any(|f| f.contains("send the deck")),
        "precondition: a Low-confidence commitment is excluded"
    );

    let report = db.run_local_maintenance(1_000_000, 30 * 24 * 3_600_000);
    assert_eq!(report.corroborated, 0);
    assert!(
        !db.inline_memory(8).iter().any(|f| f.contains("send the deck")),
        "unverified repeats must stay below the confidence gate"
    );
}

#[test]
fn local_maintenance_runs_one_bounded_projection_backfill_page() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    db.with_conn("test.seed_projection_backfill", |conn| {
        let event_id = shogun_memory::event_log::insert(
            conn,
            &ev("Backfill source", "backfill-source", 1),
        )?;
        for index in 0..205 {
            conn.execute(
                "INSERT INTO people
                       (display_name, confidence, base_confidence,
                        created_at, updated_at, last_evidence_at)
                     VALUES (?1, 0.8, 0.8, 1, 1, 1)",
                [format!("Backfill person {index}")],
            )?;
            let person_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO state_provenance(state_table,state_id,event_id,weight)
                 VALUES('people',?1,?2,1.0)",
                rusqlite::params![person_id, event_id],
            )?;
        }
        conn.execute("DELETE FROM search_document_invalidations", [])?;
        Ok::<_, rusqlite::Error>(())
    })
    .unwrap();

    db.run_local_maintenance(1, 30 * 24 * 3_600_000);

    let projected = db
        .with_conn("test.count_projection_backfill", |conn| {
            conn.query_row("SELECT count(*) FROM search_documents", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(projected, 100);
}

/// An overdue notification is a non-egress, L1-permitted local action — it can never
/// be (or become) a send. `required_level` derives L1 from the action type; the type is
/// `Action::Local`, which has no send variant, so the property is structural.
#[test]
fn overdue_notifications_are_l1_non_sends() {
    use shogun_agents::permission::Level;
    let newly = vec![
        shogun_memory::recompute::NewlyOverdue {
            id: 1,
            description: "send the deck".into(),
        },
        shogun_memory::recompute::NewlyOverdue {
            id: 2,
            description: "review the PR".into(),
        },
    ];
    let actions = overdue_notifications(&newly);
    assert_eq!(
        actions.len(),
        newly.len(),
        "one notification per newly-overdue item"
    );
    for a in &actions {
        assert_eq!(
            a.required_level(),
            Level::L1,
            "a notification is L1-permitted: {a:?}"
        );
        assert!(a.is_l1_eligible());
        assert!(
            !a.is_external_send(),
            "a notification never leaves the device: {a:?}"
        );
    }
    assert_ne!(
        actions[0], actions[1],
        "each item gets its own notification"
    );
}

/// End to end: the hourly maintenance reports a newly-overdue
/// commitment exactly once — the pass that flips it — and later passes report nothing, so the
/// desktop hook fires one notification per commitment, ever.
#[test]
fn maintenance_reports_newly_overdue_once_then_never_again() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let e = db.capture(&ev("promise", "h1", 100)).unwrap().0;
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send the deck",
            due_at: Some(5_000),
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 100,
        },
        &[shogun_memory::state::Provenance::new(e)],
    )
    .unwrap();

    const HALF_LIFE: i64 = 30 * 24 * 3_600_000;
    // Before the due time: nothing to notify.
    let before = db.run_local_maintenance(1_000, HALF_LIFE);
    assert!(before.newly_overdue.is_empty());
    assert!(overdue_notifications(&before.newly_overdue).is_empty());

    // The pass that crosses the due time notifies exactly once.
    let first = db.run_local_maintenance(10_000, HALF_LIFE);
    assert_eq!(first.newly_overdue.len(), 1);
    assert_eq!(first.overdue, 1);
    let notes = overdue_notifications(&first.newly_overdue);
    assert_eq!(notes.len(), 1);

    // Every subsequent pass: silent for the already-notified item.
    for later in [10_000, 20_000, 1_000_000] {
        let again = db.run_local_maintenance(later, HALF_LIFE);
        assert!(
            again.newly_overdue.is_empty(),
            "must never re-notify: {again:?}"
        );
    }
}

/// A reply is written *into* a conversation, so the thread's own words must lead the prompt —
/// state facts and older similar threads are supporting material, not the subject.
#[test]
fn reply_context_flattens_with_the_thread_first() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Q3 pricing"),
        ..ev("Alice asked for the renewal terms", "h1", 100)
    })
    .unwrap();
    let key = shogun_memory::thread::thread_key(
        "capture",
        None,
        Some("com.apple.Safari"),
        Some("Q3 pricing"),
    )
    .unwrap();

    let lines = db.build_reply_context(&key).as_memory_lines(10);
    assert!(
        lines[0].contains("Q3 pricing"),
        "what's in view is named first: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("renewal terms")),
        "the thread's own words are present: {lines:?}"
    );
}

#[test]
fn reply_prewarming_carries_derived_fact_and_canonical_grounding() {
    use shogun_memory::state::{NewProject, ProjectStatus, Provenance};
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let evidence_id = db
        .capture(&NewEvent {
            window_title: Some("Vendor renewal"),
            ..ev(
                "Canonical supplier contract evidence",
                "reply-grounding",
                100,
            )
        })
        .unwrap()
        .0;
    db.insert_project(
        &NewProject {
            name: "Vendor renewal",
            status: ProjectStatus::Active,
            summary: Some("supplier contract extension accepted"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 200,
        },
        &[Provenance::new(evidence_id)],
    )
    .unwrap();
    let key = shogun_memory::thread::thread_key(
        "capture",
        None,
        Some("com.apple.Safari"),
        Some("Vendor renewal"),
    )
    .unwrap();
    let pack = db.build_reply_context(&key).to_context_pack();
    assert!(pack
        .facts
        .iter()
        .any(|fact| fact.contains("supplier contract extension accepted")));
    assert!(pack.evidence.iter().any(|evidence| {
        evidence.event_id == evidence_id
            && evidence.source == "capture"
            && evidence
                .excerpt
                .contains("Canonical supplier contract evidence")
    }));
    assert!(db
        .build_reply_context("capture:unknown:surface")
        .to_context_pack()
        .facts
        .is_empty());
}

#[test]
fn reply_prompt_pack_keeps_latest_turns_in_order_and_reserves_related_grounding() {
    let evidence = |event_id| Evidence {
        event_id,
        ts: event_id,
        source: "capture".into(),
        title: None,
        excerpt: format!("evidence {event_id}"),
        frame_id: None,
    };
    let context = ReplyContext {
        turns: (0..10).map(&evidence).collect(),
        related: (100..102).map(evidence).collect(),
        ..ReplyContext::default()
    };

    let pack = context.to_context_pack();

    assert_eq!(pack.evidence.len(), crate::context::PROMPT_EVIDENCE_LIMIT);
    assert_eq!(pack.evidence[0].event_id, 4);
    assert_eq!(pack.evidence[5].event_id, 9);
    assert_eq!(pack.evidence[6].event_id, 100);
    assert_eq!(pack.evidence[7].event_id, 101);
}

#[test]
fn inline_prompt_pack_excludes_old_memory_and_keeps_latest_thread_turns() {
    let evidence = |event_id| Evidence {
        event_id,
        ts: event_id,
        source: "capture".into(),
        title: None,
        excerpt: format!("turn {event_id}"),
        frame_id: None,
    };
    let mut context = ReplyContext {
        turns: (0..12).map(&evidence).collect(),
        facts: vec!["unrelated old fact".into()],
        related: vec![evidence(100)],
        ..Default::default()
    };
    context.set_visible_text(&format!("{}latest window", "x".repeat(6_000)));

    let pack = context.to_inline_context_pack();

    assert!(pack.facts.is_empty());
    assert_eq!(pack.evidence.iter().map(|item| item.event_id).collect::<Vec<_>>(), (4..12).collect::<Vec<_>>());
    assert_eq!(context.visible_text.chars().count(), 6_000);
    assert!(context.visible_text.ends_with("latest window"));
}

#[test]
fn visible_context_stays_near_the_focused_editor() {
    let mut context = ReplyContext::default();
    let text = format!("{}focused editor text{}END-OF-WINDOW", "old ".repeat(1_200), "new ".repeat(1_200));
    context.set_visible_text_near(&text, "focused editor text");
    assert!(!context.visible_text.contains("focused editor text"));
    assert!(!context.visible_text.contains("END-OF-WINDOW"));
    assert!(context.visible_text.chars().count() <= 6_000);
}

#[test]
fn live_window_excerpt_masks_secrets_and_drops_focused_draft() {
    let mut context = ReplyContext::default();
    let draft = "Please use this exact draft";
    let text = format!("Earlier message\n{draft}\nsk-ant-api03-{}", "a".repeat(50));
    context.set_visible_text_near(&text, draft);
    assert!(context.visible_text.contains("Earlier message"));
    assert!(!context.visible_text.contains(draft));
    assert!(!context.visible_text.contains("sk-ant-api03-"));
}



#[test]
fn live_window_drops_normalized_and_long_draft_lines_before_clipping() {
    let mut context = ReplyContext::default();
    let long = "draft word ".repeat(300);
    let draft = format!("first draft line\n{long}\nfirst draft line\nlast draft line");
    let text = format!(
        "Sam: please respond\nfirst   draft line\n{}\nlast draft line\nSam: latest question",
        long.replace(' ', "  ")
    );
    context.set_visible_text_near(&text, &draft);
    assert!(!context.visible_text.contains("draft"));
    assert!(context.visible_text.contains("latest question"));
}




#[test]
fn an_empty_reply_context_is_reported_so_the_caller_can_fall_back() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let ctx = db.build_reply_context("capture:nothing:here");
    assert!(ctx.is_empty(), "nothing warmed for this thread");
    assert!(ctx.as_memory_lines(10).is_empty());
}

/// A press must read a warm pack, never build one — building on the press is what the SLO
/// forbids, so a miss is reported rather than silently papered over.
#[test]
fn the_cache_serves_the_current_thread_and_reports_a_miss_honestly() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Alpha"),
        ..ev("alpha notes", "h1", 100)
    })
    .unwrap();
    let key =
        shogun_memory::thread::thread_key("capture", None, Some("com.apple.Safari"), Some("Alpha"))
            .unwrap();

    let cache = ReplyContextCache::new();
    assert!(
        cache.get(&key).is_none(),
        "cold cache is a miss, not an inline build"
    );

    cache.put(db.build_reply_context(&key));
    assert!(cache.get(&key).is_some(), "warm for the built thread");
    assert!(
        cache.get("capture:com.apple.Safari:beta").is_none(),
        "a different thread is a miss — never serve the wrong thread's context"
    );
    assert_eq!(cache.current().map(|c| c.thread_key), Some(key));
}

#[test]
fn clearing_the_reply_context_cache_removes_current_context() {
    let cache = ReplyContextCache::new();
    cache.put(ReplyContext {
        thread_key: "sensitive".into(),
        ..ReplyContext::default()
    });
    assert!(cache.current().is_some());

    cache.clear();

    assert!(cache.current().is_none());
}

#[test]
fn reply_context_cache_marks_old_focus_stale_without_returning_text() {
    let cache = ReplyContextCache::new();
    cache.put_at(
        ReplyContext {
            facts: vec!["old focus secret".into()],
            ..ReplyContext::default()
        },
        std::time::Instant::now() - std::time::Duration::from_secs(20),
    );

    let snapshot = cache.snapshot(std::time::Duration::from_secs(10));

    assert!(matches!(snapshot, ReplyContextCacheSnapshot::Stale));
}

#[test]
fn reply_context_cache_observation_cannot_refresh_old_assembly() {
    let cache = ReplyContextCache::new();
    cache.put_at(
        ReplyContext {
            facts: vec!["still focused".into()],
            ..ReplyContext::default()
        },
        std::time::Instant::now() - std::time::Duration::from_secs(20),
    );

    cache.mark_fresh();

    assert!(matches!(
        cache.snapshot(std::time::Duration::from_secs(10)),
        ReplyContextCacheSnapshot::Stale
    ));
}

#[test]
fn reply_context_cache_requires_exact_fresh_desktop_focus() {
    let cache = ReplyContextCache::new();
    let now = std::time::Instant::now();
    cache.put_for_focus_at(
        "pid:41\0capture:mail:renewal",
        ReplyContext {
            facts: vec!["fresh grounded fact".into()],
            ..ReplyContext::default()
        },
        now,
    );
    assert!(cache
        .get_fresh_for_focus(
            "pid:41\0capture:mail:renewal",
            std::time::Duration::from_secs(5),
        )
        .is_some());
    assert!(cache
        .get_fresh_for_focus(
            "pid:99\0capture:mail:renewal",
            std::time::Duration::from_secs(5),
        )
        .is_none());
    cache.put_for_focus_at(
        "pid:41\0capture:mail:renewal",
        ReplyContext {
            facts: vec!["stale grounded fact".into()],
            ..ReplyContext::default()
        },
        now - std::time::Duration::from_secs(6),
    );
    assert!(cache
        .get_fresh_for_focus(
            "pid:41\0capture:mail:renewal",
            std::time::Duration::from_secs(5),
        )
        .is_none());
}

#[test]
fn observing_unchanged_focus_does_not_refresh_old_assembly() {
    let cache = ReplyContextCache::new();
    let focus_key = "41\0capture:mail:renewal";
    cache.put_for_focus_at(
        focus_key,
        ReplyContext {
            facts: vec!["old assembled fact".into()],
            ..ReplyContext::default()
        },
        std::time::Instant::now() - std::time::Duration::from_secs(7),
    );

    cache.mark_focus_fresh(focus_key);

    assert!(cache
        .get_fresh_for_focus(focus_key, std::time::Duration::from_secs(6))
        .is_none());
}

#[test]
fn prepared_directives_are_available_only_for_exact_fresh_focus() {
    let cache = ReplyContextCache::new();
    let focus_key = "41\0capture:mail:renewal";
    let assembled_at = std::time::Instant::now();
    cache.put_prepared_for_focus_at(
        focus_key,
        ReplyContext {
            facts: vec!["grounded fact".into()],
            ..ReplyContext::default()
        },
        "Use concise sentences.".into(),
        assembled_at,
    );

    let prepared = cache
        .get_fresh_prepared_for_focus(focus_key, std::time::Duration::from_secs(6))
        .unwrap();

    assert_eq!(prepared.focus_key, focus_key);
    assert_eq!(prepared.directives, "Use concise sentences.");
    assert!(prepared.assembly_is_fresh_at(
        std::time::Duration::from_secs(6),
        assembled_at + std::time::Duration::from_secs(6) - std::time::Duration::from_nanos(1),
    ));
    assert!(!prepared.assembly_is_fresh_at(
        std::time::Duration::from_secs(6),
        assembled_at + std::time::Duration::from_secs(6),
    ));
    assert!(cache
        .get_fresh_prepared_for_focus(
            "41\0capture:mail:other-window",
            std::time::Duration::from_secs(6),
        )
        .is_none());
}

#[test]
fn freshly_assembled_context_requires_recent_exact_focus_observation() {
    let cache = ReplyContextCache::new();
    let focus_key = "41\0capture:mail:renewal";
    cache.put_prepared_for_focus_observed_at(
        focus_key,
        ReplyContext {
            facts: vec!["fresh fact".into()],
            ..ReplyContext::default()
        },
        String::new(),
        std::time::Instant::now(),
        std::time::Instant::now() - std::time::Duration::from_secs(7),
    );

    assert!(cache
        .get_fresh_prepared_for_focus(focus_key, std::time::Duration::from_secs(6))
        .is_none());
}

#[test]
fn exact_focus_can_retain_directives_after_memory_expires() {
    let cache = ReplyContextCache::new();
    let focus_key = "41\0capture:mail:renewal";
    cache.put_prepared_for_focus_at(
        focus_key,
        ReplyContext {
            facts: vec!["expired fact".into()],
            ..ReplyContext::default()
        },
        "Use concise sentences.".into(),
        std::time::Instant::now() - std::time::Duration::from_secs(7),
    );

    let prepared = cache
        .get_prepared_for_focus(focus_key)
        .unwrap_or_else(|| panic!("exact-focus inputs should remain available"));

    assert_eq!(prepared.directives, "Use concise sentences.");
    assert!(!prepared
        .assembly_is_fresh_at(std::time::Duration::from_secs(6), std::time::Instant::now(),));
}

#[test]
fn fetched_gmail_context_can_be_keyed_to_the_visible_desktop_focus() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    db.ingest_integration(&[IngestItem {
        source: "gmail",
        kind: "email",
        title: "Q3 pricing".to_string(),
        body: "Full thread body about pricing".to_string(),
        ts_ms: 1,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }]);
    let context = db.build_reply_context_for_screen(
        "capture:com.google.Chrome:Q3 pricing",
        "(3) Q3 pricing — Gmail",
    );
    let cache = ReplyContextCache::new();
    let visible_focus = "pid:41\0capture:com.google.Chrome:Q3 pricing";
    cache.put_for_focus(visible_focus, context);
    let cached = cache
        .get_fresh_for_focus(visible_focus, std::time::Duration::from_secs(5))
        .unwrap();
    assert!(matches!(
        cached.payload_source,
        PayloadSource::Fetched { .. }
    ));
}

#[test]
fn a_referring_question_resolves_when_one_thread_clearly_dominates() {
    use shogun_memory::thread::Referent;
    let now = 10_000_000_000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    // One thread touched just now, one a week ago.
    db.capture(&NewEvent {
        window_title: Some("Q3 pricing"),
        ..ev("vendor pricing settled at 12k", "h1", now - 1_000)
    })
    .unwrap();
    db.capture(&NewEvent {
        window_title: Some("Old holiday plans"),
        ..ev("beach or mountains", "h2", now - 7 * 24 * 3_600_000)
    })
    .unwrap();

    let out = db.resolve_referent("how's that going?", None);
    assert_eq!(
        out.verdict,
        Referent::Resolved,
        "candidates: {:?}",
        out.candidates
    );
    assert!(
        out.candidates[0].title.as_deref() == Some("Q3 pricing"),
        "the live thread wins: {:?}",
        out.candidates
    );
}

/// Two equally-plausible threads must produce a question, not a guess. This is the behaviour
/// that keeps a confident wrong answer about someone's work from ever being given.
#[test]
fn two_equally_live_threads_are_ambiguous_rather_than_guessed() {
    use shogun_memory::thread::Referent;
    let now = 10_000_000_000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Alpha"),
        ..ev("alpha notes", "h1", now - 1_000)
    })
    .unwrap();
    db.capture(&NewEvent {
        window_title: Some("Beta"),
        ..ev("beta notes", "h2", now - 1_100)
    })
    .unwrap();

    let out = db.resolve_referent("any update on that?", None);
    assert_eq!(
        out.verdict,
        Referent::Ambiguous,
        "candidates: {:?}",
        out.candidates
    );
    assert!(out.candidates.len() >= 2, "the UI needs something to offer");
}

#[test]
fn the_users_own_words_break_a_tie() {
    use shogun_memory::thread::Referent;
    let now = 10_000_000_000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Alpha"),
        ..ev("alpha notes", "h1", now - 1_000)
    })
    .unwrap();
    db.capture(&NewEvent {
        window_title: Some("Beta"),
        ..ev("beta notes", "h2", now - 1_100)
    })
    .unwrap();

    let out = db.resolve_referent("any update on that alpha thing?", None);
    assert_eq!(out.verdict, Referent::Resolved);
    assert_eq!(out.candidates[0].title.as_deref(), Some("Alpha"));
}

/// What the user is looking at is a strong signal for what "that" means.
#[test]
fn the_thread_on_screen_wins_a_tie() {
    use shogun_memory::thread::Referent;
    let now = 10_000_000_000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    db.capture(&NewEvent {
        window_title: Some("Alpha"),
        ..ev("alpha notes", "h1", now - 1_000)
    })
    .unwrap();
    db.capture(&NewEvent {
        window_title: Some("Beta"),
        ..ev("beta notes", "h2", now - 1_100)
    })
    .unwrap();
    let beta_key =
        shogun_memory::thread::thread_key("capture", None, Some("com.apple.Safari"), Some("Beta"))
            .unwrap();

    let out = db.resolve_referent("any update on that?", Some(&beta_key));
    assert_eq!(out.verdict, Referent::Resolved);
    assert_eq!(out.candidates[0].title.as_deref(), Some("Beta"));
}

#[test]
fn an_empty_memory_has_no_referent_to_offer() {
    use shogun_memory::thread::Referent;
    let db = Db::open_in_memory(clock(1000)).unwrap();
    assert_eq!(
        db.resolve_referent("how's that going?", None).verdict,
        Referent::None
    );
}

/// The point of the semantic half: a question that shares no words with the answer still
/// finds it. Lexical search cannot do this, so the same query is run both ways and the
/// difference is the assertion.
#[test]
fn a_loaded_model_finds_an_answer_that_shares_no_words_with_the_question() {
    use shogun_memory::embed::{EmbedError, Embedder};

    /// A stand-in for the real model: it maps a fixed topic vocabulary onto the same vector,
    /// which is exactly the property (paraphrase → nearby vector) the real model provides.
    struct TopicEmbedder;
    impl TopicEmbedder {
        fn vector(text: &str) -> Vec<f32> {
            let t = text.to_lowercase();
            let pricing = ["pricing", "cost", "how much", "renewal fee"]
                .iter()
                .any(|w| t.contains(w));
            // Must match the store's fixed width (the vec0 table is E5-sized).
            let mut v = vec![0.0f32; shogun_memory::embed::E5_SMALL_DIM];
            if pricing {
                v[0] = 1.0;
            } else {
                v[1] = 1.0;
            }
            v
        }
    }
    impl Embedder for TopicEmbedder {
        fn dim(&self) -> usize {
            shogun_memory::embed::E5_SMALL_DIM
        }
        fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|t| Self::vector(t)).collect())
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(Self::vector(text))
        }
    }

    let plain = Db::open_in_memory(clock(1000)).unwrap();
    plain
        .capture(&ev("The renewal fee lands at 12k", "h1", 1))
        .unwrap();
    plain
        .capture(&ev("Standup notes: nothing blocking", "h2", 2))
        .unwrap();

    // Lexical only: "cost" appears nowhere, so there is nothing to match.
    let lexical = plain.assemble_context("what was the cost?", 5, 200);
    assert!(
        !lexical.evidence.iter().any(|e| e.excerpt.contains("12k")),
        "precondition: lexical search cannot find this"
    );

    // Same store, now with a model attached and the backlog embedded.
    let db = plain.with_embedder(std::sync::Arc::new(TopicEmbedder));
    assert_eq!(
        db.embed_pending(100),
        2,
        "the backlog is embedded off the write path"
    );

    let hybrid = db.assemble_context("what was the cost?", 5, 200);
    assert!(
        hybrid.evidence.iter().any(|e| e.excerpt.contains("12k")),
        "the semantic half must find the paraphrase: {:?}",
        hybrid.evidence
    );
}

#[test]
fn search_still_works_with_no_model_loaded() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    db.capture(&ev("vendor pricing settled at 12k", "h1", 1))
        .unwrap();
    assert_eq!(db.embed_pending(100), 0, "no model, nothing embedded");
    let pack = db.assemble_context("vendor pricing", 5, 200);
    assert!(pack.evidence.iter().any(|e| e.excerpt.contains("12k")));
}

/// The shipping configuration since #237: no local embedder anywhere, over a store that still
/// holds vectors an earlier build wrote. The search box must answer from FTS — stored vectors
/// are ignored rather than destroyed, and the absence of a model is not an error to report.
#[test]
fn search_is_lexical_over_a_store_that_already_holds_vectors() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    db.capture(&ev("vendor pricing settled at 12k", "h1", 1))
        .unwrap();

    // A build that had an embedder wrote vectors into this same store (clones share the
    // connection); an updated release then opens it with no embedder at all.
    let previous = db.clone().with_embedder(std::sync::Arc::new(
        shogun_memory::embed::MockEmbedder::new(shogun_memory::embed::E5_SMALL_DIM),
    ));
    assert_eq!(
        previous.embed_pending(100),
        1,
        "precondition: the store holds a vector"
    );

    let hits = db
        .try_search("vendor pricing", 5)
        .expect("lexical search must answer");
    assert!(hits.iter().any(|h| h.content.contains("12k")), "{hits:?}");
    assert_eq!(
        db.embed_pending(100),
        0,
        "no embedder, so no backlog work is queued"
    );
}

#[test]
fn assemble_context_semantically_retrieves_distilled_memory_without_shared_words() {
    use shogun_memory::embed::{EmbedError, Embedder, E5_SMALL_DIM};
    use shogun_memory::state::{NewProject, ProjectStatus, Provenance};

    struct SameTopicEmbedder;
    impl Embedder for SameTopicEmbedder {
        fn dim(&self) -> usize {
            E5_SMALL_DIM
        }

        fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|_| {
                    let mut vector = vec![0.0; E5_SMALL_DIM];
                    vector[0] = 1.0;
                    vector
                })
                .collect())
        }

        fn embed_query(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            let mut vector = vec![0.0; E5_SMALL_DIM];
            vector[0] = 1.0;
            Ok(vector)
        }
    }

    let db = Db::open_in_memory(clock(1_000))
        .unwrap()
        .with_embedder(Arc::new(SameTopicEmbedder));
    let evidence = db
        .capture(&ev("unrelated canonical wording", "derived-vector", 1))
        .unwrap()
        .0;
    db.insert_project(
        &NewProject {
            name: "Orchid",
            status: ProjectStatus::Active,
            summary: Some("supplier contract extension accepted"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 2,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
    assert_eq!(db.embed_pending(100), 2);

    let pack = db.assemble_context("vendor renewal approved", 5, 200);

    assert!(
        pack.facts
            .iter()
            .any(|fact| fact.contains("supplier contract extension accepted")),
        "{pack:?}"
    );
}

#[test]
fn ai_session_turns_become_searchable_threaded_memory() {
    use shogun_memory::ai_session::{Role, SessionTurn};
    let db = Db::open_in_memory(clock(1000)).unwrap();
    let turns = vec![
        SessionTurn {
            session_id: "sess-1".into(),
            role: Role::User,
            ts_ms: 10,
            text: "why did we drop the vendor migration?".into(),
            cwd: Some("/proj".into()),
        },
        SessionTurn {
            session_id: "sess-1".into(),
            role: Role::Assistant,
            ts_ms: 20,
            text: "Because the vendor migration needed downtime we could not take.".into(),
            cwd: Some("/proj".into()),
        },
    ];
    let s = db.ingest_ai_session(&turns);
    assert_eq!(s.newly_inserted, 2);

    // The whole point: this is now answerable context.
    let pack = db.assemble_context("vendor migration", 5, 200);
    assert!(
        pack.evidence.iter().any(|e| e.excerpt.contains("downtime")),
        "the session answer must be retrievable: {:?}",
        pack.evidence
    );
    assert!(pack.evidence.iter().all(|e| e.source == "ai_session"));

    // Both turns share the session's thread, so the conversation stays one unit.
    let keys: Vec<Option<String>> = {
        let c = db.conn.lock().unwrap();
        let mut st = c
            .prepare("SELECT thread_key FROM event_log ORDER BY id")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(keys.len(), 2);
    assert!(
        keys[0].is_some() && keys[0] == keys[1],
        "one session is one thread: {keys:?}"
    );
}

/// A session log is append-only and gets re-read as it grows, so importing it twice must not
/// double the memory.
#[test]
fn re_importing_a_session_log_does_not_duplicate_turns() {
    use shogun_memory::ai_session::{Role, SessionTurn};
    let db = Db::open_in_memory(clock(1000)).unwrap();
    let turns = vec![SessionTurn {
        session_id: "sess-1".into(),
        role: Role::User,
        ts_ms: 10,
        text: "ship it on Friday".into(),
        cwd: None,
    }];
    assert_eq!(db.ingest_ai_session(&turns).newly_inserted, 1);
    let second = db.ingest_ai_session(&turns);
    assert_eq!(
        second.newly_inserted, 0,
        "already-seen turns must not be re-inserted"
    );
    assert_eq!(second.processed, 1);
}

#[test]
fn secrets_in_an_ingested_turn_are_masked_before_storage() {
    use shogun_memory::ai_session::{Role, SessionTurn};
    let db = Db::open_in_memory(clock(1000)).unwrap();
    db.ingest_ai_session(&[SessionTurn {
        session_id: "s".into(),
        role: Role::User,
        ts_ms: 10,
        text: "deploy with ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345 please".into(),
        cwd: None,
    }]);
    let stored: String = {
        let c = db.conn.lock().unwrap();
        c.query_row("SELECT content FROM event_log", [], |r| r.get(0))
            .unwrap()
    };
    assert!(
        !stored.contains("ghp_ABCDEF"),
        "the token must not be stored: {stored}"
    );
    assert!(stored.contains("[redacted]"));
    assert!(
        stored.contains("deploy with"),
        "surrounding text is preserved: {stored}"
    );
}

/// A question about something in the event log has to reach that event: state facts alone would
/// leave this content unreachable no matter what the user asked.
#[test]
fn assemble_context_retrieves_the_event_that_answers_the_question() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    db.capture(&ev(
        "Vendor pricing settled at 12k for the renewal",
        "h1",
        1,
    ))
    .unwrap();
    db.capture(&ev("Standup notes: nothing blocking today", "h2", 2))
        .unwrap();

    let pack = db.assemble_context("vendor pricing", 5, 200);
    assert!(
        pack.evidence.iter().any(|e| e.excerpt.contains("12k")),
        "the answering event must be retrieved: {:?}",
        pack.evidence
    );
    // Attribution rides along so an answer can cite it.
    let hit = pack
        .evidence
        .iter()
        .find(|e| e.excerpt.contains("12k"))
        .unwrap();
    assert_eq!(hit.source, "capture");
    assert!(hit.event_id > 0);
}

#[test]
fn form_context_shares_project_evidence_and_keeps_longer_context() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    let source = format!("Mira builds Nimbus, a desktop app. {} Nimbus helps researchers recover useful context from their work.", "Project implementation details. ".repeat(12));
    db.capture(&ev(&source, "form-project", 1)).unwrap();
    let items = db.assemble_form_context(
        Some("Mira"),
        &[
            "Name of the product",
            "What are you building?",
            "Who is it for?",
            "Why would this residency be useful?",
        ],
    );
    assert!(items.iter().any(|item| item.text.contains("Nimbus")));
    assert!(items.iter().any(|item| item.text.chars().count() > 300));
    assert!(items.iter().all(|item| item.text.chars().count() <= 1_500));
    assert!(items.len() <= 24);
}

#[test]
fn assemble_context_retrieves_query_relevant_meeting_not_latest() {
    use shogun_memory::session::{open, NewSession};

    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let conn = db.conn.lock().unwrap();
    let old = open(
        &conn,
        &NewSession {
            kind: "meeting",
            started_at: 1_000,
            title: Some("Phoenix planning"),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: 0.8,
            provenance: "{}",
        },
    )
    .unwrap();
    let recent = open(
        &conn,
        &NewSession {
            kind: "meeting",
            started_at: 9_000,
            title: Some("Daily standup"),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: 0.8,
            provenance: "{}",
        },
    )
    .unwrap();
    shogun_memory::meeting_recaps::save(
        &conn,
        old,
        "Phoenix launch is targeted for March with beta in February.",
        "[]",
        "[]",
        None,
        "m",
        2_000,
    )
    .unwrap();
    shogun_memory::meeting_recaps::save(
        &conn,
        recent,
        "Nothing blocking today.",
        "[]",
        "[]",
        None,
        "m",
        10_000,
    )
    .unwrap();
    drop(conn);

    let pack = db.assemble_context("Phoenix launch beta", 5, 300);
    let meeting = pack
        .evidence
        .iter()
        .find(|e| e.source == "meeting")
        .expect("meeting evidence must be present");
    assert!(meeting.excerpt.contains("Phoenix"));
    assert_eq!(meeting.event_id, -old);
    assert!(
        !pack
            .evidence
            .iter()
            .any(|e| e.source == "meeting" && e.excerpt.contains("Nothing blocking")),
        "unrelated latest meeting must not appear: {:?}",
        pack.evidence
    );
}

#[test]
fn federated_context_is_summary_first_grounded_bounded_warm_and_stable() {
    use shogun_memory::briefs::{BriefLine, BriefPayload};
    use shogun_memory::period_summaries::{NewPeriodSummary, PeriodLevel};
    use shogun_memory::search::{FederatedSearchOptions, FederatedSourceKind};
    use shogun_memory::state::{CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};

    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let raw_id = db
        .capture(&ev(
            "zephyrledger raw canonical decision",
            "federated-raw",
            1_000,
        ))
        .unwrap()
        .0;
    let period_ground_id = db
        .capture(&ev(
            "canonical quarter evidence without the query term",
            "federated-period",
            2_000,
        ))
        .unwrap()
        .0;
    db.insert_commitment(
        &NewCommitment {
            direction: CommitmentDirection::Mine,
            counterparty_id: None,
            description: "zephyrledger state commitment",
            due_at: None,
            status: CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 3_000,
        },
        &[Provenance::new(raw_id)],
    )
    .unwrap();

    let brief = BriefPayload {
        date: "2026-08-24".to_owned(),
        today: Vec::new(),
        commitments_due: vec![BriefLine {
            text: "zephyrledger brief checkpoint".to_owned(),
            provenance_event_id: Some(raw_id),
            evidence_kind: None,
            possibly: true,
        }],
        open_loops: Vec::new(),
        what_happened: Vec::new(),
        suggested_actions: Vec::new(),
        charm_line: None,
        pattern_line: None,
        meetings: vec![],
    };
    let session_id = db
        .open_meeting(Some("Zephyrledger meeting"), None, 0.9, "{}")
        .unwrap();
    {
        let mut conn = db.conn.lock().unwrap();
        shogun_memory::briefs::upsert_brief(
            &conn,
            "2026-08-24",
            &serde_json::to_string(&brief).unwrap(),
            false,
            9_000,
        )
        .unwrap();
        shogun_memory::meeting_recaps::save(
            &conn,
            session_id,
            "zephyrledger meeting recap",
            "[]",
            "[]",
            None,
            "fixture-model",
            9_500,
        )
        .unwrap();
        let child_summary_id = shogun_memory::period_summaries::save(
            &mut conn,
            &NewPeriodSummary {
                level: PeriodLevel::Hour,
                period_start_ms: 0,
                period_end_ms: 5_000,
                title: "Source hour",
                content: "compact source hour without the query term",
                generated: false,
                generation_model: None,
                event_ids: &[period_ground_id],
                child_summary_ids: &[],
                now_ms: 9_700,
            },
        )
        .unwrap();
        shogun_memory::period_summaries::save(
            &mut conn,
            &NewPeriodSummary {
                level: PeriodLevel::Day,
                period_start_ms: 0,
                period_end_ms: 5_000,
                title: "Quarter memory",
                content: "zephyrledger period-only compact outcome",
                generated: false,
                generation_model: None,
                event_ids: &[],
                child_summary_ids: &[child_summary_id],
                now_ms: 9_800,
            },
        )
        .unwrap();

        let federated = shogun_memory::search::search_federated(
            &conn,
            "zephyrledger",
            None,
            10_000,
            &FederatedSearchOptions::default(),
            8,
        )
        .unwrap();
        assert_eq!(federated.cold.partitions_visited, 0);
        assert_eq!(federated.cold.rows_scanned, 0);
        assert!(federated.hits.iter().all(|hit| !hit.tiers.cold));
        assert!(federated
            .hits
            .iter()
            .any(|hit| hit.source_kind == FederatedSourceKind::Event));
        assert!(federated.hits.iter().any(|hit| {
            matches!(
                hit.source_kind,
                FederatedSourceKind::Derived(
                    shogun_memory::search_documents::SearchDocumentKind::PeriodSummary
                )
            )
        }));
    }

    let first = db.assemble_context("zephyrledger", 8, 120);
    let second = db.assemble_context("zephyrledger", 8, 120);
    assert_eq!(
        first, second,
        "identical local state must produce stable ordered context"
    );
    let period_summary = first
        .facts
        .iter()
        .position(|fact| fact.contains("period-only compact outcome"))
        .expect("period-only V22 hit must reach context");
    let live_state = first
        .facts
        .iter()
        .position(|fact| fact.contains("you committed"))
        .expect("ordinary confidence-gated state remains present");
    assert!(
        period_summary < live_state,
        "compact query hits precede general live state"
    );
    assert!(first.facts[..live_state]
        .iter()
        .all(|fact| fact.starts_with("summary (unverified):")
            || fact.starts_with("possibly: summary (unverified):")));
    assert!(
        first.facts.iter().any(|fact| {
            fact.starts_with("possibly: summary (unverified):") && fact.contains("brief checkpoint")
        }),
        "medium-confidence derived memory keeps the existing possibly wording"
    );
    assert!(
        first.evidence.iter().any(|item| {
            item.event_id == period_ground_id && item.excerpt.contains("canonical quarter evidence")
        }),
        "period summary retains one canonical raw grounding row: {:?}",
        first.evidence
    );
    let unique_ids = first
        .evidence
        .iter()
        .map(|item| item.event_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique_ids.len(),
        first.evidence.len(),
        "shared derived provenance is emitted once"
    );
    assert!(first.evidence.len() <= 8);
    assert!(first
        .evidence
        .iter()
        .all(|item| item.excerpt.chars().count() <= 122));

    let config = shogun_fusion::compress::CompressionConfig {
        enabled: true,
        budget_tokens: 256,
        ..Default::default()
    };
    let (compressed, stats, fell_back) =
        db.assemble_context_compressed_within("zephyrledger", 8, 120, &[], &config, CompressBudget::Unbounded);
    assert!(!fell_back);
    assert!(stats.post_tokens <= config.budget_tokens);
    assert!(compressed
        .facts
        .iter()
        .any(|fact| fact.contains("period-only compact outcome")));
    assert!(compressed
        .evidence
        .iter()
        .any(|item| item.event_id == period_ground_id));
}

#[test]
fn assemble_context_caps_a_huge_capture_to_the_excerpt_budget() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    let huge = format!(
        "{}decision: ship on Friday{}",
        "filler ".repeat(500),
        " tail".repeat(500)
    );
    db.capture(&ev(&huge, "h1", 1)).unwrap();

    let pack = db.assemble_context("decision", 5, 120);
    let e = pack.evidence.first().expect("retrieved");
    assert!(
        e.excerpt.contains("decision: ship on Friday"),
        "match kept: {}",
        e.excerpt
    );
    assert!(
        e.excerpt.chars().count() <= 122,
        "budget held: {}",
        e.excerpt.chars().count()
    );
}

/// An empty/garbage question must degrade to the state facts, never error or return junk.
#[test]
fn assemble_context_without_a_match_still_returns_state_facts() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    let e = db.capture(&ev("unrelated", "h1", 1)).unwrap().0;
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send Alice the deck",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 1,
        },
        &[shogun_memory::state::Provenance::new(e)],
    )
    .expect("commitment");

    let pack = db.assemble_context("", 5, 200);
    assert!(pack.evidence.is_empty(), "empty query retrieves nothing");
    assert!(
        pack.facts.iter().any(|f| f.contains("send Alice the deck")),
        "state facts still ground the answer: {:?}",
        pack.facts
    );
}

#[test]
fn assemble_context_excludes_unrelated_global_state_from_memory_lookup() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let evidence = db
        .capture(&ev("Phoenix launch moved to Friday", "relevant", 1))
        .unwrap()
        .0;
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send Alice the unrelated deck",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 1,
        },
        &[shogun_memory::state::Provenance::new(evidence)],
    )
    .unwrap();

    let pack = db.assemble_context("When is the Phoenix launch?", 5, 200);

    assert!(pack
        .evidence
        .iter()
        .any(|item| item.excerpt.contains("Friday")));
    assert!(
        !pack.facts.iter().any(|fact| fact.contains("Alice")),
        "{pack:?}"
    );
}

#[test]
fn assemble_context_keeps_query_matching_state_fact() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let evidence = db
        .capture(&ev("Phoenix follow-up", "matching-state", 1))
        .unwrap()
        .0;
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send the Phoenix launch plan",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 1,
        },
        &[shogun_memory::state::Provenance::new(evidence)],
    )
    .unwrap();

    let pack = db.assemble_context("What is next for Phoenix?", 5, 200);

    assert!(
        pack.facts.iter().any(|fact| fact.contains("launch plan")),
        "{pack:?}"
    );
}

#[test]
fn inline_memory_gates_low_confidence_out() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let e = db.capture(&ev("evidence", "h1", 1)).unwrap().0;
    let prov = [shogun_memory::state::Provenance::new(e)];
    // a high-confidence commitment (stated as fact) and a low-confidence one (dropped)
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "send Alice the deck",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 1,
        },
        &prov,
    )
    .expect("high commitment");
    db.insert_commitment(
        &shogun_memory::state::NewCommitment {
            direction: shogun_memory::state::CommitmentDirection::Mine,
            counterparty_id: None,
            description: "maybe ping the vendor",
            due_at: None,
            status: shogun_memory::state::CommitmentStatus::Open,
            project_id: None,
            confidence: 0.3,
            now: 1,
        },
        &prov,
    )
    .expect("low commitment");

    let mem = db.inline_memory(10);
    assert!(
        mem.iter().any(|m| m.contains("send Alice the deck")),
        "high-confidence fact is included: {mem:?}"
    );
    assert!(
        !mem.iter().any(|m| m.contains("maybe ping the vendor")),
        "low-confidence guess must not be handed to the model as fact (FR-ST-20): {mem:?}"
    );
}

#[test]
fn ingested_email_commitment_reaches_the_state_tables() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    // an email in which the user states a commitment — the same heuristic that fires on captured
    // text should fire here, feeding the state tables at low confidence (FR-ST-02).
    let items = vec![IngestItem {
        source: "gmail",
        kind: "email",
        title: "Re: deck".into(),
        body: "Thanks — I'll send the final deck by Friday.".into(),
        ts_ms: 100,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }];
    let summary = db.ingest_integration(&items);
    assert_eq!(summary.newly_inserted, 1);
    assert!(
        summary.candidates >= 1,
        "the email commitment should yield a candidate"
    );

    // it lands as a low-confidence commitment, linked back to the ingested email.
    let commitments = db.commitments_due(1_000);
    assert!(
        !commitments.is_empty(),
        "the email commitment reached the state table"
    );
    assert!(
        commitments
            .iter()
            .all(|c| c.confidence <= shogun_memory::extract::LOCAL_RULE_MAX_CONFIDENCE),
        "an extracted email commitment is low-confidence, never asserted as fact (FR-ST-20)"
    );
}

#[test]
fn ingested_calendar_event_is_searchable_and_does_not_extract_speech_acts() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    // A calendar description can contain "I'll send…" — that is not a user promise. Issue #35:
    // structured facts skip the untrusted extractor.
    let items = vec![IngestItem {
        source: "gcal",
        kind: "calendar_event",
        title: "Standup".into(),
        body: "Daily standup. I'll send the deck after.".into(),
        ts_ms: 100,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }];
    let summary = db.ingest_integration(&items);
    assert_eq!(summary.newly_inserted, 1);
    assert_eq!(
        summary.candidates, 0,
        "calendar descriptions are not speech-act extract"
    );
    assert!(
        db.commitments_due(1_000).is_empty(),
        "a calendar row must not become a possibly-commitment"
    );
    let hits = db.search("standup", 10);
    assert!(
        hits.iter().any(|h| h.source == "gcal"),
        "the event stays searchable"
    );
}

#[test]
fn re_syncing_the_same_item_touches_not_duplicates() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    let item = IngestItem {
        source: "gmail",
        kind: "email",
        title: "Invoice".into(),
        body: "Payment is due next week".into(),
        ts_ms: 100,
        external_id: String::new(),
        calendar: None,
        mail: None,
    };
    let first = db.ingest_integration(std::slice::from_ref(&item));
    assert_eq!(first.newly_inserted, 1);
    // re-sync the identical item (same source + content) → dedup touch, no new row, no re-extract
    let second = db.ingest_integration(std::slice::from_ref(&item));
    assert_eq!(
        (second.processed, second.newly_inserted, second.candidates),
        (1, 0, 0),
        "an unchanged re-sync must not duplicate the event or re-extract"
    );
}

/// One sync batch with new items → exactly one `IntegrationSynced` on the bus, carrying the
/// source tag and the newly-inserted count — never item content (design §2.2 wiring).
#[test]
fn a_sync_with_new_items_publishes_exactly_one_integration_synced() {
    use crate::bus::{Bus, BusEvent};
    use shogun_mcp::sync::IngestItem;
    let bus = Bus::new(8);
    let mut sub = bus.subscribe();
    let db = Db::open_in_memory(clock(1)).unwrap().with_bus(bus);
    let items = vec![
        IngestItem {
            source: "gmail",
            kind: "email",
            title: "A".into(),
            body: "first mail".into(),
            ts_ms: 100,
            external_id: String::new(),
            calendar: None,
            mail: None,
        },
        IngestItem {
            source: "gmail",
            kind: "email",
            title: "B".into(),
            body: "second mail".into(),
            ts_ms: 200,
            external_id: String::new(),
            calendar: None,
            mail: None,
        },
    ];
    let summary = db.ingest_integration(&items);
    assert_eq!(summary.newly_inserted, 2);
    let ev = sub.try_recv().expect("one event for the batch");
    assert_eq!(
        *ev,
        BusEvent::IntegrationSynced {
            source: "gmail",
            count: 2
        }
    );
    assert!(
        sub.try_recv().is_none(),
        "exactly one event per synced source, not one per item"
    );
}

/// A sync that changes nothing (dedup-only re-sync, or an empty batch) stays silent — no
/// event means no needless cache invalidation downstream.
#[test]
fn a_sync_with_zero_new_items_publishes_nothing() {
    use crate::bus::Bus;
    use shogun_mcp::sync::IngestItem;
    let bus = Bus::new(8);
    let db = Db::open_in_memory(clock(1)).unwrap().with_bus(bus.clone());
    let item = IngestItem {
        source: "gmail",
        kind: "email",
        title: "Invoice".into(),
        body: "Payment is due next week".into(),
        ts_ms: 100,
        external_id: String::new(),
        calendar: None,
        mail: None,
    };
    db.ingest_integration(std::slice::from_ref(&item));
    // Subscribe after the first (publishing) sync; only silence must follow.
    let mut sub = bus.subscribe();
    let second = db.ingest_integration(std::slice::from_ref(&item));
    assert_eq!(
        second.newly_inserted, 0,
        "precondition: an unchanged re-sync is dedup-only"
    );
    assert!(
        sub.try_recv().is_none(),
        "a dedup-only sync must not publish"
    );
    db.ingest_integration(&[]);
    assert!(sub.try_recv().is_none(), "an empty batch must not publish");
}

/// The §2.2 loop end-to-end: sync lands new items → `IntegrationSynced` → the daemon's
/// subscription empties the cache, so the next `get` is an honest miss until the focus path
/// re-assembles. Invalidation clears — it never rebuilds inline.
#[test]
fn an_integration_synced_event_empties_the_cache_until_reassembled() {
    use crate::bus::Bus;
    use shogun_mcp::sync::IngestItem;
    let bus = Bus::new(8);
    let db = Db::open_in_memory(clock(10_000))
        .unwrap()
        .with_bus(bus.clone());
    let cache = ReplyContextCache::new();
    let mut inv = SyncInvalidator::new(&bus, cache.clone());

    db.capture(&NewEvent {
        window_title: Some("Alpha"),
        ..ev("alpha notes", "h1", 100)
    })
    .unwrap();
    let key =
        shogun_memory::thread::thread_key("capture", None, Some("com.apple.Safari"), Some("Alpha"))
            .unwrap();
    cache.put(db.build_reply_context(&key));
    assert!(
        cache.get(&key).is_some(),
        "precondition: warm before the sync"
    );

    // A non-sync event leaves the warm pack alone.
    bus.publish(crate::bus::BusEvent::CacheUpdated);
    assert_eq!(inv.pump(), 0);
    assert!(
        cache.get(&key).is_some(),
        "unrelated events must not evict the pack"
    );

    let summary = db.ingest_integration(&[IngestItem {
        source: "gmail",
        kind: "email",
        title: "Re: Alpha".into(),
        body: "new material for the alpha thread".into(),
        ts_ms: 200,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }]);
    assert_eq!(summary.newly_inserted, 1);
    assert_eq!(inv.pump(), 1, "one sync event handled");
    assert!(
        cache.get(&key).is_none(),
        "stale pack is gone — a miss, not an inline rebuild"
    );
    assert!(
        cache.current().is_none(),
        "cleared entirely, whatever thread was held"
    );

    // The focus path re-assembles; the cache serves again.
    cache.put(db.build_reply_context(&key));
    assert!(cache.get(&key).is_some(), "fresh after re-assembly");
}

/// The subscription is tick-driven, not a spin: pumping a quiet bus does no work and returns
/// at once (`try_recv` is non-blocking), so an idle daemon burns nothing between ticks.
#[test]
fn pumping_a_quiet_bus_returns_immediately_without_spinning() {
    use crate::bus::Bus;
    let bus = Bus::new(8);
    let cache = ReplyContextCache::new();
    let mut inv = SyncInvalidator::new(&bus, cache);
    let t0 = std::time::Instant::now();
    for _ in 0..1_000 {
        assert_eq!(inv.pump(), 0, "a quiet bus handles nothing");
    }
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(100),
        "1000 quiet pumps must be near-instant (non-blocking drain, no spin/wait): {:?}",
        t0.elapsed()
    );
}

#[test]
fn capture_writes_then_dedup_touches_same_row() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let (id1, touched1) = db.capture(&ev("hello", "h", 100)).unwrap();
    assert!(!touched1);
    let (id2, touched2) = db.capture(&ev("hello", "h", 200)).unwrap();
    assert!(touched2, "same content_hash must touch, not append");
    assert_eq!(id1, id2);
}

#[test]
fn hot_starts_ready_and_clones_share_durable_sanitized_rows() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    let clone = db.clone();
    let raw = "visible\u{200b} AKIAIOSFODNN7EXAMPLE";
    let (event_id, _) = db.capture(&ev(raw, "raw-hash", 200)).unwrap();
    assert_eq!(clone.hot_status().phase, HotPhase::Ready);
    let rows = hot_test_candidates(&clone, &["capture"], 1);
    assert_eq!(rows[0].preview.id, event_id);
    assert_ne!(rows[0].preview.excerpt, raw);
}

#[test]
fn hot_touch_promotes_one_canonical_id_without_duplicate() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    let (event_id, _) = db.capture(&ev("same", "same-hash", 100)).unwrap();
    db.capture(&ev("same", "same-hash", 200)).unwrap();
    db.capture(&ev("same", "same-hash", 50)).unwrap();
    let rows = hot_test_candidates(&db, &["capture"], 10);
    assert_eq!(
        rows.iter()
            .filter(|event| event.preview.id == event_id)
            .count(),
        1
    );
    assert_eq!(rows[0].preview.ts, 100);
    let conn = db.conn.lock().unwrap();
    let stored = event_log::hot_event_by_id(&conn, event_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.last_seen_at, 200);
    drop(conn);
    assert!(db.rebuild_hot_now());
    assert_eq!(
        hot_test_candidates(&db, &["capture"], 10)[0].preview.id,
        event_id
    );
}

#[test]
fn failed_durable_write_never_reaches_hot() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    db.capture(&ev("durable", "h1", 100)).unwrap();
    drop_table(&db, "event_log");
    assert!(db.try_capture(&ev("rejected", "h2", 200)).is_err());
    let rows = hot_test_candidates(&db, &["capture"], 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].preview.excerpt, "durable");
}

#[test]
fn hot_fault_falls_back_and_full_repair_restores_fast_path() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    db.capture(&ev("first", "h1", 100)).unwrap();
    let hot = db.hot.clone();
    let _ = std::thread::spawn(move || {
        let _guard = hot.lock().unwrap();
        panic!("poison Hot for recovery test");
    })
    .join();
    assert_eq!(db.recent_context_previews(1, 80)[0].1.excerpt, "first");
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
    assert!(db.capture(&ev("second", "h2", 200)).is_some());
    assert!(db.memory_health().faults_total > 0);
    assert!(db.rebuild_hot_now());
    assert!(!db.hot.is_poisoned());
    assert_eq!(db.hot_status().phase, HotPhase::Ready);
    assert_eq!(hot_test_candidates(&db, &["capture"], 2).len(), 2);
}

#[test]
fn hydration_fault_marks_hot_unready() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    db.capture(&ev("first", "h1", 100)).unwrap();
    drop_table(&db, "event_log");
    {
        let conn = db.conn.lock().unwrap();
        db.reconcile_hot_locked(&conn, 1);
    }
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
    assert!(db.hot_context_candidates(&["capture"], 1, 80).is_none());
}

#[test]
fn rebuild_ready_cannot_overwrite_a_newer_serialized_fault() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let clock: Clock = {
        let calls = calls.clone();
        let reached = reached.clone();
        let release = release.clone();
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::AcqRel) == 2 {
                let (lock, ready) = &*reached;
                *lock.lock().unwrap() = true;
                ready.notify_one();
                let (lock, released) = &*release;
                let mut permit = lock.lock().unwrap();
                while !*permit {
                    permit = released.wait(permit).unwrap();
                }
            }
            1_000
        })
    };
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock);
    let rebuild_db = db.clone();
    let rebuild = std::thread::spawn(move || rebuild_db.rebuild_hot_now());
    let (lock, ready) = &*reached;
    let mut at_publish = lock.lock().unwrap();
    while !*at_publish {
        at_publish = ready.wait(at_publish).unwrap();
    }
    drop(at_publish);

    match db.conn.try_lock() {
        Ok(conn) => {
            db.mark_hot_failed();
            drop(conn);
            let (lock, released) = &*release;
            *lock.lock().unwrap() = true;
            released.notify_one();
            assert!(rebuild.join().unwrap());
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            let (lock, released) = &*release;
            *lock.lock().unwrap() = true;
            released.notify_one();
            assert!(rebuild.join().unwrap());
            let conn = db.conn.lock().unwrap();
            db.mark_hot_failed();
            drop(conn);
        }
        Err(std::sync::TryLockError::Poisoned(_)) => panic!("unexpected DB poison"),
    }
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
}

#[test]
fn initial_phase_publication_survives_status_mutex_contention() {
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    let status = db.hot_status.lock().unwrap();
    let rebuild_db = db.clone();
    let rebuild = std::thread::spawn(move || rebuild_db.rebuild_hot_now());
    assert!(rebuild.join().unwrap());
    drop(status);
    assert_eq!(db.hot_status().phase, HotPhase::Ready);
}

#[test]
fn terminal_phase_publication_survives_status_mutex_contention() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rebuild_clock: Clock = {
        let calls = calls.clone();
        let reached = reached.clone();
        let release = release.clone();
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::AcqRel) == 2 {
                let (lock, ready) = &*reached;
                *lock.lock().unwrap() = true;
                ready.notify_one();
                let (lock, released) = &*release;
                let mut permit = lock.lock().unwrap();
                while !*permit {
                    permit = released.wait(permit).unwrap();
                }
            }
            1_000
        })
    };
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), rebuild_clock);
    let rebuild_db = db.clone();
    let rebuild = std::thread::spawn(move || rebuild_db.rebuild_hot_now());
    let (lock, ready) = &*reached;
    let mut at_terminal = lock.lock().unwrap();
    while !*at_terminal {
        at_terminal = ready.wait(at_terminal).unwrap();
    }
    drop(at_terminal);
    let status = db.hot_status.lock().unwrap();
    let (lock, released) = &*release;
    *lock.lock().unwrap() = true;
    released.notify_one();
    assert!(rebuild.join().unwrap());
    drop(status);
    assert_eq!(db.hot_status().phase, HotPhase::Ready);
}

#[test]
fn frame_delete_hydration_fault_marks_hot_unready() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let event_id = db
        .capture(&NewEvent {
            source: "screen_ocr",
            ..ev("frame text", "frame-fault", 1_000)
        })
        .unwrap()
        .0;
    let first = db
        .store_screen_frame(Some(event_id), None, None, None, None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[1, 2, 3])
        .unwrap();
    db.store_screen_frame(Some(event_id), None, None, None, None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[4, 5, 6])
        .unwrap();
    db.with_conn("test.invalid_hot_projection", |conn| {
        conn.execute(
            "UPDATE event_log SET source = CAST(x'FF' AS TEXT) WHERE id = ?1",
            [event_id],
        )?;
        Ok::<_, rusqlite::Error>(())
    })
    .unwrap();

    assert!(db.delete_screen_frame(first).unwrap());
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
}

#[test]
fn delete_and_ocr_update_reconcile_hot_after_commit() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    let old = db.capture(&ev("old OCR", "h1", 100)).unwrap().0;
    db.capture(&ev("newer", "h2", 200)).unwrap();
    assert!(db.update_event_ocr_text(old, "refreshed OCR").unwrap());
    let rows = hot_test_candidates(&db, &["capture"], 10);
    assert_eq!(
        rows.into_iter()
            .find(|event| event.preview.id == old)
            .unwrap()
            .preview
            .excerpt,
        "refreshed OCR"
    );
    db.delete_since(150).unwrap();
    assert_eq!(hot_test_candidates(&db, &["capture"], 10).len(), 1);
    db.delete_all().unwrap();
    assert!(hot_test_candidates(&db, &["capture"], 10).is_empty());
}

#[test]
fn delete_since_drops_a_deleted_meeting_s_rows_from_recent_memory() {
    // #900: a meeting that ran across the cutoff is deleted whole, with the transcript row
    // `meeting_index` wrote for it. That row is dated at the meeting's start, out of reach of
    // the recent-memory sweep from the cutoff on, so it is dropped by id.
    let (clock, now) = ticking_clock();
    let db = Db::open_in_memory(clock).unwrap();
    let meeting = db.open_meeting(Some("Sync"), None, 0.9, "{}").unwrap();
    let me = shogun_memory::transcript_segments::Speaker::Me;
    db.append_transcript(meeting, 1_100, me, "early words", 0.9);
    db.append_transcript(meeting, 2_500, me, "late words", 0.9);
    now.store(3_000, std::sync::atomic::Ordering::Relaxed);
    assert!(db.close_meeting(meeting));
    let recent = |db: &Db| -> Vec<String> {
        hot_test_candidates(db, &["meeting"], 10)
            .into_iter()
            .map(|event| event.preview.excerpt)
            .collect()
    };
    assert!(recent(&db).iter().any(|text| text.contains("late words")));

    db.delete_since(2_000).unwrap();
    let after = recent(&db);
    assert!(!after.iter().any(|text| text.contains("early words")), "{after:?}");
    assert!(!after.iter().any(|text| text.contains("late words")), "{after:?}");
}

#[test]
fn hot_context_path_and_miss_fallback_preserve_previews() {
    let db = Db::open_in_memory(clock(200)).unwrap();
    db.capture(&ev("current work", "h1", 200)).unwrap();
    let from_hot = db.recent_context_previews(1, 80);
    db.hot.lock().unwrap().clear();
    let from_fallback = db.recent_context_previews(1, 80);
    assert_eq!(from_hot, from_fallback);
}

#[test]
fn hot_context_matches_durable_cross_source_dedup_and_underfill() {
    let db = Db::open_in_memory(clock(500)).unwrap();
    for (source, body, hash, ts) in [
        ("capture", "same excerpt", "capture-same", 500),
        ("screen_ocr", "same excerpt", "ocr-same", 490),
        ("gcal", "older distinct", "gcal-distinct", 300),
    ] {
        db.try_capture(&NewEvent {
            source,
            ..ev(body, hash, ts)
        })
        .unwrap();
    }
    let from_hot = db.recent_context_previews(2, 80);
    db.mark_hot_failed();
    let from_durable = db.recent_context_previews(2, 80);
    assert_eq!(from_hot, from_durable);
    assert_eq!(from_hot.len(), 2);
    assert!(from_hot.iter().any(|(source, _)| source == "gcal"));
}

#[test]
fn maximum_legal_hot_context_request_completes_end_to_end_merge() {
    // Mirrors `recent_context_previews`: seven sources, so 128 / 7 = 18 per source is the
    // largest request the hot snapshot accepts.
    const SOURCES: [&str; 7] = [
        "capture",
        "screen_ocr",
        "screen_text",
        "ai_session",
        "meeting",
        "gmail",
        "gcal",
    ];
    const LIMIT: usize = 18;

    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let mut newest_id = 0;
    for source in SOURCES {
        for index in 0..LIMIT {
            let marker = format!("{source}-{index:02}-");
            let content = format!(
                "{marker}{}",
                "x".repeat(shogun_memory::hot::HOT_CONTENT_MAX_BYTES - marker.len()),
            );
            let hash = format!("max-context-{source}-{index}");
            newest_id = db
                .try_capture(&NewEvent {
                    source,
                    ..ev(&content, &hash, 1_000 + newest_id)
                })
                .unwrap()
                .0;
        }
    }
    db.with_conn("test.prove_hot_context_path", |conn| {
        conn.execute(
            "UPDATE event_log SET content = 'durable sentinel' WHERE id = ?1",
            [newest_id],
        )?;
        Ok::<_, rusqlite::Error>(())
    })
    .unwrap();

    let previews = db.recent_context_previews(LIMIT, shogun_memory::hot::HOT_CONTENT_MAX_BYTES);
    assert_eq!(previews.len(), LIMIT);
    assert!(previews[0].1.excerpt.starts_with("gcal-17-"));
}

#[test]
fn hot_context_matches_durable_touch_and_out_of_order_semantics() {
    let db = Db::open_in_memory(clock(500)).unwrap();
    db.capture(&ev("old provenance touched now", "touched", 100))
        .unwrap();
    db.capture(&ev("old provenance touched now", "touched", 500))
        .unwrap();
    db.try_capture(&NewEvent {
        source: "gmail",
        ..ev("newer provenance", "gmail-newer", 300)
    })
    .unwrap();
    let from_hot = db.recent_context_previews(2, 80);
    db.mark_hot_failed();
    let from_durable = db.recent_context_previews(2, 80);
    assert_eq!(from_hot, from_durable);
    assert_eq!(from_hot[0].0, "gmail");
    assert_eq!(from_hot[1].1.ts, 100);
}

#[test]
fn lossy_hot_projection_declines_to_exact_durable_preview() {
    let db = Db::open_in_memory(clock(500)).unwrap();
    let huge_title = "界".repeat(20_000);
    db.try_capture(&NewEvent {
        window_title: Some(&huge_title),
        ..ev("bounded body", "huge-title", 500)
    })
    .unwrap();
    let exact = db.recent_context_previews(1, 80);
    assert_eq!(
        exact[0].1.window_title.as_deref(),
        Some(huge_title.as_str())
    );
    db.mark_hot_failed();
    assert_eq!(exact, db.recent_context_previews(1, 80));
}

#[test]
fn hot_thread_snapshot_matches_durable_order_with_large_turn() {
    use shogun_memory::ai_session::{Role, SessionTurn};

    let db = Db::open_in_memory(clock(2_000)).unwrap();
    let large = "x".repeat(shogun_memory::hot::HOT_CONTENT_MAX_BYTES);
    let turns = (0..REPLY_TURNS)
        .map(|index| SessionTurn {
            session_id: "thread-parity".into(),
            role: if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            ts_ms: if index == 0 {
                1_900
            } else {
                1_000 + index as i64
            },
            text: if index == 0 {
                large.clone()
            } else {
                format!("turn {index}")
            },
            cwd: None,
        })
        .collect::<Vec<_>>();
    db.ingest_ai_session(&turns);
    let thread_key: String = db
        .with_conn("test.thread_key", |conn| {
            conn.query_row(
                "SELECT thread_key FROM event_log WHERE source = 'ai_session' LIMIT 1",
                [],
                |row| row.get(0),
            )
        })
        .unwrap();
    let from_hot = db.build_reply_context(&thread_key).turns;
    db.mark_hot_failed();
    let from_durable = db.build_reply_context(&thread_key).turns;
    assert_eq!(from_hot, from_durable);
    assert_eq!(from_hot.len(), REPLY_TURNS);
    assert!(from_hot
        .iter()
        .all(|turn| { turn.excerpt.chars().count() <= REPLY_TURN_CHARS + 2 }));
}

#[test]
fn every_direct_event_writer_and_frame_delete_reconcile_hot() {
    use shogun_mcp::sync::IngestItem;
    use shogun_memory::ai_session::{Role, SessionTurn};

    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.ingest_integration(&[IngestItem {
        source: "gmail",
        kind: "email",
        title: "Thread".into(),
        body: "integration body".into(),
        ts_ms: 900,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }]);
    db.ingest_ai_session(&[SessionTurn {
        session_id: "session-1".into(),
        role: Role::User,
        ts_ms: 950,
        text: "AI turn".into(),
        cwd: None,
    }]);
    db.append_note("user note").unwrap();
    let meeting = db.open_meeting(Some("Sync"), None, 0.9, "{}").unwrap();
    db.append_transcript(
        meeting,
        975,
        shogun_memory::transcript_segments::Speaker::Me,
        "meeting words",
        0.9,
    );
    assert!(db.close_meeting(meeting));
    let (ocr_event, _, _) = db
        .ingest_screen_ocr(
            Some("com.example.App"),
            Some("Window"),
            "OCR body",
            0,
            Some(1),
        )
        .unwrap();
    let frame = db
        .store_screen_frame(
            Some(ocr_event),
            None,
            Some("com.example.App"),
            Some("Window"),
            Some(1),
            1,
            1,
            shogun_memory::screen_frames::OcrRegion::WholeFrame,
            &[1, 2, 3],
        )
        .unwrap();

    let rows = hot_test_candidates(
        &db,
        &["gmail", "ai_session", "user", "meeting", "screen_ocr"],
        20,
    );
    assert!(rows.iter().any(|event| event.source == "gmail"));
    assert!(rows.iter().any(|event| event.source == "ai_session"));
    assert!(rows.iter().any(|event| event.source == "user"));
    assert!(rows.iter().any(|event| event.source == "meeting"));
    assert!(db.delete_screen_frame(frame).unwrap());
    assert!(!hot_test_candidates(&db, &["screen_ocr"], 20)
        .iter()
        .any(|event| event.preview.id == ocr_event));
}

#[test]
fn failed_later_meeting_rewrite_leaves_durable_and_hot_index_atomic() {
    let db = Db::open_in_memory(clock(2_000)).unwrap();
    let meeting = db.open_meeting(Some("Atomic"), None, 0.9, "{}").unwrap();
    db.append_transcript(
        meeting,
        1_000,
        shogun_memory::transcript_segments::Speaker::Me,
        "old transcript",
        0.9,
    );
    assert!(db.save_meeting_note(meeting, "old note"));
    assert!(db.close_meeting(meeting));
    db.append_transcript(
        meeting,
        1_500,
        shogun_memory::transcript_segments::Speaker::Me,
        "new transcript",
        0.9,
    );
    {
        let conn = db.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_later_note_rewrite BEFORE UPDATE OF content ON event_log
                 WHEN OLD.kind = 'note'
                 BEGIN SELECT RAISE(ABORT, 'induced later failure'); END;",
        )
        .unwrap();
    }
    assert!(!db.save_meeting_note(meeting, "new note"));

    let hot = hot_test_candidates(&db, &["meeting"], 10);
    assert!(hot
        .iter()
        .any(|row| row.preview.excerpt.contains("old transcript")));
    assert!(!hot
        .iter()
        .any(|row| row.preview.excerpt.contains("new transcript")));
    let durable = db
        .with_conn("test.meeting_index", |conn| {
            event_log::recent_previews_by_source(conn, "meeting", 10, 1_000)
        })
        .unwrap();
    assert!(durable
        .iter()
        .any(|row| row.excerpt.contains("old transcript")));
    assert!(!durable
        .iter()
        .any(|row| row.excerpt.contains("new transcript")));
}

#[test]
fn pinned_meeting_index_does_not_report_a_durable_note_save_failure() {
    let db = Db::open_in_memory(clock(2_000)).unwrap();
    let meeting = db.open_meeting(Some("Pinned"), None, 0.9, "{}").unwrap();
    assert!(db.save_meeting_note(meeting, "old note"));
    assert!(db.close_meeting(meeting));
    db.try_freeze_full_cycle("pinned-note-cycle", 0, 3_000)
        .unwrap();

    assert!(db.save_meeting_note(meeting, "new durable note"));
    assert_eq!(
        db.meeting_note(meeting).as_deref(),
        Some("new durable note")
    );
    let indexed_content: String = db
        .with_conn("test.pinned_meeting_content", |conn| {
            conn.query_row(
                "SELECT content FROM event_log
                 WHERE session_id = ?1 AND source = 'meeting' AND kind = 'note'",
                [meeting],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(indexed_content, "old note");
}

#[test]
fn transcript_exclusion_after_close_leaves_durable_and_hot_search() {
    let db = Db::open_in_memory(clock(2_000)).unwrap();
    let meeting = db.open_meeting(Some("Exclude"), None, 0.9, "{}").unwrap();
    for (ts, text) in [(1_000, "public roadmap line"), (1_100, "zebracorn private line")] {
        db.append_transcript(meeting, ts, shogun_memory::transcript_segments::Speaker::Me, text, 0.9);
    }
    assert!(db.close_meeting(meeting));
    assert!(hot_test_candidates(&db, &["meeting"], 10).iter().any(|row| row.preview.excerpt.contains("zebracorn")));
    let private = db.meeting_transcript_edit_rows(meeting).unwrap().into_iter().find(|row| row.text.contains("zebracorn")).unwrap();

    db.edit_meeting_transcript(meeting, &[shogun_memory::transcript_segments::TranscriptEdit {
        segment_id: private.id, expected_ts: private.ts, expected_text: private.text.clone(), text: None, excluded: Some(true),
    }]).unwrap();

    let hot = hot_test_candidates(&db, &["meeting"], 10);
    assert!(!hot.iter().any(|row| row.preview.excerpt.contains("zebracorn")), "Hot must drop the excluded line");
    assert!(hot.iter().any(|row| row.preview.excerpt.contains("public roadmap line")));
    let durable = db
        .with_conn("test.meeting_exclusion", |conn| event_log::recent_previews_by_source(conn, "meeting", 10, 1_000))
        .unwrap();
    assert!(!durable.iter().any(|row| row.excerpt.contains("zebracorn")));
}

#[test]
fn context_previews_never_serve_stale_hot_meeting_text_after_another_connection_edits() {
    let dir = std::env::temp_dir().join(format!(
        "shogun-meeting-hot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("memory.sqlite3");
    let writer = Db::open(&path, clock(2_000)).unwrap();
    let meeting = writer.open_meeting(Some("Two processes"), None, 0.9, "{}").unwrap();
    for (ts, text) in [(1_000, "budget review stays on Friday"), (1_100, "zebracorn private line")] {
        writer.append_transcript(meeting, ts, shogun_memory::transcript_segments::Speaker::Me, text, 0.9);
    }
    assert!(writer.close_meeting(meeting));

    // The desktop process: its Hot tier is built while the old transcript is current.
    let reader = Db::open(&path, clock(2_000)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // One phase read per turn (see `on_disk_restart_background_rebuild_restores_recent_event`).
    let phase = loop {
        let phase = reader.hot_status().phase;
        if phase != HotPhase::Rebuilding && phase != HotPhase::Empty {
            break phase;
        }
        assert!(std::time::Instant::now() < deadline, "Hot rebuild exceeded ten seconds");
        std::thread::yield_now();
    };
    assert_eq!(phase, HotPhase::Ready);
    assert!(hot_test_candidates(&reader, &["meeting"], 10).iter().any(|row| row.preview.excerpt.contains("zebracorn")));
    assert!(reader.recent_context_previews(5, 400).iter().any(|(_, row)| row.excerpt.contains("zebracorn")));

    // Another process (MCP/REST) corrects one line and excludes the other. Same event ID.
    let rows = writer.meeting_transcript_edit_rows(meeting).unwrap();
    let edit = |text: &str, change: Option<&str>, excluded: Option<bool>| {
        let row = rows.iter().find(|row| row.text == text).unwrap();
        shogun_memory::transcript_segments::TranscriptEdit {
            segment_id: row.id, expected_ts: row.ts, expected_text: row.text.clone(),
            text: change.map(str::to_string), excluded,
        }
    };
    writer.edit_meeting_transcript(meeting, &[
        edit("zebracorn private line", None, Some(true)),
        edit("budget review stays on Friday", Some("budget review moves to Monday"), None),
    ]).unwrap();

    // The reader's Hot still holds the old body; its context must not.
    assert!(hot_test_candidates(&reader, &["meeting"], 10).iter().any(|row| row.preview.excerpt.contains("zebracorn")));
    let previews = reader.recent_context_previews(5, 400);
    let meeting_text: Vec<_> = previews.iter().filter(|(source, _)| source == "meeting").map(|(_, row)| row.excerpt.as_str()).collect();
    assert!(!meeting_text.iter().any(|text| text.contains("zebracorn") || text.contains("Friday")), "{meeting_text:?}");
    assert!(meeting_text.iter().any(|text| text.contains("Monday")), "{meeting_text:?}");
    drop((writer, reader));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pinned_transcript_edit_is_refused_and_leaves_the_transcript_unchanged() {
    let db = Db::open_in_memory(clock(2_000)).unwrap();
    let meeting = db.open_meeting(Some("Pinned edit"), None, 0.9, "{}").unwrap();
    db.append_transcript(meeting, 1_000, shogun_memory::transcript_segments::Speaker::Me, "zebracorn pinned line", 0.9);
    db.append_transcript(meeting, 1_100, shogun_memory::transcript_segments::Speaker::Me, "other line", 0.9);
    assert!(db.close_meeting(meeting));
    db.try_freeze_full_cycle("pinned-transcript-cycle", 0, 3_000).unwrap();
    let row = db.meeting_transcript_edit_rows(meeting).unwrap().remove(0);

    assert!(db.edit_meeting_transcript(meeting, &[shogun_memory::transcript_segments::TranscriptEdit {
        segment_id: row.id, expected_ts: row.ts, expected_text: row.text.clone(), text: None, excluded: Some(true),
    }]).is_err());
    assert!(db.meeting_transcript_edit_rows(meeting).unwrap().iter().all(|row| !row.excluded));
}

#[test]
fn synchronous_hot_rebuild_meets_ten_second_target_on_dense_corpus() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    {
        let conn = db.conn.lock().unwrap();
        for id in 0..5_000 {
            let body = format!("representative event {id} {}", "x".repeat(512));
            event_log::insert(&conn, &ev(&body, &format!("dense-{id}"), id)).unwrap();
        }
    }

    let started = std::time::Instant::now();
    assert!(db.rebuild_hot_now());
    let status = db.hot_status();

    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(status.event_count, 5_000);
    eprintln!(
        "[hot-test] elapsed_ms={} events={} bytes={}",
        status.elapsed_ms, status.event_count, status.bytes
    );
}

#[test]
fn expired_rebuild_deadline_fails_once_and_releases_connection() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    db.capture(&ev("deadline row", "deadline", 10_000)).unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(!db
        .hot_rebuilder()
        .run(cancel, std::time::Instant::now(), None));
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
    assert!(db.conn.try_lock().is_ok());
}

#[test]
fn background_failure_has_one_attempt_and_shutdown_joins() {
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    drop_table(&db, "event_log");
    db.start_hot_rebuild();
    db.hot_task.shutdown();
    assert_eq!(
        db.hot_task
            .attempts
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert!(db.hot_task.join.lock().unwrap().is_none());
    assert_eq!(db.hot_status().phase, HotPhase::Failed);
}

#[test]
fn completed_worker_can_start_one_fault_triggered_repair() {
    use std::sync::atomic::Ordering;

    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    db.start_hot_rebuild();
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let finished = db
            .hot_task
            .join
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|run| run.handle.is_finished());
        if finished || std::time::Instant::now() >= wait_deadline {
            assert!(finished);
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(db.hot_task.attempts.load(Ordering::Acquire), 1);
    db.mark_hot_failed();
    let repair_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while db.hot_task.attempts.load(Ordering::Acquire) < 2
        && std::time::Instant::now() < repair_deadline
    {
        std::thread::yield_now();
    }
    while db
        .hot_task
        .join
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|run| !run.handle.is_finished())
        && std::time::Instant::now() < repair_deadline
    {
        std::thread::yield_now();
    }
    db.hot_task.shutdown();
    assert_eq!(db.hot_task.attempts.load(Ordering::Acquire), 2);
    assert_eq!(db.hot_status().phase, HotPhase::Ready);
}

#[test]
fn fault_in_worker_exit_tail_is_consumed_by_one_repair() {
    use std::sync::atomic::Ordering;

    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    let reached = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    *db.hot_task.finish_barrier.lock().unwrap() = Some((reached.clone(), release.clone()));
    db.start_hot_rebuild();
    reached.wait();
    assert_eq!(db.hot_phase(), HotPhase::Ready);

    let fault_db = db.clone();
    let fault = std::thread::spawn(move || fault_db.mark_hot_failed());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while db.hot_phase() != HotPhase::Failed && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(db.hot_phase(), HotPhase::Failed);
    release.wait();
    fault.join().unwrap();
    while db.hot_task.attempts.load(Ordering::Acquire) < 2 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    while db
        .hot_task
        .join
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|run| !run.handle.is_finished())
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    db.hot_task.shutdown();
    assert_eq!(db.hot_task.attempts.load(Ordering::Acquire), 2);
    assert!(!db.hot_task.pending_repair.load(Ordering::Acquire));
    assert_eq!(db.hot_status().phase, HotPhase::Ready);
}

#[test]
fn rebuild_unwind_removes_progress_handler_and_active_flag() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let rebuild_clock: Clock = {
        let calls = calls.clone();
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::AcqRel) == 1 {
                panic!("induced rebuild unwind");
            }
            1_000
        })
    };
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), rebuild_clock);
    let cancel = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(false));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        db.hot_rebuilder().run(
            cancel.clone(),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            Some(active.clone()),
        )
    }));
    assert!(result.is_err());
    assert!(!active.load(Ordering::Acquire));
    cancel.store(true, Ordering::Release);
    let conn = match db.conn.lock() {
        Ok(conn) => conn,
        Err(poisoned) => poisoned.into_inner(),
    };
    let sum: i64 = conn
        .query_row(
            "WITH RECURSIVE n(x) AS (
                 VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 5000
             ) SELECT sum(x) FROM n",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sum, 12_502_500);
}

#[test]
fn last_db_owner_cancels_and_releases_worker_resources() {
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    let weak_conn = Arc::downgrade(&db.conn);
    let weak_task = Arc::downgrade(&db.hot_task);
    db.start_hot_rebuild();
    let clone = db.clone();
    drop(db);
    assert!(weak_conn.upgrade().is_some());
    drop(clone);
    assert!(weak_conn.upgrade().is_none());
    assert!(weak_task.upgrade().is_none());
}

#[test]
fn weak_rebuilder_waiting_at_barrier_does_not_retain_db_resources() {
    let db = Db::new(shogun_memory::open_in_memory().unwrap(), clock(1_000));
    let weak_conn = Arc::downgrade(&db.conn);
    let rebuilder = db.hot_rebuilder();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        rebuilder.run(
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            None,
        )
    });
    drop(db);
    assert!(weak_conn.upgrade().is_none());
    barrier.wait();
    assert!(!worker.join().unwrap());
}

#[test]
fn rebuild_racing_writer_and_reader_loses_no_committed_id() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let writer_db = db.clone();
    let writer_barrier = barrier.clone();
    let writer = std::thread::spawn(move || {
        writer_barrier.wait();
        writer_db
            .capture(&ev("racing write", "race-hash", 1_000))
            .unwrap()
            .0
    });
    let rebuild_db = db.clone();
    let rebuild_barrier = barrier.clone();
    let rebuild = std::thread::spawn(move || {
        rebuild_barrier.wait();
        assert!(rebuild_db.rebuild_hot_now());
    });
    barrier.wait();
    let event_id = writer.join().unwrap();
    rebuild.join().unwrap();

    let reader = db.clone();
    let snapshot = std::thread::spawn(move || hot_test_candidates(&reader, &["capture"], 10))
        .join()
        .unwrap();
    assert_eq!(
        snapshot
            .iter()
            .filter(|event| event.preview.id == event_id)
            .count(),
        1
    );
}

#[test]
fn on_disk_restart_background_rebuild_restores_recent_event() {
    let dir = std::env::temp_dir().join(format!(
        "shogun-hot-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("memory.sqlite3");
    {
        let db = Db::open(&path, clock(1_000)).unwrap();
        db.capture(&ev("restart durable", "restart-hash", 1_000))
            .unwrap();
    }

    let restarted = Db::open(&path, clock(1_000)).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // One read per turn. The phase is an AtomicU8 a background thread walks
    // Empty -> Rebuilding -> Ready, so a condition that reads it twice can see `Empty` on the
    // first read and `Rebuilding` on the second: neither arm matches, the loop exits mid-rebuild,
    // and the assert below fails with `left: Rebuilding`. That is the whole of this test's
    // flakiness — it needs the scheduler to interleave those two reads, which a loaded CI runner
    // running the full suite in parallel does and an isolated run almost never does.
    let phase = loop {
        let phase = restarted.hot_status().phase;
        if phase != HotPhase::Rebuilding && phase != HotPhase::Empty {
            break phase;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Hot rebuild exceeded ten seconds"
        );
        std::thread::yield_now();
    };

    assert_eq!(phase, HotPhase::Ready);
    assert_eq!(
        hot_test_candidates(&restarted, &["capture"], 1)[0]
            .preview
            .excerpt,
        "restart durable"
    );
    drop(restarted);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dreamcycle_effect_fence_is_atomic_across_sqlite_connections() {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!(
        "shogun-dream-fence-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("memory.sqlite3");
    let now = Arc::new(AtomicI64::new(10_000));
    let make_clock = || {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst)) as Clock
    };
    let first = Db::open(&path, make_clock()).unwrap();
    let second = Db::open(&path, make_clock()).unwrap();
    let cycle_id = "atomic-fence-cycle";
    first.try_freeze_full_cycle(cycle_id, 0, 10_000).unwrap();
    let first_lease = first
        .try_claim_dream_cycle_run(cycle_id, "owner-a".to_string())
        .unwrap()
        .unwrap();

    let (locked_tx, locked_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let writer_db = first.clone();
    let writer_lease = first_lease.clone();
    let writer = std::thread::spawn(move || {
        let _fence = enter_dream_effect_fence(cycle_id, &writer_lease);
        writer_db.with_conn_reported("test.dream_atomic_effect", |conn| {
            locked_tx
                .send(())
                .map_err(|_| "test lock notification failed".to_string())?;
            release_rx
                .recv()
                .map_err(|_| "test effect release failed".to_string())?;
            shogun_memory::jobs::upsert(conn, cycle_id, "consolidation", "done", 0, 10_000, 10_000)
                .map_err(|_| "test effect write failed".to_string())
        })
    });
    locked_rx.recv().unwrap();

    now.store(
        10_000 + super::dream_cycle::DREAM_RUN_LEASE_MS + 1,
        Ordering::SeqCst,
    );
    let (claim_tx, claim_rx) = mpsc::sync_channel(1);
    let claimant = second.clone();
    let claim = std::thread::spawn(move || {
        let result = claimant.try_claim_dream_cycle_run(cycle_id, "owner-b".to_string());
        claim_tx.send(result).unwrap();
    });
    let early_claim = claim_rx.recv_timeout(Duration::from_millis(100)).ok();
    let interleaved = matches!(&early_claim, Some(Ok(Some(_))));

    release_tx.send(()).unwrap();
    assert!(writer.join().unwrap().is_ok());
    let initial_claim = match early_claim {
        Some(result) => result,
        None => claim_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
    };
    claim.join().unwrap();
    assert!(
        !interleaved,
        "takeover interleaved while the effect transaction was paused"
    );
    let second_lease = match initial_claim {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(_) => second
            .try_claim_dream_cycle_run(cycle_id, "owner-b".to_string())
            .unwrap()
            .unwrap(),
    };
    assert_ne!(first_lease.generation, second_lease.generation);

    {
        let _stale_fence = enter_dream_effect_fence(cycle_id, &first_lease);
        assert!(first
            .try_record_job(cycle_id, JobKind::Compression, JobState::Done, 0, 10_000)
            .is_err());
    }
    let runs = first.try_cycle_runs(cycle_id).unwrap();
    assert!(runs.iter().any(|run| run.kind == JobKind::Consolidation));
    assert!(!runs.iter().any(|run| run.kind == JobKind::Compression));

    drop(second);
    drop(first);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn context_actions_ranks_gated_candidates_for_the_screen() {
    use shogun_fusion::assemble::ScreenContext;
    let db = Db::open_in_memory(clock(1)).unwrap();
    let e = db.capture(&ev("evidence", "h1", 1)).unwrap().0;
    // #764: a reply goes to the counterparty's stored address, never to the loop's description.
    let dana = db
        .insert_person(
            &shogun_memory::state::NewPerson {
                display_name: "Dana",
                aliases_json: None,
                emails_json: Some(r#"["dana@client.example"]"#),
                handles_json: None,
                relationship_summary: None,
                confidence: 0.9,
                now: 1,
            },
            &[shogun_memory::state::Provenance::new(e)],
        )
        .unwrap();
    // a high-confidence reply-needed loop mentioning "roadmap", and a low-confidence one
    db.insert_open_loop(
        &shogun_memory::state::NewOpenLoop {
            kind: shogun_memory::state::OpenLoopKind::ReplyNeeded,
            description: "reply about the roadmap",
            counterparty_id: Some(dana),
            project_id: None,
            opened_at: 1,
            confidence: 0.9,
            now: 1,
        },
        &[shogun_memory::state::Provenance::new(e)],
    )
    .unwrap();
    db.insert_open_loop(
        &shogun_memory::state::NewOpenLoop {
            kind: shogun_memory::state::OpenLoopKind::Other,
            description: "vague low-confidence thing",
            counterparty_id: None,
            project_id: None,
            opened_at: 1,
            confidence: 0.2, // below the gate — must not become an action (FR-ST-20)
            now: 1,
        },
        &[shogun_memory::state::Provenance::new(e)],
    )
    .unwrap();

    // The counterparty is on screen (the warm excerpt the shell now passes): her loop belongs here.
    let screen = ScreenContext {
        app_bundle_id: "com.apple.Mail".into(),
        window_title: "roadmap thread".into(),
        excerpt: "Dana: any news on the roadmap?".into(),
        ..ScreenContext::default()
    };
    let cache = db.context_actions(screen, None);
    // the low-confidence loop is gated out; the reply-needed one is present as an action
    assert!(!cache.actions.is_empty());
    // B-5: the reply-needed loop also proposes DraftReply — the ONLY send-family candidate,
    // and it carries L3 (invariant 4: approval-required by construction, never auto-run).
    assert!(
        cache
            .actions
            .iter()
            .all(|a| a.action.is_external_send() == (a.level == shogun_fusion::Level::L3)),
        "L3 iff external send — locals stay L1/L2, sends are never below L3 (invariant 4)"
    );
    assert!(
        cache.actions.iter().any(|a| a.action
            == shogun_fusion::Action::Send(shogun_fusion::SendAction::SendEmail {
                to: "dana@client.example".into()
            })),
        "a reply-needed loop with an address must produce the DraftReply candidate to it (B-5)"
    );
    assert!(
        cache.facts.iter().any(|f| f.contains("roadmap")),
        "gated fact present"
    );
    assert!(
        !cache.facts.iter().any(|f| f.contains("vague")),
        "low-confidence fact excluded"
    );
}

/// A commitment the user resolved (or a loop they closed) is finished work — it must not
/// re-surface as a fact or occupy one of the four action slots on the next panel open.
#[test]
fn context_actions_excludes_resolved_state() {
    use shogun_fusion::assemble::ScreenContext;
    let db = Db::open_in_memory(clock(1)).unwrap();
    let e = db.capture(&ev("evidence", "h1", 1)).unwrap().0;
    let cid = db
        .insert_commitment(
            &shogun_memory::state::NewCommitment {
                direction: shogun_memory::state::CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the finished deck",
                due_at: Some(10),
                status: shogun_memory::state::CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &[shogun_memory::state::Provenance::new(e)],
        )
        .unwrap();
    let lid = db
        .insert_open_loop(
            &shogun_memory::state::NewOpenLoop {
                kind: shogun_memory::state::OpenLoopKind::ReplyNeeded,
                description: "reply about the finished deck",
                counterparty_id: None,
                project_id: None,
                opened_at: 1,
                confidence: 0.9,
                now: 1,
            },
            &[shogun_memory::state::Provenance::new(e)],
        )
        .unwrap();
    db.resolve_commitment(cid);
    db.resolve_open_loop(lid);

    let screen = ScreenContext {
        app_bundle_id: "com.apple.Mail".into(),
        window_title: "finished deck".into(),
        salient: vec!["deck".into()],
        ..ScreenContext::default()
    };
    let cache = db.context_actions(screen, None);
    assert!(
        !cache.facts.iter().any(|f| f.contains("finished deck")),
        "resolved state must not come back as a fact: {:?}",
        cache.facts
    );
    assert!(
        !cache
            .actions
            .iter()
            .any(|a| a.rationale.contains("finished deck")),
        "resolved state must not occupy an action slot"
    );
}

/// The action row as the daemon builds it: a snoozed loop stays off the row, an unreviewed mail
/// guess never becomes an L3 reply, a person is found by alias in the warm excerpt, and a
/// commitment owed to me resolves through `UpdateState` rather than a placeholder.
#[test]
fn context_actions_match_the_screen_skip_snoozed_rows_and_never_send_on_a_guess() {
    use shogun_fusion::assemble::{LabelKey, ScreenContext};
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let e = db.capture(&ev("evidence", "h1", 1)).unwrap().0;
    let prov = [shogun_memory::state::Provenance::new(e)];
    let kenji = db
        .insert_person(
            &shogun_memory::state::NewPerson {
                display_name: "Kenji Watanabe",
                aliases_json: Some(r#"["kwatanabe"]"#),
                emails_json: Some(r#"["kenji@example.com"]"#),
                handles_json: None,
                relationship_summary: None,
                confidence: 0.9,
                now: 1,
            },
            &prov,
        )
        .unwrap();
    let snoozed = db
        .insert_open_loop(
            &shogun_memory::state::NewOpenLoop {
                kind: shogun_memory::state::OpenLoopKind::ReplyNeeded,
                description: "Reply to Kenji about the cache fix",
                counterparty_id: Some(kenji),
                project_id: None,
                opened_at: 1,
                confidence: 0.9,
                now: 1,
            },
            &prov,
        )
        .unwrap();
    let theirs = db
        .insert_commitment(
            &shogun_memory::state::NewCommitment {
                direction: shogun_memory::state::CommitmentDirection::Theirs,
                counterparty_id: Some(kenji),
                description: "Kenji owes the benchmark numbers",
                due_at: None,
                status: shogun_memory::state::CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &prov,
        )
        .unwrap();
    db.with_conn_mut("test.snooze", |conn| {
        conn.execute("UPDATE open_loops SET deferred_until = 10000000 WHERE id = ?1", [snoozed])
    })
    .unwrap();

    let screen = ScreenContext {
        app_bundle_id: "com.tinyspeck.slackmacgap".into(),
        window_title: "#eng - Example Co - Slack".into(),
        excerpt: "kwatanabe: pushed the cache fix, numbers coming tomorrow".into(),
        ..ScreenContext::default()
    };
    let cache = db.context_actions(screen.clone(), None);
    assert!(
        !cache.actions.iter().any(|a| a.rationale.contains("Reply to Kenji")),
        "a snoozed loop stays off the row: {:?}",
        cache.actions
    );
    let owed = cache
        .actions
        .iter()
        .find(|a| a.rationale.contains("benchmark numbers"))
        .expect("the person is on screen, so what they owe me belongs on the row");
    assert_eq!(
        owed.action,
        shogun_fusion::Action::Local(shogun_fusion::LocalAction::UpdateState {
            table: "commitments",
            state_id: theirs
        })
    );
    assert_eq!(owed.label.key, LabelKey::MarkDone);
    assert!(cache.actions.iter().any(|a| a.label.key == LabelKey::LookUp && a.label.subject == "Kenji Watanabe"));
    assert!(!cache.generic_fallback);

    // Wake the loop and mark it an unreviewed local mail guess: it surfaces, but never as a send.
    db.with_conn_mut("test.guess", |conn| -> Result<(), rusqlite::Error> {
        conn.execute("UPDATE open_loops SET deferred_until = NULL WHERE id = ?1", [snoozed])?;
        conn.execute(
            "INSERT INTO mail_triage (thread_key, latest_event_id, label_key, bucket, attention, confidence, stage,
                                      open_loop_id, classified_at_ms, updated_at_ms)
             VALUES ('gmail:t1', 1, 'people', 'important', 'needs_reply', 0.8, 'local', ?1, 1, 1)",
            [snoozed],
        )?;
        Ok(())
    })
    .unwrap();
    let cache = db.context_actions(screen, None);
    assert!(cache.actions.iter().any(|a| a.rationale.contains("Reply to Kenji")), "{:?}", cache.actions);
    assert!(
        cache.actions.iter().all(|a| !a.action.is_external_send()),
        "an unreviewed guess never drives an L3 reply: {:?}",
        cache.actions
    );
}

/// SLO-02 / FR-CF-02: the action row (rows read + matching + ranking) for a mature memory and a
/// full screen excerpt. Prints p50/p95; the gate is the 150ms action budget.
#[test]
fn context_actions_row_build_fits_the_action_slo() {
    use shogun_fusion::assemble::ScreenContext;
    use std::time::Instant;
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.with_conn_mut("slo.seed", |conn| -> Result<(), rusqlite::Error> {
        let tx = conn.savepoint()?;
        tx.execute(
            "INSERT INTO event_log (id,ts,source,kind,content,content_hash,last_seen_at)
             VALUES (1,1,'capture','text','seed evidence','slo-seed',1)",
            [],
        )?;
        for id in 1..=300 {
            tx.execute(
                "INSERT INTO people (id,display_name,aliases,emails,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                 VALUES (?1,?2,?3,?4,0.8,0.8,1,1,1)",
                rusqlite::params![
                    id,
                    format!("Person{id} Surname{id}"),
                    format!(r#"["handle{id}"]"#),
                    format!(r#"["person{id}@example.com"]"#)
                ],
            )?;
            tx.execute(
                "INSERT INTO commitments (id,direction,counterparty_id,description,due_at,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                 VALUES (?1,'mine',?1,?2,?3,'open',0.8,0.8,1,1,1)",
                rusqlite::params![id, format!("Send the quarterly report part {id} to Person{id}"), 1_000 + id * 3_600_000],
            )?;
            tx.execute(
                "INSERT INTO open_loops (id,kind,description,counterparty_id,status,opened_at,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                 VALUES (?1,'reply_needed',?2,?1,'open',1,0.8,0.8,1,1,1)",
                rusqlite::params![id, format!("Reply to Person{id} about invoice INV-{id}")],
            )?;
            for table in ["people", "commitments", "open_loops"] {
                tx.execute(
                    "INSERT INTO state_provenance (state_table,state_id,event_id,weight) VALUES (?1,?2,1,1)",
                    rusqlite::params![table, id],
                )?;
            }
        }
        for id in 1..=60 {
            tx.execute(
                "INSERT INTO projects (id,name,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                 VALUES (?1,?2,'active',0.8,0.8,1,1,1)",
                rusqlite::params![id, format!("Project Codename{id}")],
            )?;
            tx.execute(
                "INSERT INTO state_provenance (state_table,state_id,event_id,weight) VALUES ('projects',?1,1,1)",
                [id],
            )?;
        }
        tx.commit()
    })
    .unwrap();
    let excerpt = "Person42 Surname42 <person42@example.com> wrote: about invoice INV-42 and the quarterly report. ".repeat(12);
    let screen = ScreenContext {
        app_bundle_id: "com.google.Chrome".into(),
        window_title: "Invoice INV-42 - me@example.com - Example Mail".into(),
        website_host: Some("mail.google.com".into()),
        excerpt: excerpt.chars().take(1_200).collect(),
        selection: None,
        ..ScreenContext::default()
    };
    let mut samples = Vec::new();
    for i in 0..31 {
        let start = Instant::now();
        let cache = db.context_actions(screen.clone(), None);
        let us = start.elapsed().as_micros();
        assert!(!cache.generic_fallback, "the thread's person and loop match");
        assert!(cache.actions.len() <= 4);
        if i > 0 {
            samples.push(us);
        }
    }
    samples.sort_unstable();
    let (p50, p95) = (samples[samples.len() / 2], samples[samples.len() * 95 / 100]);
    // The pure half (matching + ranking) on the same rows, to show where the time goes.
    let rows = db.fusion_rows();
    let mut pure = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        let states = shogun_fusion::candidates::build_candidates(&screen, &rows, 1_000);
        let cache = shogun_fusion::assemble::assemble(
            screen.clone(),
            &states,
            "",
            &shogun_fusion::assemble::Intent::default(),
            &[],
        );
        assert!(!cache.actions.is_empty());
        pure.push(start.elapsed().as_micros());
    }
    pure.sort_unstable();
    eprintln!(
        "notch action row: 300 people + 300 commitments + 300 loops + 60 projects, 1.2k excerpt; \
         full p50={p50}us p95={p95}us; matching+ranking only p50={}us p95={}us",
        pure[15],
        pure[28]
    );
    // SLO-02 is 150ms on the optimized build; this is the unoptimized test build on a shared CI
    // runner, gated with the same 2× headroom as `context_actions` review benchmark above.
    assert!(p95 < 300_000, "SLO-02 action row p95 {p95}us");
    assert!(pure[28] < 150_000, "matching + ranking p95 {}us", pure[28]);
}

#[test]
fn ingest_capture_collapses_and_extracts() {
    let db = Db::open_in_memory(clock(1000)).unwrap();
    // a realistic-length window body (the 98% near-dup rule is calibrated for real captures,
    // not one-liners): filler around the actionable sentences.
    let filler = "Weekly sync notes. Attendees reviewed the roadmap and open items. ".repeat(3);
    let base = format!("{filler}I'll send the deck. Waiting on legal. {filler}");
    // first ingest: new event + extraction of the promise/open-loop candidates
    let (id1, t1, cands1) = db
        .ingest_capture(Some("com.apple.Mail"), Some("Inbox"), &base, 5)
        .unwrap();
    assert!(!t1);
    assert_eq!(cands1.len(), 2, "one commitment + one open loop extracted");
    // near-duplicate re-read (one appended char on a long body): collapses (touch), no re-extract
    let typed = format!("{base}x");
    let (id2, t2, cands2) = db
        .ingest_capture(Some("com.apple.Mail"), Some("Inbox"), &typed, 3)
        .unwrap();
    assert!(t2, "a near-duplicate body must dedup-touch");
    assert_eq!(id1, id2);
    assert!(cands2.is_empty(), "touch must not re-extract");
    // still exactly one commitment
    assert_eq!(db.commitments_due(1000).len(), 1);
}

#[test]
fn exact_ax_revisions_preserve_edits_and_touch_unchanged_state() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    let checkpoint = "stable chrome\nlaunch date is October 10";
    let (checkpoint_id, checkpoint_touched, _) = db
        .ingest_capture_checkpoint(Some("com.example.App"), Some("Project"), checkpoint, 5)
        .unwrap();
    let (delta_id, delta_touched, _) = db
        .ingest_capture_delta(
            Some("com.example.App"),
            Some("Project"),
            "launch date is October 17",
            3,
        )
        .unwrap();

    assert!(!checkpoint_touched);
    assert!(!delta_touched);
    assert_ne!(checkpoint_id, delta_id);
    assert!(db.touch_capture_event(delta_id, 2));

    let (kind, content, dwell_ms): (String, String, i64) = db
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT kind, content, dwell_ms FROM event_log WHERE id = ?1",
            [delta_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "text_delta");
    assert_eq!(content, "launch date is October 17");
    assert_eq!(dwell_ms, 5);
}

#[test]
fn capture_collapsed_touches_near_duplicate_bodies() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let base: String = "sprint board: ticket triage, blockers, owner assignments ".repeat(10);
    // first capture creates a row (fresh hash, incoming hash ignored)
    let (id1, t1) = db
        .capture_collapsed(&ev(&base, "ignored-incoming-hash", 100))
        .unwrap();
    assert!(!t1);
    // one keystroke later: ≥98% similar → collapses onto the same row (dedup-touch)
    let typed = format!("{base}!");
    let (id2, t2) = db
        .capture_collapsed(&ev(&typed, "another-ignored-hash", 200))
        .unwrap();
    assert!(t2, "a near-duplicate body must dedup-touch, not append");
    assert_eq!(id1, id2);
    // an unrelated body makes a new event
    let (id3, t3) = db
        .capture_collapsed(&ev("completely unrelated capture", "x", 300))
        .unwrap();
    assert!(!t3);
    assert_ne!(id3, id1);

    let n: i64 = {
        let c = db.conn.lock().unwrap();
        c.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        n, 2,
        "two distinct events: the collapsed body + the unrelated one"
    );
}

#[test]
fn screen_ocr_near_duplicates_collapse_per_app_not_across_apps() {
    // screen_ocr carries app attribution, so its collapse is app-scoped like capture's:
    // a near-identical body read from a second app must become its own event, or that app's
    // capture is silently absorbed into the first app's row and its attribution is lost.
    let db = Db::open_in_memory(clock(1)).unwrap();
    let base: String = "quarterly roadmap doc: milestones, owners, risks, dates ".repeat(10);
    let (id_safari, t1, _) = db
        .ingest_screen_ocr(Some("com.apple.Safari"), Some("Doc"), &base, 5, Some(1))
        .unwrap();
    assert!(!t1);

    // same document, ≥98% similar, read in a *different* app → a second event
    let in_mail = format!("{base}!");
    let (id_mail, t2, _) = db
        .ingest_screen_ocr(Some("com.apple.Mail"), Some("Doc"), &in_mail, 5, Some(1))
        .unwrap();
    assert!(
        !t2,
        "a different app must not dedup-touch the first app's row"
    );
    assert_ne!(id_mail, id_safari);

    // the same app re-reading its own near-identical body still collapses
    let re_read = format!("{base}?");
    let (id_again, t3, _) = db
        .ingest_screen_ocr(Some("com.apple.Safari"), Some("Doc"), &re_read, 3, Some(1))
        .unwrap();
    assert!(
        t3,
        "a near-duplicate re-read within one app must dedup-touch"
    );
    assert_eq!(id_again, id_safari);

    let n: i64 = {
        let c = db.conn.lock().unwrap();
        c.query_row(
            "SELECT count(*) FROM event_log WHERE source = 'screen_ocr'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(n, 2, "one event per app");
}

#[test]
fn capture_and_extract_persists_low_confidence_candidates_once() {
    let db = Db::open_in_memory(clock(500)).unwrap();
    let (_id, touched, ids) = db
        .capture_and_extract(&ev("I'll send the deck. Waiting on legal.", "h", 100))
        .unwrap();
    assert!(!touched);
    assert_eq!(ids.len(), 2, "one commitment + one open loop");
    // every extracted candidate is low-confidence (FR-ST-20)
    for c in db.commitments_due(500) {
        assert!(c.confidence <= shogun_memory::extract::LOCAL_RULE_MAX_CONFIDENCE);
    }

    // a dedup-touch of the same content must NOT extract again
    let (_id2, touched2, ids2) = db
        .capture_and_extract(&ev("I'll send the deck. Waiting on legal.", "h", 200))
        .unwrap();
    assert!(touched2);
    assert!(ids2.is_empty(), "dedup-touch must not re-extract");
    assert_eq!(
        db.commitments_due(500).len(),
        1,
        "still exactly one commitment"
    );
}

#[test]
fn clones_share_one_connection() {
    // A capture through the handle is visible to a traceability read on a *clone* — proving it
    // is the same underlying connection, not a copy.
    let db = Db::open_in_memory(clock(7)).unwrap();
    let sink = db.clone().traceability_sink();
    sink.record(TraceRecord::for_chunk(
        Route::BatchApi,
        "indexing",
        "api.anthropic.com",
        "x",
        false,
    ));
    // read via the original handle
    let rows = db.trace_rows(&Filter::default());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts, 7, "the injected clock stamped the row");
}

#[test]
fn export_and_delete_all_through_the_handle() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    db.capture(&ev("a note", "h1", 10)).unwrap();
    // export sees the event
    let json = db.export_json().unwrap();
    assert!(json.contains("a note"));
    // delete wipes it, schema survives (a re-capture still works)
    // Full deletion waits for a backup's pinned reader, across cloned handles.
    let copy_guard = db.backup_copy.lock().unwrap();
    let eraser = db.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        done_tx.send(eraser.delete_all()).unwrap();
    });
    started_rx.recv().unwrap();
    assert!(done_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err());
    assert!(db.export_json().unwrap().contains("a note"));
    drop(copy_guard);
    let report = done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(report.events, 1);
    let after: serde_json::Value = serde_json::from_str(&db.export_json().unwrap()).unwrap();
    assert!(after["event_log"].as_array().unwrap().is_empty());
    assert!(db.capture(&ev("again", "h2", 20)).is_some());
}

#[test]
fn capture_and_traceability_hit_the_same_handle() {
    let db = Db::open_in_memory(clock(42)).unwrap();
    db.capture(&ev("note", "h1", 10)).unwrap();
    db.traceability_sink().record(TraceRecord::for_chunk(
        Route::MessagesApi,
        "agent",
        "api.anthropic.com",
        "chunk",
        false,
    ));
    // both writes landed on the one connection
    assert_eq!(db.trace_rows(&Filter::default()).len(), 1);
}

#[test]
fn state_write_then_fusion_supply_roundtrip() {
    use shogun_memory::state::{CommitmentDirection, CommitmentStatus, NewCommitment};

    let db = Db::open_in_memory(clock(1)).unwrap();
    // a captured event is the provenance for the commitment
    let (event_id, _) = db
        .capture(&ev("Alice asked for the report", "h1", 5))
        .unwrap();

    let id = db
        .insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the report",
                due_at: Some(50),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 5,
            },
            &[Provenance::new(event_id)],
        )
        .expect("commitment insert");
    assert!(id > 0);

    // now supply it to Fusion — at now=100 the due=50 commitment reads as overdue
    let due = db.commitments_due(100);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].description, "send the report");
    assert!(due[0].overdue, "due 50 < now 100 → overdue");
    assert_eq!(due[0].provenance_event_id, Some(event_id));
}

#[test]
fn local_brief_from_db_carries_overdue_commitments_only() {
    use shogun_memory::state::{CommitmentDirection, CommitmentStatus, NewCommitment};

    let db = Db::open_in_memory(clock(1)).unwrap();
    let (e, _) = db.capture(&ev("x", "h1", 1)).unwrap();
    let mk = |desc: &'static str, due: i64, status| NewCommitment {
        direction: CommitmentDirection::Mine,
        counterparty_id: None,
        description: desc,
        due_at: Some(due),
        status,
        project_id: None,
        confidence: 0.9,
        now: 1,
    };
    db.insert_commitment(
        &mk("overdue one", 10, CommitmentStatus::Overdue),
        &[Provenance::new(e)],
    )
    .unwrap();
    db.insert_commitment(
        &mk("future one", 9999, CommitmentStatus::Open),
        &[Provenance::new(e)],
    )
    .unwrap();

    // degraded/local brief keeps only overdue commitments (FR-MB-04)
    let brief = db.local_morning_brief(Vec::new(), 100);
    assert!(brief.degraded);
    assert_eq!(brief.commitments_due.len(), 1);
    assert_eq!(brief.commitments_due[0].text, "overdue one");
}

#[test]
fn record_feedback_wrapper_persists_the_signal_locally() {
    use shogun_memory::lessons::{list_feedback_since, FeedbackKind, LessonScope, NewFeedback};

    let db = Db::open_in_memory(clock(1)).unwrap();
    let id = db.record_feedback(
        FeedbackKind::EditBeforeApprove,
        LessonScope::Person,
        &NewFeedback {
            ts_ms: 42,
            action_kind: Some("send_email"),
            scope_ref: Some("alice@example.com"),
            before_text: Some("proposed body"),
            after_text: Some("final body"),
            ..Default::default()
        },
    );
    assert!(id.is_some(), "a healthy DB accepts the feedback write");

    // Read it back through the same connection the wrapper wrote to.
    let rows = {
        let c = db.conn.lock().unwrap();
        list_feedback_since(&c, 0).unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, FeedbackKind::EditBeforeApprove);
    assert_eq!(rows[0].scope, LessonScope::Person);
    assert_eq!(rows[0].scope_ref.as_deref(), Some("alice@example.com"));
    assert_eq!(rows[0].before_text.as_deref(), Some("proposed body"));
    assert_eq!(rows[0].after_text.as_deref(), Some("final body"));
    // Fire-and-forget shape: the wrapper returns Option, never Err — an approval action can
    // discard it with `let _ =` and can never be failed by it.
    let _: Option<i64> = id;
}

#[test]
fn generation_directives_include_active_lessons_not_feedback_text() {
    use shogun_memory::lessons::{distill, FeedbackKind, LessonScope, NewFeedback};

    let db = Db::open_in_memory(clock(1)).unwrap();
    const SECRET: &str = "SECRET_FEEDBACK_BODY_q1";
    for i in 0..3 {
        let before = format!(
                "Hi team,\nA longer draft body about the {SECRET} quarterly numbers, line {i}.\nMore detail follows in the tracker.\nBest, Taro"
            );
        let after = format!(
                "Hi team,\nA longer draft body about the {SECRET} quarterly numbers, line {i}.\nMore detail follows in the tracker."
            );
        db.record_feedback(
            FeedbackKind::EditBeforeApprove,
            LessonScope::Global,
            &NewFeedback {
                ts_ms: i,
                action_kind: Some("draft_reply"),
                before_text: Some(&before),
                after_text: Some(&after),
                ..Default::default()
            },
        );
    }
    let candidates = distill(&db.feedback_after(0));
    assert_eq!(candidates.len(), 1);
    db.upsert_lesson(&candidates[0], 100)
        .expect("lesson persists");

    let text = db.generation_directives(
        &crate::user_config::ShougunConfig::default(),
        GenerationContext::default(),
    );
    assert!(text.contains("## Learned (auto)"), "{text}");
    assert!(
        text.contains("Best, Taro"),
        "the distilled closing-line rule is injected"
    );
    assert!(
        !text.contains(SECRET),
        "feedback bodies never ride the prompt"
    );
    assert!(
        text.contains("never change which actions need confirmation"),
        "permission disclaimer stays in the learned section: {text}"
    );
}

#[test]
fn generation_directives_only_include_lessons_matching_generation_context() {
    use shogun_memory::lessons::{
        FeedbackKind, LessonCandidate, LessonKind, LessonScope, NewFeedback,
    };

    let db = Db::open_in_memory(clock(1)).unwrap();
    let seed = |scope: LessonScope, scope_ref: Option<&str>, instruction: &str| {
        let evidence = (0..3)
            .map(|ts_ms| {
                db.record_feedback(
                    FeedbackKind::EditBeforeApprove,
                    scope,
                    &NewFeedback {
                        ts_ms,
                        scope_ref,
                        action_kind: Some("draft_reply"),
                        ..Default::default()
                    },
                )
                .expect("feedback persists")
            })
            .collect();
        db.upsert_lesson(
            &LessonCandidate {
                kind: LessonKind::Style,
                scope,
                scope_ref: scope_ref.map(str::to_owned),
                instruction: instruction.to_owned(),
                evidence,
            },
            100,
        )
        .expect("lesson persists");
    };
    seed(LessonScope::Global, None, "GLOBAL_LESSON");
    seed(LessonScope::App, Some("com.apple.Mail"), "MAIL_ONLY_LESSON");
    seed(
        LessonScope::Person,
        Some("alice@example.com"),
        "ALICE_ONLY_LESSON",
    );

    let cfg = crate::user_config::ShougunConfig::default();
    let slack = db.generation_directives(
        &cfg,
        GenerationContext {
            app_bundle_id: Some("com.tinyspeck.slackmacgap"),
            ..Default::default()
        },
    );
    assert!(slack.contains("GLOBAL_LESSON"), "{slack}");
    assert!(!slack.contains("MAIL_ONLY_LESSON"), "{slack}");
    assert!(!slack.contains("ALICE_ONLY_LESSON"), "{slack}");

    let mail_to_alice = db.generation_directives(
        &cfg,
        GenerationContext {
            app_bundle_id: Some("com.apple.Mail"),
            person_id: Some("alice@example.com"),
            ..Default::default()
        },
    );
    assert!(mail_to_alice.contains("GLOBAL_LESSON"), "{mail_to_alice}");
    assert!(
        mail_to_alice.contains("MAIL_ONLY_LESSON"),
        "{mail_to_alice}"
    );
    assert!(
        mail_to_alice.contains("ALICE_ONLY_LESSON"),
        "{mail_to_alice}"
    );
}

#[test]
fn dream_cycle_resume_skips_done_jobs_from_the_db() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let cycle = "20260720";
    // first two jobs completed, the third was interrupted mid-run
    assert!(db.record_job(cycle, JobKind::Consolidation, JobState::Done, 0, 100));
    assert!(db.record_job(cycle, JobKind::Compression, JobState::Done, 0, 100));
    assert!(db.record_job(cycle, JobKind::StateUpdate, JobState::Running, 0, 100));

    // resume the full cycle: done jobs are skipped, the running one is rescheduled
    let todo = db.resume(cycle, CycleKind::Full);
    assert_eq!(todo.first(), Some(&JobKind::StateUpdate));
    assert_eq!(todo.len(), 6); // StateUpdate, ConfidenceRecalc, ColdDemotion, MorningBrief, LessonDistillation, PatternDistillation
    assert!(!todo.contains(&JobKind::Consolidation));
}

#[test]
fn dream_event_read_failure_marks_consolidation_failed_and_retryable() {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use crate::dreamcycle::run::run_cycle;

    let db = Db::open_in_memory(clock(100)).unwrap();
    drop_table(&db, "event_log");
    let classifier = LocalRuleClassifier;
    let summarizer = LocalExtractiveSummarizer;
    let runner = DbDreamRunner::new(&db, &classifier, &summarizer, 100);

    let report = run_cycle(&db, &runner, "read-failure", CycleKind::Full, 0, 100);

    assert!(matches!(report.failed, Some((JobKind::Consolidation, _))));
    assert!(db
        .resume("read-failure", CycleKind::Full)
        .contains(&JobKind::Consolidation));
    assert!(db
        .cycle_runs("read-failure")
        .iter()
        .any(|run| run.kind == JobKind::Consolidation && run.state == JobState::Failed));
}

#[test]
fn dream_candidate_write_failure_marks_consolidation_failed_and_retryable() {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use crate::dreamcycle::run::run_cycle;

    let db = Db::open_in_memory(clock(100)).unwrap();
    db.capture(&ev("I'll send the report tomorrow.", "dream-write", 50))
        .unwrap();
    drop_table(&db, "state_provenance");
    let classifier = LocalRuleClassifier;
    let summarizer = LocalExtractiveSummarizer;
    let runner = DbDreamRunner::new(&db, &classifier, &summarizer, 100);

    let report = run_cycle(&db, &runner, "write-failure", CycleKind::Full, 0, 100);

    assert!(matches!(report.failed, Some((JobKind::Consolidation, _))));
    assert!(db
        .resume("write-failure", CycleKind::Full)
        .contains(&JobKind::Consolidation));
    assert!(db
        .cycle_runs("write-failure")
        .iter()
        .any(|run| run.kind == JobKind::Consolidation && run.state == JobState::Failed));
}

/// Chat and context reads of saved screens honour Visual recall's Off switch and the user's
/// retention without waiting for the expiry sweep or a successful Off purge (issue #782).
#[test]
fn chat_reads_of_saved_screens_honour_off_and_retention() {
    use std::sync::atomic::Ordering::Relaxed;
    const DAY: i64 = 86_400_000;
    let (clock, now) = ticking_clock();
    now.store(10 * DAY, Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "shogun-recall-settings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let settings = dir.join("visual_recall.json");
    let db = Db::open_in_memory(clock)
        .unwrap()
        .with_visual_recall_settings_path(Some(settings.clone()));
    let (event, _, _) = db
        .ingest_screen_ocr(Some("com.example.App"), Some("Invoices"), "invoice totals for march", 0, Some(1))
        .unwrap();
    db.store_screen_frame(Some(event), None, Some("com.example.App"), Some("Invoices"), Some(1), 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[1, 2, 3])
        .unwrap();
    // The same writer the desktop and API faces use.
    let write = |enabled: bool, days: u32| {
        crate::capture::visual_recall::update_settings(&settings, |s| {
            s.enabled = enabled;
            s.retention = crate::capture::visual_recall::RetentionPolicy::try_days(days).unwrap();
        })
        .unwrap();
    };

    write(true, 1);
    assert_eq!(db.recall_screen_frames("invoice", 5, 200).len(), 1, "a fresh frame is found");

    // 1 day and 20 minutes later, before any sweep has run: past a 1-day retention.
    now.store(11 * DAY + 20 * 60_000, Relaxed);
    assert!(db.recall_screen_frames("invoice", 5, 200).is_empty());
    write(true, 3);
    assert_eq!(db.recall_screen_frames("invoice", 5, 200).len(), 1, "inside a 3-day retention");

    // Off reads nothing, even when the purge did not run (or failed).
    write(false, 3);
    assert!(db.recall_screen_frames("invoice", 5, 200).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fresh settings directory for one rescan test (issue #782).
fn rescan_settings_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shogun-rescan-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_recall_settings(path: &std::path::Path, enabled: bool, days: u32) {
    crate::capture::visual_recall::update_settings(path, |s| {
        s.enabled = enabled;
        s.retention = crate::capture::visual_recall::RetentionPolicy::try_days(days).unwrap();
    })
    .unwrap();
}

/// One passive saved screen with thin OCR text, as the capture loop stores it.
fn seed_rescan_frame(db: &Db) -> (i64, i64) {
    let (event, _, _) = db
        .ingest_screen_ocr(Some("com.example.App"), Some("Invoices"), "lnvoice tota1s f0r rnarch", 0, Some(1))
        .unwrap();
    let frame = db
        .store_screen_frame(Some(event), None, Some("com.example.App"), Some("Invoices"), Some(1), 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[7, 8, 9])
        .unwrap();
    (event, frame)
}

/// The shared re-scan (Memory API + desktop "Re-read text", issue #782) replaces the linked
/// event's OCR text with what recognition returns for the stored JPEG.
#[test]
fn rescan_replaces_the_frame_text_with_the_new_reading() {
    let dir = rescan_settings_dir("ok");
    let settings = dir.join("visual_recall.json");
    write_recall_settings(&settings, true, 3);
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let (event, frame) = seed_rescan_frame(&db);

    let mut seen = Vec::new();
    let rescan = db
        .rescan_screen_frame_with(&settings, frame, |jpeg, _region| {
            seen.extend_from_slice(jpeg);
            Some("invoice totals for march".to_string())
        })
        .unwrap();
    assert_eq!(seen, vec![7, 8, 9], "recognition reads the stored JPEG");
    assert_eq!(
        rescan,
        FrameRescan { frame_id: frame, event_id: event, ocr_text: "invoice totals for march".into() }
    );
    let summary = db.get_screen_frame_summary(frame).unwrap();
    assert_eq!(summary.ocr_text, "invoice totals for march", "the stored text is replaced");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A full-display preview (#969): re-reading covers the focused window only, and a preview with
/// no OCR event gets one dated when the screen was saved, linked to the frame.
#[test]
fn rescan_reads_only_the_focused_window_of_a_preview_and_links_its_first_ocr_event() {
    use shogun_memory::screen_frames::{FrameRect, OcrRegion};
    let dir = rescan_settings_dir("preview");
    let settings = dir.join("visual_recall.json");
    write_recall_settings(&settings, true, 3);
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let window = OcrRegion::Window(FrameRect { x: 250, y: 0, width: 500, height: 250 });
    let frame = db
        .store_screen_frame(None, None, Some("com.example.App"), Some("Invoices"), Some(7), 1_000, 500, window, &[7])
        .unwrap();

    let mut region = None;
    let rescan = db
        .rescan_screen_frame_with(&settings, frame, |_, r| {
            region = Some(r);
            Some("invoice totals for march".to_string())
        })
        .unwrap();
    assert_eq!(region, Some([0.25, 0.0, 0.5, 0.5]), "only the focused window is read");
    let summary = db.get_screen_frame_summary(frame).unwrap();
    assert_eq!(summary.event_id, Some(rescan.event_id));
    assert_eq!(summary.ocr_text, "invoice totals for march");
    let ts: i64 = db
        .with_conn("test.event_ts", |c| {
            c.query_row("SELECT ts FROM event_log WHERE id = ?1", [rescan.event_id], |r| r.get(0))
        })
        .unwrap();
    assert_eq!(ts, summary.created_at_ms, "the text is dated when the screen was saved");
    let hits = db.search_screen_frames_window("march", 0, i64::MAX, 10, 80);
    assert_eq!(hits.first().map(|hit| hit.frame_id), Some(frame), "the re-read text is searchable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rescan_refuses_an_expired_frame_and_never_reads_it() {
    use std::sync::atomic::Ordering::Relaxed;
    const DAY: i64 = 86_400_000;
    let dir = rescan_settings_dir("expired");
    let settings = dir.join("visual_recall.json");
    write_recall_settings(&settings, true, 1);
    let (clock, now) = ticking_clock();
    now.store(10 * DAY, Relaxed);
    let db = Db::open_in_memory(clock).unwrap();
    let (_, frame) = seed_rescan_frame(&db);

    // Past the 1-day retention, before any sweep ran: the rescan applies retention itself.
    now.store(11 * DAY + 60_000, Relaxed);
    let result = db.rescan_screen_frame_with(&settings, frame, |_, _region| {
        panic!("an expired frame must not be re-read")
    });
    assert_eq!(result, Err(FrameRescanError::Expired));
    assert_eq!(FrameRescanError::Expired.code(), RESCAN_ERR_FRAME_EXPIRED);
    assert!(db.get_screen_frame_summary(frame).is_none(), "the expired frame was purged");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rescan_refuses_when_visual_recall_is_off() {
    let dir = rescan_settings_dir("off");
    let settings = dir.join("visual_recall.json");
    write_recall_settings(&settings, false, 3);
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let (_, frame) = seed_rescan_frame(&db);

    let result = db.rescan_screen_frame_with(&settings, frame, |_, _region| {
        panic!("Off must not re-read a passive frame")
    });
    assert_eq!(result, Err(FrameRescanError::VisualRecallOff));
    assert_eq!(FrameRescanError::VisualRecallOff.code(), RESCAN_ERR_VISUAL_RECALL_OFF);
    assert!(db.get_screen_frame_summary(frame).is_none(), "Off purged the passive frame");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rescan_reports_a_missing_frame_and_a_failed_reading() {
    let dir = rescan_settings_dir("missing");
    let settings = dir.join("visual_recall.json");
    write_recall_settings(&settings, true, 3);
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let (_, frame) = seed_rescan_frame(&db);

    assert_eq!(
        db.rescan_screen_frame_with(&settings, frame + 100, |_, _region| Some("x".into())),
        Err(FrameRescanError::NotFound)
    );
    assert_eq!(FrameRescanError::NotFound.code(), RESCAN_ERR_FRAME_MISSING);
    assert_eq!(
        db.rescan_screen_frame_with(&settings, frame, |_, _region| None),
        Err(FrameRescanError::OcrFailed)
    );
    assert_eq!(FrameRescanError::OcrFailed.code(), RESCAN_ERR_OCR_FAILED);
    assert_eq!(
        db.get_screen_frame_summary(frame).unwrap().ocr_text,
        "lnvoice tota1s f0r rnarch",
        "a failed reading keeps the old text"
    );
    assert_eq!(FrameRescanError::RetentionCheckFailed.code(), RESCAN_ERR_MEMORY_UNAVAILABLE);
    assert_eq!(FrameRescanError::EventUpdateFailed.code(), RESCAN_ERR_MEMORY_UNAVAILABLE);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a settings path (tests, processes that never configured one) the hard ceiling applies.
#[test]
fn recall_without_a_settings_path_keeps_the_ceiling() {
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let (event, _, _) = db
        .ingest_screen_ocr(Some("com.example.App"), Some("Invoices"), "invoice totals", 0, Some(1))
        .unwrap();
    db.store_screen_frame(Some(event), None, Some("com.example.App"), Some("Invoices"), Some(1), 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[1])
        .unwrap();
    assert_eq!(db.recall_screen_frames("invoice", 5, 200).len(), 1);
    assert!(hot_test_candidates(&db, &["screen_ocr"], 20).iter().any(|e| e.preview.id == event));
    assert_eq!(db.purge_auto_screen_frames().unwrap(), 1, "Off purges the passive frame");
    assert!(db.recall_screen_frames("invoice", 5, 200).is_empty());
    assert!(
        !hot_test_candidates(&db, &["screen_ocr"], 20).iter().any(|e| e.preview.id == event),
        "Off also drops the frame's OCR text from Hot memory"
    );
}

/// A clock the test can advance, so ledger rows land at distinct instants the way real ones do.
fn ticking_clock() -> (Clock, Arc<std::sync::atomic::AtomicI64>) {
    let t = Arc::new(std::sync::atomic::AtomicI64::new(1_000));
    let handle = t.clone();
    (
        Arc::new(move || t.load(std::sync::atomic::Ordering::Relaxed)),
        handle,
    )
}

/// FR-DC-06: the status view is rebuilt from the ledger, so it must show the same thing after a
/// relaunch as it did live — nothing here is reported by the run that produced it.
#[test]
fn dream_status_reports_the_last_night_from_the_ledger() {
    let (clock, now) = ticking_clock();
    let db = Db::open_in_memory(clock).unwrap();
    let set = |t: i64| now.store(t, std::sync::atomic::Ordering::Relaxed);

    // an older night that completed fully
    for kind in crate::dreamcycle::plan::FULL_SEQUENCE {
        set(1_000);
        db.record_job("20260723", *kind, JobState::Done, 0, 100);
    }
    // last night: consolidation reached the Batch lane and failed
    set(9_000);
    db.record_job(
        "20260724",
        JobKind::Consolidation,
        JobState::Failed,
        100,
        200,
    );

    let status = db.dream_status("20260724", 7);
    let last = status.last.expect("a night has run");
    assert_eq!(last.cycle_id, "20260724");
    assert_eq!(last.kind, CycleKind::Full);
    assert!(!last.succeeded);
    assert_eq!(last.jobs_failed, 1);
    assert_eq!((last.input_from_ts, last.input_to_ts), (100, 200));
    // one failed night after a good one → amber, not red
    assert_eq!(
        status.indicator,
        crate::dreamcycle::health::Indicator::Amber
    );
    assert!(
        !status.full_run_done_today,
        "a failed cycle has not run today"
    );
}

#[test]
fn a_completed_night_clears_the_indicator_and_marks_today_done() {
    let (clock, now) = ticking_clock();
    let db = Db::open_in_memory(clock).unwrap();
    let set = |t: i64| now.store(t, std::sync::atomic::Ordering::Relaxed);

    set(1_000);
    db.record_job("20260723", JobKind::Consolidation, JobState::Failed, 0, 100);
    set(9_000);
    for kind in crate::dreamcycle::plan::FULL_SEQUENCE {
        db.record_job("20260724", *kind, JobState::Done, 100, 200);
    }

    let status = db.dream_status("20260724", 7);
    assert_eq!(
        status.indicator,
        crate::dreamcycle::health::Indicator::Normal
    );
    assert!(status.full_run_done_today, "tonight's full cycle completed");
}

/// #1066 B: hourly cycles live beside the nightly ledger without crowding it out. The night
/// views (`last`, `full_run_done_today`) ignore them; `last_hourly` and the day-level indicator
/// read them.
#[test]
fn hourly_cycles_are_a_separate_lane_in_the_status() {
    let (clock, now) = ticking_clock();
    let db = Db::open_in_memory(clock).unwrap();
    let set = |t: i64| now.store(t, std::sync::atomic::Ordering::Relaxed);

    set(1_000);
    for kind in crate::dreamcycle::plan::FULL_SEQUENCE {
        db.record_job("20260724", *kind, JobState::Done, 0, 100);
    }
    // Twenty hourly reviews later, the nightly cycle must still be the night the status sees.
    for h in 0..20_i64 {
        set(2_000 + h);
        for kind in crate::dreamcycle::plan::FULL_SEQUENCE {
            db.record_job(&format!("20260725-h{h:02}"), *kind, JobState::Done, 100 + h, 200 + h);
        }
    }
    let status = db.dream_status("20260724", 7);
    assert_eq!(status.last.as_ref().map(|c| c.cycle_id.as_str()), Some("20260724"));
    assert!(status.full_run_done_today, "the nightly cycle is still found");
    assert_eq!(
        status.last_hourly.as_ref().map(|c| c.cycle_id.as_str()),
        Some("20260725-h19")
    );
    assert_eq!(status.indicator, crate::dreamcycle::health::Indicator::Normal);

    // A whole day of failed hourly reviews, then another: red after three such days.
    for (n, day) in ["20260726", "20260727", "20260728"].iter().enumerate() {
        set(10_000 + n as i64 * 100);
        db.record_job(&format!("{day}-h09"), JobKind::Consolidation, JobState::Failed, 0, 1);
    }
    let failing = db.dream_status("20260724", 7);
    assert_eq!(failing.indicator, crate::dreamcycle::health::Indicator::Red);
    let json = db.dream_status_json();
    assert_eq!(json["last_hourly"]["cycle_id"], "20260728-h09");
    assert_eq!(json["last_hourly"]["succeeded"], false);
}

/// A degraded catch-up does no Batch work, so it is not evidence about the Batch lane either
/// way — it must not clear an amber indicator the failed full cycles earned.
#[test]
fn a_degraded_catch_up_does_not_clear_the_indicator() {
    let (clock, now) = ticking_clock();
    let db = Db::open_in_memory(clock).unwrap();
    let set = |t: i64| now.store(t, std::sync::atomic::Ordering::Relaxed);

    set(1_000);
    db.record_job("20260723", JobKind::Consolidation, JobState::Failed, 0, 100);
    set(5_000);
    for kind in DEGRADED_SEQUENCE {
        db.record_job("20260724", *kind, JobState::Done, 100, 200);
    }

    let status = db.dream_status("20260724", 7);
    assert_eq!(status.last.map(|c| c.kind), Some(CycleKind::Degraded));
    assert_eq!(
        status.indicator,
        crate::dreamcycle::health::Indicator::Amber
    );
    assert!(
        !status.full_run_done_today,
        "a catch-up is not the night's full cycle"
    );
}

/// Issue #285: `dream.status` derives its action facts from the ledger — an unfinished full cycle
/// resumes, one whose job is at the provider is collected (never resubmitted), and the window
/// phase is measured against the due night.
#[test]
fn dream_status_action_facts_come_from_the_ledger() {
    use crate::dreamcycle::actions::{
        available, DisabledCode, ExternalFacts, PendingFull, WindowPhase,
    };
    const HOUR: i64 = 3_600_000;
    // 2026-09-25 09:00 UTC: the 25th's 02:00–06:00 window closed three hours ago.
    let now = 1_790_294_400_000 + 9 * HOUR;
    let db = Db::open_in_memory(clock(now)).unwrap();
    let fine = ExternalFacts {
        plan_active: Some(true),
        batch_lane: Some(true),
        cap_remaining: Some(10),
        low_battery: Some(false),
        retry_offered: true,
    };

    let status = db.dream_status_at("20260925", 7, now, 0);
    assert_eq!(status.action_facts.pending_full, PendingFull::None);
    assert_eq!(status.action_facts.window, WindowPhase::CatchUp);
    let late = db.dream_status_at("20260925", 7, now + 10 * HOUR, 0);
    assert_eq!(late.action_facts.window, WindowPhase::Closed);

    // Frozen with nobody running it (a crash before submission): it resumes, however old.
    db.try_freeze_full_cycle("20260925", now - 30 * HOUR, now - 6 * HOUR).unwrap();
    let frozen = db.dream_status_at("20260925", 7, now + 10 * HOUR, 0);
    assert_eq!(frozen.action_facts.pending_full, PendingFull::Resumable);
    let a = available(&frozen.night, &frozen.action_facts, &fine);
    assert!(a.retry_full.enabled, "an unfinished cycle resumes outside the catch-up allowance");
    let lease = db
        .try_claim_dream_cycle_run("20260925", "test-owner".into())
        .unwrap()
        .expect("lease");
    let ids = vec!["item-1".to_string()];
    assert!(db.try_claim_dream_submission("20260925", &lease, &ids).unwrap());
    // While the lease is live, the night is running and both actions say so.
    let running = db.dream_status_at("20260925", 7, now, 0);
    let a = available(&running.night, &running.action_facts, &fine);
    assert_eq!(a.run_now.disabled_reason, Some(DisabledCode::AlreadyRunning));
    assert_eq!(a.retry_full.disabled_reason, Some(DisabledCode::AlreadyRunning));

    db.try_mark_dream_batch_submitted("20260925", &lease, "batch-1", &ids).unwrap();
    assert!(db.try_release_dream_cycle_run("20260925", &lease).unwrap());
    let waiting = db.dream_status_at("20260925", 7, now, 0);
    assert_eq!(waiting.action_facts.pending_full, PendingFull::AwaitingResults);
    let a = available(&waiting.night, &waiting.action_facts, &fine);
    assert!(a.run_now.enabled, "the degraded run never touches the provider's job");
    assert_eq!(a.retry_full.disabled_reason, Some(DisabledCode::ResultsPending));
}

#[test]
fn an_abandoned_night_with_a_failed_ledger_is_not_a_legacy_cycle() {
    use crate::dreamcycle::plan::{JobKind, JobState};
    const HOUR: i64 = 3_600_000;
    let db = Db::open_in_memory(clock(100 * HOUR)).unwrap();
    // A true legacy ledger (no dream_cycles row) is still found and frozen.
    db.try_record_job("20260920", JobKind::Consolidation, JobState::Failed, HOUR, 2 * HOUR).unwrap();
    assert_eq!(db.try_oldest_legacy_unfinished_full_cycle().unwrap().map(|c| c.0).as_deref(), Some("20260920"));
    db.try_freeze_full_cycle("20260920", HOUR, 2 * HOUR).unwrap();
    // Once its cycle row exists (here abandoned), the row owns it: not legacy any more.
    let lease = db.try_claim_dream_cycle_run("20260920", "o".into()).unwrap().expect("lease");
    db.try_mark_dream_cycle_abandoned("20260920", &lease).unwrap();
    assert_eq!(db.try_oldest_legacy_unfinished_full_cycle().unwrap(), None);
}

#[test]
fn dream_status_on_a_fresh_install_is_empty_and_normal() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let status = db.dream_status("20260724", 7);
    assert!(status.last.is_none());
    assert_eq!(
        status.indicator,
        crate::dreamcycle::health::Indicator::Normal
    );
    assert!(!status.full_run_done_today);
}

#[test]
fn record_job_upsert_advances_state_for_resume() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let cycle = "night";
    db.record_job(cycle, JobKind::Consolidation, JobState::Running, 0, 100);
    // still to-do while running
    assert!(db
        .resume(cycle, CycleKind::Full)
        .contains(&JobKind::Consolidation));
    // mark done → dropped from the resume set
    db.record_job(cycle, JobKind::Consolidation, JobState::Done, 0, 100);
    assert!(!db
        .resume(cycle, CycleKind::Full)
        .contains(&JobKind::Consolidation));
    // one row, not two (idempotent upsert)
    assert_eq!(db.cycle_runs(cycle).len(), 1);
}

#[test]
fn a_meeting_interval_opens_and_closes_through_the_db() {
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db
        .open_meeting(Some("Weekly sync"), Some("us.zoom.xos"), 0.35, "{}")
        .unwrap();

    assert!(db.close_meeting(id));

    let recap = db.meeting_recap(id).expect("a closed interval has a recap");
    assert_eq!(recap.title, "Weekly sync");
}

#[test]
fn a_meeting_closes_at_the_stop_press_not_after_the_drain() {
    let now = Arc::new(std::sync::atomic::AtomicI64::new(5_000));
    let ticking = Arc::clone(&now);
    let db = Db::open_in_memory(Arc::new(move || {
        ticking.load(std::sync::atomic::Ordering::SeqCst)
    }))
    .unwrap();
    let id = db.open_meeting(Some("Weekly sync"), None, 0.35, "{}").unwrap();
    // Stop at 65 s; the drain finishes 30 s later.
    now.store(95_000, std::sync::atomic::Ordering::SeqCst);
    assert!(db.close_meeting_at(id, 65_000));
    assert_eq!(db.meeting_session(id).unwrap().ended_at, Some(65_000));
    // Never stamped in the future.
    let later = db.open_meeting(Some("Next"), None, 0.35, "{}").unwrap();
    assert!(db.close_meeting_at(later, i64::MAX));
    assert_eq!(db.meeting_session(later).unwrap().ended_at, Some(95_000));
}

#[test]
fn a_meeting_recap_carries_the_note_the_user_typed() {
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db.open_meeting(Some("1:1"), None, 0.35, "{}").unwrap();

    assert!(db.save_meeting_note(id, "- discussed the roadmap"));
    db.close_meeting(id);

    assert_eq!(
        db.meeting_recap(id).unwrap().notes.as_deref(),
        Some("- discussed the roadmap")
    );
}

#[test]
fn meeting_read_failures_are_not_empty_content() -> Result<(), Box<dyn std::error::Error>> {
    let db = Db::open_in_memory(clock(5_000))?;
    let id = db.open_meeting(Some("Saved meeting"), None, 0.4, "{}").ok_or("meeting not created")?;
    assert!(matches!(db.try_meeting_recap(id), Ok(Some(_))));
    assert!(db.try_meeting_transcript(id).is_ok_and(|rows| rows.is_empty()));
    assert!(db.try_meeting_recap_full(id)?.is_none());
    // This in-memory fixture deliberately breaks the schema to exercise read failures.
    db.conn.lock().map_err(|_| "test database lock poisoned")?.execute_batch(
        "PRAGMA foreign_keys=OFF; DROP TABLE session_notes; DROP TABLE transcript_segments; DROP TABLE meeting_recaps;"
    )?;
    assert!(db.try_meeting_recap(id).is_err());
    assert!(db.try_meeting_transcript(id).is_err());
    assert!(db.try_meeting_recap_full(id).is_err());
    Ok(())
}

#[test]
fn stale_template_recap_cannot_overwrite_current_minutes() -> Result<(), String> {
    let db = Db::open_in_memory(clock(5_000)).map_err(|error| format!("open db: {error}"))?;
    let id = db
        .open_meeting(Some("Standup"), None, 0.35, "{}")
        .ok_or_else(|| "open meeting failed".to_string())?;
    if !db.set_session_template(id, Some("standup"))? {
        return Err("meeting template was not stored".to_string());
    }

    assert!(!db.save_meeting_recap_if_template(
        id,
        None,
        "stale general summary",
        "[]",
        "[]",
        None,
        "test-model",
        "[]",
        &[],
    ));
    assert!(db.meeting_recap_full(id).is_none());

    assert!(db.save_meeting_recap_if_template(
        id,
        Some("standup"),
        "current standup summary",
        "[]",
        "[]",
        Some("[]"),
        "test-model",
        "[]",
        &[],
    ));
    let recap = db
        .meeting_recap_full(id)
        .ok_or_else(|| "current recap was not stored".to_string())?;
    assert_eq!(recap.summary, "current standup summary");
    Ok(())
}

#[test]
fn closing_a_meeting_puts_it_on_the_search_spine() {
    // FR-MT-14 at the Db seam: after close, the transcript is reachable through the SAME
    // search everything else uses, attributed to a meeting.
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db
        .open_meeting(Some("Weekly sync"), Some("us.zoom.xos"), 0.35, "{}")
        .unwrap();
    db.append_transcript(
        id,
        1_100,
        shogun_memory::transcript_segments::Speaker::Other,
        "the vendor renewal was settled",
        0.9,
    );

    assert!(
        db.search("vendor renewal", 10).is_empty(),
        "not findable while still open"
    );
    assert!(db.close_meeting(id));

    let hits = db.search("vendor renewal", 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, "meeting");
}

#[test]
fn an_abandoned_meeting_reaches_search_when_swept() {
    // Crash / sleep: the session was never closed by the state machine. The startup sweep
    // closes it — and the transcript it captured must land on the spine then, or these become
    // the only meetings the user can read back but never find.
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db
        .open_meeting(Some("Interrupted sync"), Some("us.zoom.xos"), 0.35, "{}")
        .unwrap();
    db.append_transcript(
        id,
        1_100,
        shogun_memory::transcript_segments::Speaker::Other,
        "we agreed on the migration window",
        0.9,
    );

    assert_eq!(db.close_abandoned_meetings(i64::MAX), 1);
    let hits = db.search("migration window", 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, "meeting");
}

#[test]
fn a_note_flushed_after_close_still_reaches_search() {
    // The blur/debounce path: auto-wrap closed the session an instant before the webview
    // flushed the note. The save must refresh the index, or the note the user most wants
    // kept is the one line of the meeting that can never be found.
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db.open_meeting(Some("1:1"), None, 0.35, "{}").unwrap();
    db.close_meeting(id);

    assert!(db.save_meeting_note(id, "follow up on the budget line"));
    let hits = db.search("budget line", 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, "meeting");
}

#[test]
fn a_recap_for_an_interval_that_never_existed_is_none() {
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    assert!(db.meeting_recap(9_999).is_none());
}

#[test]
fn a_meeting_with_no_note_still_has_a_recap() {
    // FR-MT-19: never an empty panel, even for a meeting where nobody typed anything.
    let db = Db::open_in_memory(clock(5_000)).unwrap();
    let id = db
        .open_meeting(None, Some("us.zoom.xos"), 0.35, "{}")
        .unwrap();
    db.close_meeting(id);

    let recap = db.meeting_recap(id).unwrap();
    assert_eq!(recap.title, crate::meeting::recap::UNTITLED);
    assert_eq!(recap.notes, None);
}

#[test]
fn reply_context_prefers_fetched_gmail_thread_when_screen_matches() {
    use shogun_mcp::sync::IngestItem;
    let db = Db::open_in_memory(clock(1)).unwrap();
    // gmail 同期でスレッドを入れる（thread_key は "gmail:unknown:Q3 pricing" になる）。
    db.ingest_integration(&[IngestItem {
        source: "gmail",
        kind: "email",
        title: "Q3 pricing".to_string(),
        body: "Full thread body about pricing".to_string(),
        ts_ms: 1,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }]);
    // 画面側は capture スレッド（タブ名 "(3) Q3 pricing — Gmail"）。
    let ctx = db.build_reply_context_for_screen(
        "capture:com.google.Chrome:Q3 pricing",
        "(3) Q3 pricing — Gmail",
    );
    assert!(matches!(ctx.payload_source, PayloadSource::Fetched { .. }));
    assert!(
        ctx.turns.iter().any(|t| t.excerpt.contains("pricing")),
        "fetched body used"
    );
}

#[test]
fn reply_context_is_on_screen_only_without_a_gmail_match() {
    let db = Db::open_in_memory(clock(1)).unwrap();
    let ctx =
        db.build_reply_context_for_screen("capture:com.apple.Safari:Nothing", "Nothing — Safari");
    assert_eq!(ctx.payload_source, PayloadSource::OnScreenOnly);
}

#[test]
fn evidence_to_blocks_preserves_provenance_and_counts_tokens() {
    use shogun_fusion::block::{BlockRef, SourceKind};
    use shogun_fusion::budget::HeuristicEstimator;
    let ev = vec![Evidence {
        event_id: 7,
        ts: 100,
        source: "capture".into(),
        title: Some("t".into()),
        excerpt: "a".repeat(40),
        frame_id: None,
    }];
    let est = HeuristicEstimator::default();
    let blocks = evidence_to_blocks(&ev, 0.7, &est);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].id_ref, BlockRef::Event(7));
    assert_eq!(blocks[0].source_kind, SourceKind::Evidence);
    assert!(blocks[0].tokens > 0);
    assert!(
        (blocks[0].score_inputs.confidence - shogun_fusion::trust::EVIDENCE_TRUST_PRIOR).abs()
            < 1e-9
    );
}

#[test]
fn evidence_to_blocks_keeps_calendar_metadata_structured_and_description_as_evidence() {
    use shogun_fusion::block::SourceKind;
    use shogun_fusion::budget::HeuristicEstimator;
    let excerpt = "standup with the platform team";
    let ev = vec![
        Evidence {
            event_id: 1,
            ts: 100,
            source: "capture".into(),
            title: None,
            excerpt: excerpt.into(),
            frame_id: None,
        },
        Evidence {
            event_id: 2,
            ts: 100,
            source: "gcal".into(),
            title: Some("Standup".into()),
            excerpt: excerpt.into(),
            frame_id: None,
        },
        Evidence {
            event_id: 3,
            ts: 100,
            source: "gmail".into(),
            title: None,
            excerpt: excerpt.into(),
            frame_id: None,
        },
    ];
    let blocks = evidence_to_blocks(&ev, 0.7, &HeuristicEstimator::default());
    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks[0].source_kind, SourceKind::Evidence);
    assert_eq!(blocks[1].source_kind, SourceKind::Structured);
    assert!(blocks[1].text.contains("Title: Standup"));
    assert!(blocks[1].text.contains("Timestamp (unix ms): 100"));
    assert!(blocks[1].text.contains("Event ID: 2"));
    assert_eq!(blocks[2].source_kind, SourceKind::Evidence);
    assert_eq!(blocks[2].text, excerpt);
    assert_eq!(blocks[3].source_kind, SourceKind::Evidence);
    assert!(
        (blocks[1].score_inputs.confidence - shogun_fusion::trust::STRUCTURED_TRUST_PRIOR).abs()
            < 1e-9
    );
    for i in [0, 2, 3] {
        assert!(
            (blocks[i].score_inputs.confidence - shogun_fusion::trust::EVIDENCE_TRUST_PRIOR).abs()
                < 1e-9,
            "quoted block {i} must not look like a verified fact"
        );
    }
}

#[test]
fn equal_score_calendar_block_beats_ax_when_budget_fits_one() {
    use shogun_fusion::block::{BlockRef, SourceKind};
    use shogun_fusion::budget::HeuristicEstimator;
    use shogun_fusion::compress::{compress, Candidates, CompressionConfig};
    let excerpt = "a".repeat(400);
    let ev = vec![
        Evidence {
            event_id: 1,
            ts: 100,
            source: "capture".into(),
            title: None,
            excerpt: excerpt.clone(),
            frame_id: None,
        },
        Evidence {
            event_id: 2,
            ts: 100,
            source: "gcal".into(),
            title: Some("Standup".into()),
            excerpt,
            frame_id: None,
        },
    ];
    let blocks = evidence_to_blocks(&ev, 0.7, &HeuristicEstimator::default());
    let w = shogun_fusion::score::ScoreWeights::default();
    let cal = blocks
        .iter()
        .find(|b| b.source_kind == SourceKind::Structured)
        .unwrap();
    let ax = blocks
        .iter()
        .find(|b| matches!(b.id_ref, BlockRef::Event(1)))
        .unwrap();
    assert!(
        shogun_fusion::score::score_block(&cal.score_inputs, &w)
            > shogun_fusion::score::score_block(&ax.score_inputs, &w),
        "trust prior must move calendar metadata above AX, not only the tie-break"
    );
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 110,
        ..Default::default()
    };
    let out = compress(Candidates { blocks }, &cfg);
    assert_eq!(out.blocks.len(), 1);
    assert_eq!(out.blocks[0].id_ref, BlockRef::Event(2));
    assert_eq!(out.blocks[0].source_kind, SourceKind::Structured);
    assert!(out.blocks[0].text.contains("Title: Standup"));
}

#[test]
fn equal_search_score_calendar_ranks_above_ax_and_mail() {
    let hit = |id, source: &str| {
        (
            0.8_f64,
            Evidence {
                event_id: id,
                ts: 100,
                source: source.into(),
                title: None,
                excerpt: "same".into(),
                frame_id: None,
            },
        )
    };
    let mut ranked = [hit(1, "capture"), hit(2, "gcal"), hit(3, "gmail")];
    ranked.sort_by(cmp_ranked_evidence);
    assert_eq!(ranked[0].1.source, "gcal");
}

#[test]
fn higher_search_score_still_beats_calendar_source_rank() {
    let mut ranked = [
        (
            0.9,
            Evidence {
                event_id: 1,
                ts: 100,
                source: "capture".into(),
                title: None,
                excerpt: "ax".into(),
                frame_id: None,
            },
        ),
        (
            0.5,
            Evidence {
                event_id: 2,
                ts: 100,
                source: "gcal".into(),
                title: Some("Standup".into()),
                excerpt: "notes".into(),
                frame_id: None,
            },
        ),
    ];
    ranked.sort_by(cmp_ranked_evidence);
    assert_eq!(ranked[0].1.source, "capture");
}

#[test]
fn compressed_calendar_metadata_is_a_fact_and_description_stays_evidence() {
    use shogun_fusion::compress::CompressionConfig;
    use shogun_mcp::sync::IngestItem;

    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.ingest_integration(&[IngestItem {
        source: "gcal",
        kind: "calendar_event",
        title: "Roadmap review".into(),
        body: "Discuss the launch plan and open risks.".into(),
        ts_ms: 500,
        external_id: String::new(),
        calendar: None,
        mail: None,
    }]);
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 1_000,
        ..Default::default()
    };

    let (pack, _stats, fell_back) =
        db.assemble_context_compressed_within("launch plan", 6, 600, &[], &cfg, CompressBudget::Unbounded);

    assert!(!fell_back);
    assert!(
        pack.facts
            .iter()
            .any(|fact| fact.contains("Title: Roadmap review")),
        "calendar metadata must be trusted: {:?}",
        pack.facts
    );
    assert!(
        pack.evidence
            .iter()
            .any(|evidence| evidence.excerpt.contains("open risks")),
        "calendar description must remain available as evidence: {:?}",
        pack.evidence
    );
}

#[test]
fn record_compression_metric_persists_hash_not_text() {
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    db.record_compression_metric("report", "compressed", 100, 30, 5, 20);
    let conn = db.conn.lock().unwrap();
    let (qh, path): (String, String) = conn
        .query_row(
            "SELECT query_hash, path FROM compression_metrics LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_ne!(qh, "report"); // 本文は保存しない
    assert_eq!(path, "compressed");
}

#[test]
fn harness_counters_unmeasured_until_assemble_or_loop() {
    use crate::metrics::{
        render_snapshots_json_with_lessons_and_harness, SloRegistry, ToolLoopHarness,
    };
    let db = Db::open_in_memory(clock(1_000)).unwrap();
    assert!(db.harness_counters().is_none());
    let json = render_snapshots_json_with_lessons_and_harness(
        &SloRegistry::new().snapshot_all(),
        db.lesson_counters(),
        db.harness_counters(),
    );
    assert!(json.contains(r#""harness":{"measured":false}"#), "{json}");
    assert!(!json.contains("pre_tokens"), "{json}");

    db.record_tool_loop(ToolLoopHarness {
        uses: 2,
        refusals: 1,
        proposes: 0,
        timeouts: 1,
        median_ms: Some(210),
        slowest_ms: Some(650),
    });
    let snap = db
        .harness_counters()
        .expect("loop should mark harness measured");
    assert!(snap.compression.is_none());
    let loop_snap = snap.tool_loop.expect("tool_loop recorded");
    assert_eq!(loop_snap.uses, 2);
    assert_eq!(loop_snap.refusals, 1);
    assert_eq!(loop_snap.timeouts, 1);
    let json = render_snapshots_json_with_lessons_and_harness(
        &SloRegistry::new().snapshot_all(),
        None,
        db.harness_counters(),
    );
    assert!(json.contains(r#""uses":2"#), "{json}");
    assert!(
        json.contains(r#""compression":{"measured":false}"#),
        "{json}"
    );
    assert!(!json.contains("prompt"), "{json}");

    // clones share the live snapshot
    let clone = db.clone();
    clone.record_tool_loop(ToolLoopHarness {
        uses: 8,
        refusals: 0,
        proposes: 1,
        timeouts: 0,
        median_ms: Some(95),
        slowest_ms: Some(300),
    });
    let again = db
        .harness_counters()
        .and_then(|h| h.tool_loop)
        .expect("shared");
    assert_eq!(again.uses, 8);
    assert_eq!(again.proposes, 1);
}

#[test]
fn assemble_records_compression_harness_without_query_text() {
    use crate::metrics::render_snapshots_json_with_lessons_and_harness;
    use shogun_fusion::compress::CompressionConfig;
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    for i in 0..4 {
        db.capture(&ev(
            "vendor pricing settled at 12k for the renewal report",
            &format!("h{i}"),
            100 + i,
        ))
        .unwrap();
    }
    const SECRET: &str = "SECRET_QUERY_XYZ_harness";
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 80,
        ..Default::default()
    };
    let (_pack, stats, fell_back) = db.assemble_context_compressed_within(SECRET, 6, 600, &[], &cfg, CompressBudget::Unbounded);
    assert!(!fell_back);
    let snap = db
        .harness_counters()
        .expect("assemble should mark compression measured");
    let c = snap.compression.expect("compression snapshot");
    assert!(c.enabled);
    assert_eq!(c.pre_tokens, stats.pre_tokens as u64);
    assert_eq!(c.post_tokens, stats.post_tokens as u64);
    assert_eq!(c.dropped, stats.dropped as u64);
    let json = render_snapshots_json_with_lessons_and_harness(
        &crate::metrics::SloRegistry::new().snapshot_all(),
        None,
        db.harness_counters(),
    );
    assert!(
        !json.contains(SECRET),
        "query text must stay off the metrics face: {json}"
    );
    assert!(json.contains("pre_tokens"), "{json}");
}

/// The budget guard itself, without a stopwatch: an exhausted budget returns the raw pack
/// (same facts and evidence as `assemble_context`, stats untouched, `fell_back == true`), and an
/// unbounded one never does, so neither branch depends on how fast the machine happens to be.
#[test]
fn compression_budget_picks_the_branch_deterministically() {
    use shogun_fusion::compress::{CompressionConfig, CompressionStats};
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    for i in 0..6 {
        db.capture(&ev(
            "vendor pricing settled at 12k for the renewal report",
            &format!("h{i}"),
            100 + i,
        ))
        .unwrap();
    }
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 20,
        ..Default::default()
    };
    let raw = db.assemble_context("pricing report", 6, 600);
    assert!(!raw.evidence.is_empty(), "the seed must retrieve something");

    let (fallback, fb_stats, fell_back) = db.assemble_context_compressed_within(
        "pricing report",
        6,
        600,
        &[],
        &cfg,
        CompressBudget::Exhausted,
    );
    assert!(fell_back, "an exhausted budget must fall back to raw");
    assert_eq!(fb_stats, CompressionStats::default(), "the raw path compresses nothing");
    let ids = |pack: &ContextPack| pack.evidence.iter().map(|e| e.event_id).collect::<Vec<_>>();
    assert_eq!(ids(&fallback), ids(&raw), "fallback carries the raw evidence untouched");
    assert_eq!(fallback.facts, raw.facts);
    let harness = db.harness_counters().expect("fallback is measured");
    assert!(!harness.compression.expect("compression snapshot").enabled);

    let (compressed, stats, fell_back) = db.assemble_context_compressed_within(
        "pricing report",
        6,
        600,
        &[],
        &cfg,
        CompressBudget::Unbounded,
    );
    assert!(!fell_back, "an unbounded budget never falls back");
    assert!(stats.post_tokens <= 20, "post={}", stats.post_tokens);
    assert!(compressed.evidence.len() <= raw.evidence.len());
    let harness = db.harness_counters().expect("compression is measured");
    assert!(harness.compression.expect("compression snapshot").enabled);

    // Production keeps the 50 ms wall-clock rule.
    assert_eq!(CompressBudget::DEFAULT, CompressBudget::WallClock(50));
}

#[test]
fn compressed_context_bounds_tokens_without_falling_back() {
    // 複数 evidence をヒットさせる小さな入力を seed する。
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    for i in 0..6 {
        db.capture(&ev(
            "vendor pricing settled at 12k for the renewal report",
            &format!("h{i}"),
            100 + i,
        ))
        .unwrap();
    }
    use shogun_fusion::compress::CompressionConfig;
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 20,
        ..Default::default()
    };
    let (pack_c, stats, fell_back) =
        db.assemble_context_compressed_within("pricing report", 6, 600, &[], &cfg, CompressBudget::Unbounded);
    assert!(!fell_back, "Unbounded では必ず compress 経路を通る");
    assert!(stats.post_tokens <= 20, "post={}", stats.post_tokens);
    if stats.pre_tokens > 20 {
        assert!(
            stats.post_tokens < stats.pre_tokens,
            "pre={} post={}",
            stats.pre_tokens,
            stats.post_tokens
        );
    }
    assert!(
        !pack_c.facts.is_empty() || !pack_c.evidence.is_empty(),
        "全落ちしない"
    );
}

#[test]
fn compressed_evidence_preserves_source_and_title() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    for i in 0..3 {
        db.capture(&ev(
            "renewal report pricing detail",
            &format!("h{i}"),
            100 + i,
        ))
        .unwrap();
    }
    let raw = db.assemble_context("renewal", 6, 600);
    assert!(
        raw.evidence.iter().any(|e| !e.source.is_empty()),
        "raw has source"
    );
    let cfg = shogun_fusion::compress::CompressionConfig {
        enabled: true,
        budget_tokens: 100_000,
        ..Default::default()
    };
    let (pack, _stats, _fell) = db.assemble_context_compressed_within("renewal", 6, 600, &[], &cfg, CompressBudget::Unbounded);
    // 予算十分＝全採用。各 evidence の source/title/ts が raw と一致（0/空でない）。
    for e in &pack.evidence {
        let orig = raw
            .evidence
            .iter()
            .find(|o| o.event_id == e.event_id)
            .unwrap();
        assert_eq!(
            e.source, orig.source,
            "source mismatch for event_id={}",
            e.event_id
        );
        assert_eq!(
            e.title, orig.title,
            "title mismatch for event_id={}",
            e.event_id
        );
        assert_eq!(e.ts, orig.ts, "ts mismatch for event_id={}", e.event_id);
    }
}

/// 内容は `inline_memory`（文字列版）と一致する。
#[test]
fn inline_memory_with_refs_carries_real_ids_and_tables() {
    use shogun_fusion::block::StateTable;
    use shogun_memory::state::{
        CommitmentDirection, CommitmentStatus, NewCommitment, NewOpenLoop, OpenLoopKind, Provenance,
    };

    let db = Db::open_in_memory(clock(1)).unwrap();
    let e = db.capture(&ev("evidence for state", "h1", 1)).unwrap().0;
    let prov = [Provenance::new(e)];

    // 高 confidence の commitment と open_loop を 1 件ずつ挿入。
    let cid = db
        .insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the test report",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.9,
                now: 1,
            },
            &prov,
        )
        .expect("commitment insert");

    let lid = db
        .insert_open_loop(
            &NewOpenLoop {
                kind: OpenLoopKind::WaitingOnThem,
                description: "waiting on test approval",
                counterparty_id: None,
                project_id: None,
                opened_at: 1,
                confidence: 0.9,
                now: 1,
            },
            &prov,
        )
        .expect("open loop insert");

    let refs = db.inline_memory_with_refs(8);
    assert!(
        refs.iter()
            .any(|(_, t, id)| *t == StateTable::Commitments && *id == cid),
        "commitment の実 id が含まれるべき: {refs:?}"
    );
    assert!(
        refs.iter()
            .any(|(_, t, id)| *t == StateTable::OpenLoops && *id == lid),
        "open_loop の実 id が含まれるべき: {refs:?}"
    );

    // inline_memory（文字列版）と本文が一致（委譲による不変）。
    let strs = db.inline_memory(8);
    let refs_strs: Vec<String> = refs.iter().map(|(s, _, _)| s.clone()).collect();
    assert_eq!(
        strs, refs_strs,
        "inline_memory は inline_memory_with_refs に完全委譲するべき"
    );
}

#[test]
fn thread_summary_substitutes_for_raw_turns_under_budget() {
    use shogun_fusion::compress::CompressionConfig;

    let db = Db::open_in_memory(clock(10_000)).unwrap();
    // 同一スレッド（同 source/window_title）に長めの生ターンを複数投入。
    // 各ターンの生テキストには要約には無い語 "detail" を含めておく（差し替えの反証用）。
    for i in 0..6 {
        db.capture(&ev(
            "vendor renewal pricing discussion detail line",
            &format!("h{i}"),
            100 + i,
        ))
        .unwrap();
    }
    // そのスレッドに短い要約を付与（raw ターンより短くトークン効率が高い）。
    let tk = db
        .active_threads_between(0, 10_000)
        .first()
        .map(|t| t.thread_key.clone())
        .expect("capture が thread を作るので 1 本はあるはず");
    db.set_thread_summary(&tk, "Renewal priced at 12k; awaiting sign-off.");

    // 予算逼迫（12 トークン）: 短い要約(~10tok, relevance 0.9)が raw を押しのけて残る想定。
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 12,
        ..Default::default()
    };
    let (pack, stats, fell_back) =
        db.assemble_context_compressed_within("renewal pricing", 6, 600, std::slice::from_ref(&tk), &cfg, CompressBudget::Unbounded);

    assert!(!fell_back, "Unbounded では必ず compress 経路を通る");
    assert!(
        stats.post_tokens <= 12,
        "予算内に収まる: post={}",
        stats.post_tokens
    );

    // 要約テキストが採用され（ThreadSummary は fact 側に振り分け）、その語が生き残る。
    let joined = format!(
        "{} {}",
        pack.facts.join(" "),
        pack.evidence
            .iter()
            .map(|e| e.excerpt.clone())
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(
        joined.contains("sign-off") || joined.contains("12k"),
        "要約が生き残るべき: {joined}"
    );
    // 差し替えの証明: 予算逼迫下で要約が採用された以上、要約に無い生ターンの語
    // "detail" は落ちている（要約が raw ターンを押しのけた）。
    assert!(
        !joined.contains("detail"),
        "要約が raw ターンを押しのけるべき（生ターンの語が残っている）: {joined}"
    );
}

#[test]
fn session_summary_of_retrieved_evidence_reaches_the_compressed_pack() {
    use shogun_fusion::compress::CompressionConfig;

    let db = Db::open_in_memory(clock(10_000)).unwrap();
    // A meeting session, with a captured event attached to it that matches the query — so the
    // event is retrieved as evidence and its owning session's summary is pulled in.
    let sid = db
        .open_meeting(Some("Vendor sync"), Some("us.zoom.xos"), 0.6, "{}")
        .unwrap();
    let (ev_id, _) = db
        .capture(&ev(
            "vendor renewal pricing discussion detail line",
            "h0",
            100,
        ))
        .unwrap();
    assert!(
        db.attach_event_to_meeting(sid, ev_id),
        "the event must attach to the session"
    );
    // A short session summary, token-efficient relative to the raw turn — carries a word the raw
    // excerpt does not ("sign-off") so its arrival is unambiguous.
    db.set_session_summary(sid, "Renewal priced at 12k; awaiting sign-off.");

    // 十分予算: SessionSummary 由来テキストが pack に出る。
    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 100_000,
        ..Default::default()
    };
    let (pack, _stats, fell_back) =
        db.assemble_context_compressed_within("renewal pricing", 6, 600, &[], &cfg, CompressBudget::Unbounded);
    assert!(!fell_back);
    let joined = format!(
        "{} {}",
        pack.facts.join(" "),
        pack.evidence
            .iter()
            .map(|e| e.excerpt.clone())
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(
        joined.contains("sign-off"),
        "session の要約が pack に届くべき: {joined}"
    );

    // 予算逼迫: 短い要約(relevance 0.85)が raw ターンを押しのけて残る。
    let tight = CompressionConfig {
        enabled: true,
        budget_tokens: 12,
        ..Default::default()
    };
    let (pack_t, stats_t, fell_t) =
        db.assemble_context_compressed_within("renewal pricing", 6, 600, &[], &tight, CompressBudget::Unbounded);
    assert!(!fell_t);
    assert!(
        stats_t.post_tokens <= 12,
        "予算内に収まる: post={}",
        stats_t.post_tokens
    );
    let joined_t = format!(
        "{} {}",
        pack_t.facts.join(" "),
        pack_t
            .evidence
            .iter()
            .map(|e| e.excerpt.clone())
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(
        joined_t.contains("sign-off") || joined_t.contains("12k"),
        "予算逼迫下でも session 要約が生き残るべき: {joined_t}"
    );
}

#[test]
fn session_summary_is_deduped_against_an_already_consumed_thread_summary() {
    use shogun_fusion::compress::CompressionConfig;

    let db = Db::open_in_memory(clock(10_000)).unwrap();

    // A meeting session whose event matches the query, so its owning session is on the consume
    // path. Give the session a distinctive summary word so its presence is unambiguous.
    let sid = db
        .open_meeting(Some("Vendor sync"), Some("us.zoom.xos"), 0.6, "{}")
        .unwrap();
    let (ev_id, _) = db
        .capture(&ev("vendor renewal pricing discussion line", "h0", 100))
        .unwrap();
    assert!(db.attach_event_to_meeting(sid, ev_id));
    db.set_session_summary(sid, "Session recap sessionword only.");

    // The capture created a thread; use its real thread_key so set_thread_summary lands (the
    // threads UPDATE needs an existing row). The session carries that same thread_key — when it
    // is passed as a consumed ThreadSummary, the session's summary is redundant and skipped.
    let tk = db
        .active_threads_between(0, 10_000)
        .first()
        .map(|t| t.thread_key.clone())
        .expect("capture が thread を作る");
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET thread_key = ?1 WHERE id = ?2",
            rusqlite::params![tk, sid],
        )
        .unwrap();
    }
    // Give that thread a summary so it becomes a ThreadSummary candidate carrying its own word.
    db.set_thread_summary(&tk, "Thread recap threadword only.");

    let cfg = CompressionConfig {
        enabled: true,
        budget_tokens: 100_000,
        ..Default::default()
    };

    // thread_key consumed → the session's own summary is NOT re-added (dedup).
    let (pack, _s, fell) =
        db.assemble_context_compressed_within("renewal pricing", 6, 600, std::slice::from_ref(&tk), &cfg, CompressBudget::Unbounded);
    assert!(!fell);
    let joined = pack.facts.join(" ");
    assert!(
        joined.contains("threadword"),
        "consumed thread summary is present: {joined}"
    );
    assert!(
        !joined.contains("sessionword"),
        "session whose thread_key was consumed must be deduped out: {joined}"
    );

    // A DIFFERENT thread_key is passed (not the session's) → the session summary IS added.
    let (pack2, _s2, fell2) = db.assemble_context_compressed_within(
        "renewal pricing",
        6,
        600,
        &["mail:some-other-thread".to_string()],
        &cfg,
        CompressBudget::Unbounded,
    );
    assert!(!fell2);
    let joined2 = pack2.facts.join(" ");
    assert!(
        joined2.contains("sessionword"),
        "an unconsumed session's summary is still added: {joined2}"
    );
}

#[test]
fn edited_state_survives_persisted_brief_search_without_source_proof(
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use shogun_memory::search_documents::{self, EvidenceRef, SourceRef};
    use shogun_memory::state::{
        CommitmentDirection, CommitmentStatus, EvidenceKind, NewCommitment, NewOpenLoop,
        OpenLoopKind,
    };
    let now = 100 * 86_400_000;
    let db = Db::open_in_memory(clock(now))?;
    let (event_id, _) = db
        .capture(&ev(
            "Earlier captured context",
            "edited-brief-source",
            now - 10,
        ))
        .ok_or("capture failed")?;
    let commitment = db
        .insert_commitment(
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "Earlier commitment",
                due_at: Some(now - 1),
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.6,
                now: now - 5,
            },
            &[Provenance::new(event_id)],
        )
        .ok_or("commitment insert failed")?;
    let open_loop = {
        let mut conn = db.conn.lock().map_err(|_| "test mutex")?;
        state::insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::FollowUp,
                description: "Earlier open loop",
                counterparty_id: None,
                project_id: None,
                opened_at: now - 5,
                confidence: 0.6,
                now: now - 5,
            },
            &[Provenance::new(event_id)],
        )?
    };
    db.state_update(&serde_json::json!({"table":"commitments","id":commitment,"description":"Zircon corrected commitment"}).to_string())?;
    db.state_update(&serde_json::json!({"table":"open_loops","id":open_loop,"description":"Zircon corrected loop"}).to_string())?;
    DbDreamRunner::new(&db, &LocalRuleClassifier, &LocalExtractiveSummarizer, now)
        .local_brief(now - 86_400_000, now)?;
    let date = local_date_string(now);
    let saved = db.brief_for(&date).ok_or("missing brief")?;
    let payload: shogun_memory::briefs::BriefPayload = serde_json::from_str(&saved.payload)?;
    assert_eq!(payload.commitments_due.len(), 1);
    assert_eq!(payload.open_loops.len(), 1);
    for line in payload.commitments_due.iter().chain(&payload.open_loops) {
        assert_eq!(line.provenance_event_id, None);
        assert_eq!(line.evidence_kind, Some(EvidenceKind::EditedContext));
    }
    assert!(
        !saved.payload.contains("provenance_event_id"),
        "edited lines must not manufacture event zero"
    );
    let conn = db.conn.lock().map_err(|_| "test mutex")?;
    let candidates = search_documents::search_documents_fts(&conn, "Zircon", 20)?;
    let documents = search_documents::hydrate_documents(
        &conn,
        &candidates.iter().map(|c| c.id).collect::<Vec<_>>(),
    )?;
    let brief = documents
        .iter()
        .find(|document| document.source == SourceRef::Brief(date.clone()))
        .ok_or("all-edited Brief missing from search")?;
    assert!(brief.body.contains("Zircon corrected commitment"));
    assert!(brief.body.contains("Zircon corrected loop"));
    assert!(brief
        .evidence
        .iter()
        .all(|evidence| matches!(evidence, EvidenceRef::Brief { .. })));
    Ok(())
}

#[test]
fn early_local_brief_rebuilds_leave_notice_for_nightly_delivery() -> Result<(), Box<dyn std::error::Error>> {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use crate::dreamcycle::run::DreamJobRunner;
    let now = 100 * 86_400_000;
    let db = Db::open_in_memory(clock(now))?;
    db.conn.lock().map_err(|_| "test mutex")?.execute(
        "INSERT INTO work_patterns (kind,key,scope,scope_ref,count_7d,count_30d,trend,confidence,active,created_at,updated_at,last_seen_at) VALUES ('activity','translate','global','',12,20,1.0,0.7,1,?1,?1,?1)", [now],
    )?;
    let runner = DbDreamRunner::new(&db, &LocalRuleClassifier, &LocalExtractiveSummarizer, now);
    runner.local_brief(now - 86_400_000, now)?;
    runner.local_brief(now - 86_400_000, now)?;
    assert!(db.try_pending_pattern_notice()?.is_some());
    runner.run(JobKind::MorningBrief, now - 86_400_000, now)?;
    assert!(db.try_pending_pattern_notice()?.is_none());
    let date = crate::daemon::local_date_string(now);
    let saved = db.brief_for(&date).ok_or("missing brief")?;
    let original: shogun_memory::briefs::BriefPayload = serde_json::from_str(&saved.payload)?;
    assert!(original.pattern_line.as_deref().is_some_and(|line| line.starts_with("⚔ Noticed:")));
    runner.local_brief(now - 86_400_000, now)?;
    runner.run(JobKind::MorningBrief, now - 86_400_000, now)?;
    let saved = db.brief_for(&date).ok_or("missing rebuilt brief")?;
    let rebuilt: shogun_memory::briefs::BriefPayload = serde_json::from_str(&saved.payload)?;
    assert_eq!(rebuilt.pattern_line, original.pattern_line);
    Ok(())
}

#[test]
fn morning_brief_save_failure_keeps_pattern_notice_pending_until_retry() {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use crate::dreamcycle::plan::JobKind;
    use crate::dreamcycle::run::DreamJobRunner;

    let now = 100 * 24 * 60 * 60 * 1000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO work_patterns
                 (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence,
                  active, created_at, updated_at, last_seen_at)
             VALUES ('activity', 'translate', 'global', '', 12, 20, 1.0, 0.7,
                     1, ?1, ?1, ?1)",
            [now],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_test_brief_save
             BEFORE INSERT ON briefs
             BEGIN SELECT RAISE(FAIL, 'injected brief failure'); END;",
        )
        .unwrap();
    }
    let classifier = LocalRuleClassifier;
    let summarizer = LocalExtractiveSummarizer;
    let runner = DbDreamRunner::new(&db, &classifier, &summarizer, now);

    assert!(runner
        .run(JobKind::MorningBrief, now - 86_400_000, now)
        .is_err());
    assert!(db.try_pending_pattern_notice().unwrap().is_some());

    db.conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_test_brief_save")
        .unwrap();
    runner
        .run(JobKind::MorningBrief, now - 86_400_000, now)
        .unwrap();

    assert!(db.try_pending_pattern_notice().unwrap().is_none());
    assert!(db.recent_pattern_notice(1).is_some());
    assert!(db
        .brief_for(&crate::daemon::local_date_string(now))
        .is_some());
}

#[test]
fn dreamcycle_morning_brief_done_ledger_failure_reuses_effect_and_notice_once() {
    use crate::dreamcycle::jobs::{DbDreamRunner, LocalExtractiveSummarizer, LocalRuleClassifier};
    use crate::dreamcycle::plan::{CycleKind, JobKind, JobState, FULL_SEQUENCE};
    use crate::dreamcycle::run::run_cycle;

    let now = 100 * 24 * 60 * 60 * 1000;
    let from = now - 86_400_000;
    let db = Db::open_in_memory(clock(now)).unwrap();
    db.try_freeze_full_cycle("brief-ledger-night", from, now)
        .unwrap();
    let lease = db
        .try_claim_dream_cycle_run("brief-ledger-night", "brief-ledger-owner".to_string())
        .unwrap()
        .unwrap();
    for kind in FULL_SEQUENCE
        .iter()
        .copied()
        .take_while(|kind| *kind != JobKind::MorningBrief)
    {
        db.try_record_job("brief-ledger-night", kind, JobState::Done, from, now)
            .unwrap();
    }
    let pattern_id = {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO work_patterns
                 (kind, key, scope, scope_ref, count_7d, count_30d, trend, confidence,
                  active, created_at, updated_at, last_seen_at)
             VALUES ('activity', 'translate', 'global', '', 12, 20, 1.0, 0.7,
                     1, ?1, ?1, ?1)",
            [now],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute_batch(
            "CREATE TRIGGER fail_test_morning_brief_done
             BEFORE UPDATE OF state ON job_runs
             WHEN new.kind = 'morning_brief' AND new.state = 'done'
             BEGIN SELECT RAISE(FAIL, 'injected morning brief done failure'); END;",
        )
        .unwrap();
        id
    };
    let classifier = LocalRuleClassifier;
    let summarizer = LocalExtractiveSummarizer;
    let runner = DbDreamRunner::new(&db, &classifier, &summarizer, now)
        .with_frozen_cycle("brief-ledger-night");

    let first = run_cycle(
        &db,
        &runner,
        "brief-ledger-night",
        CycleKind::Full,
        from,
        now,
    );
    assert!(matches!(first.failed, Some((JobKind::MorningBrief, _))));
    assert!(db
        .try_dream_brief_effect_exists("brief-ledger-night")
        .unwrap());
    let date = crate::daemon::local_date_string(now);
    let first_payload = db.brief_for(&date).unwrap().payload;
    assert!(first_payload.contains("Noticed"));
    let preserved_notice_at = now + 123;
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "UPDATE work_patterns SET noticed_at = ?2 WHERE id = ?1",
            rusqlite::params![pattern_id, preserved_notice_at],
        )
        .unwrap();
        conn.execute_batch("DROP TRIGGER fail_test_morning_brief_done")
            .unwrap();
    }

    let retry = run_cycle(
        &db,
        &runner,
        "brief-ledger-night",
        CycleKind::Full,
        from,
        now,
    );

    assert!(retry.is_complete());
    assert_eq!(db.brief_for(&date).unwrap().payload, first_payload);
    let notice_after_retry: i64 = db
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT noticed_at FROM work_patterns WHERE id = ?1",
            [pattern_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notice_after_retry, preserved_notice_at);
    db.try_mark_dream_cycle_complete("brief-ledger-night", &lease)
        .unwrap();
    assert!(!db
        .try_dream_brief_effect_exists("brief-ledger-night")
        .unwrap());
}

// mail triage (issue #319)

/// A synced Gmail thread with metadata is keyed `gmail:<threadId>`, gets a Stage L verdict, a
/// `needs_reply` open loop with provenance, and one `MailTriaged` bus event carrying counts only.
#[test]
fn a_gmail_sync_with_metadata_is_triaged_and_publishes_counts() {
    use crate::bus::{Bus, BusEvent};
    use shogun_mcp::sync::{IngestItem, MailMessageMeta, MailMeta};
    use shogun_memory::mail_triage::{Attention, Bucket, BucketLock};
    let bus = Bus::new(8);
    let mut sub = bus.subscribe();
    let db = Db::open_in_memory(clock(1_000_000)).unwrap().with_bus(bus);
    db.add_mail_identity("me@select.co.jp");
    let item = IngestItem {
        source: "gmail",
        kind: "email",
        title: "Contract draft".into(),
        body: "Could you send the signed draft by Friday?".into(),
        ts_ms: 900_000,
        external_id: "t-42".into(),
        calendar: None,
        mail: Some(MailMeta {
            thread_id: "t-42".into(),
            messages: vec![MailMessageMeta {
                message_id: "m1".into(),
                from_addr: "alice@select.co.jp".into(),
                from_name: Some("Alice".into()),
                to_addrs: vec!["me@select.co.jp".into()],
                gmail_label_ids: vec!["INBOX".into()],
                internal_date_ms: 900_000,
                ..Default::default()
            }],
        }),
    };
    let summary = db.ingest_integration(std::slice::from_ref(&item));
    assert_eq!(summary.newly_inserted, 1);

    let row = db.mail_triage_get("gmail:t-42").expect("triage row");
    assert_eq!(row.label_key, "team");
    assert_eq!(row.bucket, Bucket::Important);
    assert_eq!(row.bucket_lock, Some(BucketLock::SameDomain));
    assert_eq!(row.attention, Attention::NeedsReply);
    assert_eq!(row.subject, "Contract draft");
    assert_eq!(row.thread_id, "t-42");
    assert!(row.open_loop_id.is_some(), "needs_reply ≥ 0.5 lands in open_loops");
    assert_eq!(db.mail_needs_reply_count(), 1);

    // Thread key derives from the Gmail thread id, not the subject.
    let key: String = db
        .read_conn("t", |c| {
            c.query_row("SELECT thread_key FROM event_log WHERE source = 'gmail'", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap();
    assert_eq!(key, "gmail:t-42");

    // Bus: IntegrationSynced first, then exactly one MailTriaged with counts.
    let first = sub.try_recv().expect("IntegrationSynced");
    assert!(matches!(*first, BusEvent::IntegrationSynced { source: "gmail", count: 1 }));
    let second = sub.try_recv().expect("MailTriaged");
    assert!(matches!(*second, BusEvent::MailTriaged { new: 1, needs_reply: 1 }), "{second:?}");
    assert!(sub.try_recv().is_none());

    // The user marks it handled → the counter drops, and a relabel is remembered as user stage.
    let updated = db
        .mail_triage_set(
            "gmail:t-42",
            &shogun_memory::mail_triage::UserChange { label_key: None, bucket: None, handled: Some(true) },
            shogun_memory::mail_triage::FeedbackOrigin::Ui,
        )
        .unwrap();
    assert!(updated.handled_at_ms.is_some());
    assert_eq!(db.mail_needs_reply_count(), 0);
    let relabelled = db
        .mail_triage_set(
            "gmail:t-42",
            &shogun_memory::mail_triage::UserChange { label_key: Some("people".into()), bucket: None, handled: None },
            shogun_memory::mail_triage::FeedbackOrigin::Api,
        )
        .unwrap();
    assert_eq!(relabelled.stage, shogun_memory::mail_triage::Stage::User);
    assert_eq!(relabelled.label_key, "people");
    // The mirrored feedback row carries label keys only.
    let (kind, before, after): (String, String, String) = db
        .read_conn("t", |c| {
            c.query_row("SELECT kind, before_text, after_text FROM feedback_events WHERE action_kind = 'mail_relabel'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
        })
        .unwrap();
    assert_eq!((kind.as_str(), before.as_str(), after.as_str()), ("state_resolve", "team", "people"));
}

#[test]
fn sync_cursor_persists_through_the_ingest_sink() {
    use shogun_integrations::IngestSink;
    use shogun_mcp::scope::Service;
    use shogun_mcp::sync::SyncCursor;
    let db = Db::open_in_memory(clock(5)).unwrap();
    assert!(IngestSink::cursor(&db, Service::Gmail).is_none());
    IngestSink::store_cursor(&db, Service::Gmail, &SyncCursor { watermark_ms: 77, page_token: Some("p".into()) });
    let c = IngestSink::cursor(&db, Service::Gmail).unwrap();
    assert_eq!((c.watermark_ms, c.page_token.as_deref()), (77, Some("p")));
}

/// FR-MT-06 / issue #263, end to end: a synced calendar event becomes an occurrence, the meeting
/// that starts during it is linked to it, its invitees lead the speaker candidates, and a name
/// picked from them is recorded as vouched for by the calendar.
#[test]
fn a_synced_calendar_event_links_the_meeting_and_names_its_speakers() {
    use shogun_mcp::sync::{CalendarAttendee, CalendarMeta, IngestItem};
    const HOUR: i64 = 3_600_000;
    let start = 20_000 * 24 * HOUR + 10 * HOUR;
    let db = Db::open_in_memory(clock(start + 5 * 60_000)).unwrap();
    let attendee = |email: &str, name: Option<&str>, is_self: bool| CalendarAttendee {
        email: Some(email.into()),
        name: name.map(str::to_string),
        is_self,
        response: None,
    };
    let summary = db.ingest_integration(&[IngestItem {
        source: "gcal",
        kind: "calendar_event",
        title: "Design review".into(),
        body: "Design review".into(),
        ts_ms: start - HOUR,
        external_id: "ev-1".into(),
        mail: None,
        calendar: Some(CalendarMeta {
            start_ms: start,
            end_ms: start + HOUR,
            all_day: false,
            location: Some("https://meet.google.com/abc-defg-hij".into()),
            attendees: vec![
                attendee("me@example.com", Some("Me"), true),
                attendee("tanaka@example.com", Some("Hanako Tanaka"), false),
                attendee("room@resource.example.com", None, false),
            ],
            cancelled: false,
        }),
    }]);
    assert_eq!(summary.processed, 1);

    let sid = db.open_meeting(Some("Meet - abc-defg-hij"), None, 0.8, "{}").unwrap();
    let linked: (Option<i64>, Option<String>) = db
        .with_conn("test", |c| {
            c.query_row(
                "SELECT calendar_occurrence_id, title FROM sessions WHERE id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert!(linked.0.is_some(), "the meeting is tied to the event running now");
    assert_eq!(linked.1.as_deref(), Some("Design review"), "and takes the event's name");

    let candidates = db.meeting_speaker_candidates(sid, 5);
    assert_eq!(candidates.first().map(String::as_str), Some("Hanako Tanaka"));
    assert!(!candidates.iter().any(|c| c == "Me"), "the owner is never 'someone else'");

    assert!(db.assign_meeting_speaker(sid, "other:0", "Hanako Tanaka", "user").unwrap());
    assert!(db.assign_meeting_speaker(sid, "other:1", "Somebody Else", "user").unwrap());
    let sources: Vec<(String, String)> = db
        .meeting_speakers(sid)
        .into_iter()
        .map(|s| (s.display_name, s.source))
        .collect();
    assert!(sources.contains(&("Hanako Tanaka".into(), "calendar".into())), "{sources:?}");
    assert!(sources.contains(&("Somebody Else".into(), "user".into())), "{sources:?}");

    // A meeting with no event running is a drop-in: unlinked, window title kept.
    let later = Db::open_in_memory(clock(start + 3 * HOUR)).unwrap();
    let drop_in = later.open_meeting(Some("Zoom Meeting"), None, 0.8, "{}").unwrap();
    assert!(later.meeting_speaker_candidates(drop_in, 5).iter().all(|c| c != "Hanako Tanaka"));
}

/// FR-MT-04 signal ① and FR-MT-11's event-end condition read the same occurrences the sync wrote.
#[test]
fn detection_sees_the_running_event_and_the_linked_meeting_knows_when_it_ends() {
    use shogun_mcp::sync::{CalendarMeta, IngestItem};
    const HOUR: i64 = 3_600_000;
    let start = 20_000 * 24 * HOUR + 14 * HOUR;
    let item = |id: &str, start_ms: i64, cancelled: bool| IngestItem {
        source: "gcal",
        kind: "calendar_event",
        title: "1:1".into(),
        body: format!("1:1 {id}"),
        ts_ms: start - HOUR,
        external_id: id.into(),
        mail: None,
        calendar: Some(CalendarMeta {
            start_ms,
            end_ms: start_ms + HOUR / 2,
            cancelled,
            ..CalendarMeta::default()
        }),
    };

    let during = Db::open_in_memory(clock(start + 60_000)).unwrap();
    during.ingest_integration(&[item("evt-1on1", start, false)]);
    assert_eq!(
        during.calendar_occurrence_now(),
        Some(CurrentOccurrence { id: 1, external_id: "evt-1on1".into(), ends_at: start + HOUR / 2 })
    );
    let sid = during.open_meeting(None, None, 0.8, "{}").unwrap();
    assert_eq!(during.meeting_occurrence_end(sid), Some(start + HOUR / 2));

    // Cancelled after the meeting opened: no end condition from a meeting that is not happening.
    during.ingest_integration(&[item("evt-1on1", start, true)]);
    assert_eq!(during.meeting_occurrence_end(sid), None);
    assert_eq!(during.calendar_occurrence_now(), None);

    // Nothing scheduled: no signal ①, and a drop-in meeting has no event end.
    let idle = Db::open_in_memory(clock(start - 3 * HOUR)).unwrap();
    idle.ingest_integration(&[item("evt-later", start, false)]);
    assert_eq!(idle.calendar_occurrence_now(), None);
    let drop_in = idle.open_meeting(Some("Zoom"), None, 0.8, "{}").unwrap();
    assert_eq!(idle.meeting_occurrence_end(drop_in), None);
}

/// #283: a saved screen carries two texts (its window's accessibility text and its OCR text).
/// When both match, chat evidence shows the screen once, and it is the accessibility row.
#[test]
fn a_saved_screen_is_one_row_of_evidence_even_when_both_of_its_texts_match() {
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let accessibility = db
        .capture(&NewEvent {
            window_title: Some("Orchid launch — Brave"),
            ..ev("Orchid launch checklist: ship the notch build", "ax-orchid", 9_000)
        })
        .unwrap()
        .0;
    let ocr = db
        .capture(&NewEvent {
            source: "screen_ocr",
            window_title: Some("Orchid launch — Brave"),
            ..ev("Orchid launch checklist (pixels)", "ocr-orchid", 9_000)
        })
        .unwrap()
        .0;
    let frame = db
        .store_screen_frame(
            Some(ocr),
            Some(accessibility),
            Some("com.brave.Browser"),
            Some("Orchid launch — Brave"),
            None,
            1,
            1,
            shogun_memory::screen_frames::OcrRegion::WholeFrame,
            &[1, 2, 3],
        )
        .unwrap();

    let pack = db.assemble_context("what was on my screen today about the orchid launch", 8, 200);

    let rows: Vec<_> = pack
        .evidence
        .iter()
        .filter(|item| item.frame_id == Some(frame))
        .collect();
    assert_eq!(rows.len(), 1, "one row per saved screen: {:?}", pack.evidence);
    assert_eq!(rows[0].event_id, accessibility, "the window text wins over OCR");
    assert!(
        !pack.evidence.iter().any(|item| item.event_id == ocr),
        "the OCR text of the same screen is not a second row: {:?}",
        pack.evidence
    );
}

/// Release-only wall-clock measurement of the actual context-cache path and hourly local pass.
/// Run alone: cargo test -p shogun-core --features db --release context_review_latency -- --ignored --nocapture
#[test]
#[ignore = "release wall-clock measurement; run alone"]
fn context_review_latency() {
    use rusqlite::params;
    use std::time::Instant;
    let db = Db::open_in_memory(clock(10_000)).unwrap();
    let quote = "I will send the final deck after legal approval";
    let content = format!("{} {quote}", "Background capture text. ".repeat(683));
    db.with_conn_mut("review.benchmark.seed", |conn| -> Result<(), rusqlite::Error> {
        let tx = conn.savepoint()?;
        for id in 1..=3 {
            tx.execute("INSERT INTO event_log (id,ts,source,kind,content,content_hash,last_seen_at)
                VALUES (?1,1,'capture','text',?2,?3,1)", params![id, format!("{content} source {id}"), format!("context-perf-{id}")])?;
        }
        for id in 1..=125 {
            tx.execute("INSERT INTO commitments (id,direction,description,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                VALUES (?1,'mine',?2,'open',0.7,0.7,1,1,1)", params![id, format!("Send deck {id}")])?;
            tx.execute("INSERT INTO open_loops (id,kind,description,status,opened_at,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                VALUES (?1,'follow_up',?2,'open',1,0.7,0.7,1,1,1)", params![id, format!("Review deck {id}")])?;
            tx.execute("INSERT INTO people (id,display_name,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                VALUES (?1,?2,0.7,0.7,1,1,1)", params![id, format!("Deck owner {id}")])?;
            tx.execute("INSERT INTO projects (id,name,status,confidence,base_confidence,created_at,updated_at,last_evidence_at)
                VALUES (?1,?2,'active',0.7,0.7,1,1,1)", params![id, format!("Deck project {id}")])?;
            for table in ["commitments", "open_loops", "people", "projects"] {
                tx.execute("INSERT INTO state_provenance (state_table,state_id,event_id,weight,quote,batch_reviewed)
                    SELECT ?1,?2,id,1,?3,1 FROM event_log", params![table, id, quote])?;
            }
        }
        tx.commit()
    }).unwrap();
    for links_per_row in [3, 100] {
        if links_per_row > 3 {
            db.with_conn_mut("review.benchmark.grow", |conn| -> Result<(), rusqlite::Error> {
                let tx = conn.savepoint()?;
                for id in 4..=links_per_row {
                    tx.execute("INSERT INTO event_log (id,ts,source,kind,content,content_hash,last_seen_at) VALUES (?1,1,'capture','text',?2,?3,1)", params![id,format!("{content} source {id}"),format!("context-perf-{id}")])?;
                    tx.execute("INSERT INTO state_provenance(state_table,state_id,event_id,weight,quote,batch_reviewed) SELECT state_table,state_id,?1,1,?2,1 FROM state_provenance WHERE event_id=1", params![id,format!("{quote} source {id}")])?;
                }
                tx.commit()
            }).unwrap();
        }
        for (label, retained, supported) in [
            ("exact", quote.to_owned(), true),
            ("whitespace", quote.replace(' ', "\n"), true),
            ("unsupported", "The removed quotation is no longer here".into(), false),
        ] {
            db.with_conn("review.benchmark.quote", |conn| conn.execute("UPDATE state_provenance SET quote=?1 || ' source ' || event_id", [&retained])).unwrap();
            let mut context = Vec::new();
            let mut maintenance = Vec::new();
            for sample in 0..22 {
                let start = Instant::now();
                let cache = db.context_actions(shogun_fusion::assemble::ScreenContext {
                    app_bundle_id: "com.apple.mail".into(), window_title: "Deck".into(),
                    salient: vec!["deck".into()], ..Default::default()
                }, None);
                let context_us = start.elapsed().as_micros();
                assert_eq!(!cache.facts.is_empty(), supported);
                let start = Instant::now();
                if links_per_row == 3 { db.run_local_maintenance(10_000, 30 * 86_400_000); }
                let maintenance_us = start.elapsed().as_micros();
                if sample > 0 { context.push(context_us); maintenance.push(maintenance_us); }
            }
            context.sort_unstable(); maintenance.sort_unstable();
            eprintln!("context review {label}: 500 rows across 4 tables, {links_per_row} links/row, 16KiB sources, 21 warm samples; context p50={}us p95={}us", context[10], context[19]);
            if links_per_row == 3 {
                eprintln!("local maintenance {label}: p50={}us p95={}us", maintenance[10], maintenance[19]);
            }
            // ponytail: all-invalid histories must scan every link; measure the 100-link tail,
            // but only gate supported reads until source mutation can invalidate evidence eagerly.
            if supported || links_per_row == 3 {
                assert!(context[19] < 300_000, "context-cache SLO: {}us", context[19]);
            }
        }
    }
}

/// #842: the macOS AX path (checkpoint + delta) with the clock moving past five minutes.
#[test]
fn ax_recaptures_of_one_message_stay_one_memory_as_time_passes() {
    use std::sync::atomic::{AtomicI64, Ordering};
    const MIN: i64 = 60_000;
    let app = Some("com.example.chat");
    let title = Some("Delivery chat");
    let fresh = || {
        let now = Arc::new(AtomicI64::new(1_000));
        let tick = Arc::clone(&now);
        (Db::open_in_memory(Arc::new(move || tick.load(Ordering::Relaxed))).unwrap(), now)
    };
    let rows = |db: &Db| db.try_commitment_rows().unwrap().len();

    // Unchanged message lines, re-extracted ten minutes later after the rest of the window moved.
    let (db, now) = fresh();
    let (first, _, found) = db.ingest_capture_checkpoint(app, title, "Sam: sounds good\nDana: I'll send the final deck.\nold sidebar", 1).unwrap();
    assert!(!found.is_empty(), "the AX path must extract a local candidate");
    now.fetch_add(10 * MIN, Ordering::Relaxed);
    db.ingest_capture_checkpoint(app, title, "Sam: sounds good\nDana: I'll send the final deck.\nSam: thanks\nnew sidebar", 1).unwrap();
    assert_eq!(rows(&db), 1, "a later checkpoint must not duplicate a promise still on screen");

    // Completed, still on screen (touched), then the window changes: the done item absorbs it.
    db.conn.lock().unwrap().execute("UPDATE commitments SET status='done'", []).unwrap();
    now.fetch_add(9 * MIN, Ordering::Relaxed);
    assert!(db.touch_capture_event(first, 1));
    now.fetch_add(MIN, Ordering::Relaxed);
    db.ingest_capture_checkpoint(app, title, "Sam: sounds good\nDana: I'll send the final deck.\nSam: thanks\nPat: great\nnewer sidebar", 1).unwrap();
    assert_eq!(rows(&db), 1, "re-reading a completed message must not resurrect it");

    // A delta holds only the new line; the next checkpoint shows the line above it.
    let (db, now) = fresh();
    db.ingest_capture_delta(app, title, "I'll send the final deck.", 1).unwrap();
    now.fetch_add(MIN, Ordering::Relaxed);
    db.ingest_capture_checkpoint(app, title, "Sam: sounds good\nI'll send the final deck.", 1).unwrap();
    assert_eq!(rows(&db), 1, "a delta and its checkpoint are one message");

    // A relative-time header moves on its own.
    let (db, now) = fresh();
    db.ingest_capture_checkpoint(app, title, "Dana · 2m\nI'll send the final deck.", 1).unwrap();
    now.fetch_add(2 * MIN, Ordering::Relaxed);
    db.ingest_capture_checkpoint(app, title, "Dana · 4m\nI'll send the final deck.", 1).unwrap();
    assert_eq!(rows(&db), 1, "a ticking header is not a new sender");

    // The adjacent line is the clock, not the sender: different senders stay separate.
    let (db, now) = fresh();
    db.ingest_capture_checkpoint(app, title, "Dana\n10:42 AM\nI'll send the final deck.", 1).unwrap();
    now.fetch_add(MIN, Ordering::Relaxed);
    db.ingest_capture_checkpoint(app, title, "Sam\n10:42 AM\nI'll send the final deck.", 1).unwrap();
    assert_eq!(rows(&db), 2, "the same words from two senders are two promises");
}
