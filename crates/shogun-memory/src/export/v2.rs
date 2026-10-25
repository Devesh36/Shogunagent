//! Export format v2 (ShogunAgent E27, `docs/api/export-v2.md`, `docs/api/export-v2.schema.json`).
//!
//! v2 is v1 plus what an interchange format needs. Every v1 field keeps its name, type and
//! meaning, so a v1 reader that ignores unknown fields reads a v2 entity or fact unchanged. Added:
//!
//! - **global ids**: every record carries `uri`, `urn:shogunagent:<ns>:<kind>:<id>`, where `<ns>`
//!   is the store's principal-scoped namespace ([`namespace_id`], `principal_meta`
//!   `export_namespace`). Ids are stable for the life of the store; references between records
//!   (`counterparty`, `project`, `subject`, `object`, `superseded_by`) are URIs;
//! - **bi-temporal fields**: `valid_from` / `valid_to` (valid time, as in v1) and `recorded_at`
//!   (transaction time: when this store recorded the row);
//! - **relations**: the typed `(subject, predicate, object)` rows of V97 with their own validity
//!   interval, confidence and evidence;
//! - **supersession**: `superseded_by` names the row a duplicate was folded into (V83
//!   `merged_into`); the folded row is kept, never deleted;
//! - **verifiable provenance**: each pointer adds `event_uri`, `observed_at` (the event's time),
//!   `source` and `content_hash` — `sha256:<hex>` of the event body — so a receiver that holds the
//!   same source (the email, the transcript) can check the quote against it without the export
//!   carrying the body;
//! - **a capability manifest** on the first line, so a receiver knows what the producer models.
//!
//! The only write an export v2 makes is the namespace id, once per store (like the differential
//! cursor, it holds no content).

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{IdMaps, ImportError, ImportReport, Mode};
use crate::actor::WriteContext;
use crate::state::StateTable;

/// The `format` of the v2 manifest line.
pub const FORMAT: &str = "shogunagent-export/v2";
/// The `principal_meta` key holding the store's namespace id.
pub const NAMESPACE_KEY: &str = "export_namespace";
/// The URN prefix every global id starts with.
pub const URN_PREFIX: &str = "urn:shogunagent:";

/// One v2 export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportV2 {
    pub lines: Vec<Value>,
    pub cursor: i64,
    pub since: Option<i64>,
    /// The namespace id the URIs use.
    pub namespace: String,
}

impl ExportV2 {
    pub fn jsonl(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.to_string());
            out.push('\n');
        }
        out
    }

    pub fn records(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }
}

/// The singular URI kind of a state table (also the v1 `kind` of entities and facts).
pub fn uri_kind(table: StateTable) -> &'static str {
    match table {
        StateTable::People => "person",
        StateTable::Projects => "project",
        StateTable::Commitments => "commitment",
        StateTable::OpenLoops => "open_loop",
    }
}

fn table_of_uri_kind(kind: &str) -> Option<StateTable> {
    match kind {
        "person" => Some(StateTable::People),
        "project" => Some(StateTable::Projects),
        "commitment" => Some(StateTable::Commitments),
        "open_loop" => Some(StateTable::OpenLoops),
        _ => None,
    }
}

/// `urn:shogunagent:<ns>:<kind>:<id>`.
pub fn uri(namespace: &str, kind: &str, id: i64) -> String {
    format!("{URN_PREFIX}{namespace}:{kind}:{id}")
}

/// `(namespace, kind, id)` of a v2 URI, or `None` when it is not one.
pub fn parse_uri(raw: &str) -> Option<(&str, &str, i64)> {
    let rest = raw.strip_prefix(URN_PREFIX)?;
    let mut parts = rest.rsplitn(3, ':');
    let id = parts.next()?.parse().ok()?;
    let kind = parts.next()?;
    let namespace = parts.next()?;
    (!namespace.is_empty() && !kind.is_empty()).then_some((namespace, kind, id))
}

/// Whether `ns` is a valid namespace id: 1–64 of `[0-9a-z-]`.
pub fn valid_namespace(ns: &str) -> bool {
    !ns.is_empty() && ns.len() <= 64 && ns.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-')
}

/// 128 bits from the process's hash keys and the clock: unique per store without a randomness
/// dependency. Not a secret; it only has to not collide.
fn fresh_namespace(now: i64) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut halves = [0u64; 2];
    for (i, half) in halves.iter_mut().enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_i64(now);
        h.write_usize(i);
        h.write_usize(std::process::id() as usize);
        *half = h.finish();
    }
    format!("{:016x}{:016x}", halves[0], halves[1])
}

