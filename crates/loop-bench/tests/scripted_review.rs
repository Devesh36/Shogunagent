//! The scripted-review arm (E49) end to end: the engine's hourly review and loop consolidator,
//! answered by the offline script, take local candidates to the actionable band (>= 0.5) only
//! through exact quotes, and close items only through the outcomes their thread reports. The
//! local-rules arm on the same stream stays below the band (owner decision 2026-10-02). Floors,
//! not targets: the published numbers are in docs/benchmarks/open-loop-tracking.md.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use loop_bench::adapter::{ShogunAgentAdapter, SystemAdapter};
use loop_bench::gen::{generate, Tier};
use loop_bench::harness::run_one;
use loop_bench::scripted::{ScriptedReviewAdapter, SCRIPTED_CONFIDENCE};

#[test]
fn local_rules_alone_never_reach_the_actionable_band() {
    let data = generate(Tier::Medium, 42);
    let mut adapter = ShogunAgentAdapter::default();
    let run = run_one(&mut adapter, &data).unwrap();
    let actionable = &run.points["actionable"].1;
    assert_eq!(actionable.predicted_commitments + actionable.predicted_open_loops, 0, "{actionable:?}");
    // But they do close and move their own candidates from the thread's later messages.
    let all = &run.points["all"].0;
    assert!(all.resolution_accuracy.unwrap() > 0.5, "{all:?}");
    assert!(all.supersession_accuracy.unwrap() > 0.5, "{all:?}");
    assert_eq!(all.false_closure_rate, Some(0.0));
}

#[test]
fn the_scripted_review_reaches_the_band_through_quotes_and_resolves_through_the_consolidator() {
    let data = generate(Tier::Medium, 42);
    let mut adapter = ScriptedReviewAdapter::default();
    let run = run_one(&mut adapter, &data).unwrap();
    assert_eq!(run.usage.model_calls, 0, "a script, not a model");
    assert_eq!(adapter.stats().ticks_failed, 0, "{:?}", adapter.stats());
    assert!(adapter.consolidations > 0);
    let applied = adapter.applied;
    assert!(applied.opened + applied.relisted > 0, "{applied:?}");
    assert!(applied.resolved > 0, "outcomes of reviewed rows go through the consolidator: {applied:?}");
    let actionable = &run.points["actionable"].0;
    assert!(actionable.tracking_f1.unwrap() > 0.5, "{actionable:?}");
    assert!(actionable.resolution_accuracy.unwrap() > 0.5, "{actionable:?}");
    assert_eq!(actionable.provenance_validity, Some(1.0));
    // Every actionable row carries the consolidator's confidence, never a local rule's.
    let predictions = adapter.snapshot(data.gold.end_ms).unwrap();
    for p in predictions.iter().filter(|p| p.confidence >= 0.5) {
        assert!((p.confidence - SCRIPTED_CONFIDENCE).abs() < 1e-9, "{p:?}");
    }
    assert!(adapter.lane().window_calls.load(std::sync::atomic::Ordering::Relaxed) > 0);
}
