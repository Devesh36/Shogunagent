//! Memory rows → Fusion candidates for the focused screen (FR-CF-01/03).
//!
//! The daemon reads people / projects / commitments / open loops from SQLite and hands them here
//! as plain views; `crates/fusion-eval` builds the same views from synthetic scenarios. Everything
//! that decides *whether a row belongs on this screen* lives here, once:
//!
//! - **Relevance** is a real match of the row against the screen ([`crate::matching`]): a person
//!   by name / alias / address, a project by name, a commitment or loop when its counterparty is
//!   on screen, it names someone who is, or its own keywords are (two plain ones, or one
//!   distinctive one — an identifier, a proper noun, a longer CJK term). `1.0` when matched,
//!   `0.5` for a single plain keyword (not enough on its own), `0.0` otherwise.
//! - **Urgency** is a due date: overdue, or due within 24 hours ([`urgency_of`]).
//! - Snoozed rows (`deferred_until` in the future), finished rows and done projects are not
//!   candidates at all.
//!
//! Pure: no clock, no I/O. The caller passes `now_ms`.

use std::collections::HashMap;

use crate::assemble::{ScreenContext, StateCandidate, StateKind, StateRef};
use crate::matching::{keywords, person_needles, project_needles, screen_haystack, Haystack};

/// A person row as Fusion needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonInput {
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
    pub confidence: f64,
}

/// A project row as Fusion needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectInput {
    pub id: i64,
    pub name: String,
    /// `active` / `waiting` / `paused` / `done`.
    pub status: String,
    pub confidence: f64,
}

/// A commitment row as Fusion needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitmentInput {
    pub id: i64,
    pub description: String,
    /// Owed to me (`direction = 'theirs'`).
    pub theirs: bool,
    pub counterparty_id: Option<i64>,
    pub due_at: Option<i64>,
    /// `open` / `overdue` / `done` / `cancelled`.
    pub status: String,
    pub confidence: f64,
    pub deferred_until: Option<i64>,
}

/// An open-loop row as Fusion needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenLoopInput {
    pub id: i64,
    /// `reply_needed` / `waiting_on_them` / …
    pub kind: String,
    pub description: String,
    pub counterparty_id: Option<i64>,
    pub counterparty_email: Option<String>,
    /// `open` / `closed`.
    pub status: String,
    pub confidence: f64,
    /// An unreviewed local mail guess (#926).
    pub inferred: bool,
    pub deferred_until: Option<i64>,
}

/// Everything the row is assembled from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateRows {
    pub people: Vec<PersonInput>,
    pub projects: Vec<ProjectInput>,
    pub commitments: Vec<CommitmentInput>,
    pub open_loops: Vec<OpenLoopInput>,
}

/// A row matched against the screen.
pub const MATCHED: f64 = 1.0;
/// One plain keyword in common: a hint, never enough on its own.
pub const WEAK: f64 = 0.5;

/// Urgency of an overdue item (plus up to 0.05 for how long it has been overdue, capped at a week).
pub const URGENCY_OVERDUE: f64 = 0.35;
/// Urgency of an item due within [`DUE_SOON_MS`] (plus up to 0.05 the closer it is).
pub const URGENCY_DUE_SOON: f64 = 0.25;
pub const DUE_SOON_MS: i64 = 24 * 60 * 60 * 1000;
const WEEK_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// The urgency term of FR-CF-03: overdue (by status or by a past due date) or due within 24 hours.
/// Zero for anything else, including undated rows.
pub fn urgency_of(due_at: Option<i64>, status: &str, now_ms: i64) -> f64 {
    let overdue = status == "overdue" || due_at.is_some_and(|d| d < now_ms);
    if overdue {
        let age = due_at.map_or(0.0, |d| (now_ms - d).max(0) as f64);
        return URGENCY_OVERDUE + 0.05 * (age / WEEK_MS).min(1.0);
    }
    match due_at {
        Some(d) if d - now_ms <= DUE_SOON_MS => {
            URGENCY_DUE_SOON + 0.05 * (1.0 - (d - now_ms) as f64 / DUE_SOON_MS as f64)
        }
        _ => 0.0,
    }
}

fn open_loop_kind(kind: &str) -> StateKind {
    match kind {
        "reply_needed" => StateKind::OpenLoopReplyNeeded,
        "waiting_on_them" => StateKind::OpenLoopWaiting,
        _ => StateKind::OpenLoopOther,
    }
}

