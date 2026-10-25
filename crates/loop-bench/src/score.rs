//! The scorer: a system's end-of-stream state against the gold file.
//!
//! **Matching.** A prediction and a gold item of the same kind match when the prediction cites
//! the item's opening event (or one of its update events), or when the prediction's text is
//! close to the item's (token F1 ≥ [`MATCH_F1`] against the canonical text or the quote, stop
//! words removed). Pairs are taken greedily, best score first, one-to-one: a second row for the
//! same item is an unmatched prediction (a duplicate costs precision).
//!
//! **Metrics** (each a share in `[0, 1]`; `None` when its denominator is empty — never a
//! fabricated zero):
//! - commitment / open-loop recall and precision, and their F1;
//! - resolution accuracy: of the items resolved or cancelled in the stream, the share matched
//!   **and** read as closed (end to end: an unmatched item counts as not closed);
//! - false-closure rate: of the matched items still open in the stream, the share read as closed;
//! - supersession accuracy: of the items whose due date moved, the share matched **and** read
//!   with the latest due date (same UTC day);
//! - false-commitment rate: of the distractors, the share that produced an unmatched prediction
//!   (citing the distractor's event, or with text close to it);
//! - provenance validity: of the predictions, the share whose every cited quote is non-empty
//!   and an exact substring of the cited event's content (a prediction citing nothing fails).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::gen::{Dataset, Event, FinalStatus, ItemKind, DAY_MS};

/// Token F1 at or above which a prediction's text matches an item or hits a distractor.
pub const MATCH_F1: f64 = 0.5;

/// Whether a predicted row is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictedStatus {
    Open,
    Closed,
}

/// One cited source: a dataset event id and the exact span the system holds as proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub event_id: String,
    pub quote: Option<String>,
}

/// One row of a system's state at the end of the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    pub kind: ItemKind,
    pub text: String,
    pub status: PredictedStatus,
    /// Unix ms; compared by UTC day.
    pub due_ms: Option<i64>,
    /// The system's own confidence in `[0, 1]`; operating points filter on it.
    pub confidence: f64,
    pub provenance: Vec<Citation>,
}

/// Metrics of one run at one operating point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub commitment_recall: Option<f64>,
    pub commitment_precision: Option<f64>,
    pub commitment_f1: Option<f64>,
    pub open_loop_recall: Option<f64>,
    pub open_loop_precision: Option<f64>,
    pub open_loop_f1: Option<f64>,
    /// Micro F1 over commitments and open loops together: the headline.
    pub tracking_f1: Option<f64>,
    pub resolution_accuracy: Option<f64>,
    pub false_closure_rate: Option<f64>,
    pub supersession_accuracy: Option<f64>,
    pub false_commitment_rate: Option<f64>,
    pub provenance_validity: Option<f64>,
}

/// The raw counts behind [`Metrics`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub gold_commitments: usize,
    pub gold_open_loops: usize,
    pub predicted_commitments: usize,
    pub predicted_open_loops: usize,
    pub matched_commitments: usize,
    pub matched_open_loops: usize,
    pub gold_closed: usize,
    pub closed_and_read_closed: usize,
    pub matched_open: usize,
    pub open_read_closed: usize,
    pub gold_superseded: usize,
    pub superseded_latest_due: usize,
    pub distractors: usize,
    pub distractors_hit: usize,
    pub predictions_with_valid_provenance: usize,
}

const STOP: &[&str] = &[
    "a", "an", "the", "to", "of", "for", "and", "or", "by", "on", "in", "at", "is", "it", "i", "i'll", "ll", "will", "you", "your", "me",
    "my", "we", "with", "that", "this", "be", "have", "has", "get", "send", "sends", "from", "over", "still", "can", "could", "please",
];

/// Lower-cased word tokens without stop words.
pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| !w.is_empty() && !STOP.contains(&w.as_str()))
        .collect()
}

/// Token-set F1 of two texts.
pub fn token_f1(a: &str, b: &str) -> f64 {
    let a: HashSet<String> = tokens(a).into_iter().collect();
    let b: HashSet<String> = tokens(b).into_iter().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(&b).count() as f64;
    if common == 0.0 {
        return 0.0;
    }
    let p = common / a.len() as f64;
    let r = common / b.len() as f64;
    2.0 * p * r / (p + r)
}

fn ratio(num: usize, den: usize) -> Option<f64> {
    (den > 0).then(|| num as f64 / den as f64)
}

