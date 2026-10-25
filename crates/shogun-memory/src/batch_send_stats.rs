//! Per-night Batch send telemetry (V54, issue #286 / OPEN-B2). **Counts only.**
//!
//! One row per (Dream cycle, Batch job): what the deterministic pre-filter did to the night's
//! window and the estimated input tokens with and without it. The table has no column that can
//! hold captured text (CLAUDE.md telemetry rule), and [`line_drops_by_rule`](SendStatsRow) is a
//! JSON object of fixed rule names to integers, built here from typed pairs.

use rusqlite::{params, Connection, OptionalExtension};

/// One job's send telemetry for one night.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendStatsRow {
    pub cycle_id: String,
    /// The Batch job, e.g. `consolidation`.
    pub job: String,
    pub recorded_at: i64,
    pub items_considered: i64,
    pub items_sent: i64,
    pub items_dropped_empty: i64,
    pub items_dropped_duplicate: i64,
    pub items_dropped_near_duplicate: i64,
    pub items_dropped_chrome_only: i64,
    pub lines_considered: i64,
    pub lines_dropped: i64,
    /// `(rule, lines dropped)` in the writer's fixed order.
    pub line_drops_by_rule: Vec<(String, i64)>,
    pub chars_before: i64,
    pub chars_after: i64,
    pub est_tokens_before: i64,
    pub est_tokens_after: i64,
}

impl SendStatsRow {
    /// Share of estimated input tokens kept on the device, in `[0, 1]`; 0 when nothing would
    /// have been sent. Mirrors `shogun_core::dreamcycle::prefilter::reduction_rate`.
    pub fn reduction_rate(&self) -> f64 {
        if self.est_tokens_before <= 0 || self.est_tokens_after >= self.est_tokens_before {
            return 0.0;
        }
        (self.est_tokens_before - self.est_tokens_after.max(0)) as f64
            / self.est_tokens_before as f64
    }

    /// Items dropped for any reason.
    pub fn items_dropped(&self) -> i64 {
        self.items_dropped_empty
            + self.items_dropped_duplicate
            + self.items_dropped_near_duplicate
            + self.items_dropped_chrome_only
    }
}

