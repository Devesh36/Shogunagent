//! Durable scheduling and source selection for the generated About You summary.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

const MIN_SOURCE_CHARS: usize = 12;
const DAILY_SOURCE_LIMIT: usize = 12;
const DAILY_SOURCE_CHARS: usize = 12_000;
const DAILY_CANDIDATE_POOL: i64 = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The id the model cites. A day summary's own id; for a bootstrap source its 1-based place
    /// in the frozen set ([`bootstrap_sources`]).
    pub summary_id: i64,
    /// The row behind the text in its own table (`source` names the table's kind).
    pub row_id: i64,
    pub text: String,
    pub digest: String,
    /// Backend-local day used to prove a recurring claim spans separate activity days.
    pub local_date: String,
    /// The persisted event source. Only Batch-eligible sources reach this selector.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailySource {
    pub event_id: i64,
    pub text: String,
    pub digest: String,
    pub app_bundle_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyManifestSource {
    pub event_id: i64,
    pub digest: String,
    pub activity_date: String,
}

/// Select a bounded completed-day sample across time first, then distinct apps. The source filter
/// is the same one used by every Batch lane; stored bodies already passed canonical redaction.
pub fn daily_sources(
    conn: &Connection,
    local_date: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<DailySource>, rusqlite::Error> {
    let excluded = crate::event_log::BATCH_EXCLUDED_SOURCES
        .iter()
        .map(|source| format!("'{}'", source.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "WITH candidate_ids AS (
             SELECT id FROM event_log WHERE ts>=?2 AND ts<?3
             UNION
             SELECT event_id FROM dashboard_activity_credits WHERE local_date=?1
         ), eligible AS (
             SELECT e.id,e.content_hash,e.app_bundle_id,
                    COALESCE((SELECT max(c.latest_activity_ms) FROM dashboard_activity_credits c
                               WHERE c.event_id=e.id AND c.local_date=?1),e.ts) AS activity_at
               FROM candidate_ids ids JOIN event_log e ON e.id=ids.id
              WHERE e.source NOT IN ({excluded})
                AND length(trim(e.content))>={MIN_SOURCE_CHARS}
         ), slotted AS (
             SELECT *,ntile(60) OVER (ORDER BY activity_at,id) AS slot FROM eligible
         ), sampled AS (
             SELECT *,row_number() OVER (PARTITION BY slot ORDER BY activity_at DESC,id DESC) AS slot_rank
               FROM slotted
         )
         SELECT id,content_hash,app_bundle_id FROM sampled
          WHERE slot_rank<=4 ORDER BY activity_at,id LIMIT {DAILY_CANDIDATE_POOL}"
    );
    let values = vec![
        rusqlite::types::Value::Text(local_date.to_string()),
        rusqlite::types::Value::Integer(from_ms),
        rusqlite::types::Value::Integer(to_ms),
    ];
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut candidates = Vec::new();
    let mut seen_text = HashSet::new();
    for row in rows {
        let (event_id, digest, app_bundle_id) = row?;
        let Some(text) = crate::event_log::batch_event_text(conn, event_id, 2_000)? else {
            continue;
        };
        let source = DailySource {
            event_id,
            text: text.content,
            digest,
            app_bundle_id,
        };
        let normalized = source.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.chars().count() >= MIN_SOURCE_CHARS
            && seen_text.insert(normalized.to_lowercase())
        {
            candidates.push(source);
        }
    }
    let mut selected = Vec::new();
    let mut chars = 0;
    if !candidates.is_empty() {
        for quarter in 0..4 {
            let index = candidates.len().saturating_mul(quarter * 2 + 1) / 8;
            push_daily_source(
                &mut selected,
                &mut chars,
                candidates[index.min(candidates.len() - 1)].clone(),
            );
        }
    }
    let mut apps = HashSet::new();
    for source in &candidates {
        if source
            .app_bundle_id
            .as_ref()
            .is_some_and(|app| apps.insert(app.clone()))
        {
            push_daily_source(&mut selected, &mut chars, source.clone());
        }
    }
    for source in candidates {
        push_daily_source(&mut selected, &mut chars, source);
    }
    Ok(selected)
}

fn push_daily_source(selected: &mut Vec<DailySource>, chars: &mut usize, source: DailySource) {
    if selected.iter().any(|kept| kept.event_id == source.event_id) {
        return;
    }
    let next = source.text.chars().count();
    if selected.len() < DAILY_SOURCE_LIMIT && chars.saturating_add(next) <= DAILY_SOURCE_CHARS {
        *chars += next;
        selected.push(source);
    }
}

pub fn daily_manifest(sources: &[DailySource], local_date: &str) -> Vec<DailyManifestSource> {
    sources
        .iter()
        .map(|source| DailyManifestSource {
            event_id: source.event_id,
            digest: source.digest.clone(),
            activity_date: local_date.to_string(),
        })
        .collect()
}

pub fn daily_sources_for_manifest(
    conn: &Connection,
    manifest: &[DailyManifestSource],
    local_date: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Option<Vec<DailySource>>, rusqlite::Error> {
    let mut sources = Vec::with_capacity(manifest.len());
    let mut seen = HashSet::new();
    for frozen in manifest {
        if frozen.activity_date != local_date || !seen.insert(frozen.event_id) {
            return Ok(None);
        }
        let source = conn
            .query_row(
                "SELECT id,content_hash,app_bundle_id
                   FROM event_log e WHERE id=?1 AND (ts>=?2 AND ts<?3 OR EXISTS (
                       SELECT 1 FROM dashboard_activity_credits c
                        WHERE c.event_id=e.id AND c.local_date=?4))",
                params![frozen.event_id, from_ms, to_ms, local_date],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((event_id, digest, app_bundle_id)) = source else {
            return Ok(None);
        };
        let Some(text) = crate::event_log::batch_event_text(conn, event_id, 2_000)? else {
            return Ok(None);
        };
        if digest != frozen.digest {
            return Ok(None);
        }
        sources.push(DailySource {
            event_id,
            text: text.content,
            digest,
            app_bundle_id,
        });
    }
    Ok((!sources.is_empty()).then_some(sources))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub summary: Option<String>,
    pub state: String,
    pub active_days: i64,
    pub last_success_at: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub updated_at: i64,
    pub attempt_sources_json: String,
}

pub fn active_days(conn: &Connection, through_date: &str) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM (
             SELECT local_date FROM dashboard_activity_credits
              WHERE local_date <= ?1 GROUP BY local_date
             UNION
             SELECT local_date FROM dashboard_edit_counts
              WHERE local_date <= ?1 GROUP BY local_date
         )",
        [through_date],
        |row| row.get(0),
    )
}

pub fn status(conn: &Connection, active_days: i64) -> Result<Status, rusqlite::Error> {
    conn.query_row(
        "SELECT summary,state,active_days,last_success_at,last_checked_at,updated_at,
                attempt_sources_json
           FROM about_profile_generation WHERE id=1",
        [],
        |row| {
            Ok(Status {
                summary: row.get(0)?,
                state: row.get(1)?,
                active_days: active_days.max(row.get(2)?),
                last_success_at: row.get(3)?,
                last_checked_at: row.get(4)?,
                updated_at: row.get(5)?,
                attempt_sources_json: row.get(6)?,
            })
        },
    )
    .optional()
    .map(|row| {
        row.unwrap_or(Status {
            summary: None,
            state: "locked".to_string(),
            active_days,
            last_success_at: None,
            last_checked_at: None,
            updated_at: 0,
            attempt_sources_json: "[]".to_string(),
        })
    })
}

/// About You reads only clean, model-written canonical day summaries. Their V22 provenance rows
/// retain the original events; raw capture text never enters this selector.
pub fn sources(conn: &Connection, now_ms: i64) -> Result<Vec<Source>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT id,strftime('%Y-%m-%d',period_start_ms / 1000,'unixepoch','localtime'),
                content,content_digest
           FROM period_summaries
          WHERE level='day' AND generation_mode='model' AND generated=1 AND is_dirty=0
            AND generation_model LIKE 'daily_activity_summary/%'
            AND date(period_start_ms / 1000,'unixepoch','localtime') >=
                date(?1 / 1000,'unixepoch','localtime','-14 days')
            AND period_end_ms<=?1
            AND EXISTS (SELECT 1 FROM period_summary_event_provenance p WHERE p.summary_id=period_summaries.id)
          ORDER BY period_start_ms DESC LIMIT 14",
    )?;
    let rows = statement
        .query_map([now_ms], |row| {
            Ok(Source {
                summary_id: row.get(0)?,
                row_id: row.get(0)?,
                local_date: row.get(1)?,
                source: "daily_summary".to_string(),
                text: row.get(2)?,
                digest: row.get(3)?,
            })
        })?
        .collect();
    rows
}

