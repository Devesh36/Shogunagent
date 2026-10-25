//! Time-bucketed corpus slicing — bounding how much capture enters the expensive pipeline.
//!
//! The question this exists to answer is Toru's: *"it really comes down to accuracy and cost."*
//! A slicer trades one for the other, and neither side of that trade was measurable before.
//!
//! **What the measurements say the cost actually is.** Write latency is not the constraint:
//! `BASELINE.md` puts write p95 at 0.36 ms and 100k events at 60 MB (~640 B/row). What *does* grow
//! is retrieval — p50 tracks corpus size close to linearly, 10k→100k being 10× the data and 9.8×
//! the p50 — and everything downstream that pays per item (Dream Cycle classification, Batch API
//! chunks). So slicing is not an "ingestion overload" fix; it is a **retrieval-latency and
//! model-cost** fix, and framing it as the former would optimise the one number that is already
//! fine.
//!
//! **The accuracy side has a hard ceiling.** If a slice drops the event that answers a question,
//! no retrieval improvement can recover it — recall is capped by what survived ingestion. So the
//! headline metric here is `answer_retention`: the share of answer-bearing *facts* a slice keeps.
//! It is the ceiling every downstream quality number sits under, and reporting rows-saved without
//! it would be reporting half a trade.

use serde::Serialize;

use crate::workload::{BenchEvent, GeneratedWorkload};

/// Milliseconds in a day, for bucketing a corpus whose timestamps are synthetic but ordered.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How wide a time bucket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Hour,
    /// Morning / afternoon / evening / night — the "parts of the day" split.
    PartOfDay,
    Day,
}

impl Bucket {
    pub fn width_ms(self) -> i64 {
        match self {
            Bucket::Hour => 60 * 60 * 1000,
            Bucket::PartOfDay => DAY_MS / 4,
            Bucket::Day => DAY_MS,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::Hour => "hour",
            Bucket::PartOfDay => "part-of-day",
            Bucket::Day => "day",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hour" => Some(Bucket::Hour),
            "part-of-day" | "part" => Some(Bucket::PartOfDay),
            "day" => Some(Bucket::Day),
            _ => None,
        }
    }
}

/// Which events survive when a bucket is over budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlicePolicy {
    /// Keep the first N of the bucket. The naive choice, and the one to beat: it throws away the
    /// entire back half of every busy period.
    Head,
    /// Keep N evenly spaced across the bucket, preserving temporal coverage.
    Uniform,
    /// Keep the N with the highest `dwell_ms` — attention as a relevance proxy. Ties fall back to
    /// earliest-first so the choice stays deterministic.
    Dwell,
}

impl SlicePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SlicePolicy::Head => "head",
            SlicePolicy::Uniform => "uniform",
            SlicePolicy::Dwell => "dwell",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "head" => Some(SlicePolicy::Head),
            "uniform" => Some(SlicePolicy::Uniform),
            "dwell" => Some(SlicePolicy::Dwell),
            _ => None,
        }
    }
}

/// A slicing configuration. `budget` of `None` is an exact no-op.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SliceConfig {
    pub bucket: Bucket,
    /// Events kept per bucket. `None` disables slicing entirely.
    pub budget: Option<usize>,
    pub policy: SlicePolicy,
}

impl Default for SliceConfig {
    fn default() -> Self {
        Self { bucket: Bucket::Day, budget: None, policy: SlicePolicy::Uniform }
    }
}

impl SliceConfig {
    pub fn is_disabled(&self) -> bool {
        self.budget.is_none()
    }
}

/// What a slice actually did.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SliceReport {
    pub events_before: usize,
    pub events_after: usize,
    pub buckets: usize,
    /// Buckets that exceeded the budget and therefore lost events.
    pub buckets_over_budget: usize,
    /// Answer-bearing events in the corpus before slicing.
    pub answers_before: usize,
    /// Answer-bearing events that survived.
    pub answers_after: usize,
    /// Distinct answer *facts* present before slicing.
    pub answer_facts_before: usize,
    /// Distinct answer facts still carried by at least one surviving event.
    ///
    /// This, not [`Self::answers_after`], is what bounds recall: a query is answerable when **any**
    /// event carrying its fact survives, so dropping four of five copies of a fact costs nothing.
    /// On a corpus with no redundancy the two counts are equal by construction, which is why the
    /// distinction does not move the `clean` numbers.
    pub answer_facts_after: usize,
}