/// The store's namespace id, created on first use and stored in `principal_meta`.
pub fn namespace_id(conn: &Connection, now: i64) -> Result<String, rusqlite::Error> {
    let read = |conn: &Connection| -> Result<Option<String>, rusqlite::Error> {
        conn.query_row("SELECT value FROM principal_meta WHERE key = ?1", [NAMESPACE_KEY], |r| r.get(0)).optional()
    };
    if let Some(ns) = read(conn)?.filter(|ns| valid_namespace(ns)) {
        return Ok(ns);
    }
    conn.execute(
        "INSERT INTO principal_meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![NAMESPACE_KEY, fresh_namespace(now)],
    )?;
    read(conn)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `sha256:<hex>` of a text, as v2 provenance pointers carry it.
pub fn content_hash(text: &str) -> String {
    sha256_hex(text)
}

/// `(recorded_at, merged_into)` of every row of a table.
fn row_times(conn: &Connection, table: StateTable) -> Result<HashMap<i64, (i64, Option<i64>)>, rusqlite::Error> {
    let merged = match table {
        StateTable::Commitments | StateTable::OpenLoops => "merged_into",
        StateTable::People | StateTable::Projects => "NULL",
    };
    let mut stmt = conn.prepare(&format!("SELECT id, created_at, {merged} FROM {}", table.as_str()))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, (r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))))?;
    rows.collect()
}

/// What a provenance pointer adds in v2, by event id: time, source, body hash.
struct EventFacts {
    ts: i64,
    source: String,
    hash: String,
}

