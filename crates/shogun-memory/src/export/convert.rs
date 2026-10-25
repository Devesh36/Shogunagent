//! Export v1 ↔ v2 without a store (`docs/api/export-v2.md` §Conversion).
//!
//! - **v1 → v2** is lossless in content and fills what v1 cannot know with `null`: no
//!   `recorded_at` beyond `valid_from`, no relations, no supersession, no event hashes. The
//!   manifest's capabilities say so (`relations: false`, `content_hash: null`, …), so a receiver
//!   does not read an absent relation as "none exist".
//! - **v2 → v1** drops what v1 has no field for and reports it: relations, `superseded_by`,
//!   `recorded_at`, the pointer additions. Every v1 field comes through unchanged.

use serde_json::{json, Value};

use super::v2;

/// What a conversion could not carry, by field. Counts only, never content.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Lossiness {
    /// Records carried over.
    pub records: usize,
    /// Relation records dropped (v2 → v1).
    pub relations_dropped: usize,
    /// Records whose `superseded_by` link was dropped (v2 → v1).
    pub supersession_dropped: usize,
    /// Provenance pointers whose `content_hash` / `observed_at` / `source` were dropped (v2 → v1)
    /// or are unknown (v1 → v2).
    pub pointer_fields_dropped: usize,
    /// Records whose `recorded_at` was dropped (v2 → v1) or set from `valid_from` (v1 → v2).
    pub recorded_at_approximated: usize,
    /// Lines that were not records of the input format and were skipped.
    pub skipped: usize,
}

/// Why a conversion refused its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The first line is not the manifest of the expected format.
    WrongFormat,
    /// The namespace id is not `[0-9a-z-]{1,64}`.
    BadNamespace,
}

fn parsed(text: &str) -> Vec<Value> {
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect()
}

