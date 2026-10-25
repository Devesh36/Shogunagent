#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;

use super::tests::{counts, ctx, event, quotes, seed, validate};
use super::*;
use crate::actor::{Actor, WriteContext};
use crate::state::{CommitmentDirection, CommitmentStatus, NewCommitment};

fn schema(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).expect("schema file")).expect("schema JSON")
}

/// The v1 world plus what v2 adds: a relation (person works_on project, ended), a duplicate
/// commitment folded into the first (supersession), and a closed open loop.
fn seed_v2(conn: &mut Connection) -> (i64, i64) {
    let (oren, commitment, open_loop) = seed(conn);
    let e3 = event(conn, "Oren Vale: the rollout plan will come Friday, as promised.", 3_000);
    let duplicate = state::insert_commitment(
        conn,
        &NewCommitment {
            direction: CommitmentDirection::Theirs,
            counterparty_id: Some(oren),
            description: "the rollout plan will come Friday",
            due_at: Some(5_000),
            status: CommitmentStatus::Open,
            project_id: None,
            confidence: 0.7,
            now: 3_000,
        },
        &[Provenance::new(e3)],
    )
    .unwrap();
    state::retain_quote(conn, StateTable::Commitments, duplicate, e3, "the rollout plan will come Friday").unwrap();
    conn.execute("UPDATE commitments SET merged_into = ?1 WHERE id = ?2", params![commitment, duplicate]).unwrap();
    conn.execute("UPDATE open_loops SET status = 'closed', updated_at = 4000 WHERE id = ?1", [open_loop]).unwrap();
    let project: i64 = conn.query_row("SELECT id FROM projects", [], |r| r.get(0)).unwrap();
    conn.execute(
        "INSERT INTO relations (subject_kind, subject_id, predicate, object_kind, object_id, valid_from, valid_to,
                                confidence, provenance_event_id, actor, namespace, created_at)
         VALUES ('people', ?1, 'works_on', 'projects', ?2, 1000, 6000, 0.9, ?3, 'agent:exporter', 'world', 1200)",
        params![oren, project, e3],
    )
    .unwrap();
    (commitment, duplicate)
}

fn validate_all(text: &str, schema: &Value) {
    for (n, line) in text.lines().enumerate() {
        let value: Value = serde_json::from_str(line).unwrap();
        validate(schema, schema, &value, &format!("line {n}")).unwrap_or_else(|e| panic!("{e}\n{line}"));
    }
}

