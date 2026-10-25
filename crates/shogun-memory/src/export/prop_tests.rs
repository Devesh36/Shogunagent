//! Property tests for the export v1 import parser (plan E38): seeded random packages, malformed
//! on purpose, run in the ordinary `cargo test`. The nightly-only libFuzzer target
//! (`fuzz/fuzz_targets/export_import.rs`) drives the same entry point with coverage guidance.
//!
//! Properties, for every generated package:
//! - `import_v1` never panics and never fails on the database: a malformed record is skipped,
//!   a package that is not an export is refused whole (`NotExportV1`);
//! - the report accounts for every record line (entities + facts + tombstones + skipped);
//! - no state row is ever written without provenance (invariant 5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use super::*;
use crate::actor::{Actor, WriteContext};

/// SplitMix64: seeded, so a failure replays from the printed seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

/// Any JSON value, biased toward the shapes the parser reads.
fn value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth > 2 { 7 } else { 9 }) {
        0 => Value::Null,
        1 => json!(rng.below(2) == 0),
        2 => json!(rng.next() as i64),
        3 => json!(-(rng.below(1_000_000) as i64)),
        4 => json!(f64::from(rng.below(1000) as u32) / 7.0),
        5 => json!(""),
        6 => json!(text(rng)),
        7 => Value::Array((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
        _ => {
            let mut map = serde_json::Map::new();
            for _ in 0..rng.below(4) {
                map.insert(rng.pick(&["quote", "event_id", "name", "x"]).to_string(), value(rng, depth + 1));
            }
            Value::Object(map)
        }
    }
}

fn text(rng: &mut Rng) -> String {
    let pieces = ["Aiko", " ", "send the plan", "\u{0}", "é", "😀", "\n", "'; DROP TABLE people; --", "  ", "\u{202e}", "x"];
    (0..rng.below(6)).map(|_| rng.pick(&pieces)).collect()
}

/// One record line: usually record-shaped, sometimes not JSON at all.
fn record(rng: &mut Rng) -> String {
    match rng.below(12) {
        0 => text(rng),
        1 => "{\"type\":\"fact\",".to_string(),
        2 => value(rng, 0).to_string(),
        _ => {
            let mut map = serde_json::Map::new();
            map.insert("type".into(), json!(rng.pick(&["entity", "fact", "tombstone", "manifest", "other"])));
            let fields = [
                ("kind", json!(rng.pick(&["person", "project", "commitment", "open_loop", "event", ""]))),
                ("id", json!(rng.below(5) as i64)),
                ("name", json!(text(rng))),
                ("text", json!(text(rng))),
                ("summary", json!(text(rng))),
                ("status", json!(rng.pick(&["open", "done", "closed", "active", "bogus"]))),
                ("subtype", json!(rng.pick(&["mine", "theirs", "follow_up", "x"]))),
                ("confidence", value(rng, 2)),
                ("valid_from", value(rng, 2)),
                ("updated_at", value(rng, 2)),
                ("due_at", value(rng, 2)),
                ("counterparty_id", json!(rng.below(5) as i64)),
                ("project_id", json!(rng.below(5) as i64)),
                ("aliases", value(rng, 1)),
            ];
            for (k, v) in fields {
                if rng.below(3) != 0 {
                    // Sometimes the right key holds the wrong type.
                    map.insert(k.into(), if rng.below(6) == 0 { value(rng, 1) } else { v });
                }
            }
            if rng.below(2) == 0 {
                let pointers: Vec<Value> = (0..rng.below(4))
                    .map(|_| match rng.below(4) {
                        0 => json!({ "quote": text(rng) }),
                        1 => json!({ "quote": "" }),
                        2 => json!({ "event_id": rng.below(9) }),
                        _ => value(rng, 2),
                    })
                    .collect();
                map.insert("provenance".into(), Value::Array(pointers));
            }
            Value::Object(map).to_string()
        }
    }
}

fn package(rng: &mut Rng) -> String {
    let mut lines = Vec::new();
    match rng.below(10) {
        0 => {}
        1 => lines.push(text(rng)),
        _ => lines.push(json!({ "type": "manifest", "format": FORMAT }).to_string()),
    }
    for _ in 0..rng.below(12) {
        lines.push(record(rng));
        if rng.below(8) == 0 {
            lines.push("   ".into());
        }
    }
    lines.join(if rng.below(4) == 0 { "\r\n" } else { "\n" })
}

fn orphans(conn: &Connection) -> i64 {
    ["people", "projects", "commitments", "open_loops"]
        .iter()
        .map(|table| {
            conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} t WHERE NOT EXISTS
                       (SELECT 1 FROM state_provenance p WHERE p.state_table = '{table}' AND p.state_id = t.id)"
                ),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        })
        .sum()
}

#[test]
fn random_packages_import_or_refuse_cleanly_and_account_for_every_record() {
    let mut conn = crate::open_in_memory().unwrap();
    let ctx = WriteContext::world(Actor::agent("fuzz").unwrap());
    let (mut imported, mut refused) = (0usize, 0usize);
    for seed in 0..400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0xE38);
        let pkg = package(&mut rng);
        let records = pkg.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(1);
        match import_v1(&mut conn, &pkg, &ctx, 1_760_000_000_000) {
            Ok(report) => {
                assert!(is_export_v1(&pkg), "seed {seed}: imported a package that is not an export");
                assert_eq!(
                    report.entities + report.facts + report.tombstones + report.skipped,
                    records,
                    "seed {seed}: {report:?} does not account for {records} records"
                );
                imported += report.entities + report.facts;
            }
            Err(ImportError::NotExportV1) => {
                assert!(!is_export_v1(&pkg), "seed {seed}: refused a real export");
                refused += 1;
            }
            Err(ImportError::NotExportV2) => panic!("seed {seed}: the v1 importer answered with the v2 refusal"),
            Err(ImportError::TooLarge) => panic!("seed {seed}: a small package was TooLarge"),
            Err(ImportError::Db(error)) => panic!("seed {seed}: database error on a malformed package: {error:?}"),
        }
        assert!(conn.is_autocommit(), "seed {seed}: the import left a transaction open");
        assert_eq!(orphans(&conn), 0, "seed {seed}: a state row without provenance");
    }
    // The generator reaches both outcomes: rows really were written, and packages were refused.
    assert!(imported > 20 && refused > 10, "imported {imported}, refused {refused}");
}

/// The preview's parser (`memory.import.preview`) reads the same lines: it must agree with the
/// importer on what a record is, and never panic.
#[test]
fn random_packages_never_panic_the_export_detector() {
    for seed in 0..2_000u64 {
        let mut rng = Rng(seed ^ 0xF00D);
        let pkg = package(&mut rng);
        let _ = is_export_v1(&pkg);
    }
}