pub fn manifest(sources: &[Source]) -> Result<String, serde_json::Error> {
    if sources.iter().any(|source| source.source != "daily_summary") {
        return serde_json::to_string(&serde_json::json!([
            BOOTSTRAP_MANIFEST,
            sources
                .iter()
                .map(|source| (source.source.as_str(), source.row_id, source.digest.as_str()))
                .collect::<Vec<_>>(),
        ]));
    }
    serde_json::to_string(&serde_json::json!([
        "daily_period_v1",
        sources
            .iter()
            .map(|source| (source.summary_id, source.digest.as_str()))
            .collect::<Vec<_>>(),
    ]))
}

/// Rehydrate the exact source set frozen for an attempt. New capture does not change this set;
/// an edited, deleted, or no-longer-eligible source invalidates the whole attempt instead.
pub fn sources_for_manifest(
    conn: &Connection,
    manifest: &str,
) -> Result<Option<Vec<Source>>, rusqlite::Error> {
    if let Ok((BOOTSTRAP_MANIFEST, entries)) =
        serde_json::from_str::<(&str, Vec<(String, i64, String)>)>(manifest)
    {
        return bootstrap_for_manifest(conn, &entries);
    }
    let Ok((kind, entries)) = serde_json::from_str::<(String, Vec<(i64, String)>)>(manifest) else {
        return Ok(None);
    };
    if kind != "daily_period_v1" {
        return Ok(None);
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let mut selected = Vec::with_capacity(entries.len());
    let mut seen_ids = HashSet::new();
    for (event_id, digest) in entries {
        if !seen_ids.insert(event_id) {
            return Ok(None);
        }
        let source = conn
            .query_row(
                "SELECT id,strftime('%Y-%m-%d',period_start_ms / 1000,'unixepoch','localtime'),
                        content,content_digest
                   FROM period_summaries
                  WHERE id=?1 AND level='day' AND generation_mode='model' AND generated=1
                    AND is_dirty=0 AND generation_model LIKE 'daily_activity_summary/%'
                    AND EXISTS (SELECT 1 FROM period_summary_event_provenance p
                                 WHERE p.summary_id=period_summaries.id)",
                [event_id],
                |row| {
                    Ok(Source {
                        summary_id: row.get(0)?,
                        row_id: row.get(0)?,
                        local_date: row.get(1)?,
                        source: "daily_summary".to_string(),
                        text: row.get(2)?,
                        digest: row.get(3)?,
                    })
                },
            )
            .optional()?;
        let Some(source) = source else {
            return Ok(None);
        };
        if source.digest != digest || source.text.trim().chars().count() < MIN_SOURCE_CHARS {
            return Ok(None);
        }
        selected.push(source);
    }
    Ok(Some(selected))
}

/// A prior overview may guide an update while its recorded evidence still matches canonical data.
/// It is context, never evidence, so every new line still cites the current 14-day sample.
pub fn prior_overview(conn: &Connection) -> Result<Option<String>, rusqlite::Error> {
    let prior = conn
        .query_row(
            "SELECT summary,sources_json FROM about_profile_generation WHERE id=1",
            [],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((Some(summary), manifest)) = prior else {
        return Ok(None);
    };
    if sources_for_manifest(conn, &manifest)?.is_none() {
        return Ok(None);
    }
    let overview = serde_json::from_str::<serde_json::Value>(&summary)
        .ok()
        .and_then(|value| {
            value
                .get("about")
                .and_then(|lines| lines.as_array())
                .cloned()
        })
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| line.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty());
    Ok(overview)
}

pub fn stored_manifest(conn: &Connection) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT sources_json FROM about_profile_generation WHERE id=1",
        [],
        |row| row.get(0),
    )
    .optional()
}

