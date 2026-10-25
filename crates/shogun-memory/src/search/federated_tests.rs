use super::*;
use crate::event_log::{insert, NewEvent};
use crate::search_documents::SearchDocumentKind;
use crate::state::{insert_project, NewProject, ProjectStatus, Provenance};

fn event(conn: &rusqlite::Connection, ts: i64, text: &str, hash: &str) -> i64 {
    insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: Some("Evidence"),
            content: text,
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

fn weak_meeting(conn: &mut rusqlite::Connection, ordinal: usize, text: &str) -> i64 {
    use crate::meeting_index::index_session;
    use crate::meeting_recaps;
    use crate::session::{close, open, NewSession};
    use crate::transcript_segments::{append, NewSegment, Speaker};

    let started_at = 1_000 + ordinal as i64 * 10;
    let title = format!("Routine sync {ordinal}");
    let session_id = open(
        conn,
        &NewSession {
            kind: "meeting",
            started_at,
            title: Some(&title),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: 0.9,
            provenance: "{}",
        },
    )
    .unwrap();
    append(
        conn,
        &NewSegment {
            session_id,
            ts: started_at + 1,
            speaker: Speaker::Other,
            text,
            confidence: 0.9,
        },
        started_at + 1,
    )
    .unwrap();
    close(conn, session_id, started_at + 2).unwrap();
    index_session(conn, session_id).unwrap();
    meeting_recaps::save(
        conn,
        session_id,
        text,
        "[]",
        "[]",
        None,
        "test-model",
        started_at + 3,
    )
    .unwrap();
    session_id
}

fn project_document(conn: &mut rusqlite::Connection, evidence: i64, name: &str, summary: &str) {
    insert_project(
        conn,
        &NewProject {
            name,
            status: ProjectStatus::Active,
            summary: Some(summary),
            participants_json: None,
            sources_json: None,
            confidence: 0.95,
            now: 100,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
}

fn brief_document(conn: &rusqlite::Connection, evidence: i64, date: &str, text: &str) {
    let payload = serde_json::json!({
        "date": date,
        "today": [],
        "commitments_due": [{
            "text": text,
            "provenance_event_id": evidence,
            "possibly": false
        }],
        "open_loops": [],
        "what_happened": [text],
        "suggested_actions": [],
        "charm_line": null
    });
    crate::briefs::upsert_brief(conn, date, &payload.to_string(), false, 100).unwrap();
}

fn period_document(conn: &mut rusqlite::Connection, evidence: i64, text: &str) {
    use crate::period_summaries::{save, NewPeriodSummary, PeriodLevel};

    save(
        conn,
        &NewPeriodSummary {
            level: PeriodLevel::Hour,
            period_start_ms: 0,
            period_end_ms: 400,
            title: "Required period",
            content: text,
            generated: false,
            generation_model: None,
            event_ids: &[evidence],
            child_summary_ids: &[],
            now_ms: 1_000,
        },
    )
    .unwrap();
}

fn thread_document(conn: &rusqlite::Connection, text: &str) {
    conn.execute(
        "INSERT INTO threads
           (thread_key,source,title,summary,first_activity_at,last_activity_at,
            confidence,created_at,updated_at)
         VALUES ('required-thread','capture','Required thread',?1,1,2,0.95,100,100)",
        [text],
    )
    .unwrap();
    let thread_id = conn.last_insert_rowid();
    let _ = crate::search_documents::project_source_after_write(
        conn,
        &crate::search_documents::SourceRef::Thread(thread_id),
        100,
    );
}

fn hit_matches_family(hit: &FederatedHit, family: MemorySourceFamily) -> bool {
    matches!(
        (&hit.source_kind, family),
        (
            FederatedSourceKind::Derived(SearchDocumentKind::Project),
            MemorySourceFamily::Project
        ) | (
            FederatedSourceKind::Derived(SearchDocumentKind::Brief),
            MemorySourceFamily::Brief
        ) | (
            FederatedSourceKind::Derived(SearchDocumentKind::PeriodSummary),
            MemorySourceFamily::Period
        ) | (
            FederatedSourceKind::Derived(SearchDocumentKind::ThreadSummary),
            MemorySourceFamily::Thread
        )
    )
}

#[test]
fn exact_standalone_fact_beats_twenty_weak_meeting_clusters() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(
        &conn,
        10,
        "Zephyr owner launch record",
        "exact-project-evidence",
    );
    let project_id = insert_project(
        &mut conn,
        &NewProject {
            name: "Zephyr",
            status: ProjectStatus::Active,
            summary: Some("Zephyr launch owner is Priya"),
            participants_json: None,
            sources_json: None,
            confidence: 0.95,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
    for ordinal in 0..20 {
        weak_meeting(&mut conn, ordinal, "Routine launch status reviewed");
    }

    let result = search_federated(
        &conn,
        "Zephyr launch owner",
        None,
        10_000,
        &FederatedSearchOptions::default(),
        8,
    )
    .unwrap();

    assert_eq!(
        result.hits[0].source_id,
        FederatedSourceId::Derived(crate::search_documents::SourceRef::Project(project_id)),
        "one exact fact must outrank generic overlap regardless of meeting count: {result:?}"
    );
}

#[test]
fn soft_project_intent_ranks_project_first_while_explicit_requirement_filters() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(
        &conn,
        10,
        "Phoenix status source",
        "source-project-evidence",
    );
    let project_id = insert_project(
        &mut conn,
        &NewProject {
            name: "Phoenix",
            status: ProjectStatus::Active,
            summary: Some("Phoenix project status is active"),
            participants_json: None,
            sources_json: None,
            confidence: 0.95,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
    let preferred = search_federated(
        &conn,
        "Phoenix project status",
        None,
        20,
        &FederatedSearchOptions {
            source: SourceConstraint::Prefer(MemorySourceFamily::Project),
            ..FederatedSearchOptions::default()
        },
        10,
    )
    .unwrap();
    let required = search_federated(
        &conn,
        "Phoenix project status",
        None,
        20,
        &FederatedSearchOptions {
            source: SourceConstraint::Require(MemorySourceFamily::Project),
            ..FederatedSearchOptions::default()
        },
        10,
    )
    .unwrap();

    assert_eq!(
        preferred.hits[0].source_id,
        FederatedSourceId::Derived(crate::search_documents::SourceRef::Project(project_id))
    );
    assert!(required.hits.iter().all(|hit| matches!(
        hit.source_kind,
        FederatedSourceKind::Derived(SearchDocumentKind::Project)
    )));
}

#[test]
fn explicit_source_requirement_filters_before_hundred_row_lane_limits() {
    for family in [
        MemorySourceFamily::Project,
        MemorySourceFamily::Brief,
        MemorySourceFamily::Period,
        MemorySourceFamily::Thread,
    ] {
        let mut conn = crate::open_in_memory().unwrap();
        let evidence = event(&conn, 10, "canonical source evidence", "required-source-evidence");
        if family == MemorySourceFamily::Project {
            for ordinal in 0..101 {
                brief_document(
                    &conn,
                    evidence,
                    &format!("distractor-{ordinal}"),
                    &"needle ".repeat(30),
                );
            }
            project_document(&mut conn, evidence, "Required project", "needle");
        } else {
            for ordinal in 0..101 {
                project_document(
                    &mut conn,
                    evidence,
                    &format!("Distractor {ordinal}"),
                    &"needle ".repeat(30),
                );
            }
            match family {
                MemorySourceFamily::Brief => brief_document(&conn, evidence, "required", "needle"),
                MemorySourceFamily::Period => period_document(&mut conn, evidence, "needle"),
                MemorySourceFamily::Thread => thread_document(&conn, "needle"),
                _ => unreachable!(),
            }
        }

        let result = search_federated(
            &conn,
            "needle",
            None,
            1_000,
            &FederatedSearchOptions {
                source: SourceConstraint::Require(family),
                widen_event_lexical: true,
                ..FederatedSearchOptions::default()
            },
            8,
        )
        .unwrap();

        assert!(!result.hits.is_empty(), "required family was displaced: {family:?}");
        assert!(result
            .hits
            .iter()
            .all(|hit| hit_matches_family(hit, family)));
        assert!(result.raw_event_ranks.is_empty());
        assert!(!result.event_lexical_all_history);
        assert_eq!(result.cold, ColdScanStats::default());
    }
}

#[test]
fn repeated_generic_terms_do_not_beat_distinct_query_term_coverage() {
    let conn = crate::open_in_memory().unwrap();
    let exact = event(&conn, 10, "Zephyr launch owner", "compact-exact");
    let stuffed = event(
        &conn,
        11,
        &format!("{} routine notes", "launch ".repeat(200)),
        "keyword-stuffed",
    );

    let result = search_federated(
        &conn,
        "Zephyr launch owner",
        None,
        20,
        &FederatedSearchOptions::default(),
        2,
    )
    .unwrap();

    assert_eq!(result.hits[0].source_id, FederatedSourceId::Event(exact));
    assert!(result
        .hits
        .iter()
        .any(|hit| hit.source_id == FederatedSourceId::Event(stuffed)));
}

#[test]
fn neutral_search_preserves_parent_p0_event_order_and_scores() {
    let conn = crate::open_in_memory().unwrap();
    let first = event(&conn, 10, "Phoenix quart", "p0-score-first");
    let second = event(&conn, 11, "Phoenix party", "p0-score-second");

    let result = search_federated(
        &conn,
        "Phoenix art",
        None,
        20,
        &FederatedSearchOptions::default(),
        2,
    )
    .unwrap();
    let observed = result
        .hits
        .iter()
        .map(|hit| hit.source_id.clone())
        .collect::<Vec<_>>();

    // Equal match quality: the newer row takes rank one, and the scores differ only by rank
    // (1/61 vs 1/62). Both rows carry the same source and an equal (bounded) recency prior, so
    // the ratio survives exactly.
    assert_eq!(
        observed,
        vec![FederatedSourceId::Event(second), FederatedSourceId::Event(first)]
    );
    let ratio = result.hits[0].fused_score / result.hits[1].fused_score;
    assert!((ratio - 62.0 / 61.0).abs() < 1e-9, "{ratio}");
    assert!(result.hits[0].fused_score <= 1.0 / 61.0 * 1.1 + f64::EPSILON);
}

#[test]
fn federated_search_finds_answer_present_only_in_derived_document() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(
        &conn,
        10,
        "unrelated source wording",
        "derived-only-evidence",
    );
    let project_id = insert_project(
        &mut conn,
        &NewProject {
            name: "Orchid",
            status: ProjectStatus::Active,
            summary: Some("The launch codename is 星舟計画"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();

    let result = search_federated(
        &conn,
        "星舟計画",
        None,
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert_eq!(result.hits.len(), 1);
    assert_eq!(
        result.hits[0].source_kind,
        FederatedSourceKind::Derived(SearchDocumentKind::Project)
    );
    assert_eq!(
        result.hits[0].sources[0].id,
        FederatedSourceId::Derived(crate::search_documents::SourceRef::Project(project_id))
    );
    assert!(result.hits[0]
        .evidence_ids
        .contains(&CanonicalEvidenceId::Event(evidence)));
    assert!(result.hits[0].excerpt.contains("星舟計画"));
}

#[test]
fn federated_search_finds_semantic_derived_memory_without_shared_words() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(
        &conn,
        10,
        "raw wording without the lookup concepts",
        "derived-semantic-evidence",
    );
    let project_id = insert_project(
        &mut conn,
        &NewProject {
            name: "Orchid",
            status: ProjectStatus::Active,
            summary: Some("supplier contract extension accepted"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
    assert!(
        crate::search_documents::search_documents_fts(&conn, "vendor renewal approved", 10,)
            .unwrap()
            .is_empty()
    );
    let (document_id, digest): (i64, String) = conn
        .query_row(
            "SELECT id,content_digest FROM search_documents
              WHERE source_table='projects' AND source_id=?1",
            [project_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let query_embedding = vec![0.25_f32; crate::embed::E5_SMALL_DIM];
    crate::document_vector::upsert(&conn, document_id, &digest, &query_embedding, 12).unwrap();

    let result = search_federated(
        &conn,
        "vendor renewal approved",
        Some(&query_embedding),
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert_eq!(
        result.hits[0].source_id,
        FederatedSourceId::Derived(crate::search_documents::SourceRef::Project(project_id))
    );
    assert_eq!(result.hits[0].component_ranks.derived_knn, Some(1));
}

#[test]
fn federated_search_abstains_when_nearest_semantic_memory_is_weak() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(
        &conn,
        10,
        "unrelated source wording",
        "weak-semantic-evidence",
    );
    let project_id = insert_project(
        &mut conn,
        &NewProject {
            name: "Orchid",
            status: ProjectStatus::Active,
            summary: Some("supplier contract extension accepted"),
            participants_json: None,
            sources_json: None,
            confidence: 0.9,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();
    let (document_id, digest): (i64, String) = conn
        .query_row(
            "SELECT id,content_digest FROM search_documents
              WHERE source_table=?1 AND source_id=?2",
            rusqlite::params!["projects", project_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut stored = vec![0.0_f32; crate::embed::E5_SMALL_DIM];
    stored[0] = 1.0;
    crate::document_vector::upsert(&conn, document_id, &digest, &stored, 12).unwrap();
    let mut unrelated_query = vec![0.0_f32; crate::embed::E5_SMALL_DIM];
    unrelated_query[1] = 1.0;

    let result = search_federated(
        &conn,
        "completely different topic",
        Some(&unrelated_query),
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert!(
        result.hits.is_empty(),
        "weak nearest-neighbor evidence must abstain: {result:?}"
    );
}

#[test]
fn low_confidence_derived_document_is_gated_before_results() {
    let mut conn = crate::open_in_memory().unwrap();
    let evidence = event(&conn, 10, "ordinary source", "low-confidence-evidence");
    insert_project(
        &mut conn,
        &NewProject {
            name: "Low confidence",
            status: ProjectStatus::Active,
            summary: Some("Unverified Neptune acquisition"),
            participants_json: None,
            sources_json: None,
            confidence: MIN_FACT_CONFIDENCE - 0.01,
            now: 11,
        },
        &[Provenance::new(evidence)],
    )
    .unwrap();

    let result = search_federated(
        &conn,
        "Neptune acquisition",
        None,
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();
    assert!(result.hits.is_empty());
}

#[test]
fn raw_cluster_member_does_not_raise_derived_primary_confidence() {
    use crate::meeting_index::index_session;
    use crate::meeting_recaps;
    use crate::session::{close, open, NewSession};
    use crate::transcript_segments::{append, NewSegment, Speaker};

    let mut conn = crate::open_in_memory().unwrap();
    let session_id = open(
        &conn,
        &NewSession {
            kind: "meeting",
            started_at: 1_000,
            title: Some("Confidence council"),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: MIN_FACT_CONFIDENCE,
            provenance: "{}",
        },
    )
    .unwrap();
    append(
        &conn,
        &NewSegment {
            session_id,
            ts: 1_100,
            speaker: Speaker::Other,
            text: "Review the marigold launch plan",
            confidence: 0.9,
        },
        1_100,
    )
    .unwrap();
    close(&conn, session_id, 2_000).unwrap();
    index_session(&mut conn, session_id).unwrap();
    meeting_recaps::save(
        &conn,
        session_id,
        "Marigold launch plan reviewed",
        "[]",
        "[]",
        None,
        "test-model",
        2_100,
    )
    .unwrap();

    let result = search_federated(
        &conn,
        "marigold launch",
        None,
        3_000,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert_eq!(
        result.hits[0].source_kind,
        FederatedSourceKind::Derived(SearchDocumentKind::MeetingRecap)
    );
    assert_eq!(result.hits[0].confidence, MIN_FACT_CONFIDENCE);
}

#[test]
fn month_hit_exposes_all_direct_child_period_summaries() {
    use crate::period_summaries::{save, NewPeriodSummary, PeriodLevel};

    let mut conn = crate::open_in_memory().unwrap();
    let first_event = event(&conn, 10, "first evidence", "period-child-first");
    let second_event = event(&conn, 410, "second evidence", "period-child-second");
    let first_hour = save(
        &mut conn,
        &NewPeriodSummary {
            level: PeriodLevel::Hour,
            period_start_ms: 0,
            period_end_ms: 100,
            title: "first hour",
            content: "first evidence",
            generated: false,
            generation_model: None,
            event_ids: &[first_event],
            child_summary_ids: &[],
            now_ms: 2_000,
        },
    )
    .unwrap();
    let second_hour = save(
        &mut conn,
        &NewPeriodSummary {
            level: PeriodLevel::Hour,
            period_start_ms: 400,
            period_end_ms: 500,
            title: "second hour",
            content: "second evidence",
            generated: false,
            generation_model: None,
            event_ids: &[second_event],
            child_summary_ids: &[],
            now_ms: 2_000,
        },
    )
    .unwrap();
    let first_day = save(
        &mut conn,
        &NewPeriodSummary {
            level: PeriodLevel::Day,
            period_start_ms: 0,
            period_end_ms: 400,
            title: "first day",
            content: "first evidence",
            generated: false,
            generation_model: None,
            event_ids: &[first_event],
            child_summary_ids: &[first_hour],
            now_ms: 2_001,
        },
    )
    .unwrap();
    let second_day = save(
        &mut conn,
        &NewPeriodSummary {
            level: PeriodLevel::Day,
            period_start_ms: 400,
            period_end_ms: 800,
            title: "second day",
            content: "second evidence",
            generated: false,
            generation_model: None,
            event_ids: &[second_event],
            child_summary_ids: &[second_hour],
            now_ms: 2_001,
        },
    )
    .unwrap();
    save(
        &mut conn,
        &NewPeriodSummary {
            level: PeriodLevel::Month,
            period_start_ms: 0,
            period_end_ms: 1_000,
            title: "month",
            content: "Citrine monthly synthesis",
            generated: false,
            generation_model: None,
            event_ids: &[first_event, second_event],
            child_summary_ids: &[first_day, second_day],
            now_ms: 2_002,
        },
    )
    .unwrap();

    let result = search_federated(
        &conn,
        "Citrine monthly",
        None,
        3_000,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert!(result.hits[0]
        .evidence_ids
        .contains(&CanonicalEvidenceId::PeriodSummary(first_day)));
    assert!(result.hits[0]
        .evidence_ids
        .contains(&CanonicalEvidenceId::PeriodSummary(second_day)));
}

#[test]
fn empty_query_is_empty_and_default_never_scans_cold() {
    let conn = crate::open_in_memory().unwrap();
    let result = search_federated(
        &conn,
        "   ",
        Some(&[1.0]),
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();
    assert!(result.hits.is_empty());
    assert_eq!(result.cold, ColdScanStats::default());
}

#[test]
fn repeated_queries_have_identical_order() {
    let conn = crate::open_in_memory().unwrap();
    event(&conn, 10, "stable tie token", "stable-a");
    event(&conn, 10, "stable tie token", "stable-b");
    let options = FederatedSearchOptions::default();
    let first = search_federated(&conn, "stable tie", None, 20, &options, 10).unwrap();
    let second = search_federated(&conn, "stable tie", None, 20, &options, 10).unwrap();
    let ids = |result: &FederatedSearchResult| {
        result
            .hits
            .iter()
            .map(|hit| hit.source_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), ids(&second));
    assert!(first.hits.iter().all(|hit| hit.fused_score.is_finite()));
    assert_eq!(
        first
            .hits
            .iter()
            .map(|hit| hit.fused_score)
            .collect::<Vec<_>>(),
        second
            .hits
            .iter()
            .map(|hit| hit.fused_score)
            .collect::<Vec<_>>()
    );
}

#[test]
fn meeting_recap_search_and_raw_event_deduplicate_without_losing_provenance() {
    use crate::meeting_index::index_session;
    use crate::meeting_recaps;
    use crate::session::{close, open, NewSession};
    use crate::transcript_segments::{append, NewSegment, Speaker};

    let mut conn = crate::open_in_memory().unwrap();
    let session_id = open(
        &conn,
        &NewSession {
            kind: "meeting",
            started_at: 1_000,
            title: Some("Launch council"),
            app_bundle_id: None,
            calendar_occurrence_id: None,
            confidence: 0.9,
            provenance: "{}",
        },
    )
    .unwrap();
    let segment_id = append(
        &conn,
        &NewSegment {
            session_id,
            ts: 1_100,
            speaker: Speaker::Other,
            text: "Approve the heliotrope launch window",
            confidence: 0.9,
        },
        1_100,
    )
    .unwrap();
    close(&conn, session_id, 2_000).unwrap();
    let indexed = index_session(&mut conn, session_id).unwrap();
    meeting_recaps::save(
        &conn,
        session_id,
        "Heliotrope launch window approved",
        "[]",
        "[]",
        None,
        "test-model",
        2_100,
    )
    .unwrap();
    let recap_id: i64 = conn
        .query_row(
            "SELECT id FROM meeting_recaps WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap();

    let result = search_federated(
        &conn,
        "heliotrope launch",
        None,
        3_000,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert_eq!(result.hits.len(), 1, "one logical meeting: {result:?}");
    let hit = &result.hits[0];
    assert_eq!(
        hit.sources.len(),
        3,
        "derived, meeting, and raw attribution survive"
    );
    assert!(hit
        .evidence_ids
        .contains(&CanonicalEvidenceId::Session(session_id)));
    assert!(hit
        .evidence_ids
        .contains(&CanonicalEvidenceId::TranscriptSegment(segment_id)));
    assert!(hit
        .evidence_ids
        .contains(&CanonicalEvidenceId::MeetingRecap(recap_id)));
    assert!(hit.evidence_ids.contains(&CanonicalEvidenceId::Event(
        indexed.transcript_event_id.unwrap()
    )));
    assert!(hit.component_ranks.event_fts.is_some());
    assert!(hit.component_ranks.derived_fts.is_some());
    assert!(hit.component_ranks.meeting.is_some());
    // Rank one, times at most 10 % corroboration, times the bounded priors (distilled source
    // ×1.10, recency ≤ ×1.10 — federated.rs `prior_multiplier`).
    let strongest_rank_one = 1.0 / 61.0;
    assert!(
        hit.fused_score <= strongest_rank_one * 1.1 * 1.1 * 1.1 + f64::EPSILON,
        "four representations may add only capped corroboration, not linear relevance: {hit:?}"
    );
}

#[test]
fn warm_component_rank_and_latency_are_reported() {
    let conn = crate::open_in_memory().unwrap();
    let id = event(&conn, 10, "warm vector attribution", "warm-rank");
    let embedding = vec![0.0_f32; crate::embed::E5_SMALL_DIM];
    crate::vector::upsert(&conn, id, &embedding).unwrap();

    let result = search_federated(
        &conn,
        "warm vector",
        Some(&embedding),
        20,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();

    assert_eq!(result.hits[0].component_ranks.warm_knn, Some(1));
    assert!(result.hits[0].tiers.warm);
    assert!(result.elapsed_micros < 500_000, "local SLO: {result:?}");
}

#[test]
fn cold_semantic_match_is_reachable_only_when_requested() {
    let mut conn = crate::open_in_memory().unwrap();
    let id = event(&conn, 0, "semantically archived evidence", "cold-federated");
    let embedding = vec![0.01_f32; crate::embed::E5_SMALL_DIM];
    crate::vector::upsert(&conn, id, &embedding).unwrap();
    assert!(crate::cold::demote(&mut conn, id, 0).unwrap());
    let now_ms = crate::search::WARM_WINDOW_MS * 2;

    let warm = search_federated(
        &conn,
        "no lexical overlap",
        Some(&embedding),
        now_ms,
        &FederatedSearchOptions::default(),
        10,
    )
    .unwrap();
    assert!(warm.hits.is_empty());
    assert_eq!(warm.cold, ColdScanStats::default());

    let options = FederatedSearchOptions {
        depth: SearchDepth::All,
        ..FederatedSearchOptions::default()
    };
    let deep = search_federated(
        &conn,
        "no lexical overlap",
        Some(&embedding),
        now_ms,
        &options,
        10,
    )
    .unwrap();
    assert_eq!(deep.hits[0].source_id, FederatedSourceId::Event(id));
    assert_eq!(deep.hits[0].component_ranks.cold_scan, Some(1));
    assert!(deep.hits[0].tiers.cold);
    assert_eq!(deep.cold.partitions_visited, 1);
}

fn import_claims(conn: &mut rusqlite::Connection, package: &str) -> i64 {
    let request = crate::memory_import::parse_request(&format!(
        r#"{{"package":{package},"select":"default"}}"#
    ))
    .unwrap();
    crate::memory_import::commit(conn, &request, crate::memory_import::Via::Ui, 5)
        .unwrap()
        .import_id
}

const KESTREL_IMPORT: &str = r#"{"format":"shogun-memory-import/v1","source":{"platform":"computer_history"},
    "profile":{"goals":[{"value":"Launch Kestrel analytics dashboard","confidence":"high","evidence":"e","status":"active"}],
               "preferences":[{"value":"Kestrel reviews on Fridays","confidence":"low","evidence":"e","status":"active"}]}}"#;

#[test]
fn imported_claims_are_found_weakly_with_import_provenance() {
    let mut conn = crate::open_in_memory().unwrap();
    let import_id = import_claims(&mut conn, KESTREL_IMPORT);

    let result = search_federated(
        &conn,
        "Kestrel dashboard",
        None,
        10_000,
        &FederatedSearchOptions::default(),
        8,
    )
    .unwrap();
    assert_eq!(result.hits.len(), 1, "the Low preference stays out: {result:?}");
    let hit = &result.hits[0];
    assert_eq!(
        hit.source_kind,
        FederatedSourceKind::Imported(crate::memory_import::package::Platform::ComputerHistory)
    );
    assert!(matches!(hit.source_id, FederatedSourceId::Imported(_)));
    assert_eq!(hit.evidence_ids, [CanonicalEvidenceId::MemoryImport(import_id)]);
    assert!(hit.confidence <= crate::memory_import::CONFIDENCE_MAX);
    assert!(!hit.answer_eligible, "an imported claim never supports an assertive answer");
    assert!(hit.component_ranks.imported.is_some());
    assert!(hit.excerpt.contains("Launch Kestrel analytics dashboard"));
    assert!(result.raw_event_ranks.is_empty(), "never projected as a captured event");

    // An explicit family requirement or a meeting focus leaves imported claims out.
    for options in [
        FederatedSearchOptions {
            source: SourceConstraint::Require(MemorySourceFamily::Project),
            ..FederatedSearchOptions::default()
        },
        FederatedSearchOptions {
            focus: MemoryQueryFocus::Meeting,
            ..FederatedSearchOptions::default()
        },
    ] {
        let filtered =
            search_federated(&conn, "Kestrel dashboard", None, 10_000, &options, 8).unwrap();
        assert!(filtered.hits.iter().all(|hit| !hit.source_kind.is_imported()));
    }

    // Undo the import: the claim leaves search at once (no projection to go stale).
    assert!(crate::memory_import::delete_import(&mut conn, import_id).unwrap());
    let after = search_federated(
        &conn,
        "Kestrel dashboard",
        None,
        10_000,
        &FederatedSearchOptions::default(),
        8,
    )
    .unwrap();
    assert!(after.hits.is_empty(), "{after:?}");
}

#[test]
fn observed_memory_outranks_an_equally_matching_imported_claim() {
    let mut conn = crate::open_in_memory().unwrap();
    import_claims(&mut conn, KESTREL_IMPORT);
    let observed = event(&conn, 9_000, "Launch Kestrel analytics dashboard", "kestrel-observed");

    let result = search_federated(
        &conn,
        "Kestrel analytics dashboard",
        None,
        10_000,
        &FederatedSearchOptions::default(),
        8,
    )
    .unwrap();
    assert_eq!(result.hits.len(), 2, "{result:?}");
    assert_eq!(result.hits[0].source_id, FederatedSourceId::Event(observed));
    assert!(result.hits[1].source_kind.is_imported());
    assert!(result.hits[0].fused_score > result.hits[1].fused_score);
}
