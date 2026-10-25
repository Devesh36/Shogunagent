//! Durable hour/day/week/month summaries with auditable bottom-up provenance.

use rusqlite::{params, Connection, OptionalExtension};

use crate::event_log::EventText;

const COARSE_EVENT_EVIDENCE_LIMIT: usize = 16;
const SHARD_STATE_PENDING: &str = "pending";
const SHARD_STATE_RUNNING: &str = "running";
const SHARD_STATE_DONE: &str = "done";
const SHARD_STATE_FAILED: &str = "failed";
const FAILURE_CLASS_VALIDATION: &str = "validation";
const FAILURE_CLASS_STORAGE: &str = "storage";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodLevel {
    Hour,
    Day,
    Week,
    Month,
}

impl PeriodLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    fn child(self) -> Option<Self> {
        match self {
            Self::Hour => None,
            Self::Day => Some(Self::Hour),
            Self::Week => Some(Self::Day),
            Self::Month => Some(Self::Day),
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "hour" => Some(Self::Hour),
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodSummary {
    pub id: i64,
    pub level: String,
    pub period_start_ms: i64,
    pub period_end_ms: i64,
    pub title: String,
    pub content: String,
    pub generation_mode: String,
    pub generation_model: Option<String>,
    pub generated: bool,
    pub content_digest: String,
    pub is_dirty: bool,
    pub dirty_at_ms: Option<i64>,
    pub generated_at_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub struct NewPeriodSummary<'a> {
    pub level: PeriodLevel,
    pub period_start_ms: i64,
    pub period_end_ms: i64,
    pub title: &'a str,
    pub content: &'a str,
    pub generated: bool,
    pub generation_model: Option<&'a str>,
    pub event_ids: &'a [i64],
    pub child_summary_ids: &'a [i64],
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodWindow {
    pub level: PeriodLevel,
    pub start_ms: i64,
    pub end_ms: i64,
    pub title: String,
}

/// Content-free generation health for hierarchical summaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationMetrics {
    pub total: i64,
    pub model_generated: i64,
    pub extractive: i64,
    pub dirty: i64,
    pub latest_generated_at_ms: Option<i64>,
}

pub fn generation_metrics(conn: &Connection) -> Result<GenerationMetrics, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*),
                COALESCE(sum(generation_mode = 'model'), 0),
                COALESCE(sum(generation_mode = 'extractive'), 0),
                COALESCE(sum(is_dirty), 0),
                max(generated_at_ms)
           FROM period_summaries",
        [],
        |row| {
            Ok(GenerationMetrics {
                total: row.get(0)?,
                model_generated: row.get(1)?,
                extractive: row.get(2)?,
                dirty: row.get(3)?,
                latest_generated_at_ms: row.get(4)?,
            })
        },
    )
}

/// Retryable shards, bounded and ordered bottom-up for deterministic Dream retry.
///
/// `done` and `failed`/`validation` are terminal. A validation failure is deterministic for its
/// recorded inputs; any old dirty summary remains hidden rather than retried. Every other save
/// failure is storage-related and remains retryable.
pub fn pending_windows(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<PeriodWindow>, rusqlite::Error> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT r.level, r.period_start_ms, r.period_end_ms
           FROM period_summary_shard_runs r
          WHERE r.state IN ('pending', 'running')
             OR (r.state = 'failed' AND r.failure_class = 'storage')
             OR (
                EXISTS (
                    SELECT 1 FROM period_summaries s
                     WHERE s.level = r.level
                       AND s.period_start_ms = r.period_start_ms
                       AND s.period_end_ms = r.period_end_ms
                       AND s.is_dirty = 1
                )
                AND (r.state <> 'failed' OR r.failure_class IS NULL OR r.failure_class <> 'validation')
             )
          ORDER BY CASE r.level
                     WHEN 'hour' THEN 0 WHEN 'day' THEN 1
                     WHEN 'week' THEN 2 ELSE 3
                   END,
                   r.period_start_ms, r.period_end_ms
          LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |row| {
        let level = row.get::<_, String>(0)?;
        let level = PeriodLevel::parse(&level).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(0, "level".to_owned(), rusqlite::types::Type::Text)
        })?;
        let start_ms = row.get(1)?;
        let end_ms = row.get(2)?;
        Ok(PeriodWindow {
            level,
            start_ms,
            end_ms,
            title: format!("{} summary [{start_ms},{end_ms})", level.as_str()),
        })
    })?;
    rows.collect()
}

