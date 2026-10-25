//! Dream Cycle orchestration logic (§6.7). The pure, Linux-testable core of the nightly
//! batch: the run-condition [`gate`], the resumable job [`plan`], and the Batch-API failure
//! [`health`] escalation. The job *effects* (Batch-API consolidation, Warm→Cold demotion,
//! Morning-Brief generation) are I/O the daemon runs — they live behind seams, not here — so the
//! scheduling/idempotency rules that must never regress stay unit-tested without a Mac or a network.
//!
//! Invariants this module upholds:
//! - LLM work is Batch-API/Select-KK only (invariant 5) — the plan never routes a job to the Agent
//!   lane; that lane isn't reachable from here.
//! - Batch-API failure never degrades local features (FR-DC-05, see [`health::local_features_blocked`]).

pub mod actions;
pub mod gate;
pub mod health;
#[cfg(feature = "db")]
pub mod entities;
#[cfg(feature = "db")]
pub mod loops;
#[cfg(feature = "db")]
pub mod hourly;
#[cfg(feature = "db")]
pub mod pages;
pub mod outcome;
pub mod plan;
pub mod prefilter;
#[cfg(feature = "db")]
pub mod session;
pub mod sleep_wake;
#[cfg(feature = "db")]
pub mod run;
#[cfg(feature = "db")]
pub mod jobs;
#[cfg(feature = "db")]
pub mod schedule;
