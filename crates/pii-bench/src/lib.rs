//! SHOGUN PII / secret redaction benchmark.
//!
//! The audit in `research/pii-benchmark` established what the redactor *is* by reading it. This
//! measures what actually **survives**, end to end, through the real write path — the coverage the
//! audit found missing (PII-004) and the reason the `window_title` bypass (PII-001) went unnoticed
//! for as long as it did.
//!
//! Two design rules, both learned from the audit:
//!
//! 1. **Every case runs through every boundary.** A case is a `(payload, column)` pair, not just a
//!    payload. Unit tests of the detector can never catch "the redactor is correct and simply is
//!    not called on this column"; parameterising over columns is exactly what does.
//! 2. **Survival is checked where the data lands, not where it was sent.** A row is read back from
//!    SQLite and the FTS index is queried, because the FTS trigger copies columns independently and
//!    a body that was masked tells you nothing about the title beside it.
//!
//! Every payload here is synthetic. No real credential appears anywhere in this crate.

pub mod corpus;
pub mod harness;
pub mod report;

pub use corpus::{Case, Category, Expectation, CASES};
pub use harness::{run_all, Boundary, Outcome};
pub use report::{BenchReport, CategoryScore};