/// Generate already-closed windows in caller-provided bottom-up order. Local fallback remains
/// extractive because prose always comes through the existing summarizer seam.
pub fn generate_closed(
    conn: &mut Connection,
    windows: &[PeriodWindow],
    summarize: impl Fn(&[EventText]) -> Option<String>,
    generated: bool,
    generation_model: Option<&str>,
    now_ms: i64,
) -> Result<usize, rusqlite::Error> {
    let mut written = 0;
    for window in windows.iter().filter(|window| window.end_ms <= now_ms) {
        if shard_is_terminal(conn, window)? {
            continue;
        }
        set_shard_state(conn, window, SHARD_STATE_RUNNING, None, now_ms, true)?;
        let events = if window.level == PeriodLevel::Hour {
            events_between(conn, window.start_ms, window.end_ms)?
        } else {
            bounded_events_between(conn, window.start_ms, window.end_ms, COARSE_EVENT_EVIDENCE_LIMIT)?
        };
        let children = match window.level.child() {
            Some(level) => children_between(conn, level, window.start_ms, window.end_ms)?,
            None => Vec::new(),
        };
        if (window.level != PeriodLevel::Hour && children.is_empty())
            || (window.level == PeriodLevel::Hour && events.is_empty())
        {
            // Nothing left to summarise: an old summary of this window quoted inputs that are
            // gone, so it goes too rather than staying behind as the only copy (#766).
            drop_window_summary(conn, window)?;
            // A coarser window whose children are only not written yet (a range delete queues
            // the hours and the day together) is not finished: leave it queued, so it is built
            // once they exist instead of staying empty until something else dirties it.
            let state = match window.level.child() {
                Some(child) if children_unfinished(conn, child, window)? => SHARD_STATE_PENDING,
                _ => SHARD_STATE_DONE,
            };
            set_shard_state(conn, window, state, None, now_ms, false)?;
            continue;
        }
        let inputs = if window.level == PeriodLevel::Hour {
            events.clone()
        } else {
            let mut inputs: Vec<EventText> = children
                .iter()
                .map(|child| EventText { id: child.id, content: child.content.clone() })
                .collect();
            // Selected canonical evidence grounds the coarser synthesis without replacing child
            // provenance. Bound selection keeps prompt size deterministic.
            inputs.extend(events.iter().cloned());
            inputs
        };
        let Some(content) = summarize(&inputs) else {
            set_shard_state(conn, window, SHARD_STATE_DONE, None, now_ms, false)?;
            continue;
        };
        let event_ids: Vec<i64> = events.iter().map(|event| event.id).collect();
        let child_ids: Vec<i64> = children.iter().map(|child| child.id).collect();
        if let Err(error) = save(conn, &NewPeriodSummary {
            level: window.level,
            period_start_ms: window.start_ms,
            period_end_ms: window.end_ms,
            title: &window.title,
            content: &content,
            generated,
            generation_model,
            event_ids: &event_ids,
            child_summary_ids: &child_ids,
            now_ms,
        }) {
            let failure_class = failure_class(&error);
            set_shard_state(conn, window, SHARD_STATE_FAILED, Some(failure_class), now_ms, false)?;
            return Err(error);
        }
        set_shard_state(conn, window, SHARD_STATE_DONE, None, now_ms, false)?;
        written += 1;
    }
    Ok(written)
}

fn shard_is_terminal(
    conn: &Connection,
    window: &PeriodWindow,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM period_summary_shard_runs r
             WHERE r.level=?1 AND r.period_start_ms=?2 AND r.period_end_ms=?3
               AND (
                    (r.state='failed' AND r.failure_class='validation')
                    OR (r.state='done' AND NOT EXISTS(
                        SELECT 1 FROM period_summaries s
                         WHERE s.level=r.level AND s.period_start_ms=r.period_start_ms
                           AND s.period_end_ms=r.period_end_ms AND s.is_dirty=1))
               ))",
        params![window.level.as_str(), window.start_ms, window.end_ms],
        |r| r.get(0),
    )
}

fn failure_class(error: &rusqlite::Error) -> &'static str {
    if matches!(error, rusqlite::Error::InvalidQuery) {
        FAILURE_CLASS_VALIDATION
    } else {
        FAILURE_CLASS_STORAGE
    }
}

/// Are any of this window's child windows still queued or being written?
fn children_unfinished(
    conn: &Connection,
    child_level: PeriodLevel,
    window: &PeriodWindow,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM period_summary_shard_runs
                        WHERE level = ?1 AND period_start_ms >= ?2 AND period_end_ms <= ?3
                          AND state IN ('pending', 'running'))",
        params![child_level.as_str(), window.start_ms, window.end_ms],
        |row| row.get(0),
    )
}

/// Remove a window's summary; its event and child provenance rows cascade. Its search
/// projection goes first: nothing ties that row to the summary, so it would otherwise stay
/// searchable (#766).
fn drop_window_summary(conn: &Connection, window: &PeriodWindow) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM search_documents
          WHERE source_table = 'period_summaries'
            AND source_id IN (SELECT CAST(id AS TEXT) FROM period_summaries
                               WHERE level = ?1 AND period_start_ms = ?2 AND period_end_ms = ?3)",
        params![window.level.as_str(), window.start_ms, window.end_ms],
    )?;
    conn.execute(
        "DELETE FROM period_summaries
          WHERE level = ?1 AND period_start_ms = ?2 AND period_end_ms = ?3",
        params![window.level.as_str(), window.start_ms, window.end_ms],
    )?;
    Ok(())
}

fn set_shard_state(
    conn: &Connection,
    window: &PeriodWindow,
    state: &str,
    failure_class: Option<&str>,
    now_ms: i64,
    increment_attempt: bool,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO period_summary_shard_runs
           (level,period_start_ms,period_end_ms,state,attempts,failure_class,updated_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(level,period_start_ms,period_end_ms) DO UPDATE SET
           state=excluded.state,
           attempts=period_summary_shard_runs.attempts + excluded.attempts,
           failure_class=excluded.failure_class,updated_at_ms=excluded.updated_at_ms",
        params![window.level.as_str(), window.start_ms, window.end_ms, state,
            i64::from(increment_attempt), failure_class, now_ms],
    )?;
    Ok(())
}

