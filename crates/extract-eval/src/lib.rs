//! SHOGUN state-extraction eval (issue #317; docs/extract-eval.md). The model-free slice:
//!
//! - [`golden`]: the SynthDay golden format (`synthday/v1`) and the checked-in days;
//! - [`generate`] + [`templates`]: the deterministic, seeded generator — fixed synthetic templates
//!   only, so real data cannot reach a golden file;
//! - [`conversation`]: a 1,000-turn synthetic agent conversation with ten planted promises, for
//!   the `memory.ingest` → sessionize → extract gate (E5-T2);
//! - [`score`]: per-field precision / recall / F1 with hallucinations and omissions counted
//!   separately, provenance validity, and confidence calibration on the Fusion band edges;
//! - [`runners`]: model-free runners and the adapter from `shogun_memory::extract::Candidate`;
//! - [`lint`]: the "no real data" lint over goldens and replay fixtures;
//! - [`report`]: Markdown rendering (numbers only).
//!
//! The CI gate is `tests/gate.rs` here plus `crates/shogun-core/tests/extract_eval.rs` (the Dream
//! Cycle runners, which need shogun-core's `db` feature).

pub mod conversation;
pub mod generate;
pub mod golden;
pub mod lint;
pub mod report;
pub mod rng;
pub mod runners;
pub mod score;
pub mod templates;
