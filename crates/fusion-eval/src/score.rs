//! Row metrics: precision@3, recall@4, MRR, first-hit, irrelevant-button rate, no-op buttons and
//! L3 sends proposed from unreviewed guesses.
//!
//! A button is *relevant* when the scenario lists its target, *neutral* when the target is listed
//! as acceptable or is the generic search (an honest fallback is never noise), and *irrelevant*
//! otherwise.

use crate::runner::Button;
use crate::scenario::Scenario;

#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioScore {
    pub id: String,
    pub lang: String,
    pub hard: bool,
    pub buttons: Vec<Button>,
    /// Relevant ÷ (relevant + irrelevant) among the top three. With nothing judged in the top
    /// three it is 1.0 when nothing was expected and 0.0 when something was.
    pub precision_at_3: f64,
    /// Share of the relevant targets the row shows (up to four).
    pub recall_at_4: Option<f64>,
    /// 1 / rank of the first relevant button; `None` when nothing is expected.
    pub reciprocal_rank: Option<f64>,
    /// Whether the expected lead button leads; `None` when the scenario names none.
    pub first_hit: Option<bool>,
    pub irrelevant: usize,
    pub no_op: usize,
    pub unreviewed_sends: usize,
}

pub fn score(s: &Scenario, buttons: Vec<Button>) -> ScenarioScore {
    let relevant = |b: &Button| s.expect.relevant.iter().any(|r| r == &b.target);
    let neutral =
        |b: &Button| b.target == "generic" || s.expect.acceptable.iter().any(|a| a == &b.target);
    let top3 = &buttons[..buttons.len().min(3)];
    let hits = top3.iter().filter(|b| relevant(b)).count();
    let misses = top3.iter().filter(|b| !relevant(b) && !neutral(b)).count();
    let precision_at_3 = if hits + misses > 0 {
        hits as f64 / (hits + misses) as f64
    } else if s.expect.relevant.is_empty() {
        1.0
    } else {
        0.0
    };
    let reciprocal_rank = (!s.expect.relevant.is_empty()).then(|| {
        buttons
            .iter()
            .position(relevant)
            .map(|i| 1.0 / (i + 1) as f64)
            .unwrap_or(0.0)
    });
    let recall_at_4 = (!s.expect.relevant.is_empty()).then(|| {
        let want = s.expect.relevant.len().min(4);
        let found = s
            .expect
            .relevant
            .iter()
            .filter(|r| buttons.iter().take(4).any(|b| &b.target == *r))
            .count();
        found.min(want) as f64 / want as f64
    });
    let first_hit = s.expect.first.as_ref().map(|f| buttons.first().is_some_and(|b| &b.target == f));
    ScenarioScore {
        id: s.id.clone(),
        lang: s.lang.clone(),
        hard: s.hard,
        irrelevant: buttons.iter().filter(|b| !relevant(b) && !neutral(b)).count(),
        no_op: buttons.iter().filter(|b| b.no_op).count(),
        unreviewed_sends: buttons.iter().filter(|b| b.unreviewed_send).count(),
        precision_at_3,
        recall_at_4,
        reciprocal_rank,
        first_hit,
        buttons,
    }
}

/// Aggregates over a set of scenario scores.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub scenarios: usize,
    pub precision_at_3: f64,
    pub recall_at_4: f64,
    pub mrr: f64,
    pub first_hit_rate: f64,
    pub buttons: usize,
    pub irrelevant: usize,
    pub irrelevant_rate: f64,
    pub no_op: usize,
    pub unreviewed_sends: usize,
}

fn mean(xs: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = xs.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

pub fn summarize<'a>(scores: impl IntoIterator<Item = &'a ScenarioScore> + Clone) -> Summary {
    let all: Vec<&ScenarioScore> = scores.into_iter().collect();
    let buttons: usize = all.iter().map(|s| s.buttons.len()).sum();
    let irrelevant: usize = all.iter().map(|s| s.irrelevant).sum();
    Summary {
        scenarios: all.len(),
        precision_at_3: mean(all.iter().map(|s| s.precision_at_3)),
        recall_at_4: mean(all.iter().filter_map(|s| s.recall_at_4)),
        mrr: mean(all.iter().filter_map(|s| s.reciprocal_rank)),
        first_hit_rate: mean(all.iter().filter_map(|s| s.first_hit.map(|h| if h { 1.0 } else { 0.0 }))),
        buttons,
        irrelevant,
        irrelevant_rate: if buttons == 0 { 0.0 } else { irrelevant as f64 / buttons as f64 },
        no_op: all.iter().map(|s| s.no_op).sum(),
        unreviewed_sends: all.iter().map(|s| s.unreviewed_sends).sum(),
    }
}
