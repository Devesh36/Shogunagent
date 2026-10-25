//! File exports into user-chosen folders (issue #397, V48).
//!
//! `LocalAction::WriteFile` is L2. A write asked for by an API client (MCP / REST / CLI) is a row
//! in `awaiting_confirm` until the user confirms it in the desktop; a write clicked in the desktop
//! is its own confirmation. Either way the row then records how it ended — the traceability
//! record of a local write: `kind = 'local_write'`, byte count and xxh64 digest, never content.

use rusqlite::{params, Connection, OptionalExtension};

/// The one kind a local-write row carries (the traceability vocabulary for this action).
pub const KIND: &str = "local_write";

/// One export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LocalWrite {
    pub id: i64,
    pub kind: String,
    pub source: String,
    pub source_id: i64,
    pub include_transcript: bool,
    pub format: String,
    pub folder: String,
    pub name: String,
    pub origin: String,
    pub status: String,
    pub path: Option<String>,
    pub bytes: Option<i64>,
    pub content_xxh64: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What to export and where — the request, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest<'a> {
    /// `meeting_recap`.
    pub source: &'a str,
    pub source_id: i64,
    pub include_transcript: bool,
    /// `md` / `csv`.
    pub format: &'a str,
    /// The allowed folder (or a folder beneath one) the file goes into.
    pub folder: &'a str,
    /// The file stem.
    pub name: &'a str,
    /// `ui` / `api`.
    pub origin: &'a str,
}