fn jsonl(lines: &[Value]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

/// The v1 keys of each record type, in the order v1 writes them.
const V1_ENTITY: &[&str] = &[
    "type", "kind", "id", "name", "aliases", "summary", "status", "confidence", "band", "valid_from", "valid_to", "updated_at", "actor",
    "namespace", "provenance",
];
const V1_FACT: &[&str] = &[
    "type", "kind", "id", "subtype", "text", "status", "due_at", "counterparty_id", "project_id", "confidence", "band", "valid_from",
    "valid_to", "updated_at", "actor", "namespace", "provenance",
];
const V1_TOMBSTONE: &[&str] = &["type", "kind", "id", "deleted_at", "reason"];

fn pick(record: &Value, keys: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for key in keys {
        out.insert((*key).to_string(), record.get(*key).cloned().unwrap_or(Value::Null));
    }
    Value::Object(out)
}

/// Convert an export v2 to export v1.
pub fn v2_to_v1(text: &str) -> Result<(String, Lossiness), ConvertError> {
    if !v2::is_export_v2(text) {
        return Err(ConvertError::WrongFormat);
    }
    let lines = parsed(text);
    let mut loss = Lossiness::default();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines.iter().skip(1) {
        match line["type"].as_str() {
            Some("entity" | "fact") => {
                let mut record = pick(line, if line["type"] == "entity" { V1_ENTITY } else { V1_FACT });
                let pointers = line["provenance"].as_array().cloned().unwrap_or_default();
                loss.pointer_fields_dropped += pointers.len();
                record["provenance"] =
                    Value::Array(pointers.iter().map(|p| json!({ "event_id": p["event_id"], "quote": p["quote"] })).collect());
                if !line["superseded_by"].is_null() {
                    loss.supersession_dropped += 1;
                }
                if line["recorded_at"] != line["valid_from"] {
                    loss.recorded_at_approximated += 1;
                }
                loss.records += 1;
                out.push(record);
            }
            Some("tombstone") => {
                loss.records += 1;
                out.push(pick(line, V1_TOMBSTONE));
            }
            Some("relation") => loss.relations_dropped += 1,
            _ => loss.skipped += 1,
        }
    }
    let manifest = &lines[0];
    let count = |t: &str| out.iter().filter(|r| r["type"] == t).count();
    let head = json!({
        "type": "manifest",
        "format": super::FORMAT,
        "schema_version": manifest["schema_version"],
        "generated_at": manifest["generated_at"],
        "since_cursor": manifest["since_cursor"],
        "cursor": manifest["cursor"],
        "counts": { "entities": count("entity"), "facts": count("fact"), "tombstones": count("tombstone") },
    });
    out.insert(0, head);
    Ok((jsonl(&out), loss))
}

/// Convert an export v1 to export v2 under namespace id `ns` (the exporting store's, when known).
pub fn v1_to_v2(text: &str, ns: &str) -> Result<(String, Lossiness), ConvertError> {
    if !super::is_export_v1(text) {
        return Err(ConvertError::WrongFormat);
    }
    if !v2::valid_namespace(ns) {
        return Err(ConvertError::BadNamespace);
    }
    let lines = parsed(text);
    let mut loss = Lossiness::default();
    let mut out = Vec::with_capacity(lines.len());
    let reference = |kind: &str, id: &Value| id.as_i64().map(|id| json!(v2::uri(ns, kind, id))).unwrap_or(Value::Null);
    for line in lines.iter().skip(1) {
        let mut record = line.clone();
        match line["type"].as_str() {
            Some(t @ ("entity" | "fact")) => {
                let kind = line["kind"].as_str().unwrap_or_default();
                record["uri"] = reference(kind, &line["id"]);
                record["recorded_at"] = line["valid_from"].clone();
                record["superseded_by"] = Value::Null;
                if t == "fact" {
                    record["counterparty"] = reference("person", &line["counterparty_id"]);
                    record["project"] = reference("project", &line["project_id"]);
                }
                let pointers = line["provenance"].as_array().cloned().unwrap_or_default();
                loss.pointer_fields_dropped += pointers.len();
                record["provenance"] = Value::Array(
                    pointers
                        .iter()
                        .map(|p| {
                            json!({
                                "event_id": p["event_id"],
                                "event_uri": reference("event", &p["event_id"]),
                                "quote": p["quote"],
                                "observed_at": Value::Null,
                                "source": Value::Null,
                                "content_hash": Value::Null,
                            })
                        })
                        .collect(),
                );
                loss.recorded_at_approximated += 1;
                loss.records += 1;
                out.push(record);
            }
            Some("tombstone") => {
                let kind = line["kind"].as_str().unwrap_or_default();
                let singular = crate::state::StateTable::parse(kind).map(v2::uri_kind).unwrap_or(kind);
                record["uri"] = reference(singular, &line["id"]);
                loss.records += 1;
                out.push(record);
            }
            _ => loss.skipped += 1,
        }
    }
    let manifest = &lines[0];
    let mut capabilities = v2::capabilities();
    capabilities["record_types"] = json!(["entity", "fact", "tombstone"]);
    capabilities["bitemporal"] = json!({ "valid_time": true, "transaction_time": false });
    capabilities["relations"] = json!(false);
    capabilities["supersession"] = json!(false);
    capabilities["provenance"] = json!({ "quotes": true, "content_hash": Value::Null, "observed_at": false });
    let count = |t: &str| out.iter().filter(|r| r["type"] == t).count();
    let head = json!({
        "type": "manifest",
        "format": v2::FORMAT,
        "schema_version": manifest["schema_version"],
        "generated_at": manifest["generated_at"],
        "since_cursor": manifest["since_cursor"],
        "cursor": manifest["cursor"],
        "namespace": format!("{}{ns}", v2::URN_PREFIX),
        "capabilities": capabilities,
        "counts": { "entities": count("entity"), "facts": count("fact"), "relations": 0, "tombstones": count("tombstone") },
    });
    out.insert(0, head);
    Ok((jsonl(&out), loss))
}
