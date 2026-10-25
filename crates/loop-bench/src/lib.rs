//! Open-loop and commitment tracking benchmark v1 (ShogunAgent plan E28,
//! `docs/benchmarks/open-loop-tracking.md`).
//!
//! - [`gen`]: the deterministic stream generator and the gold file;
//! - [`score`]: the scorer (recall / precision per kind, resolution, supersession, distractors,
//!   provenance validity);
//! - [`adapter`]: the system adapter trait and the shipped adapters (ShogunAgent on local rules,
//!   the "will" baseline, the oracle);
//! - [`scripted`]: the scripted-review arm (E49): the engine's Batch lane answered by an offline,
//!   deterministic script instead of a model, to measure the path to the actionable band;
//! - [`harness`]: the runner and the results file (mean and 95% interval over seeds).
//!
//! No model, no network: every number this crate produces is exact arithmetic over a synthetic
//! stream, so anyone can reproduce it from the seeds.

pub mod adapter;
pub mod gen;
pub mod harness;
pub mod score;
pub mod scripted;
