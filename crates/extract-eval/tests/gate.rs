//! The extraction-eval CI gate for this crate (issue #317; docs/extract-eval.md). Runs inside
//! `cargo test --workspace` on Linux CI — no workflow change, no model, no network.
//!
//! 1. The golden set is what the generator produces, byte for byte, and passes the no-real-data
//!    lint: real data cannot enter by construction.
//! 2. The golden set is internally consistent: an oracle that predicts exactly the gold rows
//!    scores 100% and every gold text row is supported by each of its evidence events.
//! 3. The inline ingress runner (`extract_untrusted`) stays above loose floors.
//!
//! The Dream Cycle runners (LocalRuleClassifier, Batch replay) are gated in
//! `crates/shogun-core/tests/extract_eval.rs`, which needs shogun-core's `db` feature.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use extract_eval::golden::{self, BandLabel, SynthDay, Table};
use extract_eval::score::{score_day, RawPrediction, Score, Support};
use extract_eval::{generate, lint, report, runners, templates};

fn days() -> Vec<SynthDay> {
    golden::golden_days().expect("golden days parse")
}

#[test]
fn goldens_are_exactly_what_the_generator_produces() {
    assert_eq!(golden::GOLDEN_FILES.len(), generate::GOLDEN_SPECS.len());
    for (spec, (name, checked_in)) in generate::GOLDEN_SPECS.iter().zip(golden::GOLDEN_FILES) {
        assert_eq!(spec.id, *name);
        let day = generate::generate(spec).expect("spec generates");
        let fresh = day.to_json().expect("serialises");
        assert!(
            fresh == *checked_in,
            "goldens/{name}.json differs from the generator. Golden files are generated, never \
             hand-edited: change src/templates.rs or src/generate.rs, then run \
             `cargo run -p extract-eval -- regen` and commit the result."
        );
    }
}

#[test]
fn goldens_pass_the_no_real_data_lint() {
    for day in days() {
        let findings = lint::lint_day(&day);
        assert!(findings.is_empty(), "{}: {findings:?}", day.id);
    }
}

#[test]
fn goldens_are_well_formed() {
    for day in days() {
        assert_eq!(day.format, golden::FORMAT);
        assert_eq!(day.generator.template_set, templates::TEMPLATE_SET);
        let ids: BTreeSet<i64> = day.events.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), day.events.len(), "{}: duplicate event ids", day.id);
        assert!(day.events.windows(2).all(|w| w[0].ts_ms < w[1].ts_ms), "{}: events out of order", day.id);
        let gold_ids: BTreeSet<&str> = day.gold.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(gold_ids.len(), day.gold.len(), "{}: duplicate gold ids", day.id);
        for key in &day.generator.required_templates {
            assert!(day.events.iter().any(|e| &e.template == key), "{}: required {key} missing", day.id);
        }
        for g in &day.gold {
            assert!(!g.evidence.is_empty(), "{}:{} has no evidence", day.id, g.id);
            assert!(g.fields.contains_key(g.table.key_field()), "{}:{} has no key", day.id, g.id);
            for f in g.fields.keys() {
                assert!(g.table.fields().contains(&f.as_str()), "{}:{} unknown field", day.id, g.id);
            }
            // A trap template exists to produce no commitment or open loop.
            if matches!(g.table, Table::Commitments | Table::OpenLoops) {
                for ev in &g.evidence {
                    let t = day.event(*ev).and_then(|e| templates::template(&e.template));
                    assert!(t.is_some_and(|t| !t.is_trap()), "{}:{} cites a trap", day.id, g.id);
                }
            }
        }
    }
    // Every template is exercised by at least one golden day.
    let used: BTreeSet<String> = days().iter().flat_map(|d| d.events.iter().map(|e| e.template.clone())).collect();
    let unused: Vec<&str> = templates::TEMPLATES.iter().map(|t| t.key).filter(|k| !used.contains(*k)).collect();
    assert!(unused.is_empty(), "templates no golden day uses: {unused:?}");
}

fn oracle_confidence(b: BandLabel) -> f64 {
    match b {
        BandLabel::Low => 0.3,
        BandLabel::Medium => 0.6,
        BandLabel::High => 0.9,
    }
}

/// Predict every gold row exactly, citing every evidence event.
fn oracle(day: &SynthDay, tables: &[Table]) -> Vec<RawPrediction> {
    let mut out = Vec::new();
    for g in day.gold.iter().filter(|g| tables.contains(&g.table)) {
        let key = g.fields.get(g.table.key_field()).cloned().unwrap_or_default();
        for ev in &g.evidence {
            out.push(RawPrediction {
                table: g.table,
                fields: g.fields.clone(),
                confidence: oracle_confidence(g.expected_band),
                event_id: *ev,
                support: Support::Text(key.clone()),
            });
        }
    }
    out
}

#[test]
fn an_oracle_scores_perfectly_and_gold_text_rows_are_supported() {
    let mut all = Score::default();
    let mut text = Score::default();
    for day in days() {
        all.merge(&score_day(&day, &oracle(&day, &Table::ALL), 0, runners::stored_text));
        text.merge(&score_day(&day, &oracle(&day, &[Table::Commitments, Table::OpenLoops]), 0, runners::stored_text));
    }
    let i = all.items_total();
    assert_eq!((i.hallucinated, i.omitted), (0, 0), "oracle items");
    assert_eq!(all.fields_total().f1(), Some(1.0), "oracle fields");
    assert_eq!(all.overconfident() + all.underconfident(), 0, "oracle bands");
    assert_eq!(all.band_promise_violations(), vec![]);
    // Every commitment / open loop is literally supported by each event it cites.
    assert_eq!(text.provenance.validity(), Some(1.0), "gold text rows must be supported by their evidence");
}

#[test]
fn extract_untrusted_gate() {
    let mut s = Score::default();
    for day in days() {
        s.merge(&runners::score(&day, &runners::run_extract_untrusted(&day)));
    }
    println!("{}", report::markdown("extract_untrusted (inline ingress, local rules)", &s));

    // Loose floors (retrieval_eval.rs style): catch a broken pipeline, not a point of F1.
    assert!(s.item(Table::Commitments).recall().unwrap_or(0.0) >= 0.70, "commitment recall");
    assert!(s.item(Table::Commitments).precision().unwrap_or(0.0) >= 0.55, "commitment precision");
    assert!(s.item(Table::OpenLoops).recall().unwrap_or(0.0) >= 0.70, "open-loop recall");
    assert!(s.item(Table::OpenLoops).precision().unwrap_or(0.0) >= 0.60, "open-loop precision");
    assert!(s.field(Table::Commitments, "due").precision().unwrap_or(0.0) >= 0.70, "due precision");
    assert_eq!(s.provenance.validity(), Some(1.0), "local-rule provenance must be exact");
    // The hidden-character repro: a ZWSP inside "I'll" must not hide the promise at ingress.
    let zwsp = s.by_template.get("zwsp_commit").copied().unwrap_or_default();
    assert!(zwsp.gold > 0 && zwsp.matched == zwsp.gold, "hidden character hid a promise");
    // N7: a Japanese noun inside an English sentence must not hide the English cue.
    for key in ["mix_commit_mine", "mix_commit_theirs", "mix_waiting"] {
        let mixed = s.by_template.get(key).copied().unwrap_or_default();
        assert!(mixed.gold > 0 && mixed.matched == mixed.gold, "{key}: mixed-language row missed");
    }
    // FR-ST-20: local rules stay under the Medium edge.
    assert_eq!(s.calibration.keys().copied().collect::<Vec<_>>(), vec![BandLabel::Low]);
}