fn event_facts(conn: &Connection, ids: &HashSet<i64>) -> Result<HashMap<i64, EventFacts>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached("SELECT ts, source, content FROM event_log WHERE id = ?1")?;
    let mut out = HashMap::new();
    for id in ids {
        let row = stmt
            .query_row([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .optional()?;
        if let Some((ts, source, content)) = row {
            out.insert(*id, EventFacts { ts, source, hash: sha256_hex(&content) });
        }
    }
    Ok(out)
}

fn v2_pointer(ns: &str, event_id: i64, quote: Value, events: &HashMap<i64, EventFacts>) -> Value {
    let facts = events.get(&event_id);
    json!({
        "event_id": event_id,
        "event_uri": uri(ns, "event", event_id),
        "quote": quote,
        "observed_at": facts.map(|f| json!(f.ts)).unwrap_or(Value::Null),
        "source": facts.map(|f| json!(f.source)).unwrap_or(Value::Null),
        "content_hash": facts.map(|f| json!(f.hash)).unwrap_or(Value::Null),
    })
}

/// The capability manifest: what this producer models, so a receiver can tell an absent field
/// from an unsupported one.
pub fn capabilities() -> Value {
    json!({
        "record_types": ["entity", "fact", "relation", "tombstone"],
        "entity_kinds": ["person", "project"],
        "fact_kinds": ["commitment", "open_loop"],
        "global_ids": "urn",
        "bitemporal": { "valid_time": true, "transaction_time": true },
        "relations": true,
        "supersession": true,
        "provenance": { "quotes": true, "content_hash": "sha256", "observed_at": true },
        "confidence": { "scale": "0..1", "bands": { "high": 0.8, "medium": 0.5 } },
        "namespaces": ["world", "agent_scratch:*"],
        "tombstones": true,
        "differential": true,
        "import_confidence_cap": crate::memory_import::CONFIDENCE_MAX,
    })
}

/// Build an export v2 at `now` under namespace `ns`: the v1 records, enriched, plus relations.
pub fn export_v2(conn: &Connection, since: Option<i64>, now: i64, ns: &str) -> Result<ExportV2, rusqlite::Error> {
    let v1 = super::export_v1(conn, since, now)?;
    let tables = [StateTable::People, StateTable::Projects, StateTable::Commitments, StateTable::OpenLoops];
    let mut times = HashMap::new();
    for table in tables {
        times.insert(uri_kind(table), row_times(conn, table)?);
    }

    // Relations changed after `since`: recorded after it or ended after it.
    let rel_filter = match since {
        Some(since) => format!("WHERE created_at > {since} OR (valid_to IS NOT NULL AND valid_to > {since})"),
        None => String::new(),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT id, subject_kind, subject_id, predicate, object_kind, object_id, valid_from, valid_to,
                confidence, provenance_event_id, actor, namespace, created_at
         FROM relations {rel_filter} ORDER BY id"
    ))?;
    #[allow(clippy::type_complexity)]
    let relations: Vec<(i64, String, i64, String, String, i64, i64, Option<i64>, f64, i64, Option<String>, String, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut event_ids: HashSet<i64> = HashSet::new();
    for line in &v1.lines[1..] {
        for pointer in line.get("provenance").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = pointer.get("event_id").and_then(Value::as_i64) {
                event_ids.insert(id);
            }
        }
    }
    event_ids.extend(relations.iter().map(|r| r.9));
    let events = event_facts(conn, &event_ids)?;

    let fact_ref = |table: StateTable, id: Option<i64>| id.map(|id| json!(uri(ns, uri_kind(table), id))).unwrap_or(Value::Null);
    let mut records = Vec::with_capacity(v1.lines.len() + relations.len());
    for line in &v1.lines[1..] {
        let mut record = line.clone();
        match line["type"].as_str() {
            Some("entity" | "fact") => {
                let kind = line["kind"].as_str().unwrap_or_default();
                let id = line["id"].as_i64().unwrap_or_default();
                let (recorded, merged) = times.get(kind).and_then(|t| t.get(&id)).copied().unwrap_or((line["valid_from"].as_i64().unwrap_or(now), None));
                record["uri"] = json!(uri(ns, kind, id));
                record["recorded_at"] = json!(recorded);
                record["superseded_by"] = match table_of_uri_kind(kind) {
                    Some(table) => fact_ref(table, merged),
                    None => Value::Null,
                };
                if line["type"] == "fact" {
                    record["counterparty"] = fact_ref(StateTable::People, line["counterparty_id"].as_i64());
                    record["project"] = fact_ref(StateTable::Projects, line["project_id"].as_i64());
                }
                let pointers: Vec<Value> = line["provenance"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| v2_pointer(ns, p["event_id"].as_i64().unwrap_or_default(), p["quote"].clone(), &events))
                    .collect();
                record["provenance"] = Value::Array(pointers);
            }
            Some("tombstone") => {
                let kind = line["kind"].as_str().unwrap_or_default();
                let singular = StateTable::parse(kind).map(uri_kind).unwrap_or(kind);
                record["uri"] = json!(uri(ns, singular, line["id"].as_i64().unwrap_or_default()));
            }
            _ => {}
        }
        records.push(record);
    }
    // Relations go after the facts and before the tombstones.
    let first_tombstone = records.iter().position(|r| r["type"] == "tombstone").unwrap_or(records.len());
    let mut relation_lines = Vec::with_capacity(relations.len());
    for (id, sk, sid, predicate, ok, oid, valid_from, valid_to, confidence, event_id, actor, namespace, created) in relations {
        let (Some(subject), Some(object)) = (StateTable::parse(&sk), StateTable::parse(&ok)) else {
            continue;
        };
        relation_lines.push(json!({
            "type": "relation",
            "uri": uri(ns, "relation", id),
            "id": id,
            "subject": uri(ns, uri_kind(subject), sid),
            "predicate": predicate,
            "object": uri(ns, uri_kind(object), oid),
            "confidence": confidence,
            "band": super::band(confidence),
            "valid_from": valid_from,
            "valid_to": valid_to,
            "recorded_at": created,
            "actor": actor,
            "namespace": namespace,
            "provenance": [v2_pointer(ns, event_id, Value::Null, &events)],
        }));
    }
    records.splice(first_tombstone..first_tombstone, relation_lines);

    let count = |t: &str| records.iter().filter(|r| r["type"] == t).count();
    let newest_relation = records
        .iter()
        .filter(|r| r["type"] == "relation")
        .filter_map(|r| r["recorded_at"].as_i64().into_iter().chain(r["valid_to"].as_i64()).max())
        .max();
    let cursor = v1.cursor.max(newest_relation.unwrap_or(i64::MIN));
    let manifest = json!({
        "type": "manifest",
        "format": FORMAT,
        "schema_version": crate::LATEST_SCHEMA_VERSION,
        "generated_at": now,
        "since_cursor": since,
        "cursor": cursor,
        "namespace": format!("{URN_PREFIX}{ns}"),
        "capabilities": capabilities(),
        "counts": {
            "entities": count("entity"),
            "facts": count("fact"),
            "relations": count("relation"),
            "tombstones": count("tombstone"),
        },
    });
    let mut lines = Vec::with_capacity(records.len() + 1);
    lines.push(manifest);
    lines.extend(records);
    Ok(ExportV2 { lines, cursor, since, namespace: ns.to_string() })
}