fn f1(p: Option<f64>, r: Option<f64>) -> Option<f64> {
    match (p, r) {
        (Some(p), Some(r)) if p + r > 0.0 => Some(2.0 * p * r / (p + r)),
        (Some(_), Some(_)) => Some(0.0),
        (None, Some(_)) => Some(0.0), // nothing predicted, something to find: F1 is 0.
        _ => None,
    }
}

/// Whether every citation of `p` is an exact span of the cited event.
pub fn provenance_valid(p: &Prediction, events: &HashMap<&str, &Event>) -> bool {
    !p.provenance.is_empty()
        && p.provenance.iter().all(|c| {
            c.quote.as_deref().is_some_and(|q| !q.trim().is_empty() && events.get(c.event_id.as_str()).is_some_and(|e| e.content.contains(q)))
        })
}

/// Score `predictions` (already filtered to the operating point) against `data`.
pub fn score(data: &Dataset, predictions: &[Prediction]) -> (Metrics, Counts) {
    let gold = &data.gold;
    let events: HashMap<&str, &Event> = data.events.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut counts = Counts {
        gold_commitments: gold.items.iter().filter(|i| i.kind == ItemKind::Commitment).count(),
        gold_open_loops: gold.items.iter().filter(|i| i.kind == ItemKind::OpenLoop).count(),
        predicted_commitments: predictions.iter().filter(|p| p.kind == ItemKind::Commitment).count(),
        predicted_open_loops: predictions.iter().filter(|p| p.kind == ItemKind::OpenLoop).count(),
        distractors: gold.distractors.len(),
        ..Counts::default()
    };

    // Candidate pairs with a score; citing the item's own events outranks any text match.
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (pi, p) in predictions.iter().enumerate() {
        let cited: HashSet<&str> = p.provenance.iter().map(|c| c.event_id.as_str()).collect();
        for (gi, g) in gold.items.iter().enumerate() {
            if g.kind != p.kind {
                continue;
            }
            let text = token_f1(&p.text, &g.text).max(token_f1(&p.text, &g.quote));
            let cites = cited.contains(g.opened_event.as_str()) && cited.len() <= 3
                || g.superseded_by_events.iter().any(|e| cited.contains(e.as_str()));
            // A bundled note opens several items: citing it is only decisive with some text overlap.
            let shared_event = gold.items.iter().filter(|o| o.opened_event == g.opened_event).count() > 1;
            let score = if cites && (!shared_event || text >= 0.25) { 1.0 + text } else { text };
            if score >= MATCH_F1 {
                pairs.push((score, pi, gi));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut matched_pred: HashMap<usize, usize> = HashMap::new();
    let mut matched_gold: HashMap<usize, usize> = HashMap::new();
    for (_, pi, gi) in pairs {
        if matched_pred.contains_key(&pi) || matched_gold.contains_key(&gi) {
            continue;
        }
        matched_pred.insert(pi, gi);
        matched_gold.insert(gi, pi);
    }
    for (&gi, &pi) in &matched_gold {
        let g = &gold.items[gi];
        let p = &predictions[pi];
        match g.kind {
            ItemKind::Commitment => counts.matched_commitments += 1,
            ItemKind::OpenLoop => counts.matched_open_loops += 1,
        }
        if g.status == FinalStatus::Open {
            counts.matched_open += 1;
            if p.status == PredictedStatus::Closed {
                counts.open_read_closed += 1;
            }
        }
    }
    for (gi, g) in gold.items.iter().enumerate() {
        let p = matched_gold.get(&gi).map(|&pi| &predictions[pi]);
        if g.status != FinalStatus::Open {
            counts.gold_closed += 1;
            if p.is_some_and(|p| p.status == PredictedStatus::Closed) {
                counts.closed_and_read_closed += 1;
            }
        }
        if !g.superseded_by_events.is_empty() {
            counts.gold_superseded += 1;
            let latest = g.due_ms.map(|d| d.div_euclid(DAY_MS));
            if p.is_some_and(|p| p.due_ms.map(|d| d.div_euclid(DAY_MS)) == latest) {
                counts.superseded_latest_due += 1;
            }
        }
    }
    for d in &gold.distractors {
        let hit = predictions.iter().enumerate().any(|(pi, p)| {
            !matched_pred.contains_key(&pi)
                && (p.provenance.iter().any(|c| c.event_id == d.event && !matches!(c.quote.as_deref(), Some(q) if token_f1(q, &d.span) < MATCH_F1))
                    || token_f1(&p.text, &d.span) >= MATCH_F1)
        });
        if hit {
            counts.distractors_hit += 1;
        }
    }
    counts.predictions_with_valid_provenance = predictions.iter().filter(|p| provenance_valid(p, &events)).count();

    let c_recall = ratio(counts.matched_commitments, counts.gold_commitments);
    let c_precision = ratio(counts.matched_commitments, counts.predicted_commitments);
    let l_recall = ratio(counts.matched_open_loops, counts.gold_open_loops);
    let l_precision = ratio(counts.matched_open_loops, counts.predicted_open_loops);
    let all_recall = ratio(counts.matched_commitments + counts.matched_open_loops, counts.gold_commitments + counts.gold_open_loops);
    let all_precision = ratio(counts.matched_commitments + counts.matched_open_loops, predictions.len());
    let metrics = Metrics {
        commitment_recall: c_recall,
        commitment_precision: c_precision,
        commitment_f1: f1(c_precision, c_recall),
        open_loop_recall: l_recall,
        open_loop_precision: l_precision,
        open_loop_f1: f1(l_precision, l_recall),
        tracking_f1: f1(all_precision, all_recall),
        resolution_accuracy: ratio(counts.closed_and_read_closed, counts.gold_closed),
        false_closure_rate: ratio(counts.open_read_closed, counts.matched_open),
        supersession_accuracy: ratio(counts.superseded_latest_due, counts.gold_superseded),
        false_commitment_rate: ratio(counts.distractors_hit, counts.distractors),
        provenance_validity: ratio(counts.predictions_with_valid_provenance, predictions.len()),
    };
    (metrics, counts)
}

/// The gold itself as predictions: the scorer's upper bound (every metric 1, false rates 0).
pub fn oracle_predictions(data: &Dataset) -> Vec<Prediction> {
    data.gold
        .items
        .iter()
        .map(|g| Prediction {
            kind: g.kind,
            text: g.text.clone(),
            status: if g.status == FinalStatus::Open { PredictedStatus::Open } else { PredictedStatus::Closed },
            due_ms: g.due_ms,
            confidence: 1.0,
            provenance: vec![Citation { event_id: g.opened_event.clone(), quote: Some(g.quote.clone()) }],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::gen::{generate, Tier};

    #[test]
    fn the_oracle_scores_perfectly_and_nothing_scores_nothing() {
        for tier in Tier::EVERY {
            let data = generate(tier, 42);
            let (m, _) = score(&data, &oracle_predictions(&data));
            for value in [m.commitment_recall, m.commitment_precision, m.open_loop_recall, m.open_loop_precision, m.resolution_accuracy, m.provenance_validity, m.tracking_f1] {
                assert_eq!(value, Some(1.0), "{tier:?}: {m:?}");
            }
            assert_eq!(m.false_commitment_rate, Some(0.0), "{tier:?}");
            assert_eq!(m.false_closure_rate, Some(0.0));
            if tier != Tier::Easy {
                assert_eq!(m.supersession_accuracy, Some(1.0));
            }
            let (empty, _) = score(&data, &[]);
            assert_eq!((empty.commitment_recall, empty.commitment_precision, empty.tracking_f1), (Some(0.0), None, Some(0.0)));
            assert_eq!(empty.provenance_validity, None, "no predictions: no provenance rate, not a zero");
        }
    }

    #[test]
    fn duplicates_wrong_dates_and_fake_quotes_are_penalised() {
        let data = generate(Tier::Hard, 42);
        let mut preds = oracle_predictions(&data);
        // A duplicate of the first item costs precision, not recall.
        let dup = preds[0].clone();
        preds.push(dup);
        // The first superseded item keeps its first due date.
        let moved = data.gold.items.iter().position(|g| !g.superseded_by_events.is_empty()).unwrap();
        preds[moved].due_ms = data.gold.items[moved].first_due_ms;
        // One quote that is not in its event.
        preds[1].provenance[0].quote = Some("words that were never said".into());
        let (m, c) = score(&data, &preds);
        assert_eq!(m.commitment_recall, Some(1.0));
        assert!(m.commitment_precision.unwrap() < 1.0);
        assert_eq!(c.superseded_latest_due, c.gold_superseded - 1);
        assert_eq!(c.predictions_with_valid_provenance, preds.len() - 1);
    }

    #[test]
    fn token_f1_ignores_stop_words_and_case() {
        assert_eq!(token_f1("I'll send the Q3 budget to Oren", "send the q3 budget to oren by Friday"), 2.0 * 1.0 * 0.75 / 1.75);
        assert_eq!(token_f1("", "x"), 0.0);
    }
}
