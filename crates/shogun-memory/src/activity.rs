//! Content-free dashboard activity derived from retained capture events.

use rusqlite::{params, Connection};

pub const TOP_APPS_PER_RANGE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    ScribeRewrite,
    InlineEdit,
    /// A reply draft (#279 TL-B) inserted into the field the user came from.
    ReplyDraftInsert,
}

impl EditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScribeRewrite => "scribe_rewrite",
            Self::InlineEdit => "inline_edit",
            Self::ReplyDraftInsert => "reply_draft_insert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppUsage {
    pub app_bundle_id: String,
    pub captured_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageTotals {
    pub apps: Vec<AppUsage>,
    pub rewrite_count: i64,
    pub inline_edit_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayUsage {
    pub local_date: String,
    pub captured_ms: i64,
}

/// Add one captured interval to a retained event. The caller splits local-day boundaries before
/// this write, so each row remains deletable with its canonical event. `completed_at_ms` is the
/// UTC completion instant used for timezone-independent range erasure.
pub fn record_capture_credits(
    conn: &Connection,
    event_id: i64,
    completed_at_ms: i64,
    credits: &[(String, i64)],
) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare(
        "INSERT INTO dashboard_activity_credits
             (event_id, local_date, captured_ms, latest_activity_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(event_id, local_date) DO UPDATE
            SET captured_ms = captured_ms + excluded.captured_ms,
                latest_activity_ms = max(latest_activity_ms, excluded.latest_activity_ms)",
    )?;
    for (local_date, captured_ms) in credits.iter().filter(|(_, ms)| *ms > 0) {
        statement.execute(params![event_id, local_date, captured_ms, completed_at_ms])?;
    }
    Ok(())
}

/// Record one terminal native text application. Failed, cancelled, and preview paths never call
/// this function, so an increment means a write landed. `completed_at_ms` keeps forget semantics
/// stable if the device timezone changes after recording.
pub fn record_edit(
    conn: &Connection,
    local_date: &str,
    kind: EditKind,
    completed_at_ms: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO dashboard_edit_counts (local_date, kind, count, latest_activity_ms)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(local_date, kind) DO UPDATE
            SET count = count + 1,
                latest_activity_ms = max(latest_activity_ms, excluded.latest_activity_ms)",
        params![local_date, kind.as_str(), completed_at_ms],
    )?;
    Ok(())
}