/// Whether `text` is an export v2 (its first line is the v2 manifest).
pub fn is_export_v2(text: &str) -> bool {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .is_some_and(|first| first["type"] == "manifest" && first["format"] == FORMAT)
}

/// Import an export v2 into `conn` as `ctx` at `now`, in one transaction. Same rules as
/// [`super::import_v1`] (new ids, one evidence event per pointer, confidence capped, the importer
/// is the writer, tombstones counted not applied), and additionally: rows keep their
/// `recorded_at`, evidence events keep each pointer's `observed_at`, relations are restored
/// between the new rows, and `superseded_by` becomes `merged_into` on the new row.
pub fn import_v2(conn: &mut Connection, text: &str, ctx: &WriteContext, now: i64) -> Result<ImportReport, ImportError> {
    if text.len() > super::MAX_IMPORT_BYTES {
        return Err(ImportError::TooLarge);
    }
    if !is_export_v2(text) {
        return Err(ImportError::NotExportV2);
    }
    let own_transaction = conn.is_autocommit();
    if own_transaction {
        conn.execute_batch("BEGIN IMMEDIATE")?;
    }
    let result = super::import_records(conn, text, ctx, now, Mode::V2).map(|(report, _)| report);
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

/// The `(table, old id)` a URI names, when it is a state row URI.
fn state_ref(raw: Option<&str>) -> Option<(StateTable, i64)> {
    let (_, kind, id) = parse_uri(raw?)?;
    Some((table_of_uri_kind(kind)?, id))
}

/// After entities and facts are restored: supersession links, then relations.
pub(crate) fn restore_links(
    conn: &mut Connection,
    records: &[Value],
    maps: &IdMaps,
    ctx: &WriteContext,
    now: i64,
    report: &mut ImportReport,
) -> Result<(), ImportError> {
    let cap = crate::memory_import::CONFIDENCE_MAX;
    for record in records.iter().filter(|r| r["type"] == "fact") {
        let Some(table) = record["kind"].as_str().and_then(table_of_uri_kind) else { continue };
        let (Some(new_id), Some((target_table, target_old))) =
            (record["id"].as_i64().and_then(|id| maps.get(table, id)), state_ref(record["superseded_by"].as_str()))
        else {
            continue;
        };
        if target_table != table {
            continue;
        }
        if let Some(target) = maps.get(table, target_old) {
            conn.execute(&format!("UPDATE {} SET merged_into = ?1 WHERE id = ?2", table.as_str()), params![target, new_id])?;
            report.superseded += 1;
        }
    }
    for record in records.iter().filter(|r| r["type"] == "relation") {
        let subject = state_ref(record["subject"].as_str()).and_then(|(t, id)| maps.get(t, id).map(|n| (t, n)));
        let object = state_ref(record["object"].as_str()).and_then(|(t, id)| maps.get(t, id).map(|n| (t, n)));
        let predicate = record["predicate"].as_str().filter(|p| !p.trim().is_empty());
        let (Some((st, sid)), Some((ot, oid)), Some(predicate)) = (subject, object, predicate) else {
            report.skipped += 1;
            continue;
        };
        let valid_from = record["valid_from"].as_i64().unwrap_or(now);
        let valid_to = record["valid_to"].as_i64().filter(|to| *to >= valid_from);
        let recorded = record["recorded_at"].as_i64().unwrap_or(valid_from);
        let confidence = record["confidence"].as_f64().unwrap_or(cap).clamp(0.0, cap);
        let evidence = super::evidence_events(conn, record, predicate, valid_from, true, ctx)?;
        report.events += evidence.len();
        let Some((event_id, _)) = evidence.first() else {
            report.skipped += 1;
            continue;
        };
        conn.execute(
            "INSERT INTO relations (subject_kind, subject_id, predicate, object_kind, object_id, valid_from, valid_to,
                                    confidence, provenance_event_id, actor, namespace, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![st.as_str(), sid, predicate, ot.as_str(), oid, valid_from, valid_to, confidence, event_id, ctx.actor_sql(), ctx.namespace_sql(), recorded],
        )?;
        report.relations += 1;
    }
    Ok(())
}
