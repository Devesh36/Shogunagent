//! Timing of the pure half of the action row (matching + ranking) on a mature synthetic memory,
//! against SLO-02's 150ms budget. The SQLite reads are timed in shogun-core's
//! `context_actions_row_build_fits_the_action_slo`. `cargo run -p fusion-eval --release -- bench`
//! prints device-like numbers; the test runs the same loop in the debug build.

use std::time::Instant;

use shogun_fusion::assemble::{assemble, Intent, ScreenContext};
use shogun_fusion::candidates::{
    build_candidates, CommitmentInput, OpenLoopInput, PersonInput, ProjectInput, StateRows,
};

/// 300 people (with an alias and an address each), 300 commitments, 300 loops, 60 projects.
pub fn mature_rows() -> StateRows {
    let people = (1..=300)
        .map(|i| PersonInput {
            id: i,
            name: format!("Person{i} Surname{i}"),
            aliases: vec![format!("handle{i}")],
            emails: vec![format!("person{i}@example.com")],
            confidence: 0.8,
        })
        .collect();
    let commitments = (1..=300)
        .map(|i| CommitmentInput {
            id: 1_000 + i,
            description: format!("Send the quarterly report part {i} to Person{i}"),
            counterparty_id: Some(i),
            due_at: Some(i * 3_600_000),
            status: "open".into(),
            confidence: 0.8,
            ..CommitmentInput::default()
        })
        .collect();
    let open_loops = (1..=300)
        .map(|i| OpenLoopInput {
            id: 2_000 + i,
            kind: "reply_needed".into(),
            description: format!("Reply to Person{i} about invoice INV-{i}"),
            counterparty_id: Some(i),
            counterparty_email: Some(format!("person{i}@example.com")),
            status: "open".into(),
            confidence: 0.8,
            ..OpenLoopInput::default()
        })
        .collect();
    let projects = (1..=60)
        .map(|i| ProjectInput { id: 3_000 + i, name: format!("Codename{i} rollout"), status: "active".into(), confidence: 0.8 })
        .collect();
    StateRows { people, projects, commitments, open_loops }
}

/// A mail thread with a full 1.2k excerpt.
pub fn busy_screen() -> ScreenContext {
    let excerpt = "Person42 Surname42 <person42@example.com> wrote: about invoice INV-42 and the quarterly report. ".repeat(12);
    ScreenContext {
        app_bundle_id: "com.google.Chrome".into(),
        window_title: "Invoice INV-42 - me@example.com - Example Mail".into(),
        website_host: Some("mail.google.com".into()),
        excerpt: excerpt.chars().take(1_200).collect(),
        ..ScreenContext::default()
    }
}

/// `(p50, p95)` in microseconds over `samples` runs (after one warm-up).
pub fn time_row_build(samples: usize) -> (u128, u128) {
    let rows = mature_rows();
    let screen = busy_screen();
    let mut out = Vec::with_capacity(samples);
    for i in 0..=samples {
        let start = Instant::now();
        let states = build_candidates(&screen, &rows, 0);
        let cache = assemble(screen.clone(), &states, "", &Intent::default(), &[]);
        let us = start.elapsed().as_micros();
        if cache.actions.is_empty() {
            return (u128::MAX, u128::MAX);
        }
        if i > 0 {
            out.push(us);
        }
    }
    out.sort_unstable();
    let p = |q: usize| out[(out.len() * q / 100).min(out.len() - 1)];
    (p(50), p(95))
}
