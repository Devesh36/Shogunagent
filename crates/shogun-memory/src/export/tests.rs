#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use super::*;
use crate::actor::{Actor, WriteContext};
use crate::state::{CommitmentDirection, CommitmentStatus, NewCommitment, NewOpenLoop, NewPerson, OpenLoopKind};

/// The published schema, read from the repository so the test checks the file users get.
fn schema() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/export-v1.schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("schema file")).expect("schema JSON")
}

/// A validator for exactly the JSON Schema keywords `export-v1.schema.json` uses. Any other
/// keyword fails the test, so the schema cannot grow a rule this check silently ignores.
pub(super) fn validate(schema: &Value, root: &Value, value: &Value, at: &str) -> Result<(), String> {
    let Some(rules) = schema.as_object() else {
        return Err(format!("{at}: schema is not an object"));
    };
    for (keyword, rule) in rules {
        match keyword.as_str() {
            "$schema" | "$id" | "title" | "description" | "$defs" => {}
            "$ref" => {
                let name = rule.as_str().and_then(|r| r.strip_prefix("#/$defs/")).ok_or(format!("{at}: bad $ref"))?;
                validate(&root["$defs"][name], root, value, at)?;
            }
            "oneOf" => {
                let passing = rule.as_array().unwrap().iter().filter(|s| validate(s, root, value, at).is_ok()).count();
                if passing != 1 {
                    return Err(format!("{at}: matches {passing} of oneOf"));
                }
            }
            "type" => {
                let types: Vec<&str> = match rule {
                    Value::String(t) => vec![t.as_str()],
                    Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
                    _ => return Err(format!("{at}: bad type")),
                };
                let ok = types.iter().any(|t| match *t {
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "string" => value.is_string(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "number" => value.is_number(),
                    "null" => value.is_null(),
                    "boolean" => value.is_boolean(),
                    _ => false,
                });
                if !ok {
                    return Err(format!("{at}: {value} is not {types:?}"));
                }
            }
            "const" if value != rule => return Err(format!("{at}: {value} != {rule}")),
            "const" => {}
            "enum" if !rule.as_array().unwrap().contains(value) => return Err(format!("{at}: {value} not in enum")),
            "enum" => {}
            "required" => {
                for key in rule.as_array().unwrap().iter().filter_map(Value::as_str) {
                    if value.get(key).is_none() {
                        return Err(format!("{at}: missing {key}"));
                    }
                }
            }
            "properties" => {
                for (key, sub) in rule.as_object().unwrap() {
                    if let Some(field) = value.get(key) {
                        validate(sub, root, field, &format!("{at}.{key}"))?;
                    }
                }
            }
            "additionalProperties" => {
                assert_eq!(rule, &Value::Bool(false), "only additionalProperties: false is used");
                let allowed = rules.get("properties").and_then(Value::as_object).unwrap();
                if let Some(object) = value.as_object() {
                    if let Some(extra) = object.keys().find(|k| !allowed.contains_key(*k)) {
                        return Err(format!("{at}: unexpected property {extra}"));
                    }
                }
            }
            "items" => {
                for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                    validate(rule, root, item, &format!("{at}[{index}]"))?;
                }
            }
            "minimum" if value.as_f64().is_some_and(|n| n < rule.as_f64().unwrap()) => return Err(format!("{at}: below minimum")),
            "maximum" if value.as_f64().is_some_and(|n| n > rule.as_f64().unwrap()) => return Err(format!("{at}: above maximum")),
            "minimum" | "maximum" => {}
            "minLength" if value.as_str().is_some_and(|s| s.chars().count() < rule.as_u64().unwrap() as usize) => {
                return Err(format!("{at}: shorter than minLength"))
            }
            "minLength" => {}
            "pattern" => {
                let text = value.as_str().unwrap_or_default();
                let ns_ok = |ns: &str| !ns.is_empty() && ns.len() <= 64 && ns.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-');
                let ok = match rule.as_str() {
                    Some("^(world|agent_scratch:.+)$") => text == "world" || text.strip_prefix("agent_scratch:").is_some_and(|id| !id.is_empty()),
                    Some("^urn:shogunagent:[0-9a-z-]{1,64}$") => text.strip_prefix("urn:shogunagent:").is_some_and(ns_ok),
                    Some("^urn:shogunagent:[0-9a-z-]{1,64}:[a-z_]+:-?[0-9]+$") => {
                        let parts: Vec<&str> = text.strip_prefix("urn:shogunagent:").map(|r| r.split(':').collect()).unwrap_or_default();
                        parts.len() == 3
                            && ns_ok(parts[0])
                            && !parts[1].is_empty()
                            && parts[1].bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                            && parts[2].strip_prefix('-').unwrap_or(parts[2]).bytes().all(|b| b.is_ascii_digit())
                            && !parts[2].trim_start_matches('-').is_empty()
                    }
                    Some("^sha256:[0-9a-f]{64}$") => {
                        text.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                    }
                    other => panic!("{at}: the validator does not know the pattern {other:?}; extend it with the schema"),
                };
                if !ok || !value.is_string() {
                    return Err(format!("{at}: {value} does not match {rule}"));
                }
            }
            other => panic!("{at}: the test validator does not implement `{other}`; extend it with the schema"),
        }
    }
    Ok(())
}

