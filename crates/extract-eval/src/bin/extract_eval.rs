//! `cargo run -p extract-eval` — print the model-free extraction report over the golden days.
//! `cargo run -p extract-eval -- regen` — rewrite `goldens/*.json` from the generator.
//!
//! The Dream Cycle runners (LocalRuleClassifier, Batch replay) need shogun-core's `db` feature;
//! their report prints from `cargo test -p shogun-core --features db --test extract_eval -- --nocapture`.

use extract_eval::{generate, golden, report, runners, score::Score};

fn regen() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens");
    for spec in generate::GOLDEN_SPECS {
        let day = generate::generate(spec)?;
        let path = dir.join(format!("{}.json", spec.id));
        std::fs::write(&path, day.to_json()?)?;
        println!("wrote {} ({} events, {} gold rows)", path.display(), day.events.len(), day.gold.len());
    }
    Ok(())
}

fn report() -> Result<(), Box<dyn std::error::Error>> {
    let days = golden::golden_days()?;
    let mut total = Score::default();
    for day in &days {
        total.merge(&runners::score(day, &runners::run_extract_untrusted(day)));
    }
    println!("{}", report::markdown("extract_untrusted (inline ingress, local rules)", &total));
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("regen") => regen(),
        None | Some("report") => report(),
        Some(_) => Err("usage: extract-eval [report|regen]".into()),
    }
}
