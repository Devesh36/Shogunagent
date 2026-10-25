//! Local-search SLO gates (NFR-SLO-04: 500 ms), measured in wall-clock time against the worst-case
//! fixtures.
//!
//! Every test here is an `#[ignore]`d measurement, run explicitly in release, one at a time:
//!
//! ```text
//! cargo test -p shogun-memory --release --test search_slo -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `.github/workflows/memory-bench.yml` (manual dispatch) runs them next to the 100k benchmark.
//!
//! A wall-clock p95 in the default `cargo test` measures the machine, not the code: elapsed time
//! inflates with CPU contention alone. This file first moved into its own binary because the gate
//! flaked as a lib test (p95 517–1650 ms under parallel runs vs ~150–450 ms alone); even alone in
//! its binary it still failed about one run in five under a heavily loaded parallel workspace run
//! (reproduced: 16 of 20 runs next to 20 busy loops on 4 cores, ~76 ms on an idle one).
//!
//! The rule: the default run keeps a fixed small set of **deterministic** tests for these
//! fixtures, which assert behaviour and work done (SQLite VM instructions, statements prepared),
//! never elapsed time: `tests/evidence_search_bounds.rs` for the evidence-validation fixture and
//! `tests/imported_search_bounds.rs` for the imported-claim lane. A new wall-clock gate goes here
//! as `#[ignore]`, with its deterministic half in one of those files (or a new one like them).
#![allow(clippy::unwrap_used)]

mod support;

use shogun_memory::search::{search_federated, FederatedSearchOptions};

use support::evidence_heavy_meeting;

/// The evidence-validation worst case: 100 matching meetings (`limit` 100 candidates), each with
/// 30 persisted evidence rows that are all validated before the meeting may be answered from.
/// The federated search must stay inside NFR-SLO-04.
///
/// Explicit, not part of the default run (see the module docs). Its deterministic half (hits
/// capped at `limit` and all answer-eligible, validation batched, VM work linear in evidence rows
/// under a fixed budget) runs on every `cargo test` in `tests/evidence_search_bounds.rs`.
#[test]
#[ignore = "wall-clock SLO gate: run with --release -- --ignored --nocapture (see module docs)"]
fn max_candidate_evidence_validation_meets_local_search_slo() {
    /// At least 20 samples before a p95 verdict — the same floor memory-bench applies.
    const SAMPLES: usize = 21;
    let mut conn = shogun_memory::open_in_memory().unwrap();
    for ordinal in 0..100 {
        evidence_heavy_meeting(&mut conn, ordinal, 30);
    }

    let mut elapsed_micros = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let result = search_federated(
            &conn,
            "routine evidence",
            None,
            20_000,
            &FederatedSearchOptions::default(),
            100,
        )
        .unwrap();
        assert!(
            !result.hits.is_empty(),
            "adversarial fixture must produce hits"
        );
        assert!(
            result.hits.iter().all(|hit| hit.answer_eligible),
            "every candidate has complete persisted evidence"
        );
        elapsed_micros.push(result.elapsed_micros);
    }
    let (p50, p95) = (
        nearest_rank(&mut elapsed_micros, 50),
        nearest_rank(&mut elapsed_micros, 95),
    );
    eprintln!(
        "federated evidence SLO ({SAMPLES} samples, {profile}): p50={p50}us p95={p95}us",
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );

    assert!(p95 < 500_000, "local search SLO p50={p50}us p95={p95}us");
}

