//! `loop-bench`: generate, run and score the open-loop tracking benchmark v1.
//!
//! ```text
//! loop-bench run --adapter <shogunagent|shogunagent-scripted-review|will-baseline|oracle> [--tiers easy,medium,hard]
//!                [--seeds N] [--first-seed 42] [--out results.json] [--merge-into benchmarks.json]
//! loop-bench generate --tier <t> --seed <n> --out <dir>      # events.jsonl + gold.json
//! loop-bench score --dir <dir> [--tiers …] [--seeds N] [--first-seed 42] [--out results.json]
//! ```
//!
//! Hand-rolled argument parsing, like memory-bench: no clap.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use loop_bench::adapter::{by_name, AdapterInfo, Usage};
use loop_bench::gen::{generate, Tier};
use loop_bench::harness::{self, Latency, Run, METRIC_NAMES, OPERATING_POINTS};
use loop_bench::score::Prediction;
use serde_json::Value;

const HELP: &str = "loop-bench — open-loop and commitment tracking benchmark v1 (docs/benchmarks/open-loop-tracking.md)

USAGE
  loop-bench run --adapter <NAME> [OPTIONS]
      Drive a built-in adapter through every (tier, seed) and print the summary.
      NAME: shogunagent (the engine on its local rules), shogunagent-scripted-review (the engine
      with its Batch lane answered by an offline scripted reviewer, NOT a model), will-baseline,
      oracle.
  loop-bench generate --tier <easy|medium|hard> --seed <N> --out <DIR>
      Write DIR/events.jsonl (the stream) and DIR/gold.json (the answer key).
  loop-bench score --dir <DIR> [OPTIONS]
      Score another system: DIR/<tier>-<seed>.json holds its predictions for each run
      ({\"adapter\": {...}, \"predictions\": [...], \"latency\"?: {...}, \"usage\"?: {...}}).

OPTIONS
  --tiers <LIST>        comma-separated tiers (default easy,medium,hard; also heldout)
  --seeds <N>           number of seeds (default 5)
  --first-seed <N>      first seed (default 42); seeds are consecutive
  --out <FILE>          write the results JSON here
  --merge-into <FILE>   put the open_loop_tracking row into a benchmarks.json (run/score)
  -h, --help            this text
";

struct Args {
    command: String,
    adapter: Option<String>,
    tiers: Vec<Tier>,
    seeds: usize,
    first_seed: u64,
    tier: Option<Tier>,
    seed: Option<u64>,
    out: Option<PathBuf>,
    dir: Option<PathBuf>,
    merge_into: Option<PathBuf>,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        command: argv.first().cloned().ok_or("missing command")?,
        adapter: None,
        tiers: Tier::ALL.to_vec(),
        seeds: 5,
        first_seed: 42,
        tier: None,
        seed: None,
        out: None,
        dir: None,
        merge_into: None,
    };
    let mut i = 1;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let value = || argv.get(i + 1).cloned().ok_or(format!("{flag} needs a value"));
        match flag {
            "--adapter" => args.adapter = Some(value()?),
            "--tiers" => {
                args.tiers = value()?.split(',').map(|t| Tier::parse(t.trim()).ok_or(format!("unknown tier {t}"))).collect::<Result<_, _>>()?;
            }
            "--seeds" => args.seeds = value()?.parse().map_err(|_| "--seeds is a number")?,
            "--first-seed" => args.first_seed = value()?.parse().map_err(|_| "--first-seed is a number")?,
            "--tier" => args.tier = Some(Tier::parse(&value()?).ok_or("unknown tier")?),
            "--seed" => args.seed = Some(value()?.parse().map_err(|_| "--seed is a number")?),
            "--out" => args.out = Some(PathBuf::from(value()?)),
            "--dir" => args.dir = Some(PathBuf::from(value()?)),
            "--merge-into" => args.merge_into = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown option {other}")),
        }
        i += 2;
    }
    Ok(args)
}

/// RFC 3339 UTC of now, without a date crate.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let d = shogun_memory::due::civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", d.year, d.month, d.day, s / 3600, (s / 60) % 60, s % 60)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

fn fmt(v: &Value) -> String {
    match v.get("mean").and_then(Value::as_f64) {
        Some(mean) => {
            let ci = v.get("ci95").and_then(Value::as_array);
            match ci.and_then(|c| Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))) {
                Some((lo, hi)) if (hi - lo).abs() > 1e-9 => format!("{mean:.3} [{lo:.3},{hi:.3}]"),
                _ => format!("{mean:.3}"),
            }
        }
        None => "—".to_string(),
    }
}

