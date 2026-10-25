//! Entries for the public harnesses the plan names (E12-T2): OmniMemEval and the Mem0 harness.
//!
//! This build locates their datasets and nothing more. Running either needs a reader model (to
//! answer from retrieved memory) and a judge model (to score the answer), and the default run
//! calls no model; so the entries are emitted with `status: "not_run"` and a reason that says
//! what is missing, instead of a number. The environment variables are the hook the adapters
//! will read when they land, so a run configured today stays valid then.

use super::{Suite, SuiteStatus};

/// One external harness and the variable that points at its dataset.
pub struct ExternalSuite {
    pub name: &'static str,
    pub env_var: &'static str,
    /// What the dataset holds, for the reason text.
    pub dataset: &'static str,
}

pub const EXTERNAL: &[ExternalSuite] = &[
    ExternalSuite {
        name: "omnimemeval",
        env_var: "SHOGUN_OMNIMEMEVAL_DIR",
        dataset: "OmniMemEval question set",
    },
    ExternalSuite {
        name: "mem0-harness",
        env_var: "SHOGUN_MEM0_HARNESS_DIR",
        dataset: "Mem0 public harness conversations",
    },
];

/// The `not_run` entry for one external suite. `env` is injected so a test can set the variable
/// without touching the process environment.
pub fn stub(spec: &ExternalSuite, env: impl Fn(&str) -> Option<String>) -> Suite {
    let reason = match env(spec.env_var) {
        None => format!(
            "{} is not set; the {} is not bundled with this repository",
            spec.env_var, spec.dataset
        ),
        Some(dir) if !std::path::Path::new(&dir).is_dir() => format!(
            "{} points at a path that is not a directory; the {} was not found",
            spec.env_var, spec.dataset
        ),
        Some(dir) => format!(
            "{} found under {}; the reader and judge adapters are not implemented in this build \
             and no model is called by default",
            spec.dataset,
            std::path::Path::new(&dir)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "the configured directory".to_string())
        ),
    };
    Suite {
        name: spec.name,
        status: SuiteStatus::NotRun,
        metric: None,
        value: None,
        ci95: None,
        n: 0,
        tokens_per_answer: None,
        write_cost_micro_usd: None,
        p95_ms: None,
        direction: None,
        workload: None,
        queries_per_seed: None,
        per_seed: Vec::new(),
        reason: Some(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_variable_means_not_run_with_the_variable_named() {
        let suite = stub(&EXTERNAL[0], |_| None);
        assert_eq!(suite.status, SuiteStatus::NotRun);
        assert!(suite.value.is_none() && suite.ci95.is_none() && suite.n == 0);
        assert!(suite
            .reason
            .as_deref()
            .unwrap()
            .contains("SHOGUN_OMNIMEMEVAL_DIR"));
    }

    #[test]
    fn a_missing_directory_is_reported_not_treated_as_a_dataset() {
        let suite = stub(&EXTERNAL[1], |_| Some("/definitely/not/here".to_string()));
        assert_eq!(suite.status, SuiteStatus::NotRun);
        assert!(suite.reason.as_deref().unwrap().contains("not a directory"));
    }

    #[test]
    fn a_present_directory_is_located_but_still_not_run_without_a_model() {
        let dir = std::env::temp_dir();
        let suite = stub(&EXTERNAL[0], |_| Some(dir.to_string_lossy().into_owned()));
        assert_eq!(suite.status, SuiteStatus::NotRun);
        let reason = suite.reason.as_deref().unwrap();
        assert!(reason.contains("found under"), "{reason}");
        assert!(reason.contains("no model is called"), "{reason}");
        // The absolute path is machine metadata and stays out of the artifact.
        assert!(!reason.contains(&dir.to_string_lossy().into_owned()) || dir.parent().is_none());
    }
}