/// How a commitment / loop description relates to the screen.
fn text_relevance(
    description: &str,
    counterparty: Option<i64>,
    hay: &Haystack,
    on_screen: &HashMap<i64, Vec<String>>,
) -> f64 {
    if counterparty.is_some_and(|id| on_screen.contains_key(&id)) {
        return MATCHED;
    }
    // A description that names someone who is on screen ("Send the deck to Dana").
    let own = Haystack::new([description]);
    if on_screen.values().flatten().any(|n| own.contains_name(n)) {
        return MATCHED;
    }
    let mut hits = 0usize;
    let mut distinctive = false;
    for k in keywords(description) {
        if hay.contains(&k.text) {
            distinctive |= k.distinctive;
            hits += 1;
        }
    }
    let by_keywords = match hits {
        0 => 0.0,
        1 if !distinctive => WEAK,
        _ => MATCHED,
    };
    // The row belongs to someone and the screen is about someone else: shared topic words do not
    // make it this screen's row ("Reply to Marcus about the renewal" on Dana's renewal thread).
    if counterparty.is_some() && !on_screen.is_empty() {
        return by_keywords.min(WEAK);
    }
    by_keywords
}

/// Build the Fusion candidates for `screen` from `rows`. Snoozed and finished rows are skipped;
/// every other row comes back with its relevance and urgency so [`crate::assemble`] can apply the
/// floor, the confidence gate and the ranking.
pub fn build_candidates(screen: &ScreenContext, rows: &StateRows, now_ms: i64) -> Vec<StateCandidate> {
    let hay = screen_haystack(
        &screen.window_title,
        &screen.app_bundle_id,
        screen.website_host.as_deref(),
        &screen.excerpt,
        screen.selection.as_deref(),
        &screen.salient,
    );
    let names: HashMap<i64, &str> = rows.people.iter().map(|p| (p.id, p.name.as_str())).collect();
    let mut on_screen: HashMap<i64, Vec<String>> = HashMap::new();
    let mut out = Vec::with_capacity(rows.people.len() + rows.projects.len() + rows.commitments.len() + rows.open_loops.len());

    for p in &rows.people {
        let needles = person_needles(&p.name, &p.aliases, &p.emails);
        let matched = !hay.is_empty() && needles.iter().any(|n| hay.contains_name(n));
        // Only a person who could inform an action marks the screen as theirs.
        if matched && crate::confidence::may_inform_action(p.confidence) {
            on_screen.insert(p.id, needles);
        }
        out.push(StateCandidate {
            kind: StateKind::Person,
            summary: p.name.clone(),
            subject: p.name.clone(),
            confidence: p.confidence,
            relevance: if matched { MATCHED } else { 0.0 },
            recipient: None,
            inferred: false,
            urgency: 0.0,
            state: None,
            counterparty: None,
        });
    }
    for p in &rows.projects {
        if p.status == "done" {
            continue;
        }
        let matched = !hay.is_empty() && project_needles(&p.name).iter().any(|n| hay.contains(n));
        out.push(StateCandidate {
            kind: StateKind::Project,
            summary: p.name.clone(),
            subject: p.name.clone(),
            confidence: p.confidence,
            relevance: if matched { MATCHED } else { 0.0 },
            recipient: None,
            inferred: false,
            urgency: 0.0,
            state: None,
            counterparty: None,
        });
    }
    for c in &rows.commitments {
        if !matches!(c.status.as_str(), "open" | "overdue") || c.deferred_until.is_some_and(|t| t > now_ms) {
            continue;
        }
        out.push(StateCandidate {
            kind: if c.theirs { StateKind::CommitmentTheirs } else { StateKind::CommitmentMine },
            summary: c.description.clone(),
            subject: c.description.clone(),
            confidence: c.confidence,
            relevance: text_relevance(&c.description, c.counterparty_id, &hay, &on_screen),
            recipient: None,
            inferred: false,
            urgency: urgency_of(c.due_at, &c.status, now_ms),
            state: Some(StateRef { table: "commitments", id: c.id }),
            counterparty: c.counterparty_id.and_then(|id| names.get(&id)).map(|n| n.to_string()),
        });
    }
    for l in &rows.open_loops {
        if l.status != "open" || l.deferred_until.is_some_and(|t| t > now_ms) {
            continue;
        }
        out.push(StateCandidate {
            kind: open_loop_kind(&l.kind),
            summary: l.description.clone(),
            subject: l.description.clone(),
            confidence: l.confidence,
            relevance: text_relevance(&l.description, l.counterparty_id, &hay, &on_screen),
            // #764: a reply is only offered to a real address (assemble checks it).
            recipient: l.counterparty_email.clone(),
            inferred: l.inferred,
            urgency: 0.0,
            state: Some(StateRef { table: "open_loops", id: l.id }),
            counterparty: l.counterparty_id.and_then(|id| names.get(&id)).map(|n| n.to_string()),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000_000;
    const H: i64 = 3_600_000;

    fn screen(title: &str, excerpt: &str) -> ScreenContext {
        ScreenContext {
            app_bundle_id: "com.apple.mail".into(),
            window_title: title.into(),
            excerpt: excerpt.into(),
            ..ScreenContext::default()
        }
    }

    fn rel(c: &[StateCandidate], summary: &str) -> f64 {
        c.iter().find(|s| s.summary == summary).map_or(-1.0, |s| s.relevance)
    }

    #[test]
    fn urgency_is_overdue_or_due_within_a_day() {
        assert!(urgency_of(Some(NOW - H), "open", NOW) >= URGENCY_OVERDUE);
        assert!(urgency_of(None, "overdue", NOW) >= URGENCY_OVERDUE);
        let soon = urgency_of(Some(NOW + 3 * H), "open", NOW);
        assert!((URGENCY_DUE_SOON..URGENCY_OVERDUE).contains(&soon));
        assert_eq!(urgency_of(Some(NOW + 30 * H), "open", NOW), 0.0);
        assert_eq!(urgency_of(None, "open", NOW), 0.0);
        // Longer overdue is a little more urgent, never past the cap.
        assert!(urgency_of(Some(NOW - 100 * H), "overdue", NOW) > urgency_of(Some(NOW - H), "overdue", NOW));
        assert!(urgency_of(Some(NOW - 10_000 * H), "overdue", NOW) <= URGENCY_OVERDUE + 0.05 + 1e-9);
    }

    #[test]
    fn a_counterparty_on_screen_grounds_their_rows() {
        let rows = StateRows {
            people: vec![PersonInput { id: 1, name: "田中 太郎".into(), confidence: 0.9, ..Default::default() }],
            open_loops: vec![OpenLoopInput {
                id: 7,
                kind: "reply_needed".into(),
                description: "見積の件で返信".into(),
                counterparty_id: Some(1),
                status: "open".into(),
                confidence: 0.9,
                ..Default::default()
            }],
            ..Default::default()
        };
        let got = build_candidates(&screen("Re: ご相談", "田中さんからのメール"), &rows, NOW);
        assert_eq!(rel(&got, "田中 太郎"), MATCHED);
        assert_eq!(rel(&got, "見積の件で返信"), MATCHED);
        let loop_ = got.iter().find(|s| s.kind == StateKind::OpenLoopReplyNeeded).unwrap();
        assert_eq!(loop_.counterparty.as_deref(), Some("田中 太郎"));
        assert_eq!(loop_.state, Some(StateRef { table: "open_loops", id: 7 }));
    }

    #[test]
    fn generic_words_alone_never_ground_a_row() {
        let rows = StateRows {
            commitments: vec![
                CommitmentInput {
                    id: 1,
                    description: "Renew the Google Workspace billing".into(),
                    status: "open".into(),
                    confidence: 0.9,
                    ..Default::default()
                },
                CommitmentInput {
                    id: 2,
                    description: "契約内容を確認して対応する".into(),
                    status: "open".into(),
                    confidence: 0.9,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut s = screen("New Tab - Google Chrome", "確認方法と対応状況");
        s.app_bundle_id = "com.google.Chrome".into();
        let got = build_candidates(&s, &rows, NOW);
        assert_eq!(rel(&got, "Renew the Google Workspace billing"), 0.0);
        assert_eq!(rel(&got, "契約内容を確認して対応する"), 0.0);
    }

    #[test]
    fn snoozed_and_finished_rows_are_not_candidates() {
        let rows = StateRows {
            commitments: vec![
                CommitmentInput { id: 1, description: "snoozed".into(), status: "open".into(), confidence: 0.9, deferred_until: Some(NOW + H), ..Default::default() },
                CommitmentInput { id: 2, description: "woke up".into(), status: "open".into(), confidence: 0.9, deferred_until: Some(NOW - H), ..Default::default() },
                CommitmentInput { id: 3, description: "done".into(), status: "done".into(), confidence: 0.9, ..Default::default() },
            ],
            open_loops: vec![OpenLoopInput { id: 4, kind: "other".into(), description: "closed".into(), status: "closed".into(), confidence: 0.9, ..Default::default() }],
            projects: vec![ProjectInput { id: 5, name: "Shipped".into(), status: "done".into(), confidence: 0.9 }],
            ..Default::default()
        };
        let got = build_candidates(&screen("x", ""), &rows, NOW);
        let summaries: Vec<&str> = got.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(summaries, vec!["woke up"]);
    }

    #[test]
    fn one_plain_keyword_is_weak_two_or_one_distinctive_is_a_match() {
        let rows = StateRows {
            commitments: vec![
                CommitmentInput { id: 1, description: "Share the checklist".into(), status: "open".into(), confidence: 0.9, ..Default::default() },
                CommitmentInput { id: 2, description: "Finish the launch checklist".into(), status: "open".into(), confidence: 0.9, ..Default::default() },
                CommitmentInput { id: 3, description: "Publish the Orion changelog".into(), status: "open".into(), confidence: 0.9, ..Default::default() },
            ],
            ..Default::default()
        };
        let got = build_candidates(&screen("Weekly sync", "Agenda: launch checklist, Orion"), &rows, NOW);
        assert_eq!(rel(&got, "Share the checklist"), WEAK);
        assert_eq!(rel(&got, "Finish the launch checklist"), MATCHED);
        assert_eq!(rel(&got, "Publish the Orion changelog"), MATCHED);
    }
}
