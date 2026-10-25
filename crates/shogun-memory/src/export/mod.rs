//! Export format v1 (ShogunAgent E7, `docs/api/export-v1.md`, `docs/api/export-v1.schema.json`).
//!
//! JSON Lines. The first line is the **manifest**; every other line is one record:
//!
//! - `entity` — a person or a project: name, aliases, summary, status, confidence and band, the
//!   validity interval (`valid_from` = created, `valid_to` = when it stopped being current),
//!   `namespace`, the last writer (`actor`) and its provenance pointers;
//! - `fact` — a commitment or an open loop, with the same fields plus its subtype, status, due date
//!   and the entity ids it names;
//! - `tombstone` — a row deleted from the store (V95): kind, id, when and why. Never the content.
//!
//! A provenance pointer is `{event_id, quote}`: the event in the exporting store and the exact
//! span of it that proves the row (`null` when none was retained). Event bodies are not exported:
//! the export is the world model, not the log.
//!
//! **Differential export** (E7-T3): with `since`, only rows changed after it (`updated_at > since`)
//! and tombstones written after it are emitted. The manifest's `cursor` is the export's own time;
//! [`advance_cursor`] stores it in `principal_meta.export_cursor` (V96) so the next differential
//! export starts there.
//!
//! **Import** ([`import_v1`], E7-T2) restores the rows of an export into a store under the
//! importer's write context. Each provenance pointer becomes one event of its own (source
//! `import`, the quote as its text, or the row's text when there is no quote) so the row's
//! provenance and its quote hold in the new store. Imported confidence is capped at
//! [`crate::memory_import::CONFIDENCE_MAX`]: an imported claim is not a fact this store observed.
//! Tombstones name ids of the exporting store and are counted, not applied.
//!
//! **Export v2** ([`v2`], E27) is this format plus global ids, record times, relations,
//! supersession links, source hashes and a capability manifest; [`convert`] maps v1 ↔ v2 without
//! a store. Both imports share one restore.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::actor::WriteContext;
use crate::event_log::{self, NewEvent};
use crate::state::{self, Provenance, StateTable};
use crate::MemoryError;

/// The `format` of the manifest line.
pub const FORMAT: &str = "shogunagent-export/v1";
/// The `principal_meta` key of the differential cursor (V96).
pub const CURSOR_KEY: &str = "export_cursor";
/// The source and kind of the events an import creates for provenance pointers.
pub const IMPORT_SOURCE: &str = "import";
pub const IMPORT_KIND: &str = "export_v1_evidence";
/// Largest export an import accepts (bytes).
pub const MAX_IMPORT_BYTES: usize = 16 * 1024 * 1024;

/// One export: the JSON Lines text and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportV1 {
    pub lines: Vec<Value>,
    /// The cursor the manifest names (the export's time).
    pub cursor: i64,
    pub since: Option<i64>,
}

impl ExportV1 {
    /// The JSON Lines text: one compact JSON object per line, newline-terminated.
    pub fn jsonl(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.to_string());
            out.push('\n');
        }
        out
    }

    /// Records after the manifest.
    pub fn records(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }
}

fn band(confidence: f64) -> &'static str {
    if confidence >= 0.8 {
        "high"
    } else if confidence >= 0.5 {
        "medium"
    } else {
        "low"
    }
}

fn json_list(raw: Option<String>) -> Vec<String> {
    raw.and_then(|r| serde_json::from_str::<Vec<String>>(&r).ok()).unwrap_or_default()
}

/// Every provenance pointer of a table, by state id, oldest link first.
fn provenance(conn: &Connection, table: StateTable) -> Result<HashMap<i64, Vec<Value>>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT state_id, event_id, quote FROM state_provenance WHERE state_table = ?1 ORDER BY state_id, id")?;
    let mut map: HashMap<i64, Vec<Value>> = HashMap::new();
    let rows = stmt.query_map([table.as_str()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))?;
    for row in rows {
        let (state_id, event_id, quote) = row?;
        map.entry(state_id).or_default().push(json!({ "event_id": event_id, "quote": quote }));
    }
    Ok(map)
}