impl SliceReport {
    /// Share of events kept. The cost side of the trade.
    pub fn retention(&self) -> Option<f64> {
        (self.events_before > 0).then(|| self.events_after as f64 / self.events_before as f64)
    }

    /// Share of answer-bearing events kept — the **ceiling on recall** the slice imposes.
    ///
    /// This is the number that makes the trade legible. A slice that keeps 30% of rows but only
    /// 60% of answers has capped recall at 0.60 before retrieval has done anything at all.
    pub fn answer_retention(&self) -> Option<f64> {
        (self.answer_facts_before > 0)
            .then(|| self.answer_facts_after as f64 / self.answer_facts_before as f64)
    }

    /// Share of answer-bearing *events* kept — the copy-level view.
    ///
    /// Reported next to [`Self::answer_retention`] because the gap between the two is precisely the
    /// redundancy the slice was able to exploit. They are equal when every fact is stated once.
    pub fn answer_event_retention(&self) -> Option<f64> {
        (self.answers_before > 0).then(|| self.answers_after as f64 / self.answers_before as f64)
    }
}

/// Apply a slice in place, returning what it did.
///
/// `answer_facts` are the fact ids the query set expects, so retention can be reported for the
/// events that actually matter rather than for the corpus as a whole. Passing an empty slice
/// leaves `answer_retention` as `None` rather than a misleading 1.0.
pub fn apply(
    w: &mut GeneratedWorkload,
    cfg: &SliceConfig,
    answer_facts: &[String],
) -> SliceReport {
    // A linear scan per event is O(events x answers). Fine here — answer sets are in the
    // hundreds and this runs once, before ingest — but it is the first thing to change if the
    // answer set ever grows to corpus scale.
    let is_answer = |e: &BenchEvent| answer_facts.contains(&e.fact_id);
    let distinct_answer_facts = |events: &[BenchEvent]| -> usize {
        let mut seen: Vec<&str> = events
            .iter()
            .filter(|e| is_answer(e))
            .map(|e| e.fact_id.as_str())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };
    let mut report = SliceReport {
        events_before: w.events.len(),
        answers_before: w.events.iter().filter(|e| is_answer(e)).count(),
        answer_facts_before: distinct_answer_facts(&w.events),
        ..Default::default()
    };
    let Some(budget) = cfg.budget else {
        report.events_after = w.events.len();
        report.answers_after = report.answers_before;
        report.answer_facts_after = report.answer_facts_before;
        return report;
    };

    // Group by bucket. Events arrive in timestamp order, so a single pass suffices and the
    // grouping stays stable — a shuffle here would make the slice non-reproducible from the seed.
    let width = cfg.bucket.width_ms().max(1);
    let mut kept: Vec<BenchEvent> = Vec::with_capacity(w.events.len().min(budget * 8));
    let mut bucket_start = 0usize;
    let mut buckets = 0usize;
    let mut over = 0usize;

    while bucket_start < w.events.len() {
        let key = w.events[bucket_start].ts / width;
        let mut end = bucket_start + 1;
        while end < w.events.len() && w.events[end].ts / width == key {
            end += 1;
        }
        buckets += 1;
        let group = &w.events[bucket_start..end];
        if group.len() <= budget {
            kept.extend_from_slice(group);
        } else {
            over += 1;
            kept.extend(select(group, budget, cfg.policy));
        }
        bucket_start = end;
    }

    // Timestamp order is restored because `Dwell` picks out of order, and an unordered event_log
    // would make every downstream latency and Cold-partition number describe a corpus the product
    // would never produce.
    kept.sort_by_key(|e| e.ts);
    report.events_after = kept.len();
    report.answers_after = kept.iter().filter(|e| is_answer(e)).count();
    report.answer_facts_after = distinct_answer_facts(&kept);
    report.buckets = buckets;
    report.buckets_over_budget = over;
    w.events = kept;
    report
}