/// True only after every closed shard reached a terminal ledger state.
///
/// Validation failures are terminal because retrying unchanged inputs cannot succeed. Storage
/// failures are deliberately non-terminal so a later Dream cycle resumes them.
pub fn all_shards_done(
    conn: &Connection,
    windows: &[PeriodWindow],
    now_ms: i64,
) -> Result<bool, rusqlite::Error> {
    for window in windows.iter().filter(|window| window.end_ms <= now_ms) {
        if !shard_is_terminal(conn, window)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Restore V22 search projection after projection-only repair. Canonical summary rows and both
/// provenance tables remain untouched; V21 publication is rebuilt only after they are readable.
pub fn reproject_all(conn: &mut Connection, now_ms: i64) -> Result<usize, rusqlite::Error> {
    let summaries: Vec<PeriodSummary> = {
        let mut stmt = conn.prepare(
            "SELECT id, level, period_start_ms, period_end_ms, title, content, generation_mode,
                    generation_model, generated, content_digest, is_dirty, dirty_at_ms,
                    generated_at_ms, created_at_ms, updated_at_ms
               FROM period_summaries ORDER BY id",
        )?;
        let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let tx = conn.transaction()?;
    for summary in &summaries {
        let projection_digest =
            crate::briefs::digest(&format!("{}\0{}", summary.title, summary.content));
        tx.execute(
            "INSERT INTO search_documents
               (document_kind,source_table,source_id,title,body,period_start_ms,period_end_ms,
                confidence,generation_kind,generation_model,generated_at_ms,content_digest,
                source_updated_at_ms,is_dirty,dirty_at_ms,last_projected_at_ms,last_failure_class,
                created_at_ms,updated_at_ms)
             VALUES('period_summary','period_summaries',?1,?2,?3,?4,?5,1.0,?6,?7,?8,?9,
                    ?10,?11,?12,?13,NULL,?14,?15)",
            params![summary.id.to_string(), summary.title, summary.content,
                summary.period_start_ms, summary.period_end_ms, summary.generation_mode,
                summary.generation_model, summary.generated_at_ms, projection_digest,
                summary.updated_at_ms, i64::from(summary.is_dirty), summary.dirty_at_ms, now_ms,
                summary.created_at_ms, summary.updated_at_ms],
        )?;
        let document_id = tx.last_insert_rowid();
        let mut events = tx.prepare(
            "SELECT event_id,weight FROM period_summary_event_provenance
              WHERE summary_id=?1 ORDER BY event_id",
        )?;
        let evidence = events.query_map([summary.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(events);
        for (event_id, weight) in evidence {
            tx.execute(
                "INSERT INTO search_document_provenance(document_id,evidence_kind,event_id,weight)
                 VALUES(?1,'event',?2,?3)",
                params![document_id, event_id, weight],
            )?;
        }
    }
    tx.commit()?;
    Ok(summaries.len())
}

/// Canonical event evidence in exact half-open bounds, oldest first.
pub fn events_between(
    conn: &Connection,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<EventText>, rusqlite::Error> {
    crate::event_log::events_in_range(conn, start_ms, end_ms)
}

/// Oldest canonical event evidence with a deterministic bound for day/week/month prompts.
fn bounded_events_between(
    conn: &Connection,
    start_ms: i64,
    end_ms: i64,
    limit: usize,
) -> Result<Vec<EventText>, rusqlite::Error> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT id, content FROM event_log WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, id LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![start_ms, end_ms, limit], |row| {
        Ok(EventText { id: row.get(0)?, content: row.get(1)? })
    })?;
    rows.collect()
}

/// Clean direct children fully contained by the requested parent bounds.
pub fn children_between(
    conn: &Connection,
    child_level: PeriodLevel,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<PeriodSummary>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, level, period_start_ms, period_end_ms, title, content, generation_mode,
                generation_model, generated, content_digest, is_dirty, dirty_at_ms,
                generated_at_ms, created_at_ms, updated_at_ms
           FROM period_summaries
          WHERE level = ?1 AND period_start_ms >= ?2 AND period_end_ms <= ?3 AND is_dirty = 0
          ORDER BY period_start_ms, id",
    )?;
    let rows = stmt.query_map(params![child_level.as_str(), start_ms, end_ms], row)?;
    rows.collect()
}

pub fn get(
    conn: &Connection,
    level: PeriodLevel,
    start_ms: i64,
    end_ms: i64,
) -> Result<Option<PeriodSummary>, rusqlite::Error> {
    conn.query_row(
        "SELECT id, level, period_start_ms, period_end_ms, title, content, generation_mode,
                generation_model, generated, content_digest, is_dirty, dirty_at_ms,
                generated_at_ms, created_at_ms, updated_at_ms
           FROM period_summaries WHERE level = ?1 AND period_start_ms = ?2 AND period_end_ms = ?3",
        params![level.as_str(), start_ms, end_ms],
        row,
    )
    .optional()
}

/// Summaries of one level that overlap `[from_ms, to_ms)`, oldest first, at most `limit`.
pub fn overlapping(
    conn: &Connection,
    level: PeriodLevel,
    from_ms: i64,
    to_ms: i64,
    limit: usize,
) -> Result<Vec<PeriodSummary>, rusqlite::Error> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT id, level, period_start_ms, period_end_ms, title, content, generation_mode,
                generation_model, generated, content_digest, is_dirty, dirty_at_ms,
                generated_at_ms, created_at_ms, updated_at_ms
           FROM period_summaries
          WHERE level = ?1 AND period_end_ms > ?2 AND period_start_ms < ?3
          ORDER BY period_start_ms, id LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![level.as_str(), from_ms, to_ms, limit], row)?;
    rows.collect()
}

/// Upsert summary, replace both provenance sets, then publish search projection in one transaction.
/// If provenance validation fails, no summary or V21 projection becomes visible.
pub fn save(
    conn: &mut Connection,
    input: &NewPeriodSummary<'_>,
) -> Result<i64, rusqlite::Error> {
    validate(conn, input)?;
    save_validated(conn, input)
}

/// Publish a model-written calendar-day summary with direct event provenance. A dedup-touched
/// capture may have an old canonical timestamp, so its day membership comes from activity credits.
pub fn save_model_day(
    conn: &mut Connection,
    input: &NewPeriodSummary<'_>,
    local_date: &str,
) -> Result<i64, rusqlite::Error> {
    let canonical_window: bool = conn.query_row(
        "SELECT strftime('%Y-%m-%d %H:%M:%S',?1 / 1000,'unixepoch','localtime')=?3||' 00:00:00'
             AND strftime('%Y-%m-%d %H:%M:%S',?2 / 1000,'unixepoch','localtime')=
                 date(?3,'+1 day')||' 00:00:00'",
        params![input.period_start_ms, input.period_end_ms, local_date],
        |row| row.get(0),
    )?;
    if input.level != PeriodLevel::Day
        || !input.generated
        || input.generation_model.is_none()
        || input.event_ids.is_empty()
        || !input.child_summary_ids.is_empty()
        || input.period_start_ms >= input.period_end_ms
        || input.content.trim().is_empty()
        || !canonical_window
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    for event_id in input.event_ids {
        let valid: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM event_log e WHERE e.id=?1 AND (
                 e.ts>=?2 AND e.ts<?3 OR EXISTS (
                    SELECT 1 FROM dashboard_activity_credits c
                     WHERE c.event_id=e.id AND c.local_date=?4)))",
            params![event_id, input.period_start_ms, input.period_end_ms, local_date],
            |row| row.get(0),
        )?;
        if !valid {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    let id = save_validated(conn, input)?;
    set_shard_state(
        conn,
        &PeriodWindow {
            level: PeriodLevel::Day,
            start_ms: input.period_start_ms,
            end_ms: input.period_end_ms,
            title: input.title.to_string(),
        },
        SHARD_STATE_DONE,
        None,
        input.now_ms,
        false,
    )?;
    Ok(id)
}

fn save_validated(
    conn: &mut Connection,
    input: &NewPeriodSummary<'_>,
) -> Result<i64, rusqlite::Error> {
    let tx = conn.savepoint()?;
    let digest = crate::briefs::digest(input.content);
    let projection_digest = crate::briefs::digest(&format!("{}\0{}", input.title, input.content));
    let mode = if input.generated { "model" } else { "extractive" };
    tx.execute(
        "INSERT INTO period_summaries
           (level, period_start_ms, period_end_ms, title, content, generation_mode,
            generation_model, generated, content_digest, is_dirty, dirty_at_ms,
            generated_at_ms, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, ?10, ?10, ?10)
         ON CONFLICT(level, period_start_ms, period_end_ms) DO UPDATE SET
           title=excluded.title, content=excluded.content,
           generation_mode=excluded.generation_mode, generation_model=excluded.generation_model,
           generated=excluded.generated, content_digest=excluded.content_digest,
           is_dirty=0, dirty_at_ms=NULL, generated_at_ms=excluded.generated_at_ms,
           updated_at_ms=excluded.updated_at_ms",
        params![
            input.level.as_str(), input.period_start_ms, input.period_end_ms, input.title,
            input.content, mode, input.generation_model, i64::from(input.generated), digest,
            input.now_ms
        ],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM period_summaries
          WHERE level=?1 AND period_start_ms=?2 AND period_end_ms=?3",
        params![input.level.as_str(), input.period_start_ms, input.period_end_ms],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM period_summary_event_provenance WHERE summary_id=?1", [id])?;
    tx.execute("DELETE FROM period_summary_child_provenance WHERE summary_id=?1", [id])?;
    for event_id in input.event_ids {
        tx.execute(
            "INSERT INTO period_summary_event_provenance(summary_id,event_id) VALUES(?1,?2)",
            params![id, event_id],
        )?;
    }
    for child_id in input.child_summary_ids {
        tx.execute(
            "INSERT INTO period_summary_child_provenance(summary_id,child_summary_id) VALUES(?1,?2)",
            params![id, child_id],
        )?;
    }

    let source_id = id.to_string();
    tx.execute(
        "INSERT INTO search_documents
           (document_kind,source_table,source_id,title,body,period_start_ms,period_end_ms,
            confidence,generation_kind,generation_model,generated_at_ms,content_digest,
            source_updated_at_ms,is_dirty,dirty_at_ms,last_projected_at_ms,last_failure_class,
            created_at_ms,updated_at_ms)
         VALUES('period_summary','period_summaries',?1,?2,?3,?4,?5,1.0,?6,?7,?8,?9,
                ?8,0,NULL,?8,NULL,?8,?8)
         ON CONFLICT(source_table,source_id) DO UPDATE SET
           title=excluded.title,body=excluded.body,period_start_ms=excluded.period_start_ms,
           period_end_ms=excluded.period_end_ms,generation_kind=excluded.generation_kind,
           generation_model=excluded.generation_model,generated_at_ms=excluded.generated_at_ms,
           content_digest=excluded.content_digest,source_updated_at_ms=excluded.source_updated_at_ms,
           is_dirty=0,dirty_at_ms=NULL,last_projected_at_ms=excluded.last_projected_at_ms,
           last_failure_class=NULL,updated_at_ms=excluded.updated_at_ms",
        params![
            source_id, input.title, input.content, input.period_start_ms, input.period_end_ms,
            mode, input.generation_model, input.now_ms, projection_digest
        ],
    )?;
    let document_id: i64 = tx.query_row(
        "SELECT id FROM search_documents WHERE source_table='period_summaries' AND source_id=?1",
        [source_id],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM search_document_provenance WHERE document_id=?1", [document_id])?;
    for event_id in input.event_ids {
        tx.execute(
            "INSERT INTO search_document_provenance(document_id,evidence_kind,event_id)
             VALUES(?1,'event',?2)",
            params![document_id, event_id],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

fn validate(conn: &Connection, input: &NewPeriodSummary<'_>) -> Result<(), rusqlite::Error> {
    if input.period_start_ms >= input.period_end_ms || input.content.trim().is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if input.generated != input.generation_model.is_some() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    for event_id in input.event_ids {
        let valid: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM event_log WHERE id=?1 AND ts>=?2 AND ts<?3)",
            params![event_id, input.period_start_ms, input.period_end_ms],
            |r| r.get(0),
        )?;
        if !valid {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    match input.level.child() {
        None if !input.child_summary_ids.is_empty() => return Err(rusqlite::Error::InvalidQuery),
        Some(expected) => {
            if input.child_summary_ids.is_empty() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            for child_id in input.child_summary_ids {
                let valid: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM period_summaries
                      WHERE id=?1 AND level=?2 AND period_start_ms>=?3 AND period_end_ms<=?4
                        AND is_dirty=0)",
                    params![child_id, expected.as_str(), input.period_start_ms, input.period_end_ms],
                    |r| r.get(0),
                )?;
                if !valid {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
        }
        None => {}
    }
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PeriodSummary> {
    Ok(PeriodSummary {
        id: r.get(0)?, level: r.get(1)?, period_start_ms: r.get(2)?, period_end_ms: r.get(3)?,
        title: r.get(4)?, content: r.get(5)?, generation_mode: r.get(6)?,
        generation_model: r.get(7)?, generated: r.get::<_, i64>(8)? != 0,
        content_digest: r.get(9)?, is_dirty: r.get::<_, i64>(10)? != 0,
        dirty_at_ms: r.get(11)?, generated_at_ms: r.get(12)?, created_at_ms: r.get(13)?,
        updated_at_ms: r.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{insert, NewEvent};

    fn event(conn: &Connection, ts: i64, content: &str, hash: &str) -> i64 {
        insert(conn, &NewEvent {
            ts, source: "capture", kind: "text", app_bundle_id: None, window_title: None,
            content, content_hash: hash, dwell_ms: 0, display_id: None, window_bounds: None,
        }).unwrap()
    }

    #[test]
    fn hour_is_half_open_and_publishes_only_after_provenance_lands() {
        let mut conn = crate::open_in_memory().unwrap();
        let inside = event(&conn, 999, "inside evidence", "a");
        let edge = event(&conn, 1_000, "next hour", "b");
        let id = save(&mut conn, &NewPeriodSummary {
            level: PeriodLevel::Hour, period_start_ms: 0, period_end_ms: 1_000,
            title: "hour", content: "inside evidence", generated: false,
            generation_model: None, event_ids: &[inside], child_summary_ids: &[], now_ms: 2_000,
        }).unwrap();
        assert!(id > 0);
        assert_eq!(events_between(&conn, 0, 1_000).unwrap(), vec![EventText { id: inside, content: "inside evidence".into() }]);
        assert_ne!(inside, edge);
        let projected: i64 = conn.query_row("SELECT count(*) FROM search_documents WHERE source_table='period_summaries'", [], |r| r.get(0)).unwrap();
        assert_eq!(projected, 1);
    }

    #[test]
    fn overlapping_returns_only_the_level_and_window_asked_for_oldest_first() {
        let mut conn = crate::open_in_memory().unwrap();
        for (start, end) in [(0, 1_000), (1_000, 2_000), (2_000, 3_000)] {
            let e = event(&conn, start + 1, "text", &format!("h{start}"));
            save(&mut conn, &NewPeriodSummary {
                level: PeriodLevel::Hour, period_start_ms: start, period_end_ms: end,
                title: "hour", content: "text", generated: false,
                generation_model: None, event_ids: &[e], child_summary_ids: &[], now_ms: 9_000,
            }).unwrap();
        }
        let hit = overlapping(&conn, PeriodLevel::Hour, 500, 1_500, 10).unwrap();
        assert_eq!(hit.iter().map(|p| p.period_start_ms).collect::<Vec<_>>(), vec![0, 1_000]);
        assert_eq!(overlapping(&conn, PeriodLevel::Hour, 0, 3_000, 2).unwrap().len(), 2);
        assert!(overlapping(&conn, PeriodLevel::Day, 0, 3_000, 10).unwrap().is_empty());
    }

    #[test]
    fn a_window_whose_events_are_gone_keeps_no_old_summary() {
        // #766: re-running a window left with no events must not mark it done and keep the old
        // text, which quotes the deleted events.
        let mut conn = crate::open_in_memory().unwrap();
        let secret = event(&conn, 10, "Offer letter for Dana: base 185k.", "a");
        save(&mut conn, &NewPeriodSummary {
            level: PeriodLevel::Hour, period_start_ms: 0, period_end_ms: 100,
            title: "hour", content: "Offer letter for Dana: base 185k.", generated: false,
            generation_model: None, event_ids: &[secret], child_summary_ids: &[], now_ms: 200,
        }).unwrap();
        conn.execute("DELETE FROM event_log WHERE id = ?1", [secret]).unwrap();
        let window = PeriodWindow {
            level: PeriodLevel::Hour, start_ms: 0, end_ms: 100, title: "hour".into(),
        };
        let written = generate_closed(&mut conn, &[window], |_| Some("unused".into()), false, None, 300).unwrap();
        assert_eq!(written, 0);
        assert!(get(&conn, PeriodLevel::Hour, 0, 100).unwrap().is_none());
        let still_searchable: i64 = conn
            .query_row("SELECT count(*) FROM search_documents WHERE source_table='period_summaries'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(still_searchable, 0, "the search projection goes with the summary");
    }

    #[test]
    fn a_day_waits_for_hours_that_are_still_queued_and_is_then_rebuilt() {
        // After a range delete the hours and the day are queued together. A day processed while
        // its hours are not written yet must stay queued, not be marked done and left empty.
        let mut conn = crate::open_in_memory().unwrap();
        event(&conn, 10, "Lunch moved to Friday.", "a");
        let hour = PeriodWindow { level: PeriodLevel::Hour, start_ms: 0, end_ms: 100, title: "hour".into() };
        let day = PeriodWindow { level: PeriodLevel::Day, start_ms: 0, end_ms: 1_000, title: "day".into() };
        set_shard_state(&conn, &hour, SHARD_STATE_PENDING, None, 0, false).unwrap();
        set_shard_state(&conn, &day, SHARD_STATE_PENDING, None, 0, false).unwrap();
        let summarize = |inputs: &[EventText]| Some(inputs.iter().map(|e| e.content.as_str()).collect::<Vec<_>>().join(" "));

        // Day first, hours not written yet: nothing to build from, and it stays queued.
        assert_eq!(generate_closed(&mut conn, std::slice::from_ref(&day), summarize, false, None, 2_000).unwrap(), 0);
        assert!(!shard_is_terminal(&conn, &day).unwrap());

        // Hours, then the day again: it is rebuilt from them.
        assert_eq!(generate_closed(&mut conn, &[hour, day], summarize, false, None, 2_000).unwrap(), 2);
        let rebuilt = get(&conn, PeriodLevel::Day, 0, 1_000).unwrap().unwrap();
        assert!(rebuilt.content.contains("Lunch moved to Friday."), "{}", rebuilt.content);
    }

    #[test]
    fn retry_is_idempotent_and_child_change_dirties_parent() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = event(&conn, 10, "grounded", "a");
        let hour = NewPeriodSummary { level: PeriodLevel::Hour, period_start_ms: 0,
            period_end_ms: 100, title: "hour", content: "grounded", generated: false,
            generation_model: None, event_ids: &[event_id], child_summary_ids: &[], now_ms: 200 };
        let hour_id = save(&mut conn, &hour).unwrap();
        assert_eq!(save(&mut conn, &hour).unwrap(), hour_id);
        let day_id = save(&mut conn, &NewPeriodSummary { level: PeriodLevel::Day,
            period_start_ms: 0, period_end_ms: 100, title: "day", content: "grounded",
            generated: false, generation_model: None, event_ids: &[event_id],
            child_summary_ids: &[hour_id], now_ms: 201 }).unwrap();
        conn.execute("UPDATE period_summaries SET content='changed',content_digest='changed',updated_at_ms=300 WHERE id=?1", [hour_id]).unwrap();
        assert!(get(&conn, PeriodLevel::Day, 0, 100).unwrap().unwrap().is_dirty);
        let count: i64 = conn.query_row("SELECT count(*) FROM period_summaries WHERE id IN (?1,?2)", params![hour_id, day_id], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn extractive_content_cannot_invent_evidence_in_fallback_fixture() {
        let conn = crate::open_in_memory().unwrap();
        let events = [EventText { id: 1, content: "Alpha happened. Hidden tail".into() }];
        let output = events[0].content.split('.').next().unwrap();
        assert!(events.iter().any(|event| event.content.contains(output)));
        assert_eq!(conn.query_row("SELECT count(*) FROM period_summaries", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn coarse_periods_use_bounded_oldest_event_evidence_and_complete_child_provenance() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_ids: Vec<i64> = (0..17).map(|index| {
            let content = format!("event-{index}");
            let hash = format!("coarse-{index}");
            event(&conn, 10 + index, &content, &hash)
        }).collect();
        let hour_id = save(&mut conn, &NewPeriodSummary {
            level: PeriodLevel::Hour, period_start_ms: 0, period_end_ms: 100,
            title: "hour", content: "complete child evidence", generated: false,
            generation_model: None, event_ids: &event_ids, child_summary_ids: &[], now_ms: 200,
        }).unwrap();
        let windows = [PeriodWindow { level: PeriodLevel::Day, start_ms: 0, end_ms: 100, title: "day".into() }];
        let observed = std::cell::RefCell::new(Vec::new());
        assert_eq!(generate_closed(&mut conn, &windows, |inputs| {
            observed.replace(inputs.iter().map(|input| input.id).collect());
            Some("bounded day".into())
        }, false, None, 200).unwrap(), 1);
        let expected = std::iter::once(hour_id)
            .chain(event_ids.iter().copied().take(COARSE_EVENT_EVIDENCE_LIMIT)).collect::<Vec<_>>();
        assert_eq!(*observed.borrow(), expected);
        let day_id = get(&conn, PeriodLevel::Day, 0, 100).unwrap().unwrap().id;
        let parent_events = conn.prepare("SELECT event_id FROM period_summary_event_provenance WHERE summary_id=?1 ORDER BY event_id")
            .unwrap().query_map([day_id], |row| row.get::<_, i64>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(parent_events, event_ids[..COARSE_EVENT_EVIDENCE_LIMIT]);
        let child_event_count: i64 = conn.query_row("SELECT count(*) FROM period_summary_event_provenance WHERE summary_id=?1", [hour_id], |row| row.get(0)).unwrap();
        assert_eq!(child_event_count, 17);
        let parent_child_ids = conn.prepare("SELECT child_summary_id FROM period_summary_child_provenance WHERE summary_id=?1 ORDER BY child_summary_id")
            .unwrap().query_map([day_id], |row| row.get::<_, i64>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(parent_child_ids, vec![hour_id]);
    }

    #[test]
    fn validation_failure_is_terminal_and_never_retries_unchanged_inputs() {
        let mut conn = crate::open_in_memory().unwrap();
        event(&conn, 10, "grounded retry", "retry");
        let windows = [PeriodWindow { level: PeriodLevel::Hour, start_ms: 0, end_ms: 100, title: "hour".into() }];
        let failed = generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), true, None, 200);
        assert!(matches!(failed, Err(rusqlite::Error::InvalidQuery)));
        let first: (String, i64, Option<String>) = conn.query_row(
            "SELECT state,attempts,failure_class FROM period_summary_shard_runs", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(first, (SHARD_STATE_FAILED.into(), 1, Some(FAILURE_CLASS_VALIDATION.into())));
        assert!(pending_windows(&conn, 1).unwrap().is_empty());
        assert!(all_shards_done(&conn, &windows, 200).unwrap());
        assert_eq!(generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), false, None, 201).unwrap(), 0);
        let second: (String, i64, Option<String>) = conn.query_row(
            "SELECT state,attempts,failure_class FROM period_summary_shard_runs", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(second, first);
    }

    #[test]
    fn validation_failure_stops_retrying_even_when_an_old_summary_stays_dirty() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = event(&conn, 10, "grounded retry", "retry");
        let summary_id = save(&mut conn, &NewPeriodSummary {
            level: PeriodLevel::Hour, period_start_ms: 0, period_end_ms: 100,
            title: "hour", content: "old summary", generated: false,
            generation_model: None, event_ids: &[event_id], child_summary_ids: &[], now_ms: 150,
        }).unwrap();
        conn.execute("UPDATE period_summaries SET is_dirty=1,dirty_at_ms=200 WHERE id=?1", [summary_id]).unwrap();
        let windows = [PeriodWindow { level: PeriodLevel::Hour, start_ms: 0, end_ms: 100, title: "hour".into() }];
        let failed = generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), true, None, 200);
        assert!(matches!(failed, Err(rusqlite::Error::InvalidQuery)));
        assert!(get(&conn, PeriodLevel::Hour, 0, 100).unwrap().unwrap().is_dirty);
        assert!(pending_windows(&conn, 1).unwrap().is_empty());
        assert!(all_shards_done(&conn, &windows, 200).unwrap());
        assert_eq!(generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), false, None, 201).unwrap(), 0);
        let attempts: i64 = conn.query_row("SELECT attempts FROM period_summary_shard_runs", [], |row| row.get(0)).unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn refreshed_child_reopens_validation_failed_parent() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = event(&conn, 10, "child evidence", "child-evidence");
        let child_id = save(
            &mut conn,
            &NewPeriodSummary {
                level: PeriodLevel::Hour,
                period_start_ms: 0,
                period_end_ms: 100,
                title: "hour",
                content: "child",
                generated: false,
                generation_model: None,
                event_ids: &[event_id],
                child_summary_ids: &[],
                now_ms: 150,
            },
        )
        .unwrap();
        let parent_id = save(
            &mut conn,
            &NewPeriodSummary {
                level: PeriodLevel::Day,
                period_start_ms: 0,
                period_end_ms: 100,
                title: "day",
                content: "parent",
                generated: false,
                generation_model: None,
                event_ids: &[event_id],
                child_summary_ids: &[child_id],
                now_ms: 151,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE period_summaries SET is_dirty=1,dirty_at_ms=200 WHERE id=?1",
            [parent_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO period_summary_shard_runs
               (level,period_start_ms,period_end_ms,state,attempts,failure_class,updated_at_ms)
             VALUES('day',0,100,'failed',1,'validation',200)",
            [],
        )
        .unwrap();
        assert!(pending_windows(&conn, 1).unwrap().is_empty());

        conn.execute(
            "UPDATE period_summaries
                SET content_digest='refreshed-child',updated_at_ms=201
              WHERE id=?1",
            [child_id],
        )
        .unwrap();

        let ledger: (String, Option<String>) = conn
            .query_row(
                "SELECT state,failure_class FROM period_summary_shard_runs
                  WHERE level='day' AND period_start_ms=0 AND period_end_ms=100",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ledger, ("pending".into(), None));
        let pending = pending_windows(&conn, 1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].level, PeriodLevel::Day);
    }

    #[test]
    fn storage_failure_retries_then_becomes_done_idempotently() {
        let mut conn = crate::open_in_memory().unwrap();
        event(&conn, 10, "grounded retry", "retry");
        let windows = [PeriodWindow { level: PeriodLevel::Hour, start_ms: 0, end_ms: 100, title: "hour".into() }];
        conn.execute_batch("CREATE TRIGGER reject_period_summary_insert BEFORE INSERT ON period_summaries BEGIN SELECT RAISE(ABORT, 'temporary storage fixture'); END;").unwrap();
        let failed = generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), false, None, 200);
        assert!(matches!(failed, Err(rusqlite::Error::SqliteFailure(_, _))));
        let first: (String, i64, Option<String>) = conn.query_row(
            "SELECT state,attempts,failure_class FROM period_summary_shard_runs", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(first, (SHARD_STATE_FAILED.into(), 1, Some(FAILURE_CLASS_STORAGE.into())));
        let retryable = pending_windows(&conn, 1).unwrap();
        assert_eq!(retryable.len(), 1);
        assert_eq!((retryable[0].level, retryable[0].start_ms, retryable[0].end_ms), (PeriodLevel::Hour, 0, 100));
        assert!(!all_shards_done(&conn, &windows, 200).unwrap());
        conn.execute_batch("DROP TRIGGER reject_period_summary_insert").unwrap();
        assert_eq!(generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), false, None, 201).unwrap(), 1);
        assert_eq!(generate_closed(&mut conn, &windows, |events| Some(events[0].content.clone()), false, None, 202).unwrap(), 0);
        let second: (String, i64, Option<String>) = conn.query_row(
            "SELECT state,attempts,failure_class FROM period_summary_shard_runs", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(second, (SHARD_STATE_DONE.into(), 2, None));
        assert!(all_shards_done(&conn, &windows, 202).unwrap());
    }

    #[test]
    fn federated_period_hit_is_typed_then_hidden_when_raw_evidence_is_deleted() {
        use crate::search::{
            search_federated, FederatedSearchOptions, FederatedSourceId,
        };
        use crate::search_documents::SourceRef;

        let mut conn = crate::open_in_memory().unwrap();
        let event_id = event(&conn, 10, "raw grounding", "period-federated");
        let summary_id = save(
            &mut conn,
            &NewPeriodSummary {
                level: PeriodLevel::Hour,
                period_start_ms: 0,
                period_end_ms: 100,
                title: "Hour",
                content: "Saffron milestone completed",
                generated: false,
                generation_model: None,
                event_ids: &[event_id],
                child_summary_ids: &[],
                now_ms: 200,
            },
        )
        .unwrap();

        let hit = search_federated(
            &conn,
            "Saffron milestone",
            None,
            300,
            &FederatedSearchOptions::default(),
            5,
        )
        .unwrap();
        assert_eq!(
            hit.hits[0].source_id,
            FederatedSourceId::Derived(SourceRef::PeriodSummary(summary_id))
        );
        assert_eq!(generation_metrics(&conn).unwrap().extractive, 1);

        crate::search_documents::delete_canonical_event(&mut conn, event_id).unwrap();
        assert_eq!(generation_metrics(&conn).unwrap().dirty, 1);
        assert!(search_federated(
            &conn,
            "Saffron milestone",
            None,
            300,
            &FederatedSearchOptions::default(),
            5,
        )
        .unwrap()
        .hits
        .is_empty());
        assert_eq!(
            crate::search_documents::freshness_metrics(&conn)
                .unwrap()
                .dirty_documents,
            1
        );
    }

    #[test]
    fn model_day_rejects_a_credit_date_outside_its_canonical_window() {
        let mut conn = crate::open_in_memory().unwrap();
        let start: i64 = conn
            .query_row(
                "SELECT CAST(strftime('%s','2026-01-02 00:00:00','utc') AS INTEGER)*1000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let end: i64 = conn
            .query_row(
                "SELECT CAST(strftime('%s','2026-01-03 00:00:00','utc') AS INTEGER)*1000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let event_id = event(&conn, start + 1_000, "grounded work", "day-model");
        let input = NewPeriodSummary {
            level: PeriodLevel::Day,
            period_start_ms: start,
            period_end_ms: end,
            title: "day",
            content: "Worked on grounded work.",
            generated: true,
            generation_model: Some("daily_activity_summary/test"),
            event_ids: &[event_id],
            child_summary_ids: &[],
            now_ms: end,
        };
        assert!(save_model_day(&mut conn, &input, "2026-01-03").is_err());
        assert!(save_model_day(&mut conn, &input, "2026-01-02").is_ok());
    }
}