/// The rows changed after `since` (all rows without it).
fn changed(since: Option<i64>) -> String {
    match since {
        Some(since) => format!("updated_at > {since}"),
        None => "1 = 1".to_string(),
    }
}

/// Build an export at `now`: every row of the four state tables (every namespace, every band —
/// the export is the user's), or only those changed after `since`, then the tombstones.
pub fn export_v1(conn: &Connection, since: Option<i64>, now: i64) -> Result<ExportV1, rusqlite::Error> {
    let mut records = Vec::new();
    let filter = changed(since);

    let mut prov = provenance(conn, StateTable::People)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, display_name, aliases, relationship_summary, confidence, created_at, updated_at, actor_last, namespace
         FROM people WHERE {filter} ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |r| {
        let confidence: f64 = r.get(4)?;
        Ok((r.get::<_, i64>(0)?, json!({
            "type": "entity",
            "kind": "person",
            "name": r.get::<_, String>(1)?,
            "aliases": json_list(r.get(2)?),
            "summary": r.get::<_, Option<String>>(3)?,
            "status": Value::Null,
            "confidence": confidence,
            "band": band(confidence),
            "valid_from": r.get::<_, i64>(5)?,
            "valid_to": Value::Null,
            "updated_at": r.get::<_, i64>(6)?,
            "actor": r.get::<_, Option<String>>(7)?,
            "namespace": r.get::<_, String>(8)?,
        })))
    })?;
    for row in rows {
        let (id, mut record) = row?;
        record["id"] = json!(id);
        record["provenance"] = Value::Array(prov.remove(&id).unwrap_or_default());
        records.push(record);
    }

    let mut prov = provenance(conn, StateTable::Projects)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, name, aliases, summary, status, confidence, created_at, updated_at, actor_last, namespace
         FROM projects WHERE {filter} ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |r| {
        let confidence: f64 = r.get(5)?;
        let status: String = r.get(4)?;
        let updated: i64 = r.get(7)?;
        Ok((r.get::<_, i64>(0)?, json!({
            "type": "entity",
            "kind": "project",
            "name": r.get::<_, String>(1)?,
            "aliases": json_list(r.get(2)?),
            "summary": r.get::<_, Option<String>>(3)?,
            "valid_to": if status == "done" { json!(updated) } else { Value::Null },
            "status": status,
            "confidence": confidence,
            "band": band(confidence),
            "valid_from": r.get::<_, i64>(6)?,
            "updated_at": updated,
            "actor": r.get::<_, Option<String>>(8)?,
            "namespace": r.get::<_, String>(9)?,
        })))
    })?;
    for row in rows {
        let (id, mut record) = row?;
        record["id"] = json!(id);
        record["provenance"] = Value::Array(prov.remove(&id).unwrap_or_default());
        records.push(record);
    }

    let mut prov = provenance(conn, StateTable::Commitments)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, direction, description, due_at, status, counterparty_id, project_id, confidence,
                created_at, updated_at, actor_last, namespace
         FROM commitments WHERE {filter} ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |r| {
        let confidence: f64 = r.get(7)?;
        let status: String = r.get(4)?;
        let updated: i64 = r.get(9)?;
        Ok((r.get::<_, i64>(0)?, json!({
            "type": "fact",
            "kind": "commitment",
            "subtype": r.get::<_, String>(1)?,
            "text": r.get::<_, String>(2)?,
            "due_at": r.get::<_, Option<i64>>(3)?,
            "valid_to": if matches!(status.as_str(), "done" | "cancelled") { json!(updated) } else { Value::Null },
            "status": status,
            "counterparty_id": r.get::<_, Option<i64>>(5)?,
            "project_id": r.get::<_, Option<i64>>(6)?,
            "confidence": confidence,
            "band": band(confidence),
            "valid_from": r.get::<_, i64>(8)?,
            "updated_at": updated,
            "actor": r.get::<_, Option<String>>(10)?,
            "namespace": r.get::<_, String>(11)?,
        })))
    })?;
    for row in rows {
        let (id, mut record) = row?;
        record["id"] = json!(id);
        record["provenance"] = Value::Array(prov.remove(&id).unwrap_or_default());
        records.push(record);
    }

    let mut prov = provenance(conn, StateTable::OpenLoops)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, kind, description, status, counterparty_id, project_id, confidence,
                opened_at, updated_at, actor_last, namespace
         FROM open_loops WHERE {filter} ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |r| {
        let confidence: f64 = r.get(6)?;
        let status: String = r.get(3)?;
        let updated: i64 = r.get(8)?;
        Ok((r.get::<_, i64>(0)?, json!({
            "type": "fact",
            "kind": "open_loop",
            "subtype": r.get::<_, String>(1)?,
            "text": r.get::<_, String>(2)?,
            "due_at": Value::Null,
            "valid_to": if status == "closed" { json!(updated) } else { Value::Null },
            "status": status,
            "counterparty_id": r.get::<_, Option<i64>>(4)?,
            "project_id": r.get::<_, Option<i64>>(5)?,
            "confidence": confidence,
            "band": band(confidence),
            "valid_from": r.get::<_, i64>(7)?,
            "updated_at": updated,
            "actor": r.get::<_, Option<String>>(9)?,
            "namespace": r.get::<_, String>(10)?,
        })))
    })?;
    for row in rows {
        let (id, mut record) = row?;
        record["id"] = json!(id);
        record["provenance"] = Value::Array(prov.remove(&id).unwrap_or_default());
        records.push(record);
    }

    let tomb_filter = match since {
        Some(since) => format!("deleted_at > {since}"),
        None => "1 = 1".to_string(),
    };
    let mut stmt = conn.prepare(&format!("SELECT kind, id, deleted_at, reason FROM export_tombstones WHERE {tomb_filter} ORDER BY deleted_at, kind, id"))?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "type": "tombstone",
            "kind": r.get::<_, String>(0)?,
            "id": r.get::<_, i64>(1)?,
            "deleted_at": r.get::<_, i64>(2)?,
            "reason": r.get::<_, String>(3)?,
        }))
    })?;
    for row in rows {
        records.push(row?);
    }

    let count = |t: &str| records.iter().filter(|r| r["type"] == t).count();
    // The cursor is the export's time, or the newest change it carries when that is later (a
    // provenance edit bumps `updated_at` by a millisecond), so the next differential export never
    // re-sends what this one already carried.
    let newest = records
        .iter()
        .filter_map(|r| r.get("updated_at").or_else(|| r.get("deleted_at")).and_then(Value::as_i64))
        .max()
        .unwrap_or(now);
    let cursor = now.max(newest).max(since.unwrap_or(i64::MIN));
    let manifest = json!({
        "type": "manifest",
        "format": FORMAT,
        "schema_version": crate::LATEST_SCHEMA_VERSION,
        "generated_at": now,
        "since_cursor": since,
        "cursor": cursor,
        "counts": { "entities": count("entity"), "facts": count("fact"), "tombstones": count("tombstone") },
    });
    let mut lines = Vec::with_capacity(records.len() + 1);
    lines.push(manifest);
    lines.extend(records);
    Ok(ExportV1 { lines, cursor, since })
}