/// Choose `budget` events from an over-budget bucket.
fn select(group: &[BenchEvent], budget: usize, policy: SlicePolicy) -> Vec<BenchEvent> {
    match policy {
        SlicePolicy::Head => group[..budget].to_vec(),
        SlicePolicy::Uniform => {
            // Evenly spaced indices spanning the bucket, both endpoints included. Integer
            // arithmetic rather than a float step so the selection is bit-identical on every
            // platform; `min(n - 1)` is the guard that a rounding edge cannot index past the end.
            let n = group.len();
            if budget <= 1 {
                return vec![group[0].clone()];
            }
            (0..budget)
                .map(|i| {
                    let idx = (i * (n - 1)) / (budget - 1);
                    group[idx.min(n - 1)].clone()
                })
                .collect()
        }
        SlicePolicy::Dwell => {
            let mut idx: Vec<usize> = (0..group.len()).collect();
            idx.sort_by(|a, b| {
                group[*b]
                    .dwell_ms
                    .cmp(&group[*a].dwell_ms)
                    .then(group[*a].ts.cmp(&group[*b].ts))
            });
            idx.truncate(budget);
            idx.sort_unstable();
            idx.into_iter().map(|i| group[i].clone()).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(n: usize, step_ms: i64) -> GeneratedWorkload {
        GeneratedWorkload {
            name: "test",
            events: (0..n)
                .map(|i| BenchEvent {
                    ts: (i as i64 + 1) * step_ms,
                    content: format!("event {i}"),
                    fact_id: format!("f{i}"),
                    dwell_ms: (i % 7) as i64 * 1000,
                    source: "capture".into(),
                    kind: "text".into(),
                    app_bundle_id: None,
                    window_title: None,
                })
                .collect(),
            queries: Vec::new(),
        }
    }

    #[test]
    fn no_budget_is_an_exact_no_op() {
        let mut w = corpus(100, 60_000);
        let before = w.events.len();
        let r = apply(&mut w, &SliceConfig::default(), &[]);
        assert_eq!(w.events.len(), before);
        assert_eq!(r.events_after, before);
        assert_eq!(r.retention(), Some(1.0));
    }

    #[test]
    fn a_budget_caps_each_bucket_independently() {
        // Asserted as an invariant rather than a magic total: how many day-buckets 48 hourly
        // events land in depends on the corpus's start offset, and an exact count would be
        // testing the fixture instead of the slicer.
        let mut w = corpus(48, 60 * 60 * 1000);
        let budget = 5usize;
        let cfg =
            SliceConfig { bucket: Bucket::Day, budget: Some(budget), policy: SlicePolicy::Head };
        let r = apply(&mut w, &cfg, &[]);
        assert!(r.buckets >= 2, "48 hourly events must span more than one day");
        assert!(r.events_after < r.events_before, "a biting budget must drop events");

        // The invariant that matters: no bucket survives over budget.
        let width = Bucket::Day.width_ms();
        let mut counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for e in &w.events {
            *counts.entry(e.ts / width).or_default() += 1;
        }
        assert!(
            counts.values().all(|c| *c <= budget),
            "a bucket exceeded its budget: {counts:?}"
        );
    }

    #[test]
    fn a_bucket_under_budget_is_untouched() {
        let mut w = corpus(3, 60 * 60 * 1000);
        let cfg = SliceConfig { bucket: Bucket::Day, budget: Some(10), policy: SlicePolicy::Head };
        let r = apply(&mut w, &cfg, &[]);
        assert_eq!(r.events_after, 3);
        assert_eq!(r.buckets_over_budget, 0);
    }

    #[test]
    fn slicing_preserves_timestamp_order() {
        // Dwell selects out of order; an unordered log would misrepresent every later measurement.
        let mut w = corpus(200, 60_000);
        let cfg = SliceConfig { bucket: Bucket::Hour, budget: Some(4), policy: SlicePolicy::Dwell };
        apply(&mut w, &cfg, &[]);
        assert!(w.events.windows(2).all(|p| p[0].ts <= p[1].ts), "log must stay ordered");
    }

    #[test]
    fn head_keeps_the_start_and_uniform_spans_the_bucket() {
        // The reason `uniform` is the default: `head` discards the back of every busy period.
        let mut head = corpus(600, 60_000);
        let mut uni = corpus(600, 60_000);
        let mk = |p| SliceConfig { bucket: Bucket::Day, budget: Some(3), policy: p };
        apply(&mut head, &mk(SlicePolicy::Head), &[]);
        apply(&mut uni, &mk(SlicePolicy::Uniform), &[]);
        let last_head = head.events.last().expect("kept").ts;
        let last_uni = uni.events.last().expect("kept").ts;
        assert!(
            last_uni > last_head,
            "uniform should reach later into the bucket than head ({last_uni} vs {last_head})"
        );
    }

    #[test]
    fn uniform_never_indexes_past_the_bucket() {
        for n in [2usize, 3, 7, 50, 999] {
            for budget in [1usize, 2, 3, 10, 49] {
                if budget >= n {
                    continue;
                }
                let mut w = corpus(n, 60_000);
                let cfg = SliceConfig {
                    bucket: Bucket::Day,
                    budget: Some(budget),
                    policy: SlicePolicy::Uniform,
                };
                let r = apply(&mut w, &cfg, &[]);
                assert!(r.events_after <= n, "n={n} budget={budget}");
                assert!(!w.events.is_empty(), "n={n} budget={budget}");
            }
        }
    }

    #[test]
    fn answer_retention_is_reported_against_the_answer_set() {
        let mut w = corpus(40, 60_000);
        // Two answers, one early and one late in the same hour bucket.
        let answers = vec!["f0".to_string(), "f39".to_string()];
        let cfg = SliceConfig { bucket: Bucket::Hour, budget: Some(2), policy: SlicePolicy::Head };
        let r = apply(&mut w, &cfg, &answers);
        assert_eq!(r.answers_before, 2);
        // Head keeps the first two events, so the late answer is dropped: the recall ceiling is 0.5.
        assert_eq!(r.answers_after, 1);
        assert_eq!(r.answer_retention(), Some(0.5));
    }

    #[test]
    fn an_empty_answer_set_reports_no_retention_rather_than_a_perfect_one() {
        let mut w = corpus(10, 60_000);
        let cfg = SliceConfig { bucket: Bucket::Day, budget: Some(2), policy: SlicePolicy::Head };
        let r = apply(&mut w, &cfg, &[]);
        assert!(r.answer_retention().is_none(), "no denominator is not 1.0");
    }

    #[test]
    fn slicing_is_deterministic() {
        let cfg = SliceConfig { bucket: Bucket::Hour, budget: Some(5), policy: SlicePolicy::Dwell };
        let (mut a, mut b) = (corpus(300, 60_000), corpus(300, 60_000));
        apply(&mut a, &cfg, &[]);
        apply(&mut b, &cfg, &[]);
        let ids = |w: &GeneratedWorkload| -> Vec<String> {
            w.events.iter().map(|e| e.fact_id.clone()).collect()
        };
        assert_eq!(ids(&a), ids(&b));
    }

    #[test]
    fn bucket_and_policy_names_round_trip() {
        for b in [Bucket::Hour, Bucket::PartOfDay, Bucket::Day] {
            assert_eq!(Bucket::parse(b.as_str()), Some(b));
        }
        for p in [SlicePolicy::Head, SlicePolicy::Uniform, SlicePolicy::Dwell] {
            assert_eq!(SlicePolicy::parse(p.as_str()), Some(p));
        }
        assert!(Bucket::parse("fortnight").is_none());
    }
}
