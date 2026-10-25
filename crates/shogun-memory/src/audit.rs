//! The principal's audit log (V101, ShogunAgent E18): one row per write and per approval decision.
//!
//! A record names who acted (`actor`), for which principal, through which tool or route, at which
//! tier, what came of it, when, and under which request id. It never holds a body, an argument
//! value or a row's text: [`AuditRecord::new`] refuses a field that is not a short label, so the
//! log cannot become a place content leaks into (CLAUDE.md invariant 2).

use rusqlite::{params, Connection};

/// Longest route (a tool wire name or `METHOD /path`) recorded.
pub const MAX_ROUTE_CHARS: usize = 160;
/// Longest request id recorded.
pub const MAX_REQUEST_ID_CHARS: usize = 64;
/// Longest level / outcome recorded.
pub const MAX_LABEL_CHARS: usize = 32;
/// Most records one `audit.list` page returns.
pub const MAX_LIST: usize = 500;
/// Rows the log keeps; older rows are trimmed on append.
pub const RETAIN_ROWS: i64 = 100_000;

/// One audit record, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub ts: i64,
    pub actor: String,
    pub principal: Option<String>,
    pub route: String,
    pub level: String,
    pub outcome: String,
    pub request_id: String,
}

/// A record as listed, with its row id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub record: AuditRecord,
}

/// Why a record was refused. Never carries the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    #[error("audit field `{0}` is empty, too long or not a plain label")]
    InvalidField(&'static str),
    #[error("the audit log could not be written")]
    Db,
}

impl From<rusqlite::Error> for AuditError {
    fn from(_: rusqlite::Error) -> Self {
        AuditError::Db
    }
}

/// A route contains at most one space (`POST /v1/acl/grant`); every other field none.
fn label(field: &'static str, raw: &str, max: usize, spaces: usize) -> Result<String, AuditError> {
    let value = raw.trim();
    let ok = !value.is_empty()
        && value.chars().count() <= max
        && value.chars().all(|c| !c.is_control() && (c == ' ' || !c.is_whitespace()))
        && value.matches(' ').count() <= spaces;
    if ok {
        Ok(value.to_string())
    } else {
        Err(AuditError::InvalidField(field))
    }
}

impl AuditRecord {
    /// Validate every field. `actor` follows the V92 actor rule.
    pub fn new(
        ts: i64,
        actor: &str,
        principal: Option<&str>,
        route: &str,
        level: &str,
        outcome: &str,
        request_id: &str,
    ) -> Result<Self, AuditError> {
        let actor = crate::actor::Actor::parse(actor).map_err(|_| AuditError::InvalidField("actor"))?;
        let principal = match principal.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => Some(label("principal", p, 64, 0)?),
            None => None,
        };
        Ok(Self {
            ts,
            actor: actor.as_str().to_string(),
            principal,
            route: label("route", route, MAX_ROUTE_CHARS, 1)?,
            level: label("level", level, MAX_LABEL_CHARS, 0)?,
            outcome: label("outcome", outcome, MAX_LABEL_CHARS, 0)?,
            request_id: label("request_id", request_id, MAX_REQUEST_ID_CHARS, 0)?,
        })
    }
}

/// Append `record`; returns its id. Trims the log to about [`RETAIN_ROWS`].
pub fn append(conn: &Connection, record: &AuditRecord) -> Result<i64, AuditError> {
    conn.execute(
        "INSERT INTO audit_log (ts, actor, principal, route, level, outcome, request_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.ts,
            record.actor,
            record.principal,
            record.route,
            record.level,
            record.outcome,
            record.request_id
        ],
    )?;
    let id = conn.last_insert_rowid();
    if id > RETAIN_ROWS && id % 1_000 == 0 {
        conn.execute("DELETE FROM audit_log WHERE id <= ?1", [id - RETAIN_ROWS])?;
    }
    Ok(id)
}

