//! Markdown rendering of the eval — numbers and synthetic scenario ids only.

use crate::score::{summarize, ScenarioScore, Summary};

fn row(label: &str, s: &Summary) -> String {
    format!(
        "| {label} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {}/{} ({:.0}%) | {} | {} |\n",
        s.scenarios,
        s.precision_at_3,
        s.recall_at_4,
        s.mrr,
        s.first_hit_rate,
        s.irrelevant,
        s.buttons,
        s.irrelevant_rate * 100.0,
        s.no_op,
        s.unreviewed_sends,
    )
}

/// The summary table (all / en / ja) followed by one line per scenario.
pub fn render(scores: &[ScenarioScore]) -> String {
    let mut out = String::from(
        "| set | n | P@3 | R@4 | MRR | first-hit | irrelevant buttons | no-op buttons | L3 from unreviewed |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    out.push_str(&row("core", &summarize(scores.iter().filter(|s| !s.hard))));
    for lang in ["en", "ja"] {
        out.push_str(&row(lang, &summarize(scores.iter().filter(|s| !s.hard && s.lang == lang))));
    }
    out.push_str(&row("hard", &summarize(scores.iter().filter(|s| s.hard))));
    out.push_str("\n| scenario | P@3 | RR | buttons |\n|---|---|---|---|\n");
    for s in scores {
        let buttons: Vec<String> =
            s.buttons.iter().map(|b| format!("{}:{}", b.target, b.kind)).collect();
        out.push_str(&format!(
            "| {} | {:.2} | {} | {} |\n",
            s.id,
            s.precision_at_3,
            s.reciprocal_rank.map(|r| format!("{r:.2}")).unwrap_or_else(|| "-".into()),
            buttons.join(", ")
        ));
    }
    out
}
