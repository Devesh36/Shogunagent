//! Maintenance of the V81 gram index (`event_grams_fts`).
//!
//! Every function here is best-effort for the caller: the event row is the durable record and the
//! gram row is derived from it, so a failure leaves the id in `event_grams_pending` for the next
//! pass instead of failing a capture write.

use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};

use super::terms::index_grams;

/// Rows read per back-fill / drain statement.
const BATCH: usize = 256;

/// Write (or rewrite) the grams of one event and clear it from the pending queue.
pub(crate) fn index_event(
    conn: &Connection,
    event_id: i64,
    window_title: Option<&str>,
    content: &str,
) -> Result<(), rusqlite::Error> {
    let grams = index_grams(window_title, content);
    // Delete-then-insert keeps this idempotent: the back-fill may meet a row the write path or the
    // pending drain already indexed.
    conn.execute("DELETE FROM event_grams_fts WHERE rowid = ?1", [event_id])?;
    if !grams.is_empty() {
        conn.execute(
            "INSERT INTO event_grams_fts (rowid, grams) VALUES (?1, ?2)",
            params![event_id, grams],
        )?;
    }
    conn.execute("DELETE FROM event_grams_pending WHERE event_id = ?1", [event_id])?;
    Ok(())
}

/// What one maintenance pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GramIndexReport {
    pub pending_indexed: usize,
    pub backfilled: usize,
    /// True once every pre-V81 row has been visited.
    pub backfill_done: bool,
}

