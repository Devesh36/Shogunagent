//! Capture-log reads for the Memory API: open events by id (`memory.get_events`), and fold
//! consecutive capture rows from one app + window into sessions (`memory.read_session`,
//! `memory.get_context`). Text is the stored, already-sanitized event body, char-bounded in SQL.

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use super::Db;
use crate::when::when_json;

/// Session text cap, in chars (the latest distinct text of the session).
pub const SESSION_TEXT_CHARS: usize = 600;
/// Event ids a session offers to open (newest first).
const SESSION_OPEN_IDS: usize = 5;
/// Rows one `memory.read_session` call folds. More in the window → `truncated`.
const SESSION_ROW_CAP: usize = 5_000;
/// The newest rows `memory.get_context` folds into its sessions.
const CONTEXT_ROW_CAP: usize = 1_500;
/// Sessions `memory.get_context` lists after the current one.
pub const CONTEXT_RECENT_SESSIONS: usize = 12;
/// A capture this recent means capture is running now.
pub const LIVE_MS: i64 = 15_000;

/// The app-scoped capture sources (`event_log::APP_SCOPED_SOURCES`), as SQL. `+source` keeps
/// the planner on the `ts` index instead of scanning every capture row by source. A call for a
/// connection without screen recall (owner decision 2026-10-20) folds AX capture only, never the
/// OCR `screen_text` (`shogun_mcp::source_policy`).
fn capture_sources_sql() -> &'static str {
    shogun_mcp::source_policy::capture_sources_sql(shogun_mcp::source_policy::current())
}

/// One app-scoped capture row, the input to [`sessionize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRow {
    pub id: i64,
    pub ts: i64,
    pub last_seen_at: i64,
    pub app: Option<String>,
    pub window_title: Option<String>,
    pub text: String,
}

/// Consecutive rows from one (app, window_title).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub started: i64,
    pub ended: i64,
    pub app: Option<String>,
    pub window_title: Option<String>,
    pub visits: usize,
    /// The newest non-empty text in the session.
    pub text: String,
    /// Row ids, oldest first.
    pub ids: Vec<i64>,
}

/// A window title without the browser's churn: a leading `(3) ` unread count, and the
/// ` – Audio playing` / ` - High memory usage - 885 MB` markers (either dash) wherever they
/// sit. Only the merge key uses it; sessions keep the raw title.
fn title_key(title: &str) -> String {
    let mut t = title;
    if let Some(rest) = t.strip_prefix('(') {
        if let Some((count, after)) = rest.split_once(") ") {
            if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
                t = after;
            }
        }
    }
    let mut out = t.replace(" – Audio playing", "").replace(" - Audio playing", "");
    for dash in [" - ", " – "] {
        let marker = format!("{dash}High memory usage");
        while let Some(start) = out.find(&marker) {
            let rest = &out[start + marker.len()..];
            let Some(rest) = rest.strip_prefix(" - ").or_else(|| rest.strip_prefix(" – ")) else {
                break;
            };
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let Some(tail) = rest[digits..].strip_prefix(" MB").filter(|_| digits > 0) else {
                break;
            };
            let end = out.len() - tail.len();
            out.replace_range(start..end, "");
        }
    }
    out
}

fn same_window(s: &Session, row: &CaptureRow) -> bool {
    s.app == row.app
        && s.window_title.as_deref().map(title_key) == row.window_title.as_deref().map(title_key)
}

/// Fold oldest-first rows: consecutive rows with the same app and window title (compared by
/// [`title_key`]) merge, so no two adjacent sessions share both. `ended` is the latest `last_seen_at` (a capture that repeats
/// unchanged text touches its row instead of adding one).
pub fn sessionize(rows: &[CaptureRow]) -> Vec<Session> {
    let mut sessions: Vec<Session> = Vec::new();
    for row in rows {
        let seen = row.last_seen_at.max(row.ts);
        match sessions.last_mut() {
            Some(s) if same_window(s, row) => {
                s.ended = s.ended.max(seen);
                s.visits += 1;
                s.ids.push(row.id);
                if !row.text.trim().is_empty() {
                    s.text.clone_from(&row.text);
                }
            }
            _ => sessions.push(Session {
                started: row.ts,
                ended: seen,
                app: row.app.clone(),
                window_title: row.window_title.clone(),
                visits: 1,
                text: row.text.clone(),
                ids: vec![row.id],
            }),
        }
    }
    sessions
}