pub fn note_locked(conn: &Connection, active_days: i64, now: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO about_profile_generation(id,state,active_days,created_at,updated_at)
         VALUES(1,'locked',?1,?2,?2)
         ON CONFLICT(id) DO UPDATE SET active_days=max(active_days,excluded.active_days),
             updated_at=excluded.updated_at",
        params![active_days, now],
    )?;
    Ok(())
}

pub fn note_checked(
    conn: &Connection,
    state: &str,
    active_days: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO about_profile_generation
             (id,state,active_days,last_checked_at,created_at,updated_at)
         VALUES(1,?1,?2,?3,?3,?3)
         ON CONFLICT(id) DO UPDATE SET state=excluded.state,
             active_days=max(active_days,excluded.active_days),
             last_checked_at=excluded.last_checked_at,attempt_sources_json='[]',
             attempt_context_digest='',provider_batch_id=NULL,expected_custom_id=NULL,
             submitted_at=NULL,updated_at=excluded.updated_at",
        params![state, active_days, now],
    )?;
    Ok(())
}

pub fn prepare(
    conn: &Connection,
    sources_json: &str,
    context_digest: &str,
    active_days: i64,
    now: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO about_profile_generation
             (id,state,active_days,attempt_sources_json,attempt_context_digest,generation,created_at,updated_at)
         VALUES(1,'pending',?1,?2,?3,1,?4,?4)
         ON CONFLICT(id) DO UPDATE SET state='pending',
             active_days=max(active_days,excluded.active_days),
             attempt_sources_json=excluded.attempt_sources_json,
             attempt_context_digest=excluded.attempt_context_digest,provider_batch_id=NULL,
             expected_custom_id=NULL,submitted_at=NULL,generation=generation+1,
             request_id=lower(hex(randomblob(16))),updated_at=excluded.updated_at",
        params![active_days, sources_json, context_digest, now],
    )?;
    Ok(())
}

