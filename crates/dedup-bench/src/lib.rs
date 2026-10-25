//! SHOGUN memory repetition benchmark.
//!
//! Measures how many state rows one real memory becomes — a commitment, an open loop, a person, a
//! project — when the same thing reaches the log more than once, and how often two distinct
//! memories are wrongly fused into one row.
//!
//! Both directions are measured because they trade against each other, and the product has chosen
//! a side: `knowledge.rs` opens with "exact context keys avoid fuzzy same-name merging", and
//! `state_split.rs` exists because wrong merges happen. A dedup change that lowers the row count by
//! fusing two senders' promises is a regression, not a fix, so every run reports wrong merges next
//! to repetition and the precision scenarios fail on them.
//!
//! Why a new crate rather than `extract-eval`: that scorer collapses predictions by normalised key
//! *before* scoring, on the stated assumption that "the product keeps one row per key". It measures
//! extraction quality and, by design, cannot see repetition in the product's persist path. This
//! crate measures exactly that path.

pub mod harness;
pub mod report;
pub mod scenario;

pub use harness::{
    match_cost, run, run_all, sentence_key, CostLayout, CostPoint, LabelOutcome, Outcome, TOPICS,
};
pub use report::{BenchReport, MechanismScore};
pub use scenario::{scenarios, Mechanism, Scenario, Step, Table, Truth};
