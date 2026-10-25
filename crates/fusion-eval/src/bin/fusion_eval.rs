//! `cargo run -p fusion-eval` — print the action-row eval as Markdown.
//! `cargo run -p fusion-eval --release -- bench` — time matching + ranking on a mature memory.

fn main() {
    if std::env::args().nth(1).as_deref() == Some("bench") {
        let (p50, p95) = fusion_eval::bench::time_row_build(200);
        println!("matching + ranking, 300 people / 300 commitments / 300 loops / 60 projects, 1.2k excerpt: p50={p50}us p95={p95}us");
        return;
    }
    match fusion_eval::evaluate() {
        Ok(scores) => print!("{}", fusion_eval::report::render(&scores)),
        Err(e) => {
            eprintln!("fusion-eval: {e}");
            std::process::exit(1);
        }
    }
}