pub(super) fn ctx() -> WriteContext {
    WriteContext::world(Actor::agent("exporter").unwrap())
}

pub(super) fn event(conn: &Connection, text: &str, ts: i64) -> i64 {
    let hash = event_log::content_hash(text);
    event_log::insert(
        conn,
        &NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.example.mail"),
            window_title: Some("Inbox"),
            content: text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        },
    )
    .unwrap()
}

/// A small world: two people, a project, a commitment and an open loop with quotes.
pub(super) fn seed(conn: &mut Connection) -> (i64, i64, i64) {
    let e1 = event(conn, "Oren Vale: I'll send the rollout plan by Friday.", 1_000);
    let e2 = event(conn, "Still waiting on Tamsin for the cost estimate.", 2_000);
    let oren = state::insert_person(
        conn,
        &NewPerson { display_name: "Oren Vale", aliases_json: Some(r#"["Oren"]"#), relationship_summary: Some("Leads the Heron rollout."), confidence: 0.9, now: 1_000, ..NewPerson::default() },
        &[Provenance::new(e1)],
    )
    .unwrap();
    state::insert_person(conn, &NewPerson { display_name: "Tamsin Ekberg", confidence: 0.7, now: 2_000, ..NewPerson::default() }, &[Provenance::new(e2)]).unwrap();
    state::insert_project(
        conn,
        &state::NewProject { name: "Heron Rollout", status: state::ProjectStatus::Active, summary: Some("Q3 rollout."), participants_json: None, sources_json: None, confidence: 0.85, now: 1_000 },
        &[Provenance::new(e1)],
    )
    .unwrap();
    let commitment = state::insert_commitment(
        conn,
        &NewCommitment {
            direction: CommitmentDirection::Theirs,
            counterparty_id: Some(oren),
            description: "send the rollout plan by Friday",
            due_at: Some(5_000),
            status: CommitmentStatus::Open,
            project_id: None,
            confidence: 0.9,
            now: 1_000,
        },
        &[Provenance::new(e1)],
    )
    .unwrap();
    state::retain_quote(conn, StateTable::Commitments, commitment, e1, "I'll send the rollout plan by Friday.").unwrap();
    let open_loop = state::insert_open_loop(
        conn,
        &NewOpenLoop { kind: OpenLoopKind::WaitingOnThem, description: "cost estimate from Tamsin", counterparty_id: None, project_id: None, opened_at: 1_500, confidence: 0.6, now: 2_000 },
        &[Provenance::new(e2)],
    )
    .unwrap();
    state::retain_quote(conn, StateTable::OpenLoops, open_loop, e2, "Still waiting on Tamsin for the cost estimate.").unwrap();
    (oren, commitment, open_loop)
}

pub(super) fn counts(conn: &Connection) -> Vec<i64> {
    ["people", "projects", "commitments", "open_loops"]
        .iter()
        .map(|t| conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap())
        .collect()
}

pub(super) fn quotes(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT quote FROM state_provenance WHERE quote IS NOT NULL ORDER BY quote").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    rows
}

/// E7-T1: every line of an export (full and differential, with a tombstone) validates against
/// the published schema, and the manifest counts what follows it.
#[test]
fn every_exported_line_validates_against_the_published_schema() {
    let schema = schema();
    let mut conn = crate::open_in_memory().unwrap();
    let (_, commitment, _) = seed(&mut conn);
    crate::state_edit::delete_state_as(&mut conn, StateTable::Commitments, commitment, 9_000, &ctx()).unwrap();
    let full = export_v1(&conn, None, 10_000).unwrap();
    let diff = export_v1(&conn, Some(1_500), 10_000).unwrap();
    for export in [&full, &diff] {
        let text = export.jsonl();
        assert!(is_export_v1(&text));
        for (n, line) in text.lines().enumerate() {
            let value: Value = serde_json::from_str(line).unwrap();
            validate(&schema, &schema, &value, &format!("line {n}")).unwrap_or_else(|e| panic!("{e}\n{line}"));
        }
        let manifest = &export.lines[0];
        let entities = export.lines.iter().filter(|l| l["type"] == "entity").count();
        assert_eq!(manifest["counts"]["entities"], json!(entities));
    }
    // The validator does catch a bad record.
    let bad = json!({ "type": "fact", "kind": "commitment", "id": 1 });
    assert!(validate(&schema, &schema, &bad, "bad").is_err());
    let wrong_ns = full.lines.iter().find(|l| l["type"] == "entity").cloned().map(|mut l| {
        l["namespace"] = json!("tenant_x");
        l
    });
    assert!(validate(&schema, &schema, &wrong_ns.unwrap(), "ns").is_err());
    // Shapes: provenance pointers with quotes, namespace and actor, the validity interval.
    let fact = full.lines.iter().find(|l| l["type"] == "fact" && l["kind"] == "open_loop").unwrap();
    assert_eq!(fact["provenance"][0]["quote"], "Still waiting on Tamsin for the cost estimate.");
    assert_eq!(fact["namespace"], "world");
    assert_eq!(fact["band"], "medium");
    assert_eq!(fact["valid_to"], Value::Null);
    let tombstone = full.lines.iter().find(|l| l["type"] == "tombstone").unwrap();
    assert_eq!((tombstone["kind"].as_str(), tombstone["id"].as_i64()), (Some("commitments"), Some(commitment)));
    assert_eq!(tombstone["reason"], "deleted_by:agent:exporter");
}

/// E7-T2: export → a fresh store → import gives the same row counts and the same quotes.
#[test]
fn export_then_import_into_a_fresh_store_keeps_rows_and_quotes() {
    let mut source = crate::open_in_memory().unwrap();
    seed(&mut source);
    let export = export_v1(&source, None, 10_000).unwrap();

    let mut target = crate::open_in_memory().unwrap();
    let importer = WriteContext::world(Actor::agent("importer").unwrap());
    let report = import_v1(&mut target, &export.jsonl(), &importer, 20_000).unwrap();
    assert_eq!((report.entities, report.facts, report.skipped), (3, 2, 0), "{report:?}");
    assert_eq!(counts(&target), counts(&source));
    assert_eq!(quotes(&target), quotes(&source));
    // Every imported row has provenance, records its writer, and is capped below High.
    let orphans: i64 = target
        .query_row(
            "SELECT COUNT(*) FROM commitments c WHERE NOT EXISTS (SELECT 1 FROM state_provenance p WHERE p.state_table = 'commitments' AND p.state_id = c.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
    let (actor, confidence): (String, f64) = target.query_row("SELECT actor_last, confidence FROM commitments", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(actor, "agent:importer");
    assert!(confidence <= crate::memory_import::CONFIDENCE_MAX);
    // The commitment still names its counterparty, under the new id.
    let counterparty: String = target
        .query_row("SELECT p.display_name FROM commitments c JOIN people p ON p.id = c.counterparty_id", [], |r| r.get(0))
        .unwrap();
    assert_eq!(counterparty, "Oren Vale");
    // The export of the imported store has the same shape and validates.
    let again = export_v1(&target, None, 30_000).unwrap();
    assert_eq!(again.records(), export.records());
    // Not an export: refused whole.
    assert!(matches!(import_v1(&mut target, "{\"format\":\"shogun-memory-import/v1\"}", &importer, 1), Err(ImportError::NotExportV1)));
}

/// E7-T3: a differential export from the stored cursor carries only what changed since, and a
/// fact deleted in between comes back as a tombstone.
#[test]
fn a_deleted_fact_is_a_tombstone_in_the_next_differential_export() {
    let mut conn = crate::open_in_memory().unwrap();
    let (_, commitment, open_loop) = seed(&mut conn);
    assert_eq!(cursor(&conn).unwrap(), None);
    let first = export_v1(&conn, cursor(&conn).unwrap(), 10_000).unwrap();
    advance_cursor(&conn, first.cursor).unwrap();
    assert_eq!(cursor(&conn).unwrap(), Some(10_000));
    assert_eq!(first.lines[0]["counts"]["facts"], 2);

    // Nothing changed: an empty differential export (the manifest only).
    let quiet = export_v1(&conn, cursor(&conn).unwrap(), 11_000).unwrap();
    assert_eq!(quiet.records(), 0);
    assert_eq!(quiet.lines[0]["since_cursor"], 10_000);

    // Delete one fact, edit the other: one tombstone and one changed fact.
    crate::state_edit::delete_state_as(&mut conn, StateTable::Commitments, commitment, 12_000, &ctx()).unwrap();
    conn.execute("UPDATE open_loops SET status = 'closed', updated_at = 12500 WHERE id = ?1", [open_loop]).unwrap();
    let next = export_v1(&conn, cursor(&conn).unwrap(), 13_000).unwrap();
    let tombstones: Vec<&Value> = next.lines.iter().filter(|l| l["type"] == "tombstone").collect();
    assert_eq!(tombstones.len(), 1);
    assert_eq!((tombstones[0]["kind"].as_str(), tombstones[0]["id"].as_i64()), (Some("commitments"), Some(commitment)));
    let facts: Vec<&Value> = next.lines.iter().filter(|l| l["type"] == "fact").collect();
    assert_eq!(facts.len(), 1);
    assert_eq!((facts[0]["status"].as_str(), facts[0]["valid_to"].as_i64()), (Some("closed"), Some(12_500)));
    assert!(next.lines.iter().all(|l| l["type"] != "entity"), "unchanged entities are not repeated");
    advance_cursor(&conn, next.cursor).unwrap();
    // The cursor never moves backwards.
    advance_cursor(&conn, 5).unwrap();
    assert_eq!(cursor(&conn).unwrap(), Some(13_000));
}
