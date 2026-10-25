//! `pii-bench` — measure what survives SHOGUN's redaction at each storage boundary.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::process::ExitCode;

fn main() -> ExitCode {
    let outcomes = match pii_bench::run_all() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("benchmark failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = pii_bench::BenchReport::build(outcomes);
    print!("{}", report.render());

    if let Some(dir) = std::env::args().skip(1).nth(1) {
        if std::env::args().nth(1).as_deref() == Some("--out") {
            if let Err(e) = std::fs::create_dir_all(&dir)
                .and_then(|_| {
                    let json = report.to_json().map_err(std::io::Error::other)?;
                    std::fs::write(std::path::Path::new(&dir).join("pii-bench.json"), json)
                })
            {
                eprintln!("could not write report: {e}");
                return ExitCode::FAILURE;
            }
            println!("\nReport: {dir}/pii-bench.json");
        }
    }
    ExitCode::SUCCESS
}
