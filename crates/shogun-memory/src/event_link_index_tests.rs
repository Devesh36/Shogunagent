//! V68 (#780): the lookups plan through an index instead of scanning whole tables. Checked on the
//! shipped SQLite build with `EXPLAIN QUERY PLAN`, since the planner is what decides.

use rusqlite::Connection;

fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn assert_uses(conn: &Connection, sql: &str, index: &str) {
    let rows = plan(conn, sql);
    assert!(
        rows.iter().any(|row| row.contains(index)),
        "{sql}\nexpected {index} in the plan, got {rows:#?}"
    );
    assert!(
        !rows.iter().any(|row| row.starts_with("SCAN ")),
        "{sql}\nno full scan expected, got {rows:#?}"
    );
}

#[test]
fn frame_lookup_by_event_uses_its_index() {
    let conn = crate::open_in_memory().unwrap();
    assert_uses(&conn, "SELECT id FROM screen_frames WHERE event_id = 42", "idx_screen_frames_event");
}

#[test]
fn summary_provenance_lookup_by_event_uses_its_index() {
    let conn = crate::open_in_memory().unwrap();
    assert_uses(
        &conn,
        "SELECT summary_id FROM period_summary_event_provenance WHERE event_id = 42",
        "idx_period_summary_event_provenance_event",
    );
}

/// The V22 triggers' condition, with a literal standing in for `new.ts`.
#[test]
fn containing_period_lookup_reads_by_period_end() {
    let conn = crate::open_in_memory().unwrap();
    let ts = 1_800_000_000_000_i64;
    assert_uses(
        &conn,
        &format!(
            "SELECT id FROM period_summaries WHERE {ts} >= period_start_ms AND {ts} < period_end_ms"
        ),
        "idx_period_summaries_end",
    );
    assert_uses(
        &conn,
        &format!(
            "SELECT id FROM period_summary_shard_runs WHERE {ts} >= period_start_ms AND {ts} < period_end_ms"
        ),
        "idx_period_summary_shard_runs_end",
    );
}

/// The indexes only change how the children are found, not what the cascade removes.
#[test]
fn deleting_an_event_still_reaches_its_children() {
    let conn = crate::open_in_memory().unwrap();
    let event = crate::event_log::insert(
        &conn,
        &crate::event_log::NewEvent {
            ts: 1000,
            source: "screen_ocr",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "x",
            content_hash: "h7",
            dwell_ms: 1,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO period_summaries (id, level, period_start_ms, period_end_ms, title, content,
             generation_mode, generated, content_digest, generated_at_ms, created_at_ms, updated_at_ms)
             VALUES (1, 'hour', 0, 3600000, 't', 'c', 'extractive', 0, 'd', 1, 1, 1);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO period_summary_event_provenance (summary_id, event_id) VALUES (1, ?1)",
        [event],
    )
    .unwrap();
    conn.execute("DELETE FROM event_log WHERE id = ?1", [event]).unwrap();
    let left: i64 = conn
        .query_row("SELECT count(*) FROM period_summary_event_provenance", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

/// Measurement for the PR (CLAUDE.md: latency changes ship with their numbers). Deletes 500 of
/// 20,000 events that each carry a frame and a summary-provenance row, with and without the V68
/// indexes, and prints both. Run with
/// `cargo test -p shogun-memory --release --lib v68_delete_timing -- --ignored --nocapture`.
#[test]
#[ignore = "timing harness, run by hand"]
fn v68_delete_timing() {
    fn seeded() -> Connection {
        let conn = crate::open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO period_summaries (id, level, period_start_ms, period_end_ms, title, content,
                 generation_mode, generated, content_digest, generated_at_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'day', 0, 86400000000, 't', 'c', 'extractive', 0, 'd', 1, 1, 1);",
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..20_000_i64 {
            let event = crate::event_log::insert(
                &tx,
                &crate::event_log::NewEvent {
                    ts: 1_000 + i,
                    source: "screen_ocr",
                    kind: "text",
                    app_bundle_id: None,
                    window_title: None,
                    content: "x",
                    content_hash: &format!("h{i}"),
                    dwell_ms: 1,
                    display_id: None,
                    window_bounds: None,
                },
            )
            .unwrap();
            tx.execute(
                "INSERT INTO screen_frames (created_at_ms, event_id, width, height, bytes)
                 VALUES (?1, ?2, 1, 1, x'00')",
                [1_000 + i, event],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO period_summary_event_provenance (summary_id, event_id) VALUES (1, ?1)",
                [event],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        conn
    }
    let delete_last = |conn: &Connection| {
        let started = std::time::Instant::now();
        // The product's order: the frames go first, then their events (whose provenance cascades).
        let since = 1_000 + 19_500_i64;
        conn.execute(
            "DELETE FROM screen_frames WHERE event_id IN (SELECT id FROM event_log WHERE ts >= ?1)",
            [since],
        )
        .unwrap();
        conn.execute("DELETE FROM event_log WHERE ts >= ?1", [since]).unwrap();
        started.elapsed()
    };
    let with = seeded();
    let fast = delete_last(&with);
    let without = seeded();
    without
        .execute_batch(
            "DROP INDEX idx_screen_frames_event;
             DROP INDEX idx_period_summary_event_provenance_event;
             DROP INDEX idx_period_summaries_end;
             DROP INDEX idx_period_summary_shard_runs_end;",
        )
        .unwrap();
    let slow = delete_last(&without);
    println!("delete 500 of 20,000 events: with V68 {fast:?}, without {slow:?}");
}
