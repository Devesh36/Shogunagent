//! Retrieval metrics for the AI search evaluation (issue #940).
//!
//! recall@k is the fraction of a case's relevant results that appear in the top k, averaged over
//! cases; MRR is the mean of 1 / (rank of the first relevant result), 0 when none was returned.
//! Pure, so the harness (`tests/ai_search_eval.rs`) and any later bench score the same way.

/// Mean scores over a set of cases.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scores {
    pub cases: usize,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

/// Fraction of `relevant` found in the first `k` of `ranked`. A case with no relevant result
/// scores 1.0 (nothing to miss); the harness never builds one.
pub fn recall_at(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    let top = &ranked[..ranked.len().min(k)];
    let found = relevant.iter().filter(|key| top.contains(key)).count();
    found as f64 / relevant.len() as f64
}

/// 1 / rank of the first relevant result, or 0.
pub fn reciprocal_rank(ranked: &[String], relevant: &[String]) -> f64 {
    ranked
        .iter()
        .position(|key| relevant.contains(key))
        .map_or(0.0, |index| 1.0 / (index as f64 + 1.0))
}

/// Mean recall@5, recall@10 and MRR over `(ranked, relevant)` pairs.
pub fn score<'a>(cases: impl IntoIterator<Item = (&'a [String], &'a [String])>) -> Scores {
    let mut out = Scores::default();
    for (ranked, relevant) in cases {
        out.cases += 1;
        out.recall_at_5 += recall_at(ranked, relevant, 5);
        out.recall_at_10 += recall_at(ranked, relevant, 10);
        out.mrr += reciprocal_rank(ranked, relevant);
    }
    if out.cases > 0 {
        let n = out.cases as f64;
        out.recall_at_5 /= n;
        out.recall_at_10 /= n;
        out.mrr /= n;
    }
    out
}

/// A Markdown table: one row per `(label, keyword, ai)` triple.
pub fn render_table(rows: &[(String, Scores, Scores)]) -> String {
    let mut out = String::from(
        "| category | cases | keyword R@5 | keyword R@10 | keyword MRR | AI R@5 | AI R@10 | AI MRR |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for (label, keyword, ai) in rows {
        out.push_str(&format!(
            "| {label} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
            keyword.cases,
            keyword.recall_at_5,
            keyword.recall_at_10,
            keyword.mrr,
            ai.recall_at_5,
            ai.recall_at_10,
            ai.mrr,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recall_and_mrr_follow_their_definitions() {
        let ranked = keys(&["a", "b", "c", "d", "e", "f"]);
        assert_eq!(recall_at(&ranked, &keys(&["b", "f"]), 5), 0.5);
        assert_eq!(recall_at(&ranked, &keys(&["b", "f"]), 10), 1.0);
        assert_eq!(reciprocal_rank(&ranked, &keys(&["c"])), 1.0 / 3.0);
        assert_eq!(reciprocal_rank(&ranked, &keys(&["z"])), 0.0);
        let relevant = keys(&["c"]);
        let empty: Vec<String> = Vec::new();
        let s = score([(ranked.as_slice(), relevant.as_slice()), (empty.as_slice(), relevant.as_slice())]);
        assert_eq!(s.cases, 2);
        assert_eq!(s.recall_at_5, 0.5);
        assert!((s.mrr - 1.0 / 6.0).abs() < 1e-9);
        assert!(render_table(&[("all".into(), s, s)]).contains("| all | 2 | 0.50"));
    }
}
