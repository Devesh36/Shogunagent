//! Runs a scenario through Context Fusion and names each button it produced.

use shogun_fusion::assemble::{
    assemble_with_priors, ActionCandidate, ContextCache, Intent, PatternPriors, ScreenContext,
};
use shogun_fusion::candidates::{
    build_candidates, CommitmentInput, OpenLoopInput, PersonInput, ProjectInput, StateRows,
};
use shogun_fusion::confidence::POSSIBLY_PREFIX;
use shogun_fusion::{Action, Level, LocalAction};

use crate::scenario::{Scenario, NOW_MS};

/// One button of the produced row, attributed back to the scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    /// A scenario row id, `reply_draft`, `generic`, or `unknown`.
    pub target: String,
    /// `Action::kind()`.
    pub kind: &'static str,
    pub level: Level,
    /// The tap does nothing a person would call the action: it writes a placeholder note or
    /// re-posts text that is already on screen.
    pub no_op: bool,
    /// An L3 send proposed from an unreviewed local guess (#926).
    pub unreviewed_send: bool,
}

/// The screen exactly as the Notch shell hands it to Fusion: app, title, verified host, and the
/// warm excerpt / selection.
pub fn screen_of(s: &Scenario) -> ScreenContext {
    ScreenContext {
        app_bundle_id: s.screen.bundle_id.clone(),
        window_title: s.screen.title.clone(),
        salient: Vec::new(),
        reply_language: s.screen.reply_language.clone(),
        website_host: s.screen.host.clone(),
        excerpt: s.screen.excerpt.clone(),
        selection: s.screen.selection.clone(),
    }
}

/// The scenario's rows as the daemon would read them from memory. Scenario ids become numeric
/// ids by position (people first, then projects, commitments, loops), so ids never collide.
pub fn rows_of(s: &Scenario) -> StateRows {
    let person_id = |id: &str| s.people.iter().position(|p| p.id == id).map(|i| i as i64 + 1);
    let base = s.people.len() as i64 + 1;
    StateRows {
        people: s
            .people
            .iter()
            .enumerate()
            .map(|(i, p)| PersonInput {
                id: i as i64 + 1,
                name: p.name.clone(),
                aliases: p.aliases.clone(),
                emails: p.emails.clone(),
                confidence: p.confidence,
            })
            .collect(),
        projects: s
            .projects
            .iter()
            .enumerate()
            .map(|(i, p)| ProjectInput {
                id: base + i as i64,
                name: p.name.clone(),
                status: p.status.clone(),
                confidence: p.confidence,
            })
            .collect(),
        commitments: s
            .commitments
            .iter()
            .enumerate()
            .map(|(i, c)| CommitmentInput {
                id: 1_000 + i as i64,
                description: c.description.clone(),
                theirs: c.direction == "theirs",
                counterparty_id: c.counterparty.as_deref().and_then(person_id),
                due_at: c.due_in_hours.map(Scenario::at),
                status: c.status.clone(),
                confidence: c.confidence,
                deferred_until: c.snoozed_hours.map(Scenario::at),
            })
            .collect(),
        open_loops: s
            .open_loops
            .iter()
            .enumerate()
            .map(|(i, l)| OpenLoopInput {
                id: 2_000 + i as i64,
                kind: l.kind.clone(),
                description: l.description.clone(),
                counterparty_id: l.counterparty.as_deref().and_then(person_id),
                counterparty_email: l
                    .counterparty
                    .as_deref()
                    .and_then(|id| s.person(id))
                    .and_then(|p| p.emails.first().cloned()),
                status: "open".into(),
                confidence: l.confidence,
                inferred: l.inferred,
                deferred_until: l.snoozed_hours.map(Scenario::at),
            })
            .collect(),
    }
}

/// Assemble the row for a scenario through the same two calls `Db::context_actions` makes.
pub fn run(s: &Scenario) -> ContextCache {
    let screen = screen_of(s);
    let states = build_candidates(&screen, &rows_of(s), NOW_MS);
    assemble_with_priors(screen, &states, "", &Intent::default(), &[], &PatternPriors::default())
}

/// Name each button of a row after the scenario row it came from.
pub fn attribute(s: &Scenario, cache: &ContextCache) -> Vec<Button> {
    cache.actions.iter().map(|c| button(s, c)).collect()
}

fn button(s: &Scenario, c: &ActionCandidate) -> Button {
    let text = c.rationale.strip_prefix(POSSIBLY_PREFIX).unwrap_or(&c.rationale);
    let row = s
        .commitments
        .iter()
        .find(|r| r.description == text)
        .map(|r| (r.id.clone(), false))
        .or_else(|| s.open_loops.iter().find(|r| r.description == text).map(|r| (r.id.clone(), r.inferred)))
        .or_else(|| s.people.iter().find(|r| r.name == text).map(|r| (r.id.clone(), false)))
        .or_else(|| s.projects.iter().find(|r| r.name == text).map(|r| (r.id.clone(), false)));
    let (target, inferred) = match (&c.action, row) {
        (Action::Local(LocalAction::ReplyDraft { .. }), _) => ("reply_draft".to_string(), false),
        (_, Some(found)) => found,
        (Action::Local(LocalAction::LocalSearch { .. } | LocalAction::SaveDraft { .. }), None) => {
            ("generic".to_string(), false)
        }
        _ => ("unknown".to_string(), false),
    };
    let no_op = matches!(
        c.action,
        Action::Local(LocalAction::SaveDraft { .. } | LocalAction::ShowNotification { .. })
    );
    Button {
        target,
        kind: c.action.kind(),
        level: c.level,
        no_op,
        unreviewed_send: inferred && c.action.is_external_send(),
    }
}
