//! SHOGUN Context Fusion eval (§6.5 acceptance: representative-scenario snapshot tests).
//!
//! The proactive action row is scored offline against synthetic scenarios — a focused screen
//! (bundle id, website host, window title, a bounded text excerpt / selection) plus the memory
//! rows that exist at that moment (people, projects, commitments, open loops with their
//! confidences, unreviewed-guess flags and snoozes) — in English and Japanese.
//!
//! - [`scenario`]: the checked-in scenario format (`scenarios/*.json`);
//! - [`runner`]: runs a scenario through Fusion and attributes each button to a scenario row;
//! - [`score`]: precision@3, recall@4, MRR, first-hit, irrelevant-button rate, no-op buttons, and
//!   L3 sends proposed from unreviewed guesses;
//! - [`report`]: Markdown (numbers and synthetic ids only).
//!
//! The CI gate is `tests/gate.rs`; `cargo run -p fusion-eval` prints the report.

pub mod bench;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod score;

/// Score every scenario through the current pipeline.
pub fn evaluate() -> Result<Vec<score::ScenarioScore>, String> {
    Ok(scenario::all()?
        .iter()
        .map(|s| {
            let cache = runner::run(s);
            score::score(s, runner::attribute(s, &cache))
        })
        .collect())
}
