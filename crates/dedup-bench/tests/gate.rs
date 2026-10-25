//! The CI gate for memory repetition.
//!
//! These assertions run on every `cargo test --workspace` (Linux CI). They exist so that a change to
//! the persist path — `extract::persist_candidates_deduped`, `knowledge::persist`, the predicates in
//! `state.rs`, or `memory_identity` — cannot quietly bring back one row per sighting, and cannot
//! "fix" repetition by fusing memories that are genuinely different.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;

use dedup_bench::{run_all, Mechanism, Outcome};

/// Run the corpus once per test binary. Every assertion below reads the same outcomes, and each
/// scenario opens a fresh migrated database, so recomputing per test would multiply the gate's
/// runtime by the number of tests for no additional coverage.
fn outcomes() -> Vec<Outcome> {
    static OUTCOMES: OnceLock<Vec<Outcome>> = OnceLock::new();
    OUTCOMES
        .get_or_init(|| run_all().expect("every scenario runs"))
        .clone()
}

#[test]
fn every_scenario_passes_or_fails_exactly_as_documented() {
    for o in outcomes() {
        assert!(
            o.acceptable(),
            "{} ({}) regressed: {} rows for {} memories, {} extra, {} wrong merges",
            o.id,
            o.mechanism.as_str(),
            o.rows,
            o.expected,
            o.extra_rows,
            o.wrong_merges
        );
    }
}

#[test]
fn a_documented_limitation_that_now_passes_is_reported() {
    // A known limitation that starts passing means the corpus documents a failure that no longer
    // exists. Remove the note rather than letting it hide the next real regression.
    for o in outcomes() {
        if o.known_limitation.is_some() {
            assert!(
                !o.passed(),
                "{} is marked as a known limitation but now passes; drop the note",
                o.id
            );
        }
    }
}

#[test]
fn precision_scenarios_without_a_documented_trade_off_never_wrong_merge() {
    for o in outcomes()
        .into_iter()
        .filter(|o| o.mechanism.is_precision() && o.known_limitation.is_none())
    {
        assert_eq!(
            o.wrong_merges, 0,
            "{}: two distinct memories were fused into one row",
            o.id
        );
    }
}

#[test]
fn controls_and_recaptures_write_one_row_per_memory() {
    for o in outcomes()
        .into_iter()
        .filter(|o| matches!(o.mechanism, Mechanism::Control | Mechanism::Recapture))
    {
        assert_eq!(o.extra_rows, 0, "{}: the existing dedup regressed", o.id);
    }
}

#[test]
fn every_memory_in_the_corpus_is_extracted() {
    // A scenario whose memory never reaches a row measures nothing about dedup. This catches a
    // change to the extractor's cues silently emptying a scenario.
    for o in outcomes() {
        assert_eq!(
            o.found, o.expected,
            "{}: {} of {} memories were never extracted",
            o.id, o.found, o.expected
        );
    }
}

#[test]
fn overall_repetition_stays_under_the_ceiling() {
    // Measured 2026-10-09: 1.06 rows per memory (2.18 without the identity fix). The ceiling
    // sits just above the current value, so any new repetition mechanism fails here even if each
    // scenario on its own still looks acceptable.
    let report = dedup_bench::BenchReport::build(outcomes(), Vec::new());
    let memories = report.repetition_memories as f64;
    let rows_per_memory = (memories + report.repetition_extra_rows as f64) / memories;
    assert!(
        rows_per_memory <= 1.10,
        "repetition rose to {rows_per_memory:.2} rows per memory"
    );
}