/// The contract's `where`: keys with no value are left out.
pub fn where_json(app: Option<&str>, window_title: Option<&str>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(app) = app.filter(|a| !a.is_empty()) {
        out.insert("app".into(), app.into());
    }
    if let Some(title) = window_title.filter(|t| !t.is_empty()) {
        out.insert("window_title".into(), title.into());
    }
    Value::Object(out)
}

pub fn session_json(s: &Session) -> Value {
    let open: Vec<i64> = s.ids.iter().rev().take(SESSION_OPEN_IDS).copied().collect();
    json!({
        "started": when_json(s.started),
        "ended": when_json(s.ended),
        "duration_s": (s.ended - s.started).max(0) / 1000,
        "where": where_json(s.app.as_deref(), s.window_title.as_deref()),
        "visits": s.visits,
        "text": s.text.trim().chars().take(SESSION_TEXT_CHARS).collect::<String>(),
        "open": { "tool": "memory.get_events", "ids": open },
    })
}

fn capture_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CaptureRow> {
    Ok(CaptureRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        last_seen_at: r.get(2)?,
        app: r.get(3)?,
        window_title: r.get(4)?,
        text: r.get(5)?,
    })
}

impl Db {
    /// `memory.get_events`: each id's stored text (first `max_chars` chars), in request order.
    /// Ids with no event row are listed under `missing`. A call for a connection without meeting
    /// notes and screen recall (owner decision 2026-10-20) lists a meeting transcript or a kept
    /// screen-text event under `withheld`, by id only, never its text.
    pub fn events_json(&self, ids: &[i64], max_chars: usize) -> Result<Value, String> {
        self.with_conn("memory.get_events", |c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, source, app_bundle_id, window_title, substr(content, 1, ?2),
                        length(content)
                 FROM event_log WHERE id = ?1",
            )?;
            let (mut events, mut missing, mut withheld) = (Vec::new(), Vec::new(), Vec::new());
            for &id in ids {
                let row = stmt
                    .query_row(params![id, max_chars as i64], |r| {
                        let len: i64 = r.get(6)?;
                        let source: String = r.get(2)?;
                        if !shogun_mcp::source_policy::current_allows_source(&source) {
                            return Ok(None);
                        }
                        Ok(Some(json!({
                            "id": r.get::<_, i64>(0)?,
                            "when": when_json(r.get(1)?),
                            "where": where_json(
                                r.get::<_, Option<String>>(3)?.as_deref(),
                                r.get::<_, Option<String>>(4)?.as_deref(),
                            ),
                            "source_kind": source,
                            "text": r.get::<_, String>(5)?,
                            "truncated": len > max_chars as i64,
                        })))
                    })
                    .optional()?;
                match row {
                    Some(Some(event)) => events.push(event),
                    Some(None) => withheld.push(id),
                    None => missing.push(id),
                }
            }
            let mut out = json!({ "events": events, "missing": missing });
            if !withheld.is_empty() {
                out["withheld"] = json!(withheld);
                out["withheld_reason"] = json!(shogun_mcp::source_policy::SOURCE_NOT_ALLOWED);
            }
            Ok::<_, rusqlite::Error>(out)
        })
        .map_err(|_| "memory_unavailable".to_string())
    }

    /// `memory.read_session`: capture rows in `[from_ms, to_ms)` (optionally one app: a
    /// case-insensitive substring of its name / bundle id) as chronological sessions, the first
    /// `limit` of them. `truncated` when the window held more.
    pub fn read_session_json(
        &self,
        from_ms: i64,
        to_ms: i64,
        app: Option<&str>,
        limit: usize,
    ) -> Result<Value, String> {
        let rows = self
            .with_conn("memory.read_session", |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT id, ts, last_seen_at, app_bundle_id, window_title, substr(content, 1, ?4)
                     FROM event_log
                     WHERE ts >= ?1 AND ts < ?2 AND {}
                       AND (?3 IS NULL OR instr(lower(coalesce(app_bundle_id, '')), lower(?3)) > 0)
                     ORDER BY ts, id LIMIT ?5",
                    capture_sources_sql()
                ))?;
                let rows = stmt.query_map(
                    params![from_ms, to_ms, app, SESSION_TEXT_CHARS as i64, SESSION_ROW_CAP as i64 + 1],
                    capture_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map_err(|_| "memory_unavailable".to_string())?;
        let mut truncated = rows.len() > SESSION_ROW_CAP;
        let mut sessions = sessionize(&rows[..rows.len().min(SESSION_ROW_CAP)]);
        if sessions.len() > limit {
            sessions.truncate(limit);
            truncated = true;
        }
        Ok(json!({
            "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
            "truncated": truncated,
        }))
    }

    /// `memory.get_context`'s capture-log half: `now`, `freshness`, the `current` session and up
    /// to [`CONTEXT_RECENT_SESSIONS`] earlier ones, newest first.
    pub fn current_activity_json(&self) -> Result<Value, String> {
        let now = self.now_ms();
        let mut rows = self
            .with_conn("memory.current_activity", |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT id, ts, last_seen_at, app_bundle_id, window_title, substr(content, 1, ?1)
                     FROM event_log WHERE {}
                     ORDER BY ts DESC, id DESC LIMIT ?2",
                    capture_sources_sql()
                ))?;
                let rows = stmt.query_map(
                    params![SESSION_TEXT_CHARS as i64, CONTEXT_ROW_CAP as i64],
                    capture_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map_err(|_| "memory_unavailable".to_string())?;
        rows.reverse();
        let last = rows.iter().map(|r| r.last_seen_at.max(r.ts)).max();
        let age = last.map(|last| (now - last).max(0));
        let mut sessions = sessionize(&rows);
        sessions.reverse();
        let mut newest_first = sessions.iter();
        let current = newest_first.next().map(session_json);
        let recent: Vec<Value> = newest_first
            .take(CONTEXT_RECENT_SESSIONS)
            .map(session_json)
            .collect();
        Ok(json!({
            "now": when_json(now),
            "freshness": {
                "source": "capture_log",
                "last_capture": last.map(when_json),
                "age_ms": age,
                "live": age.is_some_and(|age| age <= LIVE_MS),
            },
            "current": current,
            "recent_sessions": recent,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, ts: i64, app: &str, title: &str, text: &str) -> CaptureRow {
        CaptureRow {
            id,
            ts,
            last_seen_at: ts,
            app: Some(app.into()),
            window_title: Some(title.into()),
            text: text.into(),
        }
    }

    #[test]
    fn consecutive_same_window_rows_merge_and_neighbours_differ() {
        let mut rows = vec![
            row(1, 1_000, "chrome", "X", "a"),
            row(2, 3_000, "chrome", "X", "b"),
            row(3, 5_000, "chrome", "X", ""),
            row(4, 7_000, "code", "main.rs", "fn"),
            row(5, 9_000, "chrome", "X", "c"),
        ];
        rows[2].last_seen_at = 6_500;
        let sessions = sessionize(&rows);
        assert_eq!(sessions.len(), 3);
        let first = &sessions[0];
        assert_eq!((first.started, first.ended, first.visits), (1_000, 6_500, 3));
        // The newest non-empty text wins; an empty capture does not blank it.
        assert_eq!(first.text, "b");
        assert_eq!(first.ids, vec![1, 2, 3]);
        assert!(sessions
            .windows(2)
            .all(|w| (&w[0].app, &w[0].window_title) != (&w[1].app, &w[1].window_title)));
        let v = session_json(first);
        assert_eq!(v["duration_s"], 5);
        assert_eq!(v["open"]["ids"], json!([3, 2, 1]));
        assert_eq!(v["where"], json!({"app": "chrome", "window_title": "X"}));
    }

    #[test]
    fn browser_title_churn_does_not_split_a_tab() {
        let base = "Home / X - Google Chrome – Anant";
        for noisy in [
            "Home / X – Audio playing - Google Chrome – Anant",
            "Home / X - Audio playing - Google Chrome – Anant",
            "Home / X – Audio playing - High memory usage - 885 MB - Google Chrome – Anant",
            "Home / X - High memory usage – 1024 MB - Google Chrome – Anant",
            "(12) Home / X - Google Chrome – Anant",
        ] {
            assert_eq!(title_key(noisy), base, "{noisy}");
        }
        // Not churn: a title that merely mentions memory, or a non-numeric "(…)" prefix.
        assert_eq!(title_key("(draft) notes"), "(draft) notes");
        assert_eq!(title_key("High memory usage - 9 MB"), "High memory usage - 9 MB");
        let rows = [
            row(1, 1_000, "chrome", "Home / X - Google Chrome", "a"),
            row(2, 2_000, "chrome", "Home / X – Audio playing - Google Chrome", "b"),
        ];
        let sessions = sessionize(&rows);
        assert_eq!(sessions.len(), 1);
        // The raw title of the first row is what the session shows.
        assert_eq!(sessions[0].window_title.as_deref(), Some("Home / X - Google Chrome"));
    }

    #[test]
    fn where_leaves_out_missing_keys() {
        assert_eq!(where_json(None, Some("")), json!({}));
    }
}