/// #391 latency gate: the imported-claim lane (`instr` over `memory_import_items`, no FTS table) at
/// its worst case — 20,000 committed claims, every one matching the query — on top of the evidence
/// fixture above. The lane alone and the whole federated search must stay inside NFR-SLO-04.
///
/// Explicit, not part of the default run (see the module docs): in a parallel
/// `cargo test --workspace` it failed at p95 544–675 ms while the same build measured ~260 ms
/// alone. The deterministic half (lane capped at `limit`, linear VM work per claim, imported never
/// answer-eligible, observed first, delete removes) runs on every `cargo test` in
/// `tests/imported_search_bounds.rs`.
#[test]
#[ignore = "wall-clock SLO gate: run with --release -- --ignored --nocapture (see module docs)"]
fn imported_claim_lane_meets_local_search_slo_at_twenty_thousand_items() {
    /// At least 20 samples before a p95 verdict — the same floor memory-bench applies.
    const SAMPLES: usize = 21;
    let mut conn = shogun_memory::open_in_memory().unwrap();
    for ordinal in 0..100 {
        evidence_heavy_meeting(&mut conn, ordinal, 30);
    }
    support::insert_matching_imported_claims(&mut conn, 40, 500);

    let terms = shogun_memory::search::lexical_terms("routine evidence");
    let mut lane_micros = Vec::with_capacity(SAMPLES);
    let mut total_micros = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = std::time::Instant::now();
        let lane = shogun_memory::memory_import::search_imported(&conn, &terms, 0.5, 100).unwrap();
        lane_micros.push(started.elapsed().as_micros() as u64);
        assert_eq!(lane.len(), 100);

        let result = search_federated(
            &conn,
            "routine evidence",
            None,
            20_000,
            &FederatedSearchOptions::default(),
            100,
        )
        .unwrap();
        assert!(!result.hits.is_empty());
        total_micros.push(result.elapsed_micros);
    }
    let (lane_p50, lane_p95) = (
        nearest_rank(&mut lane_micros, 50),
        nearest_rank(&mut lane_micros, 95),
    );
    let (p50, p95) = (
        nearest_rank(&mut total_micros, 50),
        nearest_rank(&mut total_micros, 95),
    );
    eprintln!(
        "imported lane (20k items, {SAMPLES} samples, {profile}): lane p50={lane_p50}us \
         p95={lane_p95}us; federated p50={p50}us p95={p95}us",
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    assert!(p95 < 500_000, "local search SLO p50={p50}us p95={p95}us");
}

/// #727 latency gate: a Visual Recall timeline request (the total count plus one 80-row page, as the
/// desktop and REST surfaces issue them) at 100, 1,000 and 5,000 retained frames, on an encrypted
/// on-disk database with realistic JPEG payloads (so each frame row spills into overflow pages)
/// and a 50k-event full-text index behind it. Must stay inside NFR-SLO-04 at every size.
///
/// The full-text filter runs its MATCH once per request; once per frame (~0.37 ms each) this
/// request would take ~2 s at ~1k frames. The deterministic half (parity with that per-frame
/// form, and a plan that reads `event_fts` once and uncorrelated) runs on every `cargo test` in
/// `src/screen_frames_timeline_tests.rs`. Explicit, not part of the default run (see the module
/// docs); building the fixture takes a minute or two in release.
#[test]
#[ignore = "wall-clock SLO gate: run with --release -- --ignored --nocapture (see module docs)"]
fn visual_recall_timeline_meets_local_search_slo_at_five_thousand_frames() {
    use shogun_memory::screen_frames::{
        timeline_count, timeline_page, TimelineDirection, TimelineQuery,
    };
    /// 7 queries x 3 timed runs each; at least 20 samples before a p95 verdict.
    const RUNS: usize = 3;
    const PAGE: usize = 80;
    let path = std::env::temp_dir().join(format!("shogun_vr727_slo_{}.db", std::process::id()));
    let remove = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    };
    remove();
    let conn = shogun_memory::open_encrypted(&path, &shogun_memory::DbKey::new([0x72; 32])).unwrap();
    support::visual_recall_corpus(&conn, 50_000, 5_000);

    // Common, mid, rare, multi-term OR, CJK, window title, and no match.
    let queries = ["status", "renewal pricing", "zephyrquota", "customer invoice schedule",
        "請求書の再発行", "Terminal", "zzqqxxnomatch"];
    for frames in [5_000_i64, 1_000, 100] {
        conn.execute(
            "DELETE FROM screen_frames WHERE id NOT IN (
                 SELECT id FROM screen_frames ORDER BY created_at_ms DESC, id DESC LIMIT ?1)",
            [frames],
        )
        .unwrap();
        let mut micros = Vec::new();
        for query in queries {
            for run in 0..=RUNS {
                let started = std::time::Instant::now();
                let total = timeline_count(&conn, query, 0, i64::MAX, None).unwrap();
                let page = timeline_page(
                    &conn,
                    &TimelineQuery {
                        query,
                        from_ms: 0,
                        to_ms: i64::MAX,
                        app_bundle_id: None,
                        cursor: None,
                        direction: TimelineDirection::Older,
                        limit: PAGE + 1,
                    },
                )
                .unwrap();
                let elapsed = started.elapsed().as_micros() as u64;
                assert!(page.len() as i64 <= total.min(PAGE as i64 + 1));
                // The first run of each query warms the page cache and is not timed.
                if run > 0 {
                    micros.push(elapsed);
                }
            }
        }
        let n = micros.len();
        let (p50, p95) = (nearest_rank(&mut micros, 50), nearest_rank(&mut micros, 95));
        eprintln!(
            "visual recall timeline ({frames} frames, {n} samples, {profile}): p50={p50}us p95={p95}us",
            profile = if cfg!(debug_assertions) { "debug" } else { "release" }
        );
        assert!(p95 < 500_000, "local search SLO at {frames} frames: p50={p50}us p95={p95}us");
    }
    drop(conn);
    remove();
}