fn rules_json(rules: &[(String, i64)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = rules
        .iter()
        .map(|(rule, count)| (rule.clone(), serde_json::Value::from(*count)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn rules_from_json(text: &str) -> Vec<(String, i64)> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(rule, count)| count.as_i64().map(|c| (rule, c)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Insert or replace the row for `(cycle_id, job)`. A retried submission of the same frozen
/// window rewrites its own row; it never adds a second one.
pub fn upsert(conn: &Connection, row: &SendStatsRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO dream_batch_send_stats (
            cycle_id, job, recorded_at, items_considered, items_sent, items_dropped_empty,
            items_dropped_duplicate, items_dropped_near_duplicate, items_dropped_chrome_only,
            lines_considered, lines_dropped, line_drops_by_rule, chars_before, chars_after,
            est_tokens_before, est_tokens_after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT (cycle_id, job) DO UPDATE SET
            recorded_at = excluded.recorded_at,
            items_considered = excluded.items_considered,
            items_sent = excluded.items_sent,
            items_dropped_empty = excluded.items_dropped_empty,
            items_dropped_duplicate = excluded.items_dropped_duplicate,
            items_dropped_near_duplicate = excluded.items_dropped_near_duplicate,
            items_dropped_chrome_only = excluded.items_dropped_chrome_only,
            lines_considered = excluded.lines_considered,
            lines_dropped = excluded.lines_dropped,
            line_drops_by_rule = excluded.line_drops_by_rule,
            chars_before = excluded.chars_before,
            chars_after = excluded.chars_after,
            est_tokens_before = excluded.est_tokens_before,
            est_tokens_after = excluded.est_tokens_after",
        params![
            row.cycle_id,
            row.job,
            row.recorded_at,
            row.items_considered,
            row.items_sent,
            row.items_dropped_empty,
            row.items_dropped_duplicate,
            row.items_dropped_near_duplicate,
            row.items_dropped_chrome_only,
            row.lines_considered,
            row.lines_dropped,
            rules_json(&row.line_drops_by_rule),
            row.chars_before,
            row.chars_after,
            row.est_tokens_before,
            row.est_tokens_after,
        ],
    )?;
    Ok(())
}

const COLUMNS: &str = "cycle_id, job, recorded_at, items_considered, items_sent,
    items_dropped_empty, items_dropped_duplicate, items_dropped_near_duplicate,
    items_dropped_chrome_only, lines_considered, lines_dropped, line_drops_by_rule, chars_before,
    chars_after, est_tokens_before, est_tokens_after";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SendStatsRow> {
    Ok(SendStatsRow {
        cycle_id: r.get(0)?,
        job: r.get(1)?,
        recorded_at: r.get(2)?,
        items_considered: r.get(3)?,
        items_sent: r.get(4)?,
        items_dropped_empty: r.get(5)?,
        items_dropped_duplicate: r.get(6)?,
        items_dropped_near_duplicate: r.get(7)?,
        items_dropped_chrome_only: r.get(8)?,
        lines_considered: r.get(9)?,
        lines_dropped: r.get(10)?,
        line_drops_by_rule: rules_from_json(&r.get::<_, String>(11)?),
        chars_before: r.get(12)?,
        chars_after: r.get(13)?,
        est_tokens_before: r.get(14)?,
        est_tokens_after: r.get(15)?,
    })
}

/// The row for one cycle and job, if recorded.
pub fn get(conn: &Connection, cycle_id: &str, job: &str) -> rusqlite::Result<Option<SendStatsRow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM dream_batch_send_stats WHERE cycle_id = ?1 AND job = ?2"),
        params![cycle_id, job],
        from_row,
    )
    .optional()
}

/// The most recent `limit` rows, newest first (ties broken by cycle id, then job).
pub fn recent(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<SendStatsRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM dream_batch_send_stats
          ORDER BY recorded_at DESC, cycle_id DESC, job ASC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit.min(i64::MAX as usize) as i64], from_row)?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cycle: &str, at: i64, before: i64, after: i64) -> SendStatsRow {
        SendStatsRow {
            cycle_id: cycle.into(),
            job: "consolidation".into(),
            recorded_at: at,
            items_considered: 10,
            items_sent: 6,
            items_dropped_empty: 1,
            items_dropped_duplicate: 2,
            items_dropped_near_duplicate: 1,
            items_dropped_chrome_only: 0,
            lines_considered: 100,
            lines_dropped: 40,
            line_drops_by_rule: vec![("blank".into(), 30), ("repeated_label".into(), 10)],
            chars_before: 4_000,
            chars_after: 2_500,
            est_tokens_before: before,
            est_tokens_after: after,
        }
    }

    #[test]
    fn upsert_round_trips_and_replaces_its_own_row() {
        let conn = crate::open_in_memory().unwrap();
        upsert(&conn, &row("20260925", 1, 1_000, 600)).unwrap();
        upsert(&conn, &row("20260925", 2, 1_000, 500)).unwrap();
        let got = get(&conn, "20260925", "consolidation").unwrap().unwrap();
        assert_eq!(got, row("20260925", 2, 1_000, 500));
        assert!((got.reduction_rate() - 0.5).abs() < 1e-12);
        assert_eq!(got.items_dropped(), 4);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM dream_batch_send_stats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn recent_is_newest_first_and_bounded() {
        let conn = crate::open_in_memory().unwrap();
        for (i, cycle) in ["20260920", "20260921", "20260922"].iter().enumerate() {
            upsert(&conn, &row(cycle, i as i64, 100, 90)).unwrap();
        }
        let got = recent(&conn, 2).unwrap();
        assert_eq!(
            got.iter().map(|r| r.cycle_id.as_str()).collect::<Vec<_>>(),
            vec!["20260922", "20260921"]
        );
    }

    #[test]
    fn reduction_rate_is_zero_without_evidence_and_never_negative() {
        assert_eq!(row("c", 0, 0, 0).reduction_rate(), 0.0);
        assert_eq!(row("c", 0, 100, 120).reduction_rate(), 0.0);
    }

    #[test]
    fn the_table_has_no_column_for_text() {
        // Structural guarantee: every column is a count, a timestamp, an id or the rule map.
        let conn = crate::open_in_memory().unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(dream_batch_send_stats)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for banned in ["content", "text", "chunk", "line", "body", "excerpt"] {
            assert!(!cols.iter().any(|c| c == banned), "column {banned} must not exist");
        }
    }

    #[test]
    fn a_non_json_rule_map_is_refused() {
        let conn = crate::open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO dream_batch_send_stats (cycle_id, job, recorded_at, items_considered,
               items_sent, line_drops_by_rule, chars_before, chars_after, est_tokens_before,
               est_tokens_after) VALUES ('c', 'consolidation', 0, 0, 0, 'not json', 0, 0, 0, 0)",
            [],
        );
        assert!(err.is_err());
    }
}