/// `(subject name, predicate, object name, valid_from, valid_to)` of every relation.
fn relations(conn: &Connection) -> Vec<(String, String, String, i64, Option<i64>)> {
    let mut stmt = conn
        .prepare(
            "SELECT p.display_name, r.predicate, j.name, r.valid_from, r.valid_to FROM relations r
             JOIN people p ON r.subject_kind = 'people' AND p.id = r.subject_id
             JOIN projects j ON r.object_kind = 'projects' AND j.id = r.object_id ORDER BY 1, 2, 3",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

/// `(text, valid_from, valid_to, recorded_at)` of every row, from the store's own export.
fn intervals(conn: &Connection) -> Vec<(String, i64, Option<i64>, i64)> {
    let export = export_v2(conn, None, 50_000, "probe").unwrap();
    let mut out: Vec<_> = export.lines[1..]
        .iter()
        .filter(|l| l["type"] == "entity" || l["type"] == "fact")
        .map(|l| {
            let text = l["name"].as_str().or(l["text"].as_str()).unwrap().to_string();
            (text, l["valid_from"].as_i64().unwrap(), l["valid_to"].as_i64(), l["recorded_at"].as_i64().unwrap())
        })
        .collect();
    out.sort();
    out
}

/// E27: every line of a v2 export validates against the published schema; pointers carry the
/// sha256 of the source event, and the quote is a span of the body that hash names.
#[test]
fn every_v2_line_validates_and_pointers_verify_against_the_source_body() {
    let schema = schema("export-v2.schema.json");
    let mut conn = crate::open_in_memory().unwrap();
    let (commitment, duplicate) = seed_v2(&mut conn);
    let ns = namespace_id(&conn, 10_000).unwrap();
    assert_eq!(namespace_id(&conn, 99_999).unwrap(), ns, "the namespace id is stable");
    crate::state_edit::delete_state_as(&mut conn, StateTable::People, 2, 9_000, &ctx()).unwrap();
    let full = export_v2(&conn, None, 10_000, &ns).unwrap();
    let diff = export_v2(&conn, Some(2_500), 10_000, &ns).unwrap();
    for export in [&full, &diff] {
        let text = export.jsonl();
        assert!(is_export_v2(&text) && !is_export_v1(&text));
        validate_all(&text, &schema);
    }
    let manifest = &full.lines[0];
    assert_eq!(manifest["counts"]["relations"], 1);
    assert_eq!(manifest["namespace"], format!("urn:shogunagent:{ns}"));
    assert_eq!(manifest["capabilities"]["provenance"]["content_hash"], "sha256");

    for record in full.lines.iter().filter(|l| l["type"] == "fact" || l["type"] == "entity") {
        for pointer in record["provenance"].as_array().unwrap() {
            let id = pointer["event_id"].as_i64().unwrap();
            let body: String = conn.query_row("SELECT content FROM event_log WHERE id = ?1", [id], |r| r.get(0)).unwrap();
            assert_eq!(pointer["content_hash"], v2::content_hash(&body));
            if let Some(quote) = pointer["quote"].as_str() {
                assert!(body.contains(quote), "quote is a span of the hashed body");
            }
        }
    }
    let dup = full.lines.iter().find(|l| l["type"] == "fact" && l["id"] == duplicate).unwrap();
    assert_eq!(dup["superseded_by"], v2::uri(&ns, "commitment", commitment));
    let open_loop = full.lines.iter().find(|l| l["kind"] == "open_loop").unwrap();
    assert_eq!((open_loop["valid_from"].as_i64(), open_loop["recorded_at"].as_i64(), open_loop["valid_to"].as_i64()), (Some(1_500), Some(2_000), Some(4_000)));
    let relation = full.lines.iter().find(|l| l["type"] == "relation").unwrap();
    assert_eq!(relation["predicate"], "works_on");
    assert_eq!(relation["subject"], v2::uri(&ns, "person", 1));
    assert!(full.lines.iter().any(|l| l["type"] == "tombstone" && l["uri"] == v2::uri(&ns, "person", 2)));
    // The differential export carries only what changed after 2 500: the duplicate, the closed
    // loop, the tombstone; the relation was recorded before and ended after (valid_to 6 000).
    let kinds: Vec<&str> = diff.lines[1..].iter().filter_map(|l| l["type"].as_str()).collect();
    assert!(kinds.contains(&"relation") && kinds.contains(&"tombstone"), "{kinds:?}");
    assert!(!diff.lines.iter().any(|l| l["type"] == "entity"), "unchanged entities are not repeated");
    // The validator catches a malformed URI.
    let mut bad = relation.clone();
    bad["subject"] = Value::String("urn:other:1".into());
    assert!(validate(&schema, &schema, &bad, "bad").is_err());
}

/// E27: export v2 → fresh store → import v2 gives identical row counts, quotes, relations,
/// validity intervals (and record times), and the supersession link.
#[test]
fn v2_round_trip_keeps_counts_quotes_relations_intervals_and_supersession() {
    let mut source = crate::open_in_memory().unwrap();
    seed_v2(&mut source);
    let ns = namespace_id(&source, 10_000).unwrap();
    let export = export_v2(&source, None, 10_000, &ns).unwrap();

    let mut target = crate::open_in_memory().unwrap();
    let importer = WriteContext::world(Actor::agent("importer").unwrap());
    let report = import_v2(&mut target, &export.jsonl(), &importer, 20_000).unwrap();
    assert_eq!((report.entities, report.facts, report.relations, report.superseded, report.skipped), (3, 3, 1, 1, 0), "{report:?}");
    assert_eq!(counts(&target), counts(&source));
    assert_eq!(quotes(&target), quotes(&source));
    assert_eq!(relations(&target), relations(&source));
    assert_eq!(intervals(&target), intervals(&source));
    let merged: i64 = target.query_row("SELECT COUNT(*) FROM commitments WHERE merged_into IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(merged, 1);
    // Evidence events keep the source event's time.
    let ts: i64 = target
        .query_row("SELECT e.ts FROM event_log e WHERE e.content = 'the rollout plan will come Friday'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ts, 3_000);
    // Relations are written by the importer, capped like every imported claim.
    let (actor, confidence): (String, f64) = target.query_row("SELECT actor, confidence FROM relations", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(actor, "agent:importer");
    assert!(confidence <= crate::memory_import::CONFIDENCE_MAX);
    // A v1 text is not a v2 export.
    let v1 = export_v1(&source, None, 10_000).unwrap().jsonl();
    assert!(matches!(import_v2(&mut target, &v1, &importer, 1), Err(ImportError::NotExportV2)));
}

/// E27: v2 → v1 equals the store's own v1 export and reports what it dropped; v1 → v2 validates
/// against the v2 schema and restores the same rows.
#[test]
fn v1_and_v2_convert_both_ways_and_report_what_is_lost() {
    let mut conn = crate::open_in_memory().unwrap();
    seed_v2(&mut conn);
    let ns = namespace_id(&conn, 10_000).unwrap();
    let v2_text = export_v2(&conn, None, 10_000, &ns).unwrap().jsonl();
    let v1 = export_v1(&conn, None, 10_000).unwrap();

    let (down, loss) = convert::v2_to_v1(&v2_text).unwrap();
    let down_lines: Vec<Value> = down.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(down_lines, v1.lines, "v2 → v1 is exactly the v1 export");
    validate_all(&down, &schema("export-v1.schema.json"));
    assert_eq!((loss.relations_dropped, loss.supersession_dropped), (1, 1), "{loss:?}");
    assert!(loss.recorded_at_approximated >= 1, "the open loop's record time differs from its valid time");

    let (up, loss) = convert::v1_to_v2(&v1.jsonl(), "converted-1").unwrap();
    validate_all(&up, &schema("export-v2.schema.json"));
    assert_eq!(loss.records, v1.records());
    let manifest: Value = serde_json::from_str(up.lines().next().unwrap()).unwrap();
    assert_eq!(manifest["capabilities"]["relations"], false);
    let mut a = crate::open_in_memory().unwrap();
    let mut b = crate::open_in_memory().unwrap();
    import_v1(&mut a, &v1.jsonl(), &ctx(), 20_000).unwrap();
    import_v2(&mut b, &up, &ctx(), 20_000).unwrap();
    assert_eq!(counts(&a), counts(&b));
    assert_eq!(quotes(&a), quotes(&b));
    assert_eq!(intervals(&a), intervals(&b));

    assert_eq!(convert::v1_to_v2(&v2_text, "x"), Err(convert::ConvertError::WrongFormat));
    assert_eq!(convert::v1_to_v2(&v1.jsonl(), "Bad NS"), Err(convert::ConvertError::BadNamespace));
    assert_eq!(v2::parse_uri("urn:shogunagent:ab-1:open_loop:42"), Some(("ab-1", "open_loop", 42)));
    assert_eq!(v2::parse_uri("urn:other:x"), None);
}
