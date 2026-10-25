//! AI memory search requests from the Memory API (issue #940 B2b, V72).
//!
//! A `memory.search` with `mode: "ai"` is a row: asked for here, claimed and finished by the
//! desktop, which alone holds the Agent lane's credential. The hand-off, TTL, sweep and masking
//! rules are those of `translate_jobs`, which documents them.

use rusqlite::{params, Connection, OptionalExtension};

/// How long a row may exist at all (see `translate_jobs::TTL_MS`).
pub const TTL_MS: i64 = 10 * 60_000;

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSearchJob {
    pub id: i64,
    pub origin: String,
    /// The request as JSON (already masked by the caller).
    pub request: String,
    /// `pending` | `running` | `done` | `failed`.
    pub status: String,
    /// The result as JSON once `done`.
    pub result: Option<String>,
    /// A short reason code once `failed` — never provider text.
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AiSearchJob {
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed")
    }
}

const COLS: &str = "id, origin, request, status, result, error, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AiSearchJob> {
    Ok(AiSearchJob {
        id: r.get(0)?,
        origin: r.get(1)?,
        request: r.get(2)?,
        status: r.get(3)?,
        result: r.get(4)?,
        error: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

/// Delete every row older than [`TTL_MS`], whatever its status. Returns how many went.
pub fn sweep(conn: &Connection, now: i64) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "DELETE FROM ai_search_jobs WHERE created_at < ?1",
        [now.saturating_sub(TTL_MS)],
    )
}

/// Record a request. `origin` is one of `api` / `mcp` / `cli`; the CHECK constraint refuses
/// anything else.
pub fn request(
    conn: &Connection,
    origin: &str,
    request: &str,
    now: i64,
) -> Result<AiSearchJob, rusqlite::Error> {
    sweep(conn, now)?;
    conn.execute(
        "INSERT INTO ai_search_jobs (origin, request, status, created_at, updated_at)
         VALUES (?1, ?2, 'pending', ?3, ?3)",
        params![origin, request, now],
    )?;
    let id = conn.last_insert_rowid();
    get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<AiSearchJob>, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT {COLS} FROM ai_search_jobs WHERE id = ?1"),
        [id],
        row,
    )
    .optional()
}

/// Whether the desktop is working on any request right now (a `running` row within its TTL).
pub fn any_running(conn: &Connection, now: i64) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM ai_search_jobs WHERE status = 'running' AND created_at >= ?1)",
        [now.saturating_sub(TTL_MS)],
        |r| r.get::<_, bool>(0),
    )
}

fn oldest_pending(conn: &Connection) -> Result<Option<i64>, rusqlite::Error> {
    conn.query_row(
        "SELECT id FROM ai_search_jobs WHERE status = 'pending' ORDER BY id LIMIT 1",
        [],
        |r| r.get::<_, i64>(0),
    )
    .optional()
}

/// The desktop takes the oldest pending request (`pending` → `running`), after sweeping expired
/// rows. The idle path is one indexed read and no write.
pub fn claim_next(conn: &Connection, now: i64) -> Result<Option<AiSearchJob>, rusqlite::Error> {
    if oldest_pending(conn)?.is_none() {
        return Ok(None);
    }
    sweep(conn, now)?;
    let Some(id) = oldest_pending(conn)? else {
        return Ok(None);
    };
    let claimed = conn.execute(
        "UPDATE ai_search_jobs SET status = 'running', updated_at = ?1
          WHERE id = ?2 AND status = 'pending'",
        params![now, id],
    )?;
    if claimed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// Record how a claimed request ended: `Ok(result_json)` → `done`, `Err(code)` → `failed`.
/// Returns false when the row is gone (swept, or taken by a caller that gave up) or not running.
pub fn finish(
    conn: &Connection,
    id: i64,
    outcome: Result<&str, &str>,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    let changed = match outcome {
        Ok(result) => conn.execute(
            "UPDATE ai_search_jobs SET status = 'done', result = ?1, updated_at = ?2
              WHERE id = ?3 AND status = 'running'",
            params![result, now, id],
        )?,
        Err(code) => conn.execute(
            "UPDATE ai_search_jobs SET status = 'failed', error = ?1, updated_at = ?2
              WHERE id = ?3 AND status = 'running'",
            params![code, now, id],
        )?,
    };
    Ok(changed == 1)
}

/// Read a request; when it has finished, delete it and return it. A request that is still
/// pending or running is returned as it is and kept.
pub fn take_terminal(conn: &Connection, id: i64) -> Result<Option<AiSearchJob>, rusqlite::Error> {
    let job = get(conn, id)?;
    if job.as_ref().is_some_and(AiSearchJob::is_terminal) {
        conn.execute(
            "DELETE FROM ai_search_jobs WHERE id = ?1 AND status IN ('done', 'failed')",
            [id],
        )?;
    }
    Ok(job)
}

/// Delete one request whatever its state (a caller that gave up on a request nobody claimed).
pub fn discard(conn: &Connection, id: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn.execute("DELETE FROM ai_search_jobs WHERE id = ?1", [id])? == 1)
}

