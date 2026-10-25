//! Runs of user-authored skills (issue #251, V44).
//!
//! The desktop runs skills on the Agent lane; every other face (MCP / REST / CLI) runs in a process
//! without those credentials, so a run is a row: asked for here, claimed and finished by the
//! desktop. A run above L1 asked for by an API client starts in `awaiting_confirm` and only the
//! user (in the desktop) moves it on — the same L1/L2/L3 rule for AI callers as for the UI
//! (invariant 6).

use rusqlite::{params, Connection, OptionalExtension};

/// Longest input a caller may hand a run.
pub const MAX_INPUT_CHARS: usize = 4_000;

/// One run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkillRun {
    pub id: i64,
    pub skill: String,
    pub level: String,
    pub origin: String,
    pub input: String,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const COLS: &str = "id, skill, level, origin, input, status, output, error, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SkillRun> {
    Ok(SkillRun {
        id: r.get(0)?,
        skill: r.get(1)?,
        level: r.get(2)?,
        origin: r.get(3)?,
        input: r.get(4)?,
        status: r.get(5)?,
        output: r.get(6)?,
        error: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

/// The status a new run starts in: a person at the desktop is the confirmation, so a UI run is
/// `pending`; an API caller's run above L1 waits for the user.
pub fn initial_status(level: &str, origin: &str) -> &'static str {
    if origin == "ui" || level == "L1" {
        "pending"
    } else {
        "awaiting_confirm"
    }
}

/// Record a request to run `skill`. `level` is the skill's effective level (`L1`..`L3`), `origin`
/// one of `ui` / `api` / `mcp` / `cli` / `schedule`. The input is trimmed and capped.
pub fn request(
    conn: &Connection,
    skill: &str,
    level: &str,
    origin: &str,
    input: &str,
    now: i64,
) -> Result<SkillRun, rusqlite::Error> {
    let input: String = input.trim().chars().take(MAX_INPUT_CHARS).collect();
    let input = crate::sanitize::persist_body(&input).text.into_owned();
    conn.execute(
        "INSERT INTO skill_runs (skill, level, origin, input, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![skill, level, origin, input, initial_status(level, origin), now],
    )?;
    let id = conn.last_insert_rowid();
    get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<SkillRun>, rusqlite::Error> {
    conn.query_row(&format!("SELECT {COLS} FROM skill_runs WHERE id = ?1"), [id], row)
        .optional()
}

/// Newest first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<SkillRun>, rusqlite::Error> {
    let mut stmt =
        conn.prepare(&format!("SELECT {COLS} FROM skill_runs ORDER BY id DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], row)?;
    rows.collect()
}

/// The user confirms (or cancels) a run that was waiting. Returns false when it was not waiting.
pub fn decide(conn: &Connection, id: i64, confirm: bool, now: i64) -> Result<bool, rusqlite::Error> {
    let status = if confirm { "pending" } else { "cancelled" };
    Ok(conn.execute(
        "UPDATE skill_runs SET status = ?1, updated_at = ?2
          WHERE id = ?3 AND status = 'awaiting_confirm'",
        params![status, now, id],
    )? == 1)
}

/// Take the oldest pending run for execution (`pending` → `running`). One at a time.
pub fn claim_next(conn: &Connection, now: i64) -> Result<Option<SkillRun>, rusqlite::Error> {
    let Some(id) = conn
        .query_row(
            "SELECT id FROM skill_runs WHERE status = 'pending' ORDER BY id LIMIT 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let claimed = conn.execute(
        "UPDATE skill_runs SET status = 'running', updated_at = ?1
          WHERE id = ?2 AND status = 'pending'",
        params![now, id],
    )?;
    if claimed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// Record how a claimed run ended. `Ok(text)` → `done`, `Err(reason)` → `failed`. The reason is a
/// short class the caller chose ("no key", "plan") — never provider text.
pub fn finish(
    conn: &Connection,
    id: i64,
    result: Result<&str, &str>,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    let changed = match result {
        // Generated text goes through the same gate as every other model output we persist: an
        // output shaped like instructions to a later model is withheld, not stored.
        Ok(text) => match crate::sanitize::persist_generated(text) {
            Some(clean) => conn.execute(
                "UPDATE skill_runs SET status = 'done', output = ?1, updated_at = ?2
                  WHERE id = ?3 AND status = 'running'",
                params![clean.text.as_ref(), now, id],
            )?,
            None => conn.execute(
                "UPDATE skill_runs SET status = 'failed', error = 'output_withheld', updated_at = ?1
                  WHERE id = ?2 AND status = 'running'",
                params![now, id],
            )?,
        },
        Err(reason) => conn.execute(
            "UPDATE skill_runs SET status = 'failed', error = ?1, updated_at = ?2
              WHERE id = ?3 AND status = 'running'",
            params![reason, now, id],
        )?,
    };
    Ok(changed == 1)
}

/// Runs left `running` by a crash go back to `pending` (called at desktop start).
pub fn requeue_interrupted(conn: &Connection, now: i64) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "UPDATE skill_runs SET status = 'pending', updated_at = ?1 WHERE status = 'running'",
        [now],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ui_run_is_pending_and_an_api_run_above_l1_waits_for_the_user() {
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(request(&conn, "a", "L3", "ui", "", 1).unwrap().status, "pending");
        assert_eq!(request(&conn, "a", "L1", "mcp", "", 1).unwrap().status, "pending");
        // A scheduled run has no one watching either (issue #252).
        assert_eq!(request(&conn, "a", "L2", "schedule", "", 1).unwrap().status, "awaiting_confirm");
        let waiting = request(&conn, "a", "L2", "api", "x", 1).unwrap();
        assert_eq!(waiting.status, "awaiting_confirm");
        // A waiting run is never claimed.
        let first = claim_next(&conn, 2).unwrap().unwrap();
        let second = claim_next(&conn, 2).unwrap().unwrap();
        assert_eq!((first.id, second.id), (1, 2));
        assert!(claim_next(&conn, 2).unwrap().is_none());
        assert!(decide(&conn, 3, false, 2).unwrap());
        assert!(decide(&conn, waiting.id, true, 3).unwrap());
        assert!(!decide(&conn, waiting.id, true, 3).unwrap(), "only a waiting run can be decided");
        assert_eq!(claim_next(&conn, 4).unwrap().unwrap().id, waiting.id);
    }

    #[test]
    fn a_run_finishes_once_and_a_crash_requeues_it() {
        let conn = crate::open_in_memory().unwrap();
        let r = request(&conn, "weekly", "L1", "cli", &"y".repeat(MAX_INPUT_CHARS + 50), 1).unwrap();
        assert_eq!(r.input.chars().count(), MAX_INPUT_CHARS);
        claim_next(&conn, 2).unwrap();
        assert_eq!(requeue_interrupted(&conn, 3).unwrap(), 1);
        claim_next(&conn, 4).unwrap();
        assert!(finish(&conn, r.id, Ok("the report"), 5).unwrap());
        assert!(!finish(&conn, r.id, Err("again"), 6).unwrap());
        let done = get(&conn, r.id).unwrap().unwrap();
        assert_eq!((done.status.as_str(), done.output.as_deref()), ("done", Some("the report")));
        assert_eq!(recent(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_cancelled_run_never_runs() {
        let conn = crate::open_in_memory().unwrap();
        let r = request(&conn, "a", "L3", "api", "", 1).unwrap();
        assert!(decide(&conn, r.id, false, 2).unwrap());
        assert!(claim_next(&conn, 3).unwrap().is_none());
        assert_eq!(get(&conn, r.id).unwrap().unwrap().status, "cancelled");
    }
}
