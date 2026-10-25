//! Drive an adapter through datasets, score each run, and aggregate over seeds into the results
//! file (`shogunagent/loop-bench-results/v1`, documented in `docs/benchmarks/open-loop-tracking.md`).

use std::collections::BTreeMap;
use std::time::Instant;

use memory_bench::harness::stats::summarize;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapter::{AdapterInfo, SystemAdapter, Usage};
use crate::gen::{generate, Dataset, Tier, HOUR_MS};
use crate::score::{score, Counts, Metrics, Prediction};

pub const RESULTS_SCHEMA: &str = "shogunagent/loop-bench-results/v1";

/// An operating point: the predictions at or above a confidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OperatingPoint {
    pub name: &'static str,
    pub min_confidence: f64,
}

/// `all`: every row the system holds. `actionable`: rows at or above 0.5, the band a ShogunAgent
/// face renders at all (as `possibly:` below 0.8).
pub const OPERATING_POINTS: [OperatingPoint; 2] =
    [OperatingPoint { name: "all", min_confidence: 0.0 }, OperatingPoint { name: "actionable", min_confidence: 0.5 }];

/// Wall time of a run's phases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    pub ingest_ms: f64,
    pub process_ms: f64,
    pub read_ms: f64,
    pub total_ms: f64,
    pub events: usize,
    pub ingest_ms_per_event: f64,
}

/// One `(tier, seed)` run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub tier: Tier,
    pub seed: u64,
    pub points: BTreeMap<String, (Metrics, Counts)>,
    pub latency: Latency,
    pub usage: Usage,
    pub predictions: usize,
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

/// Score a finished prediction list at every operating point.
pub fn score_points(data: &Dataset, predictions: &[Prediction]) -> BTreeMap<String, (Metrics, Counts)> {
    OPERATING_POINTS
        .iter()
        .map(|point| {
            let kept: Vec<Prediction> = predictions.iter().filter(|p| p.confidence >= point.min_confidence).cloned().collect();
            (point.name.to_string(), score(data, &kept))
        })
        .collect()
}

/// Drive `adapter` through one dataset: hour by hour, then read at the end.
pub fn run_one(adapter: &mut dyn SystemAdapter, data: &Dataset) -> Result<Run, String> {
    let started = Instant::now();
    let mut latency = Latency { events: data.events.len(), ..Latency::default() };
    adapter.reset(data)?;
    let first_hour = data.events.first().map(|e| e.ts_ms.div_euclid(HOUR_MS)).unwrap_or(0);
    let last_hour = data.gold.end_ms.div_euclid(HOUR_MS);
    let mut next = 0usize;
    for hour in first_hour..last_hour {
        let end = (hour + 1) * HOUR_MS;
        let from = next;
        while next < data.events.len() && data.events[next].ts_ms < end {
            next += 1;
        }
        if next > from {
            let t = Instant::now();
            adapter.ingest(&data.events[from..next])?;
            latency.ingest_ms += ms(t);
        }
        let t = Instant::now();
        adapter.advance(end + 10 * 60_000)?;
        latency.process_ms += ms(t);
    }
    let t = Instant::now();
    let predictions = adapter.snapshot(data.gold.end_ms)?;
    latency.read_ms = ms(t);
    latency.total_ms = ms(started);
    latency.ingest_ms_per_event = if data.events.is_empty() { 0.0 } else { latency.ingest_ms / data.events.len() as f64 };
    Ok(Run {
        tier: data.gold.tier,
        seed: data.gold.seed,
        points: score_points(data, &predictions),
        latency,
        usage: adapter.usage(),
        predictions: predictions.len(),
    })
}

/// The metrics a results file aggregates, in order.
pub const METRIC_NAMES: [&str; 12] = [
    "tracking_f1",
    "commitment_recall",
    "commitment_precision",
    "commitment_f1",
    "open_loop_recall",
    "open_loop_precision",
    "open_loop_f1",
    "resolution_accuracy",
    "false_closure_rate",
    "supersession_accuracy",
    "false_commitment_rate",
    "provenance_validity",
];

fn metric(m: &Metrics, name: &str) -> Option<f64> {
    match name {
        "tracking_f1" => m.tracking_f1,
        "commitment_recall" => m.commitment_recall,
        "commitment_precision" => m.commitment_precision,
        "commitment_f1" => m.commitment_f1,
        "open_loop_recall" => m.open_loop_recall,
        "open_loop_precision" => m.open_loop_precision,
        "open_loop_f1" => m.open_loop_f1,
        "resolution_accuracy" => m.resolution_accuracy,
        "false_closure_rate" => m.false_closure_rate,
        "supersession_accuracy" => m.supersession_accuracy,
        "false_commitment_rate" => m.false_commitment_rate,
        "provenance_validity" => m.provenance_validity,
        _ => None,
    }
}

fn summary_of(values: &[f64]) -> Value {
    match summarize(values) {
        Some(s) => json!({ "mean": s.mean, "ci95": s.ci95, "n": s.n }),
        None => json!({ "mean": Value::Null, "ci95": Value::Null, "n": 0 }),
    }
}