/// Requests a crash left `running` fail rather than rerun (called at desktop start).
pub fn fail_interrupted(conn: &Connection, now: i64) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "UPDATE ai_search_jobs SET status = 'failed', error = 'interrupted', updated_at = ?1
          WHERE status = 'running'",
        [now],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_claimed_once_finished_and_taken_exactly_once() {
        let conn = crate::open_in_memory().unwrap();
        let job = request(&conn, "mcp", r#"{"question":"hi"}"#, 1_000).unwrap();
        assert_eq!(job.status, "pending");
        assert!(!any_running(&conn, 1_000).unwrap());

        let claimed = claim_next(&conn, 1_100).unwrap().unwrap();
        assert_eq!((claimed.id, claimed.status.as_str()), (job.id, "running"));
        assert!(
            claim_next(&conn, 1_100).unwrap().is_none(),
            "one claim per request"
        );
        assert!(any_running(&conn, 1_100).unwrap());

        assert_eq!(
            take_terminal(&conn, job.id).unwrap().unwrap().status,
            "running"
        );
        assert!(get(&conn, job.id).unwrap().is_some());

        assert!(finish(&conn, job.id, Ok(r#"{"results":[]}"#), 1_200).unwrap());
        assert!(
            !finish(&conn, job.id, Ok("again"), 1_300).unwrap(),
            "only a running row finishes"
        );
        let taken = take_terminal(&conn, job.id).unwrap().unwrap();
        assert_eq!(taken.result.as_deref(), Some(r#"{"results":[]}"#));
        assert!(
            get(&conn, job.id).unwrap().is_none(),
            "handed over, then gone"
        );
        assert!(take_terminal(&conn, job.id).unwrap().is_none());
    }

    #[test]
    fn a_failure_carries_a_code_and_is_also_taken_once() {
        let conn = crate::open_in_memory().unwrap();
        let job = request(&conn, "api", "{}", 0).unwrap();
        claim_next(&conn, 1).unwrap();
        assert!(finish(&conn, job.id, Err("no_agent"), 2).unwrap());
        let taken = take_terminal(&conn, job.id).unwrap().unwrap();
        assert_eq!(
            (taken.status.as_str(), taken.error.as_deref()),
            ("failed", Some("no_agent"))
        );
        assert!(get(&conn, job.id).unwrap().is_none());
    }

    #[test]
    fn rows_older_than_the_ttl_are_swept_whatever_their_status() {
        let conn = crate::open_in_memory().unwrap();
        let old = request(&conn, "cli", "{}", 0).unwrap();
        let done = request(&conn, "cli", "{}", 10).unwrap();
        claim_next(&conn, 11).unwrap();
        claim_next(&conn, 12).unwrap();
        finish(&conn, done.id, Ok("{}"), 13).unwrap();
        let fresh = request(&conn, "cli", "{}", TTL_MS + 20).unwrap();
        assert!(get(&conn, old.id).unwrap().is_none());
        assert!(get(&conn, done.id).unwrap().is_none());
        // Ids are never reused.
        assert!(fresh.id > done.id);
        assert_eq!(
            claim_next(&conn, TTL_MS + 21).unwrap().unwrap().id,
            fresh.id
        );
    }

    #[test]
    fn a_crash_fails_running_requests_instead_of_rerunning_them() {
        let conn = crate::open_in_memory().unwrap();
        let job = request(&conn, "mcp", "{}", 0).unwrap();
        claim_next(&conn, 1).unwrap();
        assert_eq!(fail_interrupted(&conn, 2).unwrap(), 1);
        let row = get(&conn, job.id).unwrap().unwrap();
        assert_eq!(
            (row.status.as_str(), row.error.as_deref()),
            ("failed", Some("interrupted"))
        );
        assert!(claim_next(&conn, 3).unwrap().is_none());
    }

    #[test]
    fn unknown_origins_are_refused_by_the_schema() {
        let conn = crate::open_in_memory().unwrap();
        assert!(request(&conn, "ui", "{}", 0).is_err());
        let job = request(&conn, "mcp", "{}", 0).unwrap();
        assert!(discard(&conn, job.id).unwrap());
        assert!(!discard(&conn, job.id).unwrap());
    }
}