/// #727 before/after: the count and the first 80-row page timed separately, for the shipped
/// once-per-request statements and for the pre-#727 per-frame `EXISTS (… MATCH …)` they replaced,
/// at 100 / 1,000 / 5,000 frames on the same corpus as the gate above. A report, not a gate: the
/// "before" side is slow by construction (seconds per request at 5k frames), so it runs fewer
/// timed samples, and each "before" result is checked against "after" so the comparison is like
/// for like. The corpus holds no self-captures, so the pre-#727 SQL (which predates the
/// self-capture filter) returns the same rows.
#[test]
#[ignore = "wall-clock measurement: run with --release -- --ignored --nocapture (minutes)"]
fn visual_recall_timeline_before_and_after_727() {
    use shogun_memory::screen_frames::{
        timeline_count, timeline_page, TimelineDirection, TimelineQuery,
    };
    const RUNS: usize = 3;
    const LEGACY_RUNS: usize = 2;
    const PAGE: i64 = 80;
    const LEGACY_COUNT: &str = "SELECT count(*) FROM screen_frames f
         WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
           AND (?3 IS NULL OR f.app_bundle_id = ?3)
           AND EXISTS (
               SELECT 1 FROM event_fts
               WHERE event_fts MATCH ?4
                 AND event_fts.rowid IN (f.event_id, f.accessibility_event_id)
           )";
    // The pre-#727 Older page with no cursor (?4/?5 NULL), summary columns and joins as shipped.
    const LEGACY_PAGE: &str = "SELECT f.id, f.created_at_ms, f.event_id, f.accessibility_event_id,
             f.app_bundle_id, f.window_title, f.display_id, f.width, f.height, length(f.bytes),
             coalesce(e.content, ''), coalesce(e.source, 'screen_ocr'), coalesce(ax.content, '')
         FROM screen_frames f
         LEFT JOIN event_log e ON e.id = f.event_id
         LEFT JOIN event_log ax ON ax.id = f.accessibility_event_id
         WHERE f.created_at_ms >= ?1 AND f.created_at_ms <= ?2
           AND (?3 IS NULL OR f.app_bundle_id = ?3)
           AND (?4 IS NULL OR f.created_at_ms < ?4 OR (f.created_at_ms = ?4 AND f.id < ?5))
           AND EXISTS (
               SELECT 1 FROM event_fts
               WHERE event_fts MATCH ?6
                 AND event_fts.rowid IN (f.event_id, f.accessibility_event_id)
           )
         ORDER BY f.created_at_ms DESC, f.id DESC LIMIT ?7";
    let path = std::env::temp_dir().join(format!("shogun_vr727_ab_{}.db", std::process::id()));
    let remove = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    };
    remove();
    let conn = shogun_memory::open_encrypted(&path, &shogun_memory::DbKey::new([0x72; 32])).unwrap();
    support::visual_recall_corpus(&conn, 50_000, 5_000);
    let queries = ["status", "renewal pricing", "zephyrquota", "customer invoice schedule",
        "請求書の再発行", "Terminal", "zzqqxxnomatch"];
    let timed = |f: &mut dyn FnMut()| {
        let started = std::time::Instant::now();
        f();
        started.elapsed().as_micros() as u64
    };
    let report = |frames: i64, label: &str, samples: &mut Vec<u64>| {
        let n = samples.len();
        let (p50, p95) = (nearest_rank(samples, 50), nearest_rank(samples, 95));
        eprintln!("vr727 {frames:>5} frames  {label:<12} n={n:<3} p50={p50:>9}us p95={p95:>9}us");
    };
    for frames in [5_000_i64, 1_000, 100] {
        conn.execute(
            "DELETE FROM screen_frames WHERE id NOT IN (
                 SELECT id FROM screen_frames ORDER BY created_at_ms DESC, id DESC LIMIT ?1)",
            [frames],
        )
        .unwrap();
        let [mut count_after, mut page_after, mut count_before, mut page_before] =
            [(); 4].map(|()| Vec::new());
        for query in queries {
            let expr = shogun_memory::search::fts_query(query).unwrap();
            let request = TimelineQuery {
                query,
                from_ms: 0,
                to_ms: i64::MAX,
                app_bundle_id: None,
                cursor: None,
                direction: TimelineDirection::Older,
                limit: PAGE as usize + 1,
            };
            // Untimed warm-up: page cache, statement shapes.
            let total = timeline_count(&conn, query, 0, i64::MAX, None).unwrap();
            let ids: Vec<i64> =
                timeline_page(&conn, &request).unwrap().iter().map(|f| f.id).collect();
            for _ in 0..RUNS {
                count_after.push(timed(&mut || {
                    timeline_count(&conn, query, 0, i64::MAX, None).unwrap();
                }));
                page_after.push(timed(&mut || {
                    timeline_page(&conn, &request).unwrap();
                }));
            }
            for _ in 0..LEGACY_RUNS {
                let mut legacy_total = 0_i64;
                count_before.push(timed(&mut || {
                    legacy_total = conn
                        .query_row(
                            LEGACY_COUNT,
                            rusqlite::params![0, i64::MAX, None::<&str>, expr],
                            |r| r.get(0),
                        )
                        .unwrap();
                }));
                assert_eq!(legacy_total, total, "count parity: {query:?}");
                let mut legacy_ids = Vec::new();
                page_before.push(timed(&mut || {
                    let mut stmt = conn.prepare(LEGACY_PAGE).unwrap();
                    legacy_ids = stmt
                        .query_map(
                            rusqlite::params![
                                0,
                                i64::MAX,
                                None::<&str>,
                                None::<i64>,
                                None::<i64>,
                                expr,
                                PAGE + 1
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                }));
                assert_eq!(legacy_ids, ids, "page parity: {query:?}");
            }
        }
        report(frames, "count before", &mut count_before);
        report(frames, "count after", &mut count_after);
        report(frames, "page before", &mut page_before);
        report(frames, "page after", &mut page_after);
    }
    drop(conn);
    remove();
}