/// The stored differential cursor, if a differential export has advanced it.
pub fn cursor(conn: &Connection) -> Result<Option<i64>, rusqlite::Error> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM principal_meta WHERE key = ?1", [CURSOR_KEY], |r| r.get(0))
        .optional()?;
    Ok(raw.and_then(|v| v.parse().ok()))
}

/// Store `cursor` as the next differential export's start. Never moves it backwards.
pub fn advance_cursor(conn: &Connection, cursor: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO principal_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value
         WHERE CAST(principal_meta.value AS INTEGER) < CAST(excluded.value AS INTEGER)",
        params![CURSOR_KEY, cursor.to_string()],
    )?;
    Ok(())
}

/// Whether `text` is an export v1 (its first line is the v1 manifest).
pub fn is_export_v1(text: &str) -> bool {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .is_some_and(|first| first["type"] == "manifest" && first["format"] == FORMAT)
}

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportReport {
    pub entities: usize,
    pub facts: usize,
    /// Evidence events created for provenance pointers.
    pub events: usize,
    /// Tombstones in the export: counted, not applied (their ids are the exporting store's).
    pub tombstones: usize,
    /// Records skipped because they were malformed or of an unknown kind.
    pub skipped: usize,
    /// Relations restored (export v2 only).
    pub relations: usize,
    /// Supersession links restored (export v2 only).
    pub superseded: usize,
}

