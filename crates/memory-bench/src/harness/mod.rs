//! The same-harness adapter (E12-T2): runs the suites this crate already measures over several
//! seeds and writes one `benchmarks.json` that states, next to every number, what produced it —
//! the commit, the seeds, the confidence interval, the context handed to a reader per answer, the
//! model spend on the write path, and the query p95.
//!
//! ```text
//! HarnessConfig  → seeds × suites           (one BenchConfig per pair, run by `runner::run`)
//!                → stats::summarize          (mean, t-based 95% interval over seeds)
//!                → external::stub            (OmniMemEval / Mem0 harness entries, `not_run`)
//!                → Benchmarks (serde)        (docs/benchmarks/benchmarks.json)
//! ```
//!
//! **No model is called in the default run.** `embedding`, `reader` and `judge` are `null`, and
//! stay `null` until an adapter that uses one is implemented and switched on; the external suites
//! are emitted with `status: "not_run"` unless an environment variable points at their datasets,
//! and even then this build only locates the dataset (see [`external`]).
//!
//! The retrieval path is the daemon's (`SearchPath::Federated`) by default, because that is the
//! code every Memory API search runs; `BASELINE.md` was measured on the hybrid path and its
//! numbers are not comparable with these (see `crates/memory-bench/README.md`).

pub mod clock;
pub mod external;
pub mod stats;

use serde::Serialize;

use crate::backend::SearchPath;
use crate::config::{BenchConfig, ConfigError, DEFAULT_K, DEFAULT_QUERIES, DEFAULT_WRITE_BATCH};
use crate::report::{BenchReport, Environment};
use crate::runner::RunError;

/// `benchmarks.json` layout version. Bump when a field changes meaning.
pub const SCHEMA_VERSION: &str = "1";
/// Seeds in the default run: the plan asks for at least ten.
pub const DEFAULT_SEEDS: usize = 10;
/// First seed; the rest follow consecutively so a run is reproducible from `(first, count)`.
pub const DEFAULT_FIRST_SEED: u64 = 42;
/// Unmeasured queries per run before measurement starts.
pub const DEFAULT_WARMUP: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness needs at least one seed")]
    NoSeeds,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("suite {suite} seed {seed}: {source}")]
    Run {
        suite: &'static str,
        seed: u64,
        #[source]
        source: RunError,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialising benchmarks: {0}")]
    Json(#[from] serde_json::Error),
}

/// Everything that decides a harness run. Serialized into the output so a number names its setup.
#[derive(Debug, Clone, Serialize)]
pub struct HarnessConfig {
    pub seeds: Vec<u64>,
    /// Events per corpus, for every suite.
    pub events: usize,
    pub k: usize,
    pub warmup: usize,
    pub write_batch: usize,
    pub search_path: SearchPath,
    /// Parent directory for the per-run scratch databases (`<dir>/<suite>-seed<N>`), each claimed
    /// atomically by `runner::run`. `None` runs in memory, which the output records.
    #[serde(skip)]
    pub db_dir: Option<String>,
}

impl HarnessConfig {
    /// The publishing configuration: ten seeds, the 100k-event scale, the daemon's path.
    pub fn publish(events: usize) -> Self {
        Self {
            seeds: (0..DEFAULT_SEEDS as u64)
                .map(|offset| DEFAULT_FIRST_SEED + offset)
                .collect(),
            events,
            k: DEFAULT_K,
            warmup: DEFAULT_WARMUP,
            write_batch: DEFAULT_WRITE_BATCH,
            search_path: SearchPath::Federated,
            db_dir: None,
        }
    }

    /// Small enough for a unit test: proves the file comes out, says nothing about memory.
    pub fn smoke() -> Self {
        Self {
            seeds: vec![DEFAULT_FIRST_SEED, DEFAULT_FIRST_SEED + 1],
            events: 600,
            k: DEFAULT_K,
            warmup: 2,
            write_batch: DEFAULT_WRITE_BATCH,
            search_path: SearchPath::Federated,
            db_dir: None,
        }
    }

    /// Every per-run configuration this harness will hand to `runner::run`, validated.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if self.seeds.is_empty() {
            return Err(HarnessError::NoSeeds);
        }
        for suite in SUITES {
            for &seed in &self.seeds {
                self.bench_config(suite, seed).validate()?;
            }
        }
        Ok(())
    }

    fn bench_config(&self, suite: &SuiteSpec, seed: u64) -> BenchConfig {
        BenchConfig {
            workload: suite.workload.to_string(),
            seed,
            events: self.events,
            queries: suite.queries,
            k: self.k,
            warmup: self.warmup,
            db_path: self
                .db_dir
                .as_ref()
                .map(|dir| format!("{dir}/{}-seed{seed}", suite.name)),
            out_dir: None,
            write_batch: self.write_batch,
            recency_weight: 0.0,
            search_path: self.search_path,
            slice: crate::slicer::SliceConfig::default(),
        }
    }
}

/// Whether a number is better when larger or when smaller — written next to it so a reader never
/// has to know the metric to read the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Higher,
    Lower,
}