/// #798: the list/get readers used by context assembly revalidate retained quotes live.
/// Synthetic, warm in-memory storage; this measures the readers, not the full UI/cache SLO.
#[test]
#[ignore = "wall-clock measurement: run in isolation with --ignored --nocapture"]
fn quoted_state_readers_latency() {
    use rusqlite::params;
    use shogun_memory::state;
    use std::time::Instant;

    let mut conn = shogun_memory::open_in_memory().unwrap();
    let quote = "I will send the final deck after legal approval";
    let content = format!("{} {quote}", "Background capture text. ".repeat(683));
    let tx = conn.transaction().unwrap();
    for id in 1..=3 {
        tx.execute(
            "INSERT INTO event_log (id,ts,source,kind,content,content_hash,last_seen_at)
             VALUES (?1,1,'capture','text',?2,?3,1)",
            params![id, content, format!("state-perf-{id}")],
        ).unwrap();
    }
    for id in 1..=250 {
        tx.execute(
            "INSERT INTO commitments (id,direction,description,status,confidence,base_confidence,
             created_at,updated_at,last_evidence_at) VALUES (?1,'mine',?2,'open',0.7,0.7,1,1,1)",
            params![id, format!("Send deck {id}")],
        ).unwrap();
        tx.execute(
            "INSERT INTO open_loops (id,kind,description,status,opened_at,confidence,base_confidence,
             created_at,updated_at,last_evidence_at) VALUES (?1,'follow_up',?2,'open',1,0.7,0.7,1,1,1)",
            params![id, format!("Review deck {id}")],
        ).unwrap();
        for table in ["commitments", "open_loops"] {
            tx.execute(
                "INSERT INTO state_provenance (state_table,state_id,event_id,weight,quote)
                 SELECT ?1,?2,id,1,?3 FROM event_log",
                params![table, id, quote],
            ).unwrap();
        }
    }
    tx.commit().unwrap();
    for (label, retained, expected) in [
        ("exact", quote.to_owned(), 250),
        ("whitespace", quote.replace(' ', "\n"), 250),
        ("unsupported", "The removed quotation is no longer here".to_owned(), 0),
    ] {
        conn.execute("UPDATE state_provenance SET quote=?1", [retained]).unwrap();
        let mut samples = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            assert_eq!(state::list_commitments(&conn).unwrap().len(), expected);
            assert_eq!(state::list_open_loops(&conn).unwrap().len(), expected);
            assert_eq!(state::get_commitment(&conn, 1).unwrap().is_some(), expected != 0);
            samples.push(start.elapsed().as_micros() as u64);
        }
        let p50 = nearest_rank(&mut samples, 50);
        let p95 = nearest_rank(&mut samples, 95);
        eprintln!("quoted state readers: {label}, 500 rows, 3 links/row, 16KiB sources, 21 samples: p50={p50}us p95={p95}us");
    }
}

/// Nearest-rank percentile (the definition `spike_harness::stats` uses for every other p95).
fn nearest_rank(samples: &mut [u64], percentile: usize) -> u64 {
    samples.sort_unstable();
    let rank = (percentile * samples.len()).div_ceil(100).max(1);
    samples[rank - 1]
}