/// Mean and 95% interval over seeds, per tier, operating point and metric; plus latency.
pub fn aggregate(runs: &[Run]) -> Value {
    let mut tiers = serde_json::Map::new();
    for tier in Tier::ALL {
        let of_tier: Vec<&Run> = runs.iter().filter(|r| r.tier == tier).collect();
        if of_tier.is_empty() {
            continue;
        }
        let mut points = serde_json::Map::new();
        for point in OPERATING_POINTS {
            let mut metrics = serde_json::Map::new();
            for name in METRIC_NAMES {
                let values: Vec<f64> = of_tier.iter().filter_map(|r| r.points.get(point.name).and_then(|(m, _)| metric(m, name))).collect();
                metrics.insert(name.to_string(), summary_of(&values));
            }
            points.insert(point.name.to_string(), Value::Object(metrics));
        }
        let lat = |f: fn(&Latency) -> f64| summary_of(&of_tier.iter().map(|r| f(&r.latency)).collect::<Vec<_>>());
        tiers.insert(
            tier.as_str().to_string(),
            json!({
                "seeds": of_tier.len(),
                "operating_points": points,
                "latency": {
                    "ingest_ms_per_event": lat(|l| l.ingest_ms_per_event),
                    "process_ms": lat(|l| l.process_ms),
                    "read_ms": lat(|l| l.read_ms),
                    "total_ms": lat(|l| l.total_ms),
                },
                "cost": {
                    "model_calls": of_tier.iter().map(|r| r.usage.model_calls).sum::<u64>(),
                    "cost_micro_usd": of_tier.iter().map(|r| r.usage.cost_micro_usd).sum::<f64>(),
                },
            }),
        );
    }
    Value::Object(tiers)
}

/// Build the results file.
pub fn results(info: &AdapterInfo, seeds: &[u64], tiers: &[Tier], runs: &[Run], generated_at: &str) -> Value {
    json!({
        "schema": RESULTS_SCHEMA,
        "benchmark": crate::gen::BENCHMARK,
        "version": crate::gen::VERSION,
        "generated_at": generated_at,
        "adapter": info,
        "seeds": seeds,
        "tiers": tiers.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        "operating_points": OPERATING_POINTS.iter().map(|p| json!({ "name": p.name, "min_confidence": p.min_confidence })).collect::<Vec<_>>(),
        "headline": { "metric": "tracking_f1", "operating_point": "all", "tier": "hard" },
        "environment": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "profile": if cfg!(debug_assertions) { "debug" } else { "release" } },
        "summary": aggregate(runs),
        "runs": runs,
    })
}

/// Generate and run every `(tier, seed)`.
pub fn run_all(adapter: &mut dyn SystemAdapter, tiers: &[Tier], seeds: &[u64]) -> Result<Vec<Run>, String> {
    let mut runs = Vec::new();
    for &tier in tiers {
        for &seed in seeds {
            runs.push(run_one(adapter, &generate(tier, seed))?);
        }
    }
    Ok(runs)
}

/// The `open_loop_tracking` row for `docs/benchmarks/benchmarks.json` (same keys as every suite
/// row there): the headline at the `all` operating point on the hard tier.
pub fn benchmarks_suite_row(runs: &[Run], info: &AdapterInfo) -> Value {
    let hard: Vec<&Run> = runs.iter().filter(|r| r.tier == Tier::Hard).collect();
    let per_seed: Vec<f64> = hard.iter().filter_map(|r| r.points.get("all").and_then(|(m, _)| m.tracking_f1)).collect();
    let s = summarize(&per_seed);
    let p95: Vec<f64> = hard.iter().map(|r| r.latency.ingest_ms_per_event).collect();
    json!({
        "name": "open_loop_tracking",
        "status": if s.is_some() { "ok" } else { "not_run" },
        "metric": "tracking_f1",
        "value": s.map(|s| s.mean),
        "ci95": s.map(|s| s.ci95),
        "n": s.map(|s| s.n).unwrap_or(0),
        "tokens_per_answer": Value::Null,
        "write_cost_micro_usd": hard.iter().map(|r| r.usage.cost_micro_usd).sum::<f64>() / hard.len().max(1) as f64,
        "p95_ms": Value::Null,
        "direction": "higher",
        "workload": format!("loop-bench hard tier ({}; operating point all)", info.name),
        "queries_per_seed": hard.first().map(|r| r.points.get("all").map(|(_, c)| c.gold_commitments + c.gold_open_loops).unwrap_or(0)),
        "per_seed": per_seed,
        "ingest_ms_per_event": summarize(&p95).map(|s| s.mean),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::adapter::{Oracle, WillBaseline};

    #[test]
    fn the_oracle_is_perfect_and_the_baseline_is_not_through_the_harness() {
        let seeds = [42, 43];
        let oracle = run_all(&mut Oracle::default(), &Tier::ALL, &seeds).unwrap();
        let baseline = run_all(&mut WillBaseline::default(), &Tier::ALL, &seeds).unwrap();
        for run in &oracle {
            assert_eq!(run.points["all"].0.tracking_f1, Some(1.0));
        }
        for run in &baseline {
            let m = &run.points["all"].0;
            assert_eq!(m.open_loop_recall, Some(0.0), "the baseline finds no open loops");
            assert_eq!(m.resolution_accuracy.unwrap_or(0.0), 0.0, "and closes nothing");
            assert!(m.tracking_f1.unwrap() < 0.7, "{m:?}");
            assert!(m.false_commitment_rate.unwrap() > 0.0, "distractors with 'will' fool it");
            assert_eq!(m.provenance_validity, Some(1.0), "its quotes are exact spans");
        }
        let file = results(&WillBaseline::default().info(), &seeds, &Tier::ALL, &baseline, "2026-10-25T00:00:00Z");
        assert_eq!(file["summary"]["hard"]["operating_points"]["all"]["open_loop_recall"]["mean"], 0.0);
        assert_eq!(file["summary"]["hard"]["seeds"], 2);
        let row = benchmarks_suite_row(&baseline, &WillBaseline::default().info());
        assert_eq!(row["name"], "open_loop_tracking");
        assert_eq!(row["per_seed"].as_array().unwrap().len(), 2);
    }
}