/// One suite this crate runs itself.
pub struct SuiteSpec {
    pub name: &'static str,
    pub workload: &'static str,
    pub queries: usize,
    pub metric: &'static str,
    pub direction: Direction,
    /// The headline number of one run, `None` when the run could not express it.
    pub extract: fn(&BenchReport) -> Option<f64>,
}

/// The suites `memory-bench` already measures, in the order they are published.
pub const SUITES: &[SuiteSpec] = &[
    SuiteSpec {
        name: "query_p95",
        workload: "clean",
        queries: DEFAULT_QUERIES,
        metric: "query_p95_ms",
        direction: Direction::Lower,
        extract: |report| report.query_latency.map(|latency| latency.p95_ms),
    },
    SuiteSpec {
        name: "temporal_recall_at_1",
        workload: "temporal",
        // One present-tense question per tracked project; the vocabulary has twelve.
        queries: 12,
        metric: "recall_at_1",
        direction: Direction::Higher,
        extract: |report| report.quality.recall_at_1,
    },
    SuiteSpec {
        name: "abstention_false_evidence",
        workload: "abstention",
        queries: DEFAULT_QUERIES,
        metric: "false_evidence_rate",
        direction: Direction::Lower,
        extract: |report| report.quality.false_evidence_rate,
    },
    SuiteSpec {
        name: "multi_session_coverage_at_5",
        workload: "multi-session",
        // Twelve projects × four split answers.
        queries: 48,
        metric: "coverage_at_5",
        direction: Direction::Higher,
        extract: |report| report.quality.coverage_at_5,
    },
];

/// A model named in the output. Absent in the default run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRef {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteStatus {
    Ok,
    NotRun,
}

/// One row of `suites`. Every numeric field is nullable: a suite that did not run, or a run that
/// could not express a metric, says `null`, never `0`.
#[derive(Debug, Clone, Serialize)]
pub struct Suite {
    pub name: &'static str,
    pub status: SuiteStatus,
    pub metric: Option<&'static str>,
    /// Mean of the per-seed values.
    pub value: Option<f64>,
    /// 95% interval of that mean over the seeds (Student's t); equal to `value` for one seed.
    pub ci95: Option<[f64; 2]>,
    /// Seeds that produced a value.
    pub n: usize,
    /// Mean estimated tokens of event text returned per query (four characters per token): the
    /// context a reader would be handed per answer.
    pub tokens_per_answer: Option<f64>,
    /// Model spend on the write path per event, in micro-USD. The default run calls no model while
    /// ingesting, so this is `0.0`, not an estimate of compute.
    pub write_cost_micro_usd: Option<f64>,
    /// Mean per-seed query p95 in milliseconds.
    pub p95_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queries_per_seed: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub per_seed: Vec<f64>,
    /// Why a suite did not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where and how the run happened.
#[derive(Debug, Clone, Serialize)]
pub struct HarnessEnvironment {
    pub os: String,
    pub arch: String,
    pub profile: &'static str,
    pub in_memory: bool,
    pub search_path: SearchPath,
    pub events: usize,
    pub k: usize,
}

/// The whole artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Benchmarks {
    pub schema: &'static str,
    /// RFC 3339, UTC.
    pub generated_at: String,
    /// Build-time commit of the binary, `null` outside a checkout.
    pub commit: Option<String>,
    /// `true` when the tree had uncommitted changes or had moved since the build.
    pub commit_dirty: Option<bool>,
    pub embedding: Option<ModelRef>,
    pub reader: Option<ModelRef>,
    pub judge: Option<ModelRef>,
    pub seeds: Vec<u64>,
    pub environment: HarnessEnvironment,
    pub suites: Vec<Suite>,
}

