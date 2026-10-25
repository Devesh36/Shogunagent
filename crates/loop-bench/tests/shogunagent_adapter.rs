//! The ShogunAgent adapter end to end on one easy stream: it runs without a model, every row it
//! reports cites an exact span of a dataset event, and it finds planted commitments. A floor,
//! not a target: the published numbers are in docs/benchmarks/open-loop-tracking.md.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use loop_bench::adapter::{ShogunAgentAdapter, SystemAdapter};
use loop_bench::gen::{generate, Tier};
use loop_bench::harness::run_one;

#[test]
fn the_engine_on_local_rules_tracks_with_valid_provenance_and_no_model() {
    let data = generate(Tier::Easy, 42);
    let mut adapter = ShogunAgentAdapter::default();
    let run = run_one(&mut adapter, &data).unwrap();
    let all = &run.points["all"].0;
    assert_eq!(run.usage.model_calls, 0);
    assert!(adapter.stats.ticks_ran > 0, "{:?}", adapter.stats);
    assert_eq!(adapter.stats.ticks_failed, 0);
    assert!(all.commitment_recall.unwrap() > 0.0, "{all:?}");
    assert_eq!(all.provenance_validity, Some(1.0), "{all:?}");
    assert!(adapter.info().model.is_none());
}
