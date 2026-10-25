//! The Context Fusion eval gate (§6.5 acceptance). Runs inside `cargo test --workspace` on Linux
//! CI — no model, no network, synthetic scenarios only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use fusion_eval::scenario::{self, Scenario};

fn scenarios() -> Vec<Scenario> {
    scenario::all().expect("scenarios parse")
}

#[test]
fn scenarios_are_well_formed() {
    let all = scenarios();
    assert!(all.len() >= 30, "the eval covers at least 30 scenarios");
    let ids: BTreeSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids.len(), all.len(), "duplicate scenario ids");
    for s in &all {
        assert!(matches!(s.lang.as_str(), "en" | "ja"), "{}: lang", s.id);
        let mut rows: BTreeSet<&str> = BTreeSet::new();
        for id in s
            .people
            .iter()
            .map(|r| r.id.as_str())
            .chain(s.projects.iter().map(|r| r.id.as_str()))
            .chain(s.commitments.iter().map(|r| r.id.as_str()))
            .chain(s.open_loops.iter().map(|r| r.id.as_str()))
        {
            assert!(rows.insert(id), "{}: duplicate row id {id}", s.id);
        }
        for target in s.expect.relevant.iter().chain(&s.expect.acceptable).chain(&s.expect.first) {
            assert!(
                rows.contains(target.as_str()) || target == "reply_draft" || target == "generic",
                "{}: expected target {target} is not a row",
                s.id
            );
        }
        if let Some(first) = &s.expect.first {
            assert!(s.expect.relevant.contains(first), "{}: first must be relevant", s.id);
        }
        assert!(
            !s.expect.generic || s.expect.relevant.is_empty(),
            "{}: a generic scenario expects nothing relevant",
            s.id
        );
        for c in &s.commitments {
            if let Some(p) = &c.counterparty {
                assert!(s.person(p).is_some(), "{}: counterparty {p}", s.id);
            }
        }
        for l in &s.open_loops {
            if let Some(p) = &l.counterparty {
                assert!(s.person(p).is_some(), "{}: counterparty {p}", s.id);
            }
        }
    }
}

/// Synthetic data only: every address is on a reserved example domain.
#[test]
fn scenarios_use_example_addresses_only() {
    for s in scenarios() {
        for p in &s.people {
            for e in &p.emails {
                let domain = e.rsplit('@').next().unwrap_or_default();
                assert!(
                    domain.split('.').any(|label| label == "example"),
                    "{}: {e} is not on an example domain",
                    s.id
                );
            }
        }
    }
}

#[test]
fn every_scenario_runs() {
    let scores = fusion_eval::evaluate().unwrap();
    assert_eq!(scores.len(), scenarios().len());
}

/// Loose floors on the core set. The baseline at 6251f54a (before the row rework) was
/// P@3 0.52 / MRR 0.92 / 48% irrelevant buttons / 65 no-op buttons / 2 L3 sends from guesses.
#[test]
fn the_core_set_stays_above_its_floors() {
    let scores = fusion_eval::evaluate().unwrap();
    let core: Vec<_> = scores.iter().filter(|s| !s.hard).collect();
    let summary = fusion_eval::score::summarize(core.iter().copied());
    let report = fusion_eval::report::render(&scores);
    assert!(summary.precision_at_3 >= 0.9, "P@3 {:.2}\n{report}", summary.precision_at_3);
    assert!(summary.mrr >= 0.9, "MRR {:.2}\n{report}", summary.mrr);
    assert!(summary.first_hit_rate >= 0.85, "first-hit {:.2}\n{report}", summary.first_hit_rate);
    assert!(summary.irrelevant_rate <= 0.1, "irrelevant {:.2}\n{report}", summary.irrelevant_rate);
}

/// Hard invariants over every scenario, hard ones included: no button is a placeholder, and no
/// unreviewed local guess ever proposes an L3 send (owner decision 2026-10-14).
#[test]
fn no_row_holds_a_no_op_or_a_send_from_an_unreviewed_guess() {
    for s in fusion_eval::evaluate().unwrap() {
        assert_eq!(s.no_op, 0, "{}: {:?}", s.id, s.buttons);
        assert_eq!(s.unreviewed_sends, 0, "{}: {:?}", s.id, s.buttons);
        assert!(s.buttons.len() <= 4, "{}", s.id);
        assert!(!s.buttons.is_empty(), "{}: FR-CF-04 never leaves the row empty", s.id);
    }
}

/// SLO-02: matching + ranking on a mature memory stays far inside the 150ms action budget, even
/// in the unoptimized test build.
#[test]
fn matching_and_ranking_fit_the_action_slo() {
    let (p50, p95) = fusion_eval::bench::time_row_build(30);
    eprintln!("matching + ranking (debug build): p50={p50}us p95={p95}us");
    assert!(p95 < 150_000, "p95 {p95}us");
}

/// §6.5 acceptance: snapshot of the row for representative scenarios (US-01 … US-11 shapes).
/// A change here is a product change — update the snapshot deliberately.
#[test]
fn representative_scenario_snapshots() {
    let expected: &[(&str, &[&str])] = &[
        ("en-gmail-reply-thread", &["l-dana:send_email:L3", "p-dana:local_search:L1"]),
        ("en-slack-dm", &["l-marcus:update_state:L2", "p-marcus:local_search:L1"]),
        ("en-xcode-unrelated", &["generic:local_search:L1"]),
        (
            "en-calendar-event",
            &[
                "c-orion-checklist:update_state:L2",
                "p-dana:local_search:L1",
                "p-marcus:local_search:L1",
                "pr-orion:local_search:L1",
            ],
        ),
        ("en-github-pr", &["l-review:update_state:L2", "p-kenji:local_search:L1"]),
        (
            "en-foreign-language-email",
            &["reply_draft:reply_draft:L2", "l-lucia:send_email:L3", "p-lucia:local_search:L1"],
        ),
        ("en-overdue-elsewhere", &["c-expense:update_state:L2", "generic:local_search:L1"]),
        ("en-snoozed-loop", &["p-priya:local_search:L1"]),
        ("en-unreviewed-mail-guess", &["p-omar:local_search:L1", "l-omar:update_state:L2"]),
        ("ja-mail-tanaka", &["l-tanaka:send_email:L3", "p-tanaka:local_search:L1"]),
        (
            "ja-calendar-event",
            &[
                "c-aoba-report:update_state:L2",
                "p-tanaka:local_search:L1",
                "p-sato:local_search:L1",
                "pr-aoba:local_search:L1",
            ],
        ),
        ("ja-generic-words", &["generic:local_search:L1"]),
    ];
    let scores = fusion_eval::evaluate().unwrap();
    for (id, want) in expected {
        let got = scores.iter().find(|s| s.id == *id).unwrap_or_else(|| panic!("{id} missing"));
        let got: Vec<String> =
            got.buttons.iter().map(|b| format!("{}:{}:{:?}", b.target, b.kind, b.level)).collect();
        assert_eq!(got, *want, "{id}");
    }
}