/// The manifest kind of a frozen bootstrap source set.
const BOOTSTRAP_MANIFEST: &str = "bootstrap_v1";
/// Total characters a bootstrap request carries, and one source at most.
const BOOTSTRAP_CHARS: usize = 40_000;
const BOOTSTRAP_SOURCE_CHARS: usize = 600;
const BOOTSTRAP_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// Kinds in the order they fill the budget: what the user wrote, then what they did (day and
/// hour summaries), then a few people and project pages.
const BOOTSTRAP_KINDS: &[&str] = &["note", "style", "day", "hour", "person", "project"];
/// People and project pages change as review runs; only rows untouched for a day are taken, so
/// the frozen set survives until the Batch result returns.
const BOOTSTRAP_SETTLED_MS: i64 = 24 * 60 * 60 * 1000;

/// Meeting text never reaches a Batch lane: a period summary built from any excluded source
/// (directly, or through a child summary) is left out.
fn period_clean_sql() -> String {
    let excluded = crate::event_log::BATCH_EXCLUDED_SOURCES
        .iter()
        .map(|source| format!("'{}'", source.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "EXISTS (SELECT 1 FROM period_summary_event_provenance p WHERE p.summary_id=s.id)
         AND NOT EXISTS (SELECT 1 FROM period_summary_event_provenance p JOIN event_log e ON e.id=p.event_id
                          WHERE p.summary_id=s.id AND e.source IN ({excluded}))
         AND NOT EXISTS (SELECT 1 FROM period_summary_child_provenance c
                           JOIN period_summary_event_provenance p ON p.summary_id=c.child_summary_id
                           JOIN event_log e ON e.id=p.event_id
                          WHERE c.summary_id=s.id AND e.source IN ({excluded}))"
    )
}

/// Ids of one kind's candidate rows, newest first. A period summary is taken an hour after its
/// period ends: it is still rewritten for about twenty minutes, which would void the frozen set.
fn bootstrap_ids(conn: &Connection, kind: &str, now_ms: i64) -> Result<Vec<i64>, rusqlite::Error> {
    let since = now_ms.saturating_sub(BOOTSTRAP_WINDOW_MS);
    let settled = now_ms.saturating_sub(BOOTSTRAP_SETTLED_MS);
    let sql = match kind {
        "note" => "SELECT id FROM user_memory_notes WHERE forgotten=0 ORDER BY updated_at DESC LIMIT 20".to_string(),
        "style" => "SELECT id FROM rewrite_style_notes WHERE user_deleted=0 AND invalidated_at IS NULL
                    ORDER BY created_at DESC LIMIT 10".to_string(),
        "person" => format!(
            "SELECT id FROM people WHERE COALESCE(person_class,'')<>'self' AND confidence>=0.5
               AND length(trim(COALESCE(relationship_summary,'')))>0 AND updated_at<={settled}
             ORDER BY updated_at DESC LIMIT 8"
        ),
        "project" => format!(
            "SELECT id FROM projects WHERE confidence>=0.5 AND length(trim(COALESCE(summary,'')))>0
               AND updated_at<={settled} ORDER BY updated_at DESC LIMIT 8"
        ),
        "day" | "hour" => format!(
            "SELECT s.id FROM period_summaries s WHERE s.level='{kind}' AND s.is_dirty=0
               AND s.period_start_ms>=?1 AND s.period_end_ms<=?2 - 3600000 AND {}
             ORDER BY s.period_start_ms DESC LIMIT {}",
            period_clean_sql(),
            if kind == "day" { 14 } else { 96 }
        ),
        _ => return Ok(Vec::new()),
    };
    let mut statement = conn.prepare(&sql)?;
    let rows = if matches!(kind, "day" | "hour") {
        statement.query_map(params![since, now_ms], |row| row.get(0))?.collect()
    } else {
        statement.query_map([], |row| row.get(0))?.collect()
    };
    rows
}

/// One row's text (clipped) and local date, or `None` when it is gone or no longer eligible.
fn bootstrap_row(conn: &Connection, kind: &str, id: i64) -> Result<Option<(String, String)>, rusqlite::Error> {
    const DATE: &str = "strftime('%Y-%m-%d',{} / 1000,'unixepoch','localtime')";
    let date = |column: &str| DATE.replace("{}", column);
    let sql = match kind {
        "note" => format!("SELECT text,{} FROM user_memory_notes WHERE id=?1 AND forgotten=0", date("created_at")),
        "style" => format!(
            "SELECT 'writing style note: ' || text,{} FROM rewrite_style_notes
              WHERE id=?1 AND user_deleted=0 AND invalidated_at IS NULL",
            date("created_at")
        ),
        "person" => format!(
            "SELECT display_name || ': ' || relationship_summary,{} FROM people
              WHERE id=?1 AND COALESCE(person_class,'')<>'self' AND confidence>=0.5
                AND length(trim(COALESCE(relationship_summary,'')))>0",
            date("updated_at")
        ),
        "project" => format!(
            "SELECT name || ': ' || summary,{} FROM projects
              WHERE id=?1 AND confidence>=0.5 AND length(trim(COALESCE(summary,'')))>0",
            date("updated_at")
        ),
        "day" | "hour" => format!(
            "SELECT s.content,{} FROM period_summaries s WHERE s.id=?1 AND s.level='{kind}' AND s.is_dirty=0 AND {}",
            date("s.period_start_ms"),
            period_clean_sql()
        ),
        _ => return Ok(None),
    };
    let row = conn
        .query_row(&sql, [id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .optional()?;
    Ok(row.map(|(text, date)| {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (text.chars().take(BOOTSTRAP_SOURCE_CHARS).collect(), date)
    }))
}

/// Everything already on this Mac that can describe the user while no model-written day summary
/// exists yet: their notes and writing-style notes, the day and hour summaries of the last 14
/// days, then settled people and project pages. Bounded by [`BOOTSTRAP_CHARS`]; deduplicated.
pub fn bootstrap_sources(conn: &Connection, now_ms: i64) -> Result<Vec<Source>, rusqlite::Error> {
    let mut selected: Vec<Source> = Vec::new();
    let mut chars = 0;
    let mut seen = HashSet::new();
    for kind in BOOTSTRAP_KINDS {
        for id in bootstrap_ids(conn, kind, now_ms)? {
            let Some((text, local_date)) = bootstrap_row(conn, kind, id)? else {
                continue;
            };
            let count = text.chars().count();
            if count < MIN_SOURCE_CHARS || !seen.insert(text.to_lowercase()) {
                continue;
            }
            if chars + count > BOOTSTRAP_CHARS {
                return Ok(selected);
            }
            chars += count;
            selected.push(Source {
                summary_id: i64::try_from(selected.len()).unwrap_or(i64::MAX).saturating_add(1),
                row_id: id,
                digest: crate::event_log::content_hash(&text),
                text,
                local_date,
                source: (*kind).to_string(),
            });
        }
    }
    Ok(selected)
}

/// Rehydrate a frozen bootstrap set: every row still there with the same text, in order.
fn bootstrap_for_manifest(
    conn: &Connection,
    entries: &[(String, i64, String)],
) -> Result<Option<Vec<Source>>, rusqlite::Error> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut sources = Vec::with_capacity(entries.len());
    for (index, (kind, id, digest)) in entries.iter().enumerate() {
        let Some((text, local_date)) = bootstrap_row(conn, kind, *id)? else {
            return Ok(None);
        };
        if &crate::event_log::content_hash(&text) != digest {
            return Ok(None);
        }
        sources.push(Source {
            summary_id: i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1),
            row_id: *id,
            digest: digest.clone(),
            text,
            local_date,
            source: kind.clone(),
        });
    }
    Ok(Some(sources))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        conn: &Connection,
        ts: i64,
        source: &str,
        content: &str,
        hash: &str,
    ) -> Result<i64, rusqlite::Error> {
        conn.execute(
            "INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
             VALUES(?1,?2,'text',?3,?4,?1)",
            params![ts, source, content, hash],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn model_day(
        conn: &Connection,
        event_id: i64,
        start: i64,
        content: &str,
        digest: &str,
    ) -> Result<i64, rusqlite::Error> {
        conn.execute(
            "INSERT INTO period_summaries
               (level,period_start_ms,period_end_ms,title,content,generation_mode,generation_model,
                generated,content_digest,is_dirty,generated_at_ms,created_at_ms,updated_at_ms)
             VALUES('day',?1,?2,'day',?3,'model','daily_activity_summary/test',1,?4,0,?2,?2,?2)",
            params![start, start + 86_400_000, content, digest],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO period_summary_event_provenance(summary_id,event_id) VALUES(?1,?2)",
            params![id, event_id],
        )?;
        Ok(id)
    }

    fn extractive_hour(
        conn: &Connection,
        event_id: i64,
        start: i64,
        content: &str,
    ) -> Result<i64, rusqlite::Error> {
        conn.execute(
            "INSERT INTO period_summaries
               (level,period_start_ms,period_end_ms,title,content,generation_mode,generation_model,
                generated,content_digest,is_dirty,generated_at_ms,created_at_ms,updated_at_ms)
             VALUES('hour',?1,?2,'hour',?3,'extractive',NULL,0,?3,0,?2,?2,?2)",
            params![start, start + 3_600_000, content],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO period_summary_event_provenance(summary_id,event_id) VALUES(?1,?2)",
            params![id, event_id],
        )?;
        Ok(id)
    }

    #[test]
    fn bootstrap_reads_notes_and_hours_but_never_meeting_text() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let now = 1_790_000_000_000;
        let screen = event(&conn, now - 7_200_000, "capture", "Harbor relay review in the editor.", "screen")?;
        let meeting = event(&conn, now - 10_800_000, "meeting", "Spoken words from a call.", "call")?;
        extractive_hour(&conn, screen, now - 7_200_000, "Harbor relay review in the editor.")?;
        extractive_hour(&conn, meeting, now - 10_800_000, "Spoken words from a call.")?;
        extractive_hour(&conn, screen, now - 3_600_000, "This hour is still being rewritten.")?;
        conn.execute(
            "INSERT INTO user_memory_notes(text,revision,forgotten,created_at,updated_at)
             VALUES('I lead the Harbor relay project.',1,0,?1,?1)",
            [now - 1_000],
        )?;
        assert!(sources(&conn, now)?.is_empty(), "no model-written day");

        let selected = bootstrap_sources(&conn, now)?;
        let kinds = selected.iter().map(|s| (s.source.as_str(), s.summary_id)).collect::<Vec<_>>();
        assert_eq!(kinds, vec![("note", 1), ("hour", 2)]);
        assert!(!selected.iter().any(|s| s.text.contains("Spoken")), "meeting text stays local");
        assert!(!selected.iter().any(|s| s.text.contains("still being")), "an unsettled hour waits");

        let frozen = manifest(&selected)?;
        assert_eq!(sources_for_manifest(&conn, &frozen)?, Some(selected));
        conn.execute("UPDATE user_memory_notes SET text='Something else entirely.'", [])?;
        assert!(sources_for_manifest(&conn, &frozen)?.is_none(), "an edited note voids the attempt");
        Ok(())
    }

    #[test]
    fn active_days_unions_capture_and_edits() -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        conn.execute(
            "INSERT INTO event_log(ts,source,kind,content,content_hash,last_seen_at)
             VALUES(1,'capture','text','fixture','fixture',1)",
            [],
        )?;
        crate::activity::record_capture_credits(
            &conn,
            conn.last_insert_rowid(),
            1,
            &[("2026-09-01".to_string(), 100)],
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-01",
            crate::activity::EditKind::InlineEdit,
            1,
        )?;
        crate::activity::record_edit(
            &conn,
            "2026-09-02",
            crate::activity::EditKind::ScribeRewrite,
            2,
        )?;
        assert_eq!(active_days(&conn, "2026-09-02")?, 2);
        Ok(())
    }

    #[test]
    fn about_sources_are_tagged_model_days_with_event_lineage(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let day = 86_400_000;
        let first = event(
            &conn,
            1_790_000_000_000,
            "capture",
            "Harbor launch checklist needs the migration review.",
            "harbor-one",
        )?;
        let second = event(
            &conn,
            1_790_000_000_000 + day,
            "capture",
            "Compiler research compares the parser implementation.",
            "compiler-two",
        )?;
        let first_day = model_day(
            &conn,
            first,
            1_790_000_000_000,
            "Worked on Harbor.",
            "day-one",
        )?;
        let second_day = model_day(
            &conn,
            second,
            1_790_000_000_000 + day,
            "Compared compiler parsers.",
            "day-two",
        )?;

        let selected = sources(&conn, 1_790_000_000_000 + 2 * day)?;
        let ids = selected
            .iter()
            .map(|source| source.summary_id)
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from([first_day, second_day]));
        let frozen =
            sources_for_manifest(&conn, &manifest(&selected)?)?.ok_or("tagged manifest")?;
        assert_eq!(frozen.len(), 2);
        assert!(sources_for_manifest(&conn, "[[1,\"day-one\"]]")?.is_none());
        Ok(())
    }

    #[test]
    fn prior_overview_uses_valid_published_evidence_outside_current_sample(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let source_id = event(
            &conn,
            1_790_000_000_000,
            "capture",
            "Harbor launch checklist needs the migration review.",
            "harbor-one",
        )?;
        let day_id = model_day(
            &conn,
            source_id,
            1_790_000_000_000,
            "Worked on Harbor.",
            "day-one",
        )?;
        let manifest = serde_json::to_string(&("daily_period_v1", vec![(day_id, "day-one")]))?;
        conn.execute(
            "INSERT INTO about_profile_generation
                 (id,summary,sources_json,state,active_days,created_at,updated_at)
             VALUES(1,?2,?1,'complete',5,1,1)",
            params![
                manifest,
                serde_json::json!({"about": [{
                    "text": "Works on Harbor.", "basis": "fact",
                    "evidence": [{"source_id": day_id, "quote": "Worked on Harbor."}]
                }]})
                .to_string()
            ],
        )?;

        assert_eq!(prior_overview(&conn)?.as_deref(), Some("Works on Harbor."));
        conn.execute(
            "UPDATE period_summaries SET is_dirty=1,updated_at_ms=2 WHERE id=?1",
            [day_id],
        )?;
        assert!(prior_overview(&conn)?.is_none());
        let cleared: Option<String> = conn.query_row(
            "SELECT summary FROM about_profile_generation WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        assert!(cleared.is_none());
        Ok(())
    }

    #[test]
    fn frozen_manifest_ignores_new_capture_and_rejects_edited_source(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let conn = crate::open_in_memory()?;
        let first = event(
            &conn,
            1_790_000_000_000,
            "capture",
            "Harbor launch checklist needs the migration review.",
            "harbor-one",
        )?;
        let day_id = model_day(
            &conn,
            first,
            1_790_000_000_000,
            "Worked on Harbor.",
            "day-one",
        )?;
        let manifest = serde_json::to_string(&("daily_period_v1", vec![(day_id, "day-one")]))?;
        let frozen = sources_for_manifest(&conn, &manifest)?
            .ok_or("frozen manifest should remain available")?;
        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].summary_id, day_id);

        conn.execute(
            "UPDATE event_log SET content='Harbor source changed entirely.',content_hash='changed' WHERE id=?1",
            [first],
        )?;
        assert!(sources_for_manifest(&conn, &manifest)?.is_none());
        Ok(())
    }
}
