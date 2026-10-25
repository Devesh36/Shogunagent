//! `dedup-bench` — run the memory repetition benchmark and print the report.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::process::ExitCode;

const USAGE: &str = "\
dedup-bench — how many state rows one real memory becomes

USAGE:
    dedup-bench [--json] [--no-cost]

OPTIONS:
    --json       Print the report as JSON instead of the table.
    --no-cost    Skip the match-cost scaling run (the slow part).
    -h, --help   Show this help.
";

/// Sizes the match-cost run grows through. Each point persists `COST_SAMPLES` fresh candidates
/// into a window that already holds that many unresolved rows.
const COST_SIZES: &[usize] = &[100, 1_000, 5_000];
const COST_SAMPLES: usize = 50;

fn main() -> ExitCode {
    let mut json = false;
    let mut cost = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--json" => json = true,
            "--no-cost" => cost = false,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown flag {other:?}\n");
                print!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    let outcomes = match dedup_bench::run_all() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("benchmark failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cost_points = if cost {
        let layouts = [
            (true, false),
            (false, false),
            (true, true),
            (false, true),
        ]
        .map(|(same_window, topical)| dedup_bench::CostLayout { same_window, topical });
        let all = layouts.iter().try_fold(Vec::new(), |mut acc, layout| {
            acc.extend(dedup_bench::match_cost(COST_SIZES, COST_SAMPLES, *layout)?);
            Ok::<_, dedup_bench::harness::HarnessError>(acc)
        });
        match all {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cost run failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        Vec::new()
    };

    let report = dedup_bench::BenchReport::build(outcomes, cost_points);
    if json {
        match report.to_json() {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("could not serialise report: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print!("{}", report.render());
    }
    ExitCode::SUCCESS
}
