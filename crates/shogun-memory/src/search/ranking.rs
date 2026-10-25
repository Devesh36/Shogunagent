/// Reciprocal Rank Fusion over several ranked id lists (each best-first, 1-based rank). The
/// score of an id is `Σ_lists 1/(k + rank)`; `k` damps the influence of low ranks (60 is the
/// canonical default). Ids are returned sorted by descending score; ties break by smaller id
/// for determinism. Pure — no DB, fully unit-tested.
pub fn reciprocal_rank_fusion(lists: &[&[i64]], k: f64) -> Vec<(i64, f64)> {
    use std::collections::HashMap;
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (i, &id) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            *scores.entry(id).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut out: Vec<(i64, f64)> = scores.into_iter().collect();
    // Descending score; deterministic tie-break by ascending id.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

/// Break near-ties in fused scores by recency, in place.
///
/// Nothing else in the ranking path reads time. `reciprocal_rank_fusion` scores purely on
/// lexical rank, and in the shipped configuration (no embedder — CLAUDE.md 2026-08-23 / #237) that
/// rank *is* bm25 alone. Where a fact is restated with a new value, the superseded row otherwise
/// reaches rank 1 in every query, because bm25 normalises by document length and the older
/// sentence happens to be shorter.
///
/// **A tie-break, not a weight.** Not `weight * normalised_recency` added to every score: RRF
/// adjacent-rank gaps are about 0.0003, so even a 0.001 weight swamps relevance (measured: `clean`
/// recall@1 drops from 1.000 to 0.213). Recency is only ever the right signal *between rows the
/// lexical scorer cannot separate* — which is exactly the failing case, two near-identical
/// sentences differing only in their value. So rows are grouped into runs whose scores lie within
/// `epsilon` of the run leader, and only within a run does the newest win. A row with a
/// meaningfully better lexical score cannot be displaced at all.
///
/// `epsilon <= 0.0` is an exact no-op.
///
/// Ordering only: this runs over an already-retrieved window, so recall@k cannot move.
///
/// `recency` must be **`last_seen_at`, not `ts`**. `insert_or_touch` collapses a byte-identical
/// restatement onto the original row and advances `last_seen_at` while leaving `ts` at the first
/// sighting. Ranking on `ts` therefore makes a re-asserted current value look *older* than the
/// stale one that replaced it.
pub fn apply_recency(scored: &mut [(i64, f64, i64)], epsilon: f64) {
    if epsilon <= 0.0 || scored.len() < 2 {
        return;
    }
    // Descending score first, so runs of comparable relevance are contiguous.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let mut start = 0usize;
    while start < scored.len() {
        let leader = scored[start].1;
        let mut end = start + 1;
        while end < scored.len() && (leader - scored[end].1).abs() <= epsilon {
            end += 1;
        }
        if end - start > 1 {
            // Newest first; ascending id keeps it deterministic when timestamps also tie.
            scored[start..end].sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        }
        start = end;
    }
}

#[cfg(test)]
mod recency_tests {
    use super::*;

    #[test]
    fn zero_epsilon_is_an_exact_no_op() {
        let original = vec![(1i64, 0.5f64, 100i64), (2, 0.4, 900)];
        let mut scored = original.clone();
        apply_recency(&mut scored, 0.0);
        assert_eq!(scored, original);
        apply_recency(&mut scored, -1.0);
        assert_eq!(scored, original, "a negative epsilon must not reorder either");
    }

    #[test]
    fn a_near_tie_is_broken_by_recency() {
        // The measured failure: two near-identical sentences, the stale one ahead by a hair.
        let mut scored = vec![(1i64, 0.01640f64, 100i64), (2, 0.01610, 900)];
        apply_recency(&mut scored, 0.001);
        assert_eq!(scored[0].0, 2, "within epsilon, the newer row leads");
    }

    #[test]
    fn a_clear_lexical_winner_is_never_displaced() {
        // Relevance must dominate recency.
        let mut scored = vec![(1i64, 0.9f64, 100i64), (2, 0.01, 900)];
        apply_recency(&mut scored, 0.001);
        assert_eq!(scored[0].0, 1);
    }

    #[test]
    fn runs_are_broken_independently() {
        // Two separate near-tie groups; recency reorders inside each, never across.
        let mut scored = vec![
            (1i64, 0.90f64, 100i64),
            (2, 0.899, 900),
            (3, 0.10, 100),
            (4, 0.099, 900),
        ];
        apply_recency(&mut scored, 0.01);
        assert_eq!(scored[0].0, 2, "newer wins the high-score run");
        assert_eq!(scored[1].0, 1);
        assert_eq!(scored[2].0, 4, "newer wins the low-score run");
        assert!(scored[0].1 > scored[2].1, "runs never interleave");
    }

    #[test]
    fn identical_timestamps_fall_back_to_a_deterministic_order() {
        let mut scored = vec![(2i64, 0.5f64, 500i64), (1, 0.5, 500)];
        apply_recency(&mut scored, 0.001);
        assert_eq!(scored[0].0, 1, "tie on score and time breaks by ascending id");
    }

    #[test]
    fn the_set_is_never_changed_only_reordered() {
        let mut scored = vec![(1i64, 0.10f64, 10i64), (2, 0.101, 20), (3, 0.099, 30)];
        apply_recency(&mut scored, 0.01);
        let mut ids: Vec<i64> = scored.iter().map(|s| s.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3], "recall@k cannot move: same ids, new order");
    }

    #[test]
    fn a_single_candidate_is_untouched() {
        let mut scored = vec![(7i64, 0.3f64, 42i64)];
        apply_recency(&mut scored, 1.0);
        assert_eq!(scored, vec![(7, 0.3, 42)]);
    }
}
