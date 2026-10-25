//! Scoring and rendering.
//!
//! Reported per `(category, boundary)` rather than as one global number. A single "redaction
//! coverage %" would average the DB path together with the column that has no gate, and the
//! headline finding — that one column bypasses it entirely — would vanish into the mean.

use serde::Serialize;

use crate::corpus::{Category, Expectation};
use crate::harness::{Boundary, Outcome};

/// Score for one category at one boundary.
#[derive(Debug, Clone, Serialize)]
pub struct CategoryScore {
    pub category: Category,
    pub boundary: Boundary,
    pub cases: usize,
    /// Cases that should have been masked and were.
    pub masked: usize,
    /// Cases that should have been masked and were not. The number that matters.
    pub leaked: usize,
    /// Cases documented as surviving by design, that did.
    pub survived_by_design: usize,
    /// Look-alikes wrongly masked — corrupting real memory.
    pub false_positives: usize,
}

impl CategoryScore {
    /// Of the cases that had to be masked, the share that were. `None` when the category has no
    /// such case at this boundary — a rate with no denominator is not zero.
    pub fn recall(&self) -> Option<f64> {
        let d = self.masked + self.leaked;
        (d > 0).then(|| self.masked as f64 / d as f64)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub scores: Vec<CategoryScore>,
    pub outcomes: Vec<Outcome>,
    pub total_leaks: usize,
    pub total_false_positives: usize,
}

impl BenchReport {
    pub fn build(outcomes: Vec<Outcome>) -> Self {
        let mut scores: Vec<CategoryScore> = Vec::new();
        for boundary in Boundary::ALL {
            for category in [
                Category::TargetedSecret,
                Category::UntargetedSecret,
                Category::PersonalData,
                Category::HighRiskIdentifier,
                Category::LookAlike,
                Category::Obfuscated,
            ] {
                let group: Vec<&Outcome> = outcomes
                    .iter()
                    .filter(|o| o.boundary == boundary && o.category == category)
                    .collect();
                if group.is_empty() {
                    continue;
                }
                let mut s = CategoryScore {
                    category,
                    boundary,
                    cases: group.len(),
                    masked: 0,
                    leaked: 0,
                    survived_by_design: 0,
                    false_positives: 0,
                };
                for o in group {
                    match o.expectation {
                        Expectation::Masked => {
                            if o.leaked() {
                                s.leaked += 1;
                            } else {
                                s.masked += 1;
                            }
                        }
                        Expectation::SurvivesByDesign => {
                            if o.leaked() {
                                s.survived_by_design += 1;
                            }
                        }
                        Expectation::Untouched => {
                            if !o.leaked() {
                                // The look-alike vanished: it was masked when it should not be.
                                s.false_positives += 1;
                            }
                        }
                    }
                }
                scores.push(s);
            }
        }
        let total_leaks = scores.iter().map(|s| s.leaked).sum();
        let total_false_positives = scores.iter().map(|s| s.false_positives).sum();
        Self { scores, outcomes, total_leaks, total_false_positives }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("\nShogunAI PII / SECRET REDACTION BENCHMARK\n");
        out.push_str("────────────────────────────────────────\n\n");

        for boundary in Boundary::ALL {
            out.push_str(&format!("Boundary: {}\n", boundary.as_str()));
            out.push_str(&format!(
                "  {:<22} {:>5} {:>7} {:>7} {:>9} {:>7}\n",
                "category", "cases", "masked", "leaked", "by-design", "recall"
            ));
            for s in self.scores.iter().filter(|s| s.boundary == boundary) {
                out.push_str(&format!(
                    "  {:<22} {:>5} {:>7} {:>7} {:>9} {:>7}\n",
                    format!("{:?}", s.category),
                    s.cases,
                    s.masked,
                    s.leaked,
                    s.survived_by_design,
                    s.recall().map_or("n/a".to_string(), |r| format!("{r:.3}")),
                ));
            }
            let fp: usize = self
                .scores
                .iter()
                .filter(|s| s.boundary == boundary)
                .map(|s| s.false_positives)
                .sum();
            out.push_str(&format!("  false positives (look-alikes masked): {fp}\n\n"));
        }

        out.push_str("Leaks — cases that had to be masked and were not:\n");
        let mut any = false;
        for o in self.outcomes.iter().filter(|o| {
            matches!(o.expectation, Expectation::Masked) && o.leaked()
        }) {
            any = true;
            out.push_str(&format!(
                "  {:<24} {:<13} row={} fts={}\n",
                o.case_id,
                o.boundary.as_str(),
                o.survived_in_row,
                o.survived_in_fts
            ));
        }
        if !any {
            out.push_str("  (none)\n");
        }
        out.push_str(&format!(
            "\nTotal leaks: {}   Total false positives: {}\n",
            self.total_leaks, self.total_false_positives
        ));
        out
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}