/// Records with `ts >= since_ms`, oldest first, at most `limit` (1..=[`MAX_LIST`]).
pub fn list(conn: &Connection, since_ms: Option<i64>, limit: usize) -> Result<Vec<AuditEntry>, AuditError> {
    let limit = i64::try_from(limit.clamp(1, MAX_LIST)).unwrap_or(100);
    let mut stmt = conn.prepare(
        "SELECT id, ts, actor, principal, route, level, outcome, request_id FROM audit_log
         WHERE ts >= ?1 ORDER BY ts, id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![since_ms.unwrap_or(i64::MIN), limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                record: AuditRecord {
                    ts: r.get(1)?,
                    actor: r.get(2)?,
                    principal: r.get(3)?,
                    route: r.get(4)?,
                    level: r.get(5)?,
                    outcome: r.get(6)?,
                    request_id: r.get(7)?,
                },
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One page of the log in append order, for the audit export (E26): records with `id > after_id`
/// and `since_ms <= ts < until_ms` (either bound optional), oldest row first, at most `limit`.
/// Keyed by row id so a page boundary never skips or repeats a record, whatever the timestamps.
pub fn page(
    conn: &Connection,
    after_id: i64,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: usize,
) -> Result<Vec<AuditEntry>, AuditError> {
    let limit = i64::try_from(limit.clamp(1, 10_000)).unwrap_or(500);
    let mut stmt = conn.prepare(
        "SELECT id, ts, actor, principal, route, level, outcome, request_id FROM audit_log
         WHERE id > ?1 AND ts >= ?2 AND ts < ?3 ORDER BY id LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(
            params![after_id, since_ms.unwrap_or(i64::MIN), until_ms.unwrap_or(i64::MAX), limit],
            |r| {
                Ok(AuditEntry {
                    id: r.get(0)?,
                    record: AuditRecord {
                        ts: r.get(1)?,
                        actor: r.get(2)?,
                        principal: r.get(3)?,
                        route: r.get(4)?,
                        level: r.get(5)?,
                        outcome: r.get(6)?,
                        request_id: r.get(7)?,
                    },
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pages_follow_append_order_within_the_window() {
        let conn = conn();
        for (ts, outcome) in [(30, "a"), (10, "b"), (20, "c"), (40, "d")] {
            append(&conn, &AuditRecord::new(ts, "agent:x", None, "memory.ingest", "L1", outcome, "r").unwrap()).unwrap();
        }
        let all = page(&conn, 0, None, None, 2).unwrap();
        assert_eq!(all.iter().map(|e| e.record.outcome.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        let next = page(&conn, all[1].id, None, None, 2).unwrap();
        assert_eq!(next.iter().map(|e| e.record.outcome.as_str()).collect::<Vec<_>>(), ["c", "d"]);
        let window = page(&conn, 0, Some(20), Some(40), 10).unwrap();
        assert_eq!(window.iter().map(|e| e.record.outcome.as_str()).collect::<Vec<_>>(), ["a", "c"], "until is exclusive");
    }

    use super::*;

    fn conn() -> Connection {
        crate::open_in_memory().expect("store")
    }

    #[test]
    fn appends_and_lists_labels_only() {
        let conn = conn();
        let a = AuditRecord::new(10, "agent:codex", Some("alice"), "memory.ingest", "L1", "ok", "req-1").unwrap();
        let b = AuditRecord::new(20, "human:owner", None, "POST /v1/approvals/3/decide", "L3", "approved", "req-2")
            .unwrap();
        append(&conn, &a).unwrap();
        append(&conn, &b).unwrap();
        let all = list(&conn, None, 10).unwrap();
        assert_eq!(all.iter().map(|e| e.record.clone()).collect::<Vec<_>>(), vec![a, b.clone()]);
        let since = list(&conn, Some(15), 10).unwrap();
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].record, b);
        assert_eq!(list(&conn, None, 1).unwrap().len(), 1);
    }

    #[test]
    fn refuses_fields_that_could_carry_content() {
        assert_eq!(
            AuditRecord::new(1, "", None, "memory.ingest", "L1", "ok", "r").unwrap_err(),
            AuditError::InvalidField("actor")
        );
        let long = "x".repeat(MAX_ROUTE_CHARS + 1);
        assert!(AuditRecord::new(1, "agent:a", None, &long, "L1", "ok", "r").is_err());
        assert!(AuditRecord::new(1, "agent:a", None, "send the forecast to Aiko", "L1", "ok", "r").is_err());
        assert!(AuditRecord::new(1, "agent:a", None, "memory.ingest", "L1", "the body", "r").is_err());
        assert!(AuditRecord::new(1, "agent:a", None, "memory.ingest", "L1", "ok", "line\nbreak").is_err());
        assert!(AuditRecord::new(1, "agent:a", Some("a b"), "memory.ingest", "L1", "ok", "r").is_err());
    }
}
