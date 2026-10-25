//! Markdown rendering of a [`Score`]. Numbers only — no event text, no descriptions — so a report
//! can be pasted into a PR or a CI log without carrying content (CLAUDE.md log rule; the data is
//! synthetic anyway, but the habit is the point).

use std::fmt::Write as _;

use crate::golden::{lowest_confidence_in, BandLabel, Table};
use crate::score::{Counts, Score};

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_string(), |x| format!("{:.1}%", x * 100.0))
}

fn num(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_string(), |x| format!("{x:.3}"))
}

fn row(out: &mut String, label: &str, c: &Counts) {
    let _ = writeln!(
        out,
        "| {label} | {} | {} | {} | {} | {} | {} | {} |",
        c.tp,
        c.hallucinated,
        c.omitted,
        c.wrong_value,
        pct(c.precision()),
        pct(c.recall()),
        pct(c.f1())
    );
}

fn band_range(b: BandLabel) -> String {
    let lo = lowest_confidence_in(b);
    match b {
        BandLabel::High => format!("[{lo:.1}, 1.0]"),
        BandLabel::Medium => format!("[{lo:.1}, {:.1})", lowest_confidence_in(BandLabel::High)),
        BandLabel::Low => format!("[0.0, {:.1})", lowest_confidence_in(BandLabel::Medium)),
    }
}

/// The one-line headline used in the PR table.
pub fn headline(s: &Score) -> String {
    let i = s.items_total();
    let f = s.fields_total();
    format!(
        "items P {} / R {} / F1 {} · fields F1 {} · provenance {} · ECE {}",
        pct(i.precision()),
        pct(i.recall()),
        pct(i.f1()),
        pct(f.f1()),
        pct(s.provenance.validity()),
        num(s.ece())
    )
}

/// Full Markdown report for one runner.
pub fn markdown(title: &str, s: &Score) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### {title}\n");
    let _ = writeln!(
        out,
        "{} days · {} events · {} gold rows · {} predicted rows (after dedup) · {} unscored entities\n",
        s.days, s.events, s.gold, s.predicted, s.unscored_entities
    );
    let header = "| | tp | hallucinated | omitted | wrong value | precision | recall | F1 |\n|---|---:|---:|---:|---:|---:|---:|---:|";
    let _ = writeln!(out, "**Rows** (does the row exist?)\n\n{header}");
    for t in Table::ALL {
        row(&mut out, t.as_str(), &s.item(t));
    }
    row(&mut out, "**all**", &s.items_total());
    let _ = writeln!(out, "\n**Fields**\n\n{header}");
    for t in Table::ALL {
        for f in t.fields() {
            row(&mut out, &format!("{}.{f}", t.as_str()), &s.field(t, f));
        }
    }
    row(&mut out, "**all (micro)**", &s.fields_total());

    let p = &s.provenance;
    let _ = writeln!(
        out,
        "\n**Provenance**: {} citations · {} valid · {} dangling · {} unsupported · validity {}",
        p.citations,
        p.valid,
        p.dangling,
        p.unsupported,
        pct(p.validity())
    );

    let _ = writeln!(
        out,
        "\n**Calibration** (buckets = `shogun_fusion::confidence::band`)\n\n| band | range | n | mean confidence | accuracy | promise | kept |\n|---|---|---:|---:|---:|---:|---|"
    );
    for b in BandLabel::ALL {
        let bucket = s.calibration.get(&b).copied().unwrap_or_default();
        let promise = b.promised_accuracy();
        let kept = match (promise, bucket.accuracy()) {
            (Some(p), Some(a)) => if a >= p { "yes" } else { "**no**" },
            (None, _) => "n/a",
            (_, None) => "—",
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {kept} |",
            b.as_str(),
            band_range(b),
            bucket.n,
            num(bucket.mean_confidence()),
            pct(bucket.accuracy()),
            promise.map_or_else(|| "—".to_string(), |x| format!("≥ {:.0}%", x * 100.0)),
        );
    }
    let _ = writeln!(
        out,
        "\nECE {} · matched rows over-confident vs gold band: {} · under-confident: {}",
        num(s.ece()),
        s.overconfident(),
        s.underconfident()
    );

    let _ = writeln!(out, "\n**By template** (where the errors come from)\n\n| template | gold | matched | omitted | hallucinated |\n|---|---:|---:|---:|---:|");
    for (k, v) in &s.by_template {
        let _ = writeln!(out, "| {k} | {} | {} | {} | {} |", v.gold, v.matched, v.omitted, v.hallucinated);
    }
    out
}
