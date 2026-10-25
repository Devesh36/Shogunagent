//! LLM usage repository (FR-HN-01 / #313): the storage side of the usage record. shogun-memory
//! owns the `llm_usage` table (invariant 1 — data gravity in the Rust core), so the write/read
//! lives here, not in the caller.
//!
//! The table (V30__llm_usage.sql) stores token counts, cache hits, latency and outcome per LLM
//! call — **there is no text column**, so a prompt or completion body cannot be persisted here
//! even by mistake (as with `traceability_log`'s chunk columns). `route` shares the
//! traceability vocabulary ([`crate::traceability::Route`]); `prompt_id`/`prompt_version` come
//! from shogun-core's prompt registry (FR-HN-02), which is what makes a metric row attributable
//! to the prompt wording that produced it.

use rusqlite::{params, Connection};

use crate::traceability::Route;

/// How a call ended, mirroring the `llm_usage.outcome` CHECK set exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    RateLimited,
    Unauthorized,
    Quota,
    Parse,
}

impl Outcome {
    /// The exact string stored in the column.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::RateLimited => "rate_limited",
            Outcome::Unauthorized => "unauthorized",
            Outcome::Quota => "quota",
            Outcome::Parse => "parse",
        }
    }

    /// Parse the stored string back. Reads stay total; the CHECK prevents unknown values.
    pub fn parse(s: &str) -> Option<Outcome> {
        Some(match s {
            "ok" => Outcome::Ok,
            "error" => Outcome::Error,
            "rate_limited" => Outcome::RateLimited,
            "unauthorized" => Outcome::Unauthorized,
            "quota" => Outcome::Quota,
            "parse" => Outcome::Parse,
            _ => return None,
        })
    }
}

/// One usage row: counts, latency, identity — never text (there is no field for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub ts: i64,
    pub route: Route,
    pub prompt_id: String,
    pub prompt_version: i64,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// True when the counts are byte-length estimates (delegate CLI / relay), not
    /// provider-reported numbers.
    pub estimated: bool,
    pub first_token_ms: Option<i64>,
    pub total_ms: i64,
    pub outcome: Outcome,
}

/// Append a usage row (one per LLM call). Returns the new row id.
pub fn insert(conn: &Connection, row: &UsageRow) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO llm_usage
           (ts, route, prompt_id, prompt_version, model, input_tokens, output_tokens,
            cache_read_tokens, cache_write_tokens, estimated, first_token_ms, total_ms, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            row.ts,
            row.route.as_str(),
            row.prompt_id,
            row.prompt_version,
            row.model,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.estimated as i64,
            row.first_token_ms,
            row.total_ms,
            row.outcome.as_str(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Aggregated counters for one prompt id (or all) over a time window — the SQL-one-query answer
/// FR-HN-01 promises ("cost / latency / failure rate per feature").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Totals {
    pub calls: i64,
    pub ok_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Calls whose counts are estimates — a caveat the UI must carry when it shows totals.
    pub estimated_calls: i64,
}

/// Sum usage at or after `since_ts`, optionally for one `prompt_id`.
pub fn totals_since(
    conn: &Connection,
    since_ts: i64,
    prompt_id: Option<&str>,
) -> Result<Totals, rusqlite::Error> {
    let sql = "SELECT count(*),
                      coalesce(sum(outcome = 'ok'), 0),
                      coalesce(sum(input_tokens), 0),
                      coalesce(sum(output_tokens), 0),
                      coalesce(sum(cache_read_tokens), 0),
                      coalesce(sum(cache_write_tokens), 0),
                      coalesce(sum(estimated), 0)
               FROM llm_usage
               WHERE ts >= ?1 AND (?2 IS NULL OR prompt_id = ?2)";
    conn.query_row(sql, params![since_ts, prompt_id], |r| {
        Ok(Totals {
            calls: r.get(0)?,
            ok_calls: r.get(1)?,
            input_tokens: r.get(2)?,
            output_tokens: r.get(3)?,
            cache_read_tokens: r.get(4)?,
            cache_write_tokens: r.get(5)?,
            estimated_calls: r.get(6)?,
        })
    })
}

/// Lanes as the credential rule draws them (CLAUDE.md invariant 5). The agent lane runs on the
/// user's own credentials — the Messages API with a BYOK key, or a delegated local CLI — and the
/// batch lane is the operator's, whether it goes direct or through the relay. Written out rather
/// than derived so that adding a route has to be a decision about which lane it belongs to.
const AGENT_ROUTES: &str = "('messages_api','local_agent')";
const BATCH_ROUTES: &str = "('batch_api','batch_relay')";

/// Completions since `since_ts`, split the way the harness face reads them.
///
/// Every client already writes a row per call, carrying the route, the prompt id, whether it
/// succeeded and when the first token arrived. Counting the same events a second time in memory
/// would put two sets of numbers in the product with nothing keeping them equal, and the
/// in-memory set would reset with the process while the rows persist. This reads the rows.
///
/// Zeros are real: no successes of that class in the window, not a missing measurement.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Completions {
    /// Agent-lane calls that returned ok — chat, drafts, Scribe.
    pub agent_ok: i64,
    /// Observed agent-lane first-token latencies, ascending. The 1s SLO is measured on these,
    /// and a call that errored before any token contributes nothing rather than a zero.
    pub first_token_ms: Vec<i64>,
    /// Batch-lane successes per prompt id, ordered by name so the face is stable between reads.
    pub batch_ok: Vec<(String, i64)>,
}