impl Benchmarks {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Run every suite over every seed and assemble the artifact. `progress` is called once per
/// finished run with the suite name and the seed, for a console that would otherwise be silent
/// for minutes.
pub fn run(
    config: &HarnessConfig,
    mut progress: impl FnMut(&'static str, u64, &BenchReport),
) -> Result<Benchmarks, HarnessError> {
    config.validate()?;
    let mut suites = Vec::with_capacity(SUITES.len() + external::EXTERNAL.len());
    let mut environment: Option<Environment> = None;
    for spec in SUITES {
        let mut values = Vec::with_capacity(config.seeds.len());
        let mut tokens = Vec::with_capacity(config.seeds.len());
        let mut p95s = Vec::with_capacity(config.seeds.len());
        for &seed in &config.seeds {
            let bench = config.bench_config(spec, seed);
            let report = crate::runner::run(&bench).map_err(|source| HarnessError::Run {
                suite: spec.name,
                seed,
                source,
            })?;
            if let Some(value) = (spec.extract)(&report) {
                values.push(value);
            }
            if let Some(estimate) = report.quality.evidence_tokens_per_query {
                tokens.push(estimate);
            }
            if let Some(latency) = report.query_latency {
                p95s.push(latency.p95_ms);
            }
            progress(spec.name, seed, &report);
            if environment.is_none() {
                environment = Some(report.environment.clone());
            }
        }
        let summary = stats::summarize(&values);
        suites.push(Suite {
            name: spec.name,
            status: SuiteStatus::Ok,
            metric: Some(spec.metric),
            value: summary.as_ref().map(|summary| summary.mean),
            ci95: summary.as_ref().map(|summary| summary.ci95),
            n: values.len(),
            tokens_per_answer: stats::mean(&tokens),
            // Lexical indexing only: nothing on the write path is a model call.
            write_cost_micro_usd: Some(0.0),
            p95_ms: stats::mean(&p95s),
            direction: Some(spec.direction),
            workload: Some(spec.workload),
            queries_per_seed: Some(spec.queries),
            per_seed: values,
            reason: None,
        });
    }
    for spec in external::EXTERNAL {
        suites.push(external::stub(spec, |name| std::env::var(name).ok()));
    }

    let environment = environment.unwrap_or_else(Environment::detect);
    Ok(Benchmarks {
        schema: SCHEMA_VERSION,
        generated_at: clock::now_rfc3339(),
        commit: environment.git_commit.clone(),
        commit_dirty: environment.git_dirty,
        embedding: None,
        reader: None,
        judge: None,
        seeds: config.seeds.clone(),
        environment: HarnessEnvironment {
            os: environment.os,
            arch: environment.arch,
            profile: environment.profile,
            in_memory: config.db_dir.is_none(),
            search_path: config.search_path,
            events: config.events,
            k: config.k,
        },
        suites,
    })
}

/// Write the artifact to `path`, replacing any previous file: `benchmarks.json` is a published
/// snapshot that a new run supersedes, unlike the per-run reports `runner::persist` keeps apart.
pub fn write(benchmarks: &Benchmarks, path: &str) -> Result<(), HarnessError> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = benchmarks.to_json()?;
    std::fs::write(path, format!("{json}\n"))?;
    Ok(())
}

fn opt(value: Option<f64>, places: usize) -> String {
    value.map_or_else(|| "n/a".to_string(), |x| format!("{x:.places$}"))
}

/// The console table.
pub fn render_summary(benchmarks: &Benchmarks) -> String {
    let mut out = String::new();
    out.push_str("\nShogunAI MEMORY BENCHMARKS (same harness)\n");
    out.push_str("────────────────────────────────────────\n");
    out.push_str(&format!(
        "generated {}  commit {}{}  seeds {}  events {}  path {}  {}\n\n",
        benchmarks.generated_at,
        benchmarks.commit.as_deref().unwrap_or("unknown"),
        if benchmarks.commit_dirty == Some(true) {
            " (dirty)"
        } else {
            ""
        },
        benchmarks.seeds.len(),
        benchmarks.environment.events,
        benchmarks.environment.search_path.as_str(),
        if benchmarks.environment.in_memory {
            "in-memory"
        } else {
            "on-disk"
        },
    ));
    out.push_str(&format!(
        "{:<30} {:<20} {:>8} {:>19} {:>3} {:>9} {:>9}\n",
        "suite", "metric", "value", "ci95", "n", "tok/ans", "p95 ms"
    ));
    for suite in &benchmarks.suites {
        match suite.status {
            SuiteStatus::Ok => out.push_str(&format!(
                "{:<30} {:<20} {:>8} {:>19} {:>3} {:>9} {:>9}\n",
                suite.name,
                suite.metric.unwrap_or("-"),
                opt(suite.value, 3),
                suite.ci95.map_or_else(
                    || "n/a".to_string(),
                    |[lo, hi]| format!("[{lo:.3}, {hi:.3}]")
                ),
                suite.n,
                opt(suite.tokens_per_answer, 0),
                opt(suite.p95_ms, 2),
            )),
            SuiteStatus::NotRun => out.push_str(&format!(
                "{:<30} not run — {}\n",
                suite.name,
                suite.reason.as_deref().unwrap_or("")
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_suite_configuration_validates_at_the_smoke_and_publish_scales() {
        HarnessConfig::smoke().validate().expect("smoke");
        HarnessConfig::publish(100_000).validate().expect("publish");
        assert_eq!(HarnessConfig::publish(100_000).seeds.len(), DEFAULT_SEEDS);
    }

    #[test]
    fn no_seeds_is_rejected() {
        let config = HarnessConfig {
            seeds: Vec::new(),
            ..HarnessConfig::smoke()
        };
        assert!(matches!(config.validate(), Err(HarnessError::NoSeeds)));
    }

    #[test]
    fn scratch_databases_are_one_directory_per_suite_and_seed() {
        let config = HarnessConfig {
            db_dir: Some("/scratch".to_string()),
            ..HarnessConfig::smoke()
        };
        let bench = config.bench_config(&SUITES[1], 7);
        assert_eq!(
            bench.db_path.as_deref(),
            Some("/scratch/temporal_recall_at_1-seed7")
        );
        assert_eq!(bench.workload, "temporal");
    }
}