/// Usage in an inclusive local-date range. `from_date = None` includes retained rows from the
/// beginning, while `through_date` keeps future-dated rows out of dashboard payloads.
pub fn usage_totals(
    conn: &Connection,
    from_date: Option<&str>,
    through_date: &str,
) -> Result<UsageTotals, rusqlite::Error> {
    let limit = i64::try_from(TOP_APPS_PER_RANGE).unwrap_or(i64::MAX);
    let mut app_statement = conn.prepare(
        "SELECT event_log.app_bundle_id, sum(dashboard_activity_credits.captured_ms)
           FROM dashboard_activity_credits
           JOIN event_log ON event_log.id = dashboard_activity_credits.event_id
          WHERE (?1 IS NULL OR dashboard_activity_credits.local_date >= ?1)
            AND dashboard_activity_credits.local_date <= ?2
            AND event_log.app_bundle_id IS NOT NULL
          GROUP BY event_log.app_bundle_id
          ORDER BY sum(dashboard_activity_credits.captured_ms) DESC, event_log.app_bundle_id
          LIMIT ?3",
    )?;
    let apps = app_statement
        .query_map(params![from_date, through_date, limit], |row| {
            Ok(AppUsage {
                app_bundle_id: row.get(0)?,
                captured_ms: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let (rewrite_count, inline_edit_count) = conn.query_row(
        "SELECT
             COALESCE(sum(CASE WHEN kind = 'scribe_rewrite' THEN count END), 0),
             COALESCE(sum(CASE WHEN kind = 'inline_edit' THEN count END), 0)
           FROM dashboard_edit_counts
          WHERE (?1 IS NULL OR local_date >= ?1)
            AND local_date <= ?2",
        params![from_date, through_date],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(UsageTotals {
        apps,
        rewrite_count,
        inline_edit_count,
    })
}

/// Sparse daily data for a fixed calendar. Missing dates mean no retained captured credit.
pub fn day_usage_between(
    conn: &Connection,
    from_date: &str,
    through_date: &str,
) -> Result<Vec<DayUsage>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT local_date, sum(captured_ms)
           FROM dashboard_activity_credits
          WHERE local_date >= ?1 AND local_date <= ?2
          GROUP BY local_date
          ORDER BY local_date",
    )?;
    let rows = statement
        .query_map(params![from_date, through_date], |row| {
            Ok(DayUsage {
                local_date: row.get(0)?,
                captured_ms: row.get(1)?,
            })
        })?
        .collect();
    rows
}

/// Oldest retained date with captured activity. It deliberately says nothing about edit counters.
pub fn first_recorded_on(
    conn: &Connection,
    through_date: &str,
) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT min(local_date) FROM dashboard_activity_credits WHERE local_date <= ?1",
        [through_date],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(conn: &Connection, app: &str) -> Result<i64, rusqlite::Error> {
        conn.execute(
            "INSERT INTO event_log (ts, source, kind, app_bundle_id, content, content_hash, last_seen_at)
             VALUES (1, 'capture', 'text', ?1, 'x', ?1, 1)",
            [app],
        )?;
        Ok(conn.last_insert_rowid())
    }

    #[test]
    fn usage_joins_retained_events_and_cascades_on_erasure() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let safari = event(&conn, "com.apple.Safari")?;
        record_capture_credits(
            &conn,
            safari,
            1_000,
            &[("2026-09-08".to_string(), 2_000), ("2026-09-09".to_string(), 1_000)],
        )?;
        record_edit(&conn, "2026-09-09", EditKind::ScribeRewrite, 1_000)?;

        let total = usage_totals(&conn, None, "2026-09-09")?;
        assert_eq!(total.apps, vec![AppUsage { app_bundle_id: "com.apple.Safari".into(), captured_ms: 3_000 }]);
        assert_eq!(total.rewrite_count, 1);
        conn.execute("DELETE FROM event_log WHERE id = ?1", [safari])?;

        assert!(usage_totals(&conn, None, "2026-09-09")?.apps.is_empty());
        assert_eq!(first_recorded_on(&conn, "2026-09-09")?, None);
        assert_eq!(usage_totals(&conn, None, "2026-09-09")?.rewrite_count, 1);
        Ok(())
    }

    #[test]
    fn range_totals_and_daily_rows_are_bounded_and_content_free() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let safari = event(&conn, "com.apple.Safari")?;
        let mail = event(&conn, "com.apple.Mail")?;
        record_capture_credits(&conn, safari, 1_000, &[("2026-09-01".to_string(), 1_000)])?;
        record_capture_credits(&conn, mail, 2_000, &[("2026-09-08".to_string(), 2_000)])?;
        record_edit(&conn, "2026-09-08", EditKind::InlineEdit, 2_000)?;
        record_capture_credits(&conn, mail, 3_000, &[("2026-09-09".to_string(), 4_000)])?;
        record_edit(&conn, "2026-09-09", EditKind::InlineEdit, 3_000)?;

        let range = usage_totals(&conn, Some("2026-09-08"), "2026-09-08")?;
        assert_eq!(range.apps, vec![AppUsage { app_bundle_id: "com.apple.Mail".into(), captured_ms: 2_000 }]);
        assert_eq!(range.inline_edit_count, 1);
        assert_eq!(
            day_usage_between(&conn, "2026-09-08", "2026-09-08")?,
            vec![DayUsage { local_date: "2026-09-08".into(), captured_ms: 2_000 }]
        );
        assert_eq!(first_recorded_on(&conn, "2026-09-08")?, Some("2026-09-01".into()));
        Ok(())
    }
}