pub fn completions_since(
    conn: &Connection,
    since_ts: i64,
) -> Result<Completions, rusqlite::Error> {
    let agent_ok = conn.query_row(
        &format!(
            "SELECT count(*) FROM llm_usage
             WHERE ts >= ?1 AND outcome = 'ok' AND route IN {AGENT_ROUTES}"
        ),
        params![since_ts],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT first_token_ms FROM llm_usage
         WHERE ts >= ?1 AND outcome = 'ok' AND route IN {AGENT_ROUTES}
           AND first_token_ms IS NOT NULL
         ORDER BY first_token_ms"
    ))?;
    let rows = stmt.query_map(params![since_ts], |r| r.get::<_, i64>(0))?;
    let first_token_ms = rows.collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut stmt = conn.prepare(&format!(
        "SELECT prompt_id, count(*) FROM llm_usage
         WHERE ts >= ?1 AND outcome = 'ok' AND route IN {BATCH_ROUTES}
         GROUP BY prompt_id ORDER BY prompt_id"
    ))?;
    let rows = stmt.query_map(params![since_ts], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let batch_ok = rows.collect::<Result<Vec<_>, _>>()?;

    Ok(Completions {
        agent_ok,
        first_token_ms,
        batch_ok,
    })
}

/// The most recent rows, newest first, for a dev/diagnostics view.
pub fn recent(conn: &Connection, limit: i64) -> Result<Vec<UsageRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT ts, route, prompt_id, prompt_version, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, estimated, first_token_ms, total_ms, outcome
         FROM llm_usage ORDER BY ts DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        let route_str: String = r.get(1)?;
        let outcome_str: String = r.get(12)?;
        Ok(UsageRow {
            ts: r.get(0)?,
            route: Route::parse(&route_str).unwrap_or(Route::MessagesApi),
            prompt_id: r.get(2)?,
            prompt_version: r.get(3)?,
            model: r.get(4)?,
            input_tokens: r.get(5)?,
            output_tokens: r.get(6)?,
            cache_read_tokens: r.get(7)?,
            cache_write_tokens: r.get(8)?,
            estimated: r.get::<_, i64>(9)? != 0,
            first_token_ms: r.get(10)?,
            total_ms: r.get(11)?,
            outcome: Outcome::parse(&outcome_str).unwrap_or(Outcome::Error),
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, prompt_id: &str, outcome: Outcome) -> UsageRow {
        UsageRow {
            ts,
            route: Route::MessagesApi,
            prompt_id: prompt_id.into(),
            prompt_version: 1,
            model: "test-model".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 80,
            cache_write_tokens: 0,
            estimated: false,
            first_token_ms: Some(210),
            total_ms: 900,
            outcome,
        }
    }

    fn routed(ts: i64, route: Route, prompt_id: &str, outcome: Outcome, ft: Option<i64>) -> UsageRow {
        UsageRow {
            route,
            first_token_ms: ft,
            ..row(ts, prompt_id, outcome)
        }
    }

    #[test]
    fn completions_split_the_two_lanes_and_ignore_failures() {
        let conn = crate::open_in_memory().unwrap();
        for r in [
            // Agent lane: BYOK and the delegated CLI are the same lane.
            routed(100, Route::MessagesApi, "inline_chat", Outcome::Ok, Some(420)),
            routed(101, Route::LocalAgent, "reply_draft", Outcome::Ok, Some(180)),
            // Ok but no first token observed — counted, but contributes no latency.
            routed(102, Route::MessagesApi, "scribe_edit", Outcome::Ok, None),
            // A failure is not a completion.
            routed(103, Route::MessagesApi, "inline_chat", Outcome::Error, Some(9_000)),
            // Batch lane, both routes, grouped by prompt.
            routed(104, Route::BatchRelay, "dream_consolidation", Outcome::Ok, None),
            routed(105, Route::BatchApi, "dream_consolidation", Outcome::Ok, None),
            routed(106, Route::BatchRelay, "morning_brief", Outcome::Ok, None),
            routed(107, Route::BatchRelay, "morning_brief", Outcome::Error, None),
            // Before the window.
            routed(50, Route::MessagesApi, "inline_chat", Outcome::Ok, Some(10)),
        ] {
            insert(&conn, &r).unwrap();
        }

        let c = completions_since(&conn, 100).unwrap();
        assert_eq!(c.agent_ok, 3, "two lanes count as one, the error does not");
        assert_eq!(
            c.first_token_ms,
            vec![180, 420],
            "ascending, and the ok call with no first token adds no zero"
        );
        assert_eq!(
            c.batch_ok,
            vec![("dream_consolidation".to_string(), 2), ("morning_brief".to_string(), 1)],
            "grouped by prompt across both batch routes, name-ordered"
        );

        // An empty window is measured and zero, not absent.
        assert_eq!(completions_since(&conn, 10_000).unwrap(), Completions::default());
    }

    #[test]
    fn insert_and_read_back_roundtrips() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(100, "chat", Outcome::Ok)).unwrap();
        let rows = recent(&conn, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_id, "chat");
        assert_eq!(rows[0].cache_read_tokens, 80);
        assert_eq!(rows[0].first_token_ms, Some(210));
        assert_eq!(rows[0].outcome, Outcome::Ok);
    }

    #[test]
    fn the_table_has_no_text_column() {
        // The structural guarantee, asserted against the real schema: no column exists whose
        // name suggests it could hold a prompt or completion body.
        let conn = crate::open_in_memory().unwrap();
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('llm_usage')").unwrap();
        let cols: Vec<String> =
            stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(!cols.is_empty(), "llm_usage table missing");
        for banned in ["text", "body", "content", "prompt_text", "chunk", "message"] {
            assert!(
                !cols.iter().any(|c| c == banned),
                "llm_usage must not grow a text-bearing column: {banned}"
            );
        }
    }

    #[test]
    fn check_constraints_reject_unknown_route_and_outcome() {
        let conn = crate::open_in_memory().unwrap();
        let bad_route = conn.execute(
            "INSERT INTO llm_usage (ts, route, prompt_id, prompt_version, model, total_ms, outcome)
             VALUES (1, 'sneaky', 'p', 1, 'm', 1, 'ok')",
            [],
        );
        assert!(bad_route.is_err(), "CHECK must reject an unknown route");
        let bad_outcome = conn.execute(
            "INSERT INTO llm_usage (ts, route, prompt_id, prompt_version, model, total_ms, outcome)
             VALUES (1, 'messages_api', 'p', 1, 'm', 1, 'meh')",
            [],
        );
        assert!(bad_outcome.is_err(), "CHECK must reject an unknown outcome");
    }

    #[test]
    fn totals_aggregate_by_window_and_prompt() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(100, "chat", Outcome::Ok)).unwrap();
        insert(&conn, &row(200, "chat", Outcome::RateLimited)).unwrap();
        insert(&conn, &row(300, "inline_draft", Outcome::Ok)).unwrap();
        let mut est = row(400, "chat", Outcome::Ok);
        est.estimated = true;
        insert(&conn, &est).unwrap();

        let all = totals_since(&conn, 0, None).unwrap();
        assert_eq!(all.calls, 4);
        assert_eq!(all.ok_calls, 3);
        assert_eq!(all.input_tokens, 400);
        assert_eq!(all.estimated_calls, 1);

        let chat = totals_since(&conn, 0, Some("chat")).unwrap();
        assert_eq!(chat.calls, 3);

        let late = totals_since(&conn, 250, None).unwrap();
        assert_eq!(late.calls, 2);
    }

    #[test]
    fn outcome_strings_roundtrip() {
        for o in [
            Outcome::Ok,
            Outcome::Error,
            Outcome::RateLimited,
            Outcome::Unauthorized,
            Outcome::Quota,
            Outcome::Parse,
        ] {
            assert_eq!(Outcome::parse(o.as_str()), Some(o));
        }
        assert_eq!(Outcome::parse("nope"), None);
    }
}
