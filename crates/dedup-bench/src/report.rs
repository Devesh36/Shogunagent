//! Scoring and rendering.

use serde::Serialize;

use crate::harness::{CostPoint, Outcome};
use crate::scenario::Mechanism;

/// Totals for one mechanism.
#[derive(Debug, Clone, Serialize)]
pub struct MechanismScore {
    pub mechanism: Mechanism,
    pub scenarios: usize,
    pub memories: usize,
    pub rows: usize,
    pub extra_rows: usize,
    pub wrong_merges: usize,
}

impl MechanismScore {
    /// Rows written per real memory. 1.000 is no repetition.
    pub fn rows_per_memory(&self) -> Option<f64> {
        (self.memories > 0).then(|| self.rows as f64 / self.memories as f64)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub benchmark: &'static str,
    pub outcomes: Vec<Outcome>,
    pub by_mechanism: Vec<MechanismScore>,
    /// Over every non-precision scenario: memories that should be one row each.
    pub repetition_memories: usize,
    pub repetition_extra_rows: usize,
    /// Over every scenario.
    pub wrong_merges: usize,
    pub passed: usize,
    pub cost: Vec<CostPoint>,
}

impl BenchReport {
    pub fn build(outcomes: Vec<Outcome>, cost: Vec<CostPoint>) -> Self {
        let mut mechanisms: Vec<Mechanism> = outcomes.iter().map(|o| o.mechanism).collect();
        mechanisms.sort();
        mechanisms.dedup();
        let by_mechanism = mechanisms
            .into_iter()
            .map(|m| {
                let group: Vec<&Outcome> = outcomes.iter().filter(|o| o.mechanism == m).collect();
                MechanismScore {
                    mechanism: m,
                    scenarios: group.len(),
                    memories: group.iter().map(|o| o.found).sum(),
                    rows: group.iter().map(|o| o.rows).sum(),
                    extra_rows: group.iter().map(|o| o.extra_rows).sum(),
                    wrong_merges: group.iter().map(|o| o.wrong_merges).sum(),
                }
            })
            .collect();
        let repetition: Vec<&Outcome> =
            outcomes.iter().filter(|o| !o.mechanism.is_precision()).collect();
        Self {
            benchmark: "dedup-bench",
            repetition_memories: repetition.iter().map(|o| o.found).sum(),
            repetition_extra_rows: repetition.iter().map(|o| o.extra_rows).sum(),
            wrong_merges: outcomes.iter().map(|o| o.wrong_merges).sum(),
            passed: outcomes.iter().filter(|o| o.passed()).count(),
            by_mechanism,
            outcomes,
            cost,
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("ShogunAI MEMORY REPETITION BENCHMARK\n");
        s.push_str("──────────────────────────────────\n\n");
        s.push_str(&format!(
            "{:<30} {:<18} {:>4} {:>5} {:>5} {:>6} {:>6}  verdict\n",
            "scenario", "mechanism", "mem", "found", "rows", "extra", "wrong"
        ));
        for o in &self.outcomes {
            s.push_str(&format!(
                "{:<30} {:<18} {:>4} {:>5} {:>5} {:>6} {:>6}  {}\n",
                o.id,
                o.mechanism.as_str(),
                o.expected,
                o.found,
                o.rows,
                o.extra_rows,
                o.wrong_merges,
                match (o.passed(), o.known_limitation.is_some()) {
                    (true, _) => "ok",
                    (false, true) => "known",
                    (false, false) => "FAIL",
                }
            ));
        }
        let known: Vec<&Outcome> = self
            .outcomes
            .iter()
            .filter(|o| !o.passed() && o.known_limitation.is_some())
            .collect();
        if !known.is_empty() {
            s.push_str("\nKnown limitations (expected failures, documented)\n");
            for o in known {
                s.push_str(&format!("  {}: {}\n", o.id, o.known_limitation.unwrap_or("")));
            }
        }

        s.push_str("\nBy mechanism\n");
        s.push_str(&format!(
            "  {:<18} {:>9} {:>8} {:>5} {:>6} {:>13} {:>6}\n",
            "mechanism", "scenarios", "memories", "rows", "extra", "rows/memory", "wrong"
        ));
        for m in &self.by_mechanism {
            s.push_str(&format!(
                "  {:<18} {:>9} {:>8} {:>5} {:>6} {:>13} {:>6}\n",
                m.mechanism.as_str(),
                m.scenarios,
                m.memories,
                m.rows,
                m.extra_rows,
                m.rows_per_memory().map_or("n/a".to_string(), |v| format!("{v:.2}")),
                m.wrong_merges
            ));
        }

        s.push_str(&format!(
            "\nRepetition: {} extra rows over {} memories ({}).  Wrong merges: {}.  Scenarios passed: {}/{}.\n",
            self.repetition_extra_rows,
            self.repetition_memories,
            if self.repetition_memories > 0 {
                format!(
                    "{:.2} rows per memory",
                    (self.repetition_memories + self.repetition_extra_rows) as f64
                        / self.repetition_memories as f64
                )
            } else {
                "n/a".to_string()
            },
            self.wrong_merges,
            self.passed,
            self.outcomes.len()
        ));

        if !self.cost.is_empty() {
            s.push_str("\nMatch cost: persisting one candidate when N unresolved rows already exist\n");
            s.push_str(&format!(
                "  {:<14} {:<12} {:>8} {:>8} {:>10} {:>10}\n",
                "existing rows", "vocabulary", "N", "samples", "p50 µs", "p95 µs"
            ));
            for c in &self.cost {
                s.push_str(&format!(
                    "  {:<14} {:<12} {:>8} {:>8} {:>10.0} {:>10.0}\n",
                    if c.layout.same_window { "same window" } else { "other windows" },
                    if c.layout.topical {
                        format!("{} topics", crate::harness::TOPICS)
                    } else {
                        "uniform".to_string()
                    },
                    c.existing,
                    c.samples,
                    c.p50_us,
                    c.p95_us
                ));
            }
        }
        s
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}