/// Drain the pending queue, then back-fill older history newest-first, within `budget`.
///
/// Bounded by wall time and by statement size, so it can run inside the hourly local maintenance
/// pass without holding the connection for long. Safe to interrupt anywhere: the cursor only
/// moves after a batch is indexed.
pub fn maintain_gram_index(
    conn: &Connection,
    budget: Duration,
) -> Result<GramIndexReport, rusqlite::Error> {
    let started = Instant::now();
    let mut report = GramIndexReport::default();
    let Some(()) = table_present(conn)? else {
        return Ok(report);
    };

    // 1. Pending ids: new rows from a path that did not index inline, and edited rows.
    while started.elapsed() < budget {
        let ids = {
            let mut stmt = conn.prepare(
                "SELECT p.event_id, e.window_title, e.content
                   FROM event_grams_pending p
                   LEFT JOIN event_log e ON e.id = p.event_id
                  ORDER BY p.event_id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([BATCH as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if ids.is_empty() {
            break;
        }
        let tx = conn.unchecked_transaction()?;
        for (id, title, content) in &ids {
            match content {
                Some(content) => index_event(&tx, *id, title.as_deref(), content)?,
                // The event is gone (deleted between queueing and now): just forget it.
                None => {
                    tx.execute("DELETE FROM event_grams_pending WHERE event_id = ?1", [id])?;
                }
            }
        }
        tx.commit()?;
        report.pending_indexed += ids.len();
    }

    // 2. History that predates V81, newest first.
    loop {
        let next: i64 = conn
            .query_row(
                "SELECT next_event_id FROM event_grams_backfill WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if next <= 0 {
            report.backfill_done = true;
            break;
        }
        if started.elapsed() >= budget {
            break;
        }
        let rows = {
            let mut stmt = conn.prepare(
                "SELECT id, window_title, content FROM event_log
                  WHERE id <= ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![next, BATCH as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let tx = conn.unchecked_transaction()?;
        let lowest = rows.last().map_or(0, |(id, _, _)| *id);
        for (id, title, content) in &rows {
            index_event(&tx, *id, title.as_deref(), content)?;
        }
        let next_cursor = if rows.len() < BATCH { 0 } else { lowest - 1 };
        tx.execute(
            "UPDATE event_grams_backfill SET next_event_id = ?1 WHERE id = 1",
            [next_cursor.max(0)],
        )?;
        tx.commit()?;
        report.backfilled += rows.len();
    }
    Ok(report)
}

/// `Some(())` when V81 is applied. A face reading an older schema must not fail on the lane.
fn table_present(conn: &Connection) -> Result<Option<()>, rusqlite::Error> {
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'event_grams_pending'",
        [],
        |row| row.get(0),
    )?;
    Ok((present > 0).then_some(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert, NewEvent};

    fn add(conn: &Connection, content: &str, hash: &str) -> i64 {
        insert(
            conn,
            &NewEvent {
                ts: 10,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: Some("メモ"),
                content,
                content_hash: hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn matching(conn: &Connection, gram: &str) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT rowid FROM event_grams_fts WHERE event_grams_fts MATCH ?1 ORDER BY rowid")
            .unwrap();
        stmt.query_map([format!("\"{gram}\"")], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<i64>, _>>()
            .unwrap()
    }

    fn pending(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM event_grams_pending", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn the_write_path_indexes_inline_and_leaves_nothing_pending() {
        let conn = crate::open_in_memory().unwrap();
        let id = add(&conn, "明日の打ち合わせは会議室Bで", "grams-inline");
        assert_eq!(matching(&conn, "会議"), vec![id]);
        assert_eq!(matching(&conn, "打合"), vec![id]);
        assert_eq!(pending(&conn), 0);
    }

    #[test]
    fn deleting_an_event_removes_its_grams() {
        let conn = crate::open_in_memory().unwrap();
        let id = add(&conn, "予算の承認", "grams-delete");
        assert_eq!(matching(&conn, "予算"), vec![id]);
        conn.execute("DELETE FROM event_log WHERE id = ?1", [id]).unwrap();
        assert!(matching(&conn, "予算").is_empty());
        conn.execute("INSERT INTO event_grams_fts(event_grams_fts) VALUES ('integrity-check')", [])
            .unwrap();
    }

    #[test]
    fn an_edit_requeues_and_maintenance_reindexes() {
        let conn = crate::open_in_memory().unwrap();
        let id = add(&conn, "請求書の確認", "grams-edit");
        conn.execute("UPDATE event_log SET content = '契約書の確認' WHERE id = ?1", [id])
            .unwrap();
        assert!(matching(&conn, "請求").is_empty(), "stale grams leave with the edit");
        assert_eq!(pending(&conn), 1);
        let report = maintain_gram_index(&conn, Duration::from_secs(5)).unwrap();
        assert_eq!(report.pending_indexed, 1);
        assert_eq!(matching(&conn, "契約"), vec![id]);
        assert_eq!(pending(&conn), 0);
    }

    #[test]
    fn rows_written_by_raw_sql_are_queued_then_indexed() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1, 'capture', 'text', '出張の予定', 'raw-grams', 1)",
            [],
        )
        .unwrap();
        assert_eq!(pending(&conn), 1);
        maintain_gram_index(&conn, Duration::from_secs(5)).unwrap();
        assert_eq!(matching(&conn, "出張").len(), 1);
    }

    #[test]
    fn backfill_walks_old_history_newest_first_and_finishes() {
        let conn = crate::open_in_memory().unwrap();
        let ids = (0..600)
            .map(|i| add(&conn, &format!("障害報告 {i}"), &format!("grams-old-{i}")))
            .collect::<Vec<_>>();
        // Simulate rows that predate V81: no grams, cursor at the newest id.
        conn.execute("DELETE FROM event_grams_fts", []).unwrap();
        conn.execute(
            "UPDATE event_grams_backfill SET next_event_id = ?1 WHERE id = 1",
            [ids[ids.len() - 1]],
        )
        .unwrap();
        let report = maintain_gram_index(&conn, Duration::from_secs(30)).unwrap();
        assert_eq!(report.backfilled, 600);
        assert!(report.backfill_done);
        assert_eq!(matching(&conn, "障害").len(), 600);
        // A second pass is a no-op.
        let again = maintain_gram_index(&conn, Duration::from_secs(5)).unwrap();
        assert_eq!(again.backfilled, 0);
        assert!(again.backfill_done);
    }

    #[test]
    fn a_zero_budget_does_no_work_and_keeps_the_cursor() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, "会議", "grams-budget");
        conn.execute("UPDATE event_grams_backfill SET next_event_id = 1 WHERE id = 1", [])
            .unwrap();
        let report = maintain_gram_index(&conn, Duration::ZERO).unwrap();
        assert_eq!(report.backfilled, 0);
        assert!(!report.backfill_done);
    }
}