/// Why an import was refused as a whole. Never carries content.
#[derive(Debug)]
pub enum ImportError {
    NotExportV1,
    NotExportV2,
    TooLarge,
    Db(MemoryError),
}

impl From<rusqlite::Error> for ImportError {
    fn from(error: rusqlite::Error) -> Self {
        ImportError::Db(MemoryError::Sqlite(error))
    }
}

impl From<MemoryError> for ImportError {
    fn from(error: MemoryError) -> Self {
        ImportError::Db(error)
    }
}

fn str_field<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

/// One evidence event per pointer: the quote as its text (the row's text when there is none).
/// With `per_pointer_ts` (export v2), an evidence event takes the pointer's `observed_at`, so the
/// source event's time survives the move; otherwise every one takes `ts`.
pub(crate) fn evidence_events(
    conn: &Connection,
    record: &Value,
    fallback: &str,
    ts: i64,
    per_pointer_ts: bool,
    ctx: &WriteContext,
) -> Result<Vec<(i64, Option<String>)>, rusqlite::Error> {
    let mut out = Vec::new();
    let pointers = record.get("provenance").and_then(Value::as_array).cloned().unwrap_or_default();
    for pointer in pointers.iter().take(50) {
        let quote = pointer.get("quote").and_then(Value::as_str).map(str::to_string);
        let text = quote.clone().unwrap_or_else(|| fallback.to_string());
        let hash = event_log::content_hash(&text);
        let pointer_ts = if per_pointer_ts { pointer.get("observed_at").and_then(Value::as_i64).unwrap_or(ts) } else { ts };
        let (id, _) = event_log::insert_or_touch_as(
            conn,
            &NewEvent {
                ts: pointer_ts,
                source: IMPORT_SOURCE,
                kind: IMPORT_KIND,
                app_bundle_id: None,
                window_title: None,
                content: &text,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
            ctx,
        )?;
        out.push((id, quote));
    }
    if out.is_empty() {
        let hash = event_log::content_hash(fallback);
        let (id, _) = event_log::insert_or_touch_as(
            conn,
            &NewEvent {
                ts,
                source: IMPORT_SOURCE,
                kind: IMPORT_KIND,
                app_bundle_id: None,
                window_title: None,
                content: fallback,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
            ctx,
        )?;
        out.push((id, None));
    }
    Ok(out)
}

/// Import an export v1 into `conn` as `ctx` at `now`, in one transaction. See the module docs.
pub fn import_v1(conn: &mut Connection, text: &str, ctx: &WriteContext, now: i64) -> Result<ImportReport, ImportError> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err(ImportError::TooLarge);
    }
    if !is_export_v1(text) {
        return Err(ImportError::NotExportV1);
    }
    let own_transaction = conn.is_autocommit();
    if own_transaction {
        conn.execute_batch("BEGIN IMMEDIATE")?;
    }
    let result = import_records(conn, text, ctx, now, Mode::V1).map(|(report, _)| report);
    if own_transaction {
        match &result {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
    result
}

/// Which export an import reads. v2 records carry every v1 field, so one restore serves both; v2
/// additionally keeps `recorded_at`, each pointer's `observed_at`, relations and supersession.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    V1,
    V2,
}

/// Exported ids → new ids, per state table.
#[derive(Debug, Default)]
pub(crate) struct IdMaps {
    pub people: HashMap<i64, i64>,
    pub projects: HashMap<i64, i64>,
    pub commitments: HashMap<i64, i64>,
    pub open_loops: HashMap<i64, i64>,
}

impl IdMaps {
    pub(crate) fn get(&self, table: StateTable, old: i64) -> Option<i64> {
        match table {
            StateTable::People => self.people.get(&old),
            StateTable::Projects => self.projects.get(&old),
            StateTable::Commitments => self.commitments.get(&old),
            StateTable::OpenLoops => self.open_loops.get(&old),
        }
        .copied()
    }
}

pub(crate) fn import_records(
    conn: &mut Connection,
    text: &str,
    ctx: &WriteContext,
    now: i64,
    mode: Mode,
) -> Result<(ImportReport, IdMaps), ImportError> {
    let cap = crate::memory_import::CONFIDENCE_MAX;
    let mut report = ImportReport::default();
    let v2 = mode == Mode::V2;
    // Exported ids → new ids, so facts keep pointing at the entities they name.
    let mut maps = IdMaps::default();
    let records: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .skip(1)
        .map(|line| serde_json::from_str::<Value>(line).unwrap_or(Value::Null))
        .collect();
    // Entities first, so a fact's counterparty / project resolves whatever the line order.
    for pass in ["entity", "fact", "tombstone"] {
        for record in records.iter().filter(|r| r["type"] == pass) {
            let confidence = record.get("confidence").and_then(Value::as_f64).unwrap_or(cap).clamp(0.0, cap);
            let valid_from = record.get("valid_from").and_then(Value::as_i64).unwrap_or(now);
            let updated = record.get("updated_at").and_then(Value::as_i64).unwrap_or(now);
            // v1 has no record time: the row is recorded when it became current. v2 carries it.
            let recorded = if v2 { record.get("recorded_at").and_then(Value::as_i64).unwrap_or(valid_from) } else { valid_from };
            let old_id = record.get("id").and_then(Value::as_i64).unwrap_or(0);
            match (pass, str_field(record, "kind")) {
                ("entity", Some(kind @ ("person" | "project"))) => {
                    let Some(name) = str_field(record, "name").filter(|n| !n.trim().is_empty()) else {
                        report.skipped += 1;
                        continue;
                    };
                    let evidence = evidence_events(conn, record, name, valid_from, v2, ctx)?;
                    report.events += evidence.len();
                    let aliases = record.get("aliases").filter(|a| a.as_array().is_some_and(|a| !a.is_empty())).map(Value::to_string);
                    let summary = str_field(record, "summary");
                    let links: Vec<Provenance> = evidence.iter().map(|(id, _)| Provenance::new(*id)).collect();
                    let (table, new_id) = if kind == "person" {
                        let id = state::insert_with_provenance_as(conn, StateTable::People, &links, ctx, |c| {
                            c.execute(
                                "INSERT INTO people (display_name, aliases, relationship_summary, confidence, base_confidence,
                                                     created_at, updated_at, last_evidence_at)
                                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?6)",
                                params![name, aliases, summary, confidence, recorded, updated.max(recorded)],
                            )?;
                            Ok(c.last_insert_rowid())
                        })?;
                        maps.people.insert(old_id, id);
                        (StateTable::People, id)
                    } else {
                        let status = str_field(record, "status").filter(|s| matches!(*s, "active" | "waiting" | "paused" | "done")).unwrap_or("active");
                        let id = state::insert_with_provenance_as(conn, StateTable::Projects, &links, ctx, |c| {
                            c.execute(
                                "INSERT INTO projects (name, status, summary, aliases, confidence, created_at, updated_at, last_evidence_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                                params![name, status, summary, aliases, confidence, recorded, updated.max(recorded)],
                            )?;
                            Ok(c.last_insert_rowid())
                        })?;
                        maps.projects.insert(old_id, id);
                        (StateTable::Projects, id)
                    };
                    for (event_id, quote) in &evidence {
                        if let Some(quote) = quote {
                            state::retain_quote(conn, table, new_id, *event_id, quote)?;
                        }
                    }
                    // Retaining a quote touches `updated_at`; keep the exported change time, which a
                    // terminal row's `valid_to` is read from.
                    conn.execute(
                        &format!("UPDATE {} SET updated_at = ?1 WHERE id = ?2", table.as_str()),
                        params![updated.max(recorded).max(valid_from), new_id],
                    )?;
                    report.entities += 1;
                }
                ("fact", Some(kind @ ("commitment" | "open_loop"))) => {
                    let Some(text) = str_field(record, "text").filter(|t| !t.trim().is_empty()) else {
                        report.skipped += 1;
                        continue;
                    };
                    let evidence = evidence_events(conn, record, text, valid_from, v2, ctx)?;
                    report.events += evidence.len();
                    let links: Vec<Provenance> = evidence.iter().map(|(id, _)| Provenance::new(*id)).collect();
                    let counterparty = record.get("counterparty_id").and_then(Value::as_i64).and_then(|id| maps.people.get(&id).copied());
                    let project = record.get("project_id").and_then(Value::as_i64).and_then(|id| maps.projects.get(&id).copied());
                    let (table, new_id) = if kind == "commitment" {
                        let direction = str_field(record, "subtype").filter(|s| matches!(*s, "mine" | "theirs")).unwrap_or("mine");
                        let status = str_field(record, "status").filter(|s| matches!(*s, "open" | "done" | "overdue" | "cancelled")).unwrap_or("open");
                        let due = record.get("due_at").and_then(Value::as_i64);
                        let id = state::insert_with_provenance_as(conn, StateTable::Commitments, &links, ctx, |c| {
                            c.execute(
                                "INSERT INTO commitments (direction, counterparty_id, description, due_at, status, project_id,
                                                          confidence, base_confidence, created_at, updated_at, last_evidence_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?9)",
                                params![direction, counterparty, text, due, status, project, confidence, recorded, updated.max(recorded)],
                            )?;
                            Ok(c.last_insert_rowid())
                        })?;
                        maps.commitments.insert(old_id, id);
                        (StateTable::Commitments, id)
                    } else {
                        let loop_kind = str_field(record, "subtype")
                            .filter(|s| matches!(*s, "reply_needed" | "waiting_on_them" | "review_pending" | "decision_pending" | "follow_up" | "other"))
                            .unwrap_or("other");
                        let status = str_field(record, "status").filter(|s| matches!(*s, "open" | "closed")).unwrap_or("open");
                        let id = state::insert_with_provenance_as(conn, StateTable::OpenLoops, &links, ctx, |c| {
                            c.execute(
                                "INSERT INTO open_loops (kind, description, counterparty_id, project_id, opened_at, staleness_days,
                                                         status, confidence, base_confidence, created_at, updated_at, last_evidence_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?7, ?8, ?9, ?9)",
                                params![loop_kind, text, counterparty, project, valid_from, status, confidence, recorded, updated.max(recorded).max(valid_from)],
                            )?;
                            Ok(c.last_insert_rowid())
                        })?;
                        maps.open_loops.insert(old_id, id);
                        (StateTable::OpenLoops, id)
                    };
                    for (event_id, quote) in &evidence {
                        if let Some(quote) = quote {
                            state::retain_quote(conn, table, new_id, *event_id, quote)?;
                        }
                    }
                    // Retaining a quote touches `updated_at`; keep the exported change time, which a
                    // terminal row's `valid_to` is read from.
                    conn.execute(
                        &format!("UPDATE {} SET updated_at = ?1 WHERE id = ?2", table.as_str()),
                        params![updated.max(recorded).max(valid_from), new_id],
                    )?;
                    report.facts += 1;
                }
                ("tombstone", _) => report.tombstones += 1,
                _ => report.skipped += 1,
            }
        }
    }
    let known: &[&str] = if v2 { &["entity", "fact", "tombstone", "relation"] } else { &["entity", "fact", "tombstone"] };
    report.skipped += records.iter().filter(|r| !r["type"].as_str().is_some_and(|t| known.contains(&t))).count();
    if v2 {
        v2::restore_links(conn, &records, &maps, ctx, now, &mut report)?;
    }
    Ok((report, maps))
}

pub mod convert;
pub mod v2;

pub use v2::{export_v2, import_v2, is_export_v2, namespace_id, ExportV2};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_v2;

#[cfg(test)]
mod prop_tests;