fn print_summary(results: &Value) {
    let name = results["adapter"]["name"].as_str().unwrap_or("?");
    println!("adapter: {name}  seeds: {}  profile: {}", results["seeds"], results["environment"]["profile"]);
    for tier in Tier::EVERY.map(Tier::as_str) {
        let Some(entry) = results["summary"].get(tier) else { continue };
        for point in OPERATING_POINTS {
            println!("\n[{tier} / {}] (n = {})", point.name, entry["seeds"]);
            for metric in METRIC_NAMES {
                println!("  {metric:<24} {}", fmt(&entry["operating_points"][point.name][metric]));
            }
        }
        println!("  {:<24} {}", "ingest_ms_per_event", fmt(&entry["latency"]["ingest_ms_per_event"]));
        println!("  {:<24} {}", "process_ms (run)", fmt(&entry["latency"]["process_ms"]));
        println!("  {:<24} {} / {}", "model_calls / cost_micro_usd", entry["cost"]["model_calls"], entry["cost"]["cost_micro_usd"]);
    }
}

fn merge(path: &Path, row: Value) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut file: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let suites = file.get_mut("suites").and_then(Value::as_array_mut).ok_or("no suites array")?;
    suites.retain(|s| s["name"] != "open_loop_tracking");
    suites.push(row);
    write(path, &(serde_json::to_string_pretty(&file).map_err(|e| e.to_string())? + "\n"))
}

fn finish(args: &Args, info: &AdapterInfo, seeds: &[u64], runs: &[Run]) -> Result<(), String> {
    let results = harness::results(info, seeds, &args.tiers, runs, &now_rfc3339());
    print_summary(&results);
    if let Some(out) = &args.out {
        write(out, &(serde_json::to_string_pretty(&results).map_err(|e| e.to_string())? + "\n"))?;
        eprintln!("wrote {}", out.display());
    }
    if let Some(path) = &args.merge_into {
        merge(path, harness::benchmarks_suite_row(runs, info))?;
        eprintln!("merged open_loop_tracking into {}", path.display());
    }
    Ok(())
}

fn real_main() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let args = parse(&argv)?;
    let seeds: Vec<u64> = (0..args.seeds as u64).map(|i| args.first_seed + i).collect();
    match args.command.as_str() {
        "run" => {
            let name = args.adapter.clone().ok_or("--adapter is required")?;
            let mut adapter = by_name(&name).ok_or(format!("unknown adapter {name}"))?;
            let runs = harness::run_all(adapter.as_mut(), &args.tiers, &seeds)?;
            finish(&args, &adapter.info(), &seeds, &runs)
        }
        "generate" => {
            let (tier, seed, out) = (args.tier.ok_or("--tier is required")?, args.seed.ok_or("--seed is required")?, args.out.clone().ok_or("--out is required")?);
            let data = generate(tier, seed);
            let mut events = String::new();
            for e in &data.events {
                events.push_str(&serde_json::to_string(e).map_err(|e| e.to_string())?);
                events.push('\n');
            }
            write(&out.join("events.jsonl"), &events)?;
            write(&out.join("gold.json"), &(serde_json::to_string_pretty(&data.gold).map_err(|e| e.to_string())? + "\n"))?;
            eprintln!("wrote {} events and the gold file to {}", data.events.len(), out.display());
            Ok(())
        }
        "score" => {
            let dir = args.dir.clone().ok_or("--dir is required")?;
            let mut runs = Vec::new();
            let mut info: Option<AdapterInfo> = None;
            for &tier in &args.tiers {
                for &seed in &seeds {
                    let path = dir.join(format!("{}-{seed}.json", tier.as_str()));
                    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    let predictions: Vec<Prediction> = serde_json::from_value(file["predictions"].clone()).map_err(|e| format!("{}: predictions: {e}", path.display()))?;
                    if info.is_none() {
                        info = serde_json::from_value(file["adapter"].clone()).ok();
                    }
                    let data = generate(tier, seed);
                    runs.push(Run {
                        tier,
                        seed,
                        points: harness::score_points(&data, &predictions),
                        latency: serde_json::from_value::<Latency>(file["latency"].clone()).unwrap_or_default(),
                        usage: serde_json::from_value::<Usage>(file["usage"].clone()).unwrap_or_default(),
                        predictions: predictions.len(),
                    });
                }
            }
            let info = info.unwrap_or(AdapterInfo { name: "external".into(), version: "?".into(), model: None, description: String::new() });
            finish(&args, &info, &seeds, &runs)
        }
        other => Err(format!("unknown command {other}; see --help")),
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("loop-bench: {why}");
            ExitCode::FAILURE
        }
    }
}