const COLS: &str = "id, kind, source, source_id, include_transcript, format, folder, name, origin, \
                    status, path, bytes, content_xxh64, error, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LocalWrite> {
    Ok(LocalWrite {
        id: r.get(0)?,
        kind: r.get(1)?,
        source: r.get(2)?,
        source_id: r.get(3)?,
        include_transcript: r.get::<_, i64>(4)? != 0,
        format: r.get(5)?,
        folder: r.get(6)?,
        name: r.get(7)?,
        origin: r.get(8)?,
        status: r.get(9)?,
        path: r.get(10)?,
        bytes: r.get(11)?,
        content_xxh64: r.get(12)?,
        error: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

/// The status a new request starts in: the desktop click is the confirmation (`writing`); an API
/// caller's request waits for the user (`awaiting_confirm`).
pub fn initial_status(origin: &str) -> &'static str {
    if origin == "ui" {
        "writing"
    } else {
        "awaiting_confirm"
    }
}

/// Record a request.
pub fn request(conn: &Connection, req: &WriteRequest<'_>, now: i64) -> Result<LocalWrite, rusqlite::Error> {
    conn.execute(
        "INSERT INTO local_writes
           (kind, source, source_id, include_transcript, format, folder, name, origin, status,
            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            KIND,
            req.source,
            req.source_id,
            req.include_transcript as i64,
            req.format,
            req.folder,
            req.name,
            req.origin,
            initial_status(req.origin),
            now
        ],
    )?;
    get(conn, conn.last_insert_rowid())?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<LocalWrite>, rusqlite::Error> {
    conn.query_row(&format!("SELECT {COLS} FROM local_writes WHERE id = ?1"), [id], row)
        .optional()
}

/// Newest first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<LocalWrite>, rusqlite::Error> {
    let mut stmt =
        conn.prepare(&format!("SELECT {COLS} FROM local_writes ORDER BY id DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], row)?;
    rows.collect()
}

/// The user confirms (`awaiting_confirm` → `writing`) or cancels a waiting request. Returns the
/// row when it moved to `writing` — the caller then performs the write and [`finish`]es it.
pub fn decide(
    conn: &Connection,
    id: i64,
    confirm: bool,
    now: i64,
) -> Result<Option<LocalWrite>, rusqlite::Error> {
    let status = if confirm { "writing" } else { "cancelled" };
    let changed = conn.execute(
        "UPDATE local_writes SET status = ?1, updated_at = ?2
          WHERE id = ?3 AND status = 'awaiting_confirm'",
        params![status, now, id],
    )?;
    if changed == 1 && confirm {
        get(conn, id)
    } else {
        Ok(None)
    }
}

/// How the write ended. `Ok((path, bytes, digest))` → `written`; `Err(reason)` → `failed`, where
/// the reason is one of the executor's fixed strings (never content).
pub fn finish(
    conn: &Connection,
    id: i64,
    result: Result<(&str, i64, &str), &str>,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    let changed = match result {
        Ok((path, bytes, digest)) => conn.execute(
            "UPDATE local_writes SET status = 'written', path = ?1, bytes = ?2, content_xxh64 = ?3,
                    updated_at = ?4
              WHERE id = ?5 AND status = 'writing'",
            params![path, bytes, digest, now, id],
        )?,
        Err(reason) => conn.execute(
            "UPDATE local_writes SET status = 'failed', error = ?1, updated_at = ?2
              WHERE id = ?3 AND status = 'writing'",
            params![reason, now, id],
        )?,
    };
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::open_in_memory().unwrap()
    }

    fn req(origin: &str) -> WriteRequest<'_> {
        WriteRequest {
            source: "meeting_recap",
            source_id: 7,
            include_transcript: false,
            format: "md",
            folder: "/Users/me/Notes",
            name: "Renewal call",
            origin,
        }
    }

    #[test]
    fn an_api_request_waits_for_the_user_and_a_ui_click_is_the_confirmation() {
        let conn = db();
        let api = request(&conn, &req("api"), 1).unwrap();
        assert_eq!(api.status, "awaiting_confirm");
        assert_eq!(api.kind, KIND);
        // Nothing can finish it before the user confirms.
        assert!(!finish(&conn, api.id, Ok(("/x.md", 1, "00")), 2).unwrap());
        let confirmed = decide(&conn, api.id, true, 3).unwrap().unwrap();
        assert_eq!(confirmed.status, "writing");
        assert!(decide(&conn, api.id, true, 4).unwrap().is_none(), "decided once");
        assert!(finish(&conn, api.id, Ok(("/Users/me/Notes/Renewal call.md", 42, "abcd")), 5).unwrap());
        let done = get(&conn, api.id).unwrap().unwrap();
        assert_eq!(done.status, "written");
        assert_eq!(done.bytes, Some(42));
        assert_eq!(done.content_xxh64.as_deref(), Some("abcd"));

        let ui = request(&conn, &req("ui"), 6).unwrap();
        assert_eq!(ui.status, "writing");
        assert!(finish(&conn, ui.id, Err("no export folder is set"), 7).unwrap());
        assert_eq!(get(&conn, ui.id).unwrap().unwrap().status, "failed");
    }

    #[test]
    fn a_cancelled_request_never_writes() {
        let conn = db();
        let api = request(&conn, &req("api"), 1).unwrap();
        assert!(decide(&conn, api.id, false, 2).unwrap().is_none());
        assert_eq!(get(&conn, api.id).unwrap().unwrap().status, "cancelled");
        assert!(!finish(&conn, api.id, Ok(("/x.md", 1, "00")), 3).unwrap());
        assert_eq!(recent(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn the_row_has_no_column_for_content_and_only_the_local_write_kind() {
        let conn = db();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('local_writes')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for forbidden in ["body", "content", "text", "route"] {
            assert!(!cols.iter().any(|c| c == forbidden), "{forbidden} must not be a column");
        }
        let bad = conn.execute(
            "INSERT INTO local_writes (kind, source, source_id, format, folder, name, origin, status,
               created_at, updated_at) VALUES ('egress', 'meeting_recap', 1, 'md', '/', 'a', 'ui',
               'writing', 1, 1)",
            [],
        );
        assert!(bad.is_err(), "the CHECK admits only local_write");
    }
}
