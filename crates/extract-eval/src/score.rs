//! The extraction scorer (FR-EV-03): per-field precision / recall / F1 with hallucinations and
//! omissions counted separately, provenance validity, and confidence calibration on the
//! product's own band boundaries.
//!
//! Pure code. The scorer never calls a model and never judges meaning beyond token overlap: a
//! prediction matches a gold row when it is in the same table, cites at least one of the gold
//! row's evidence events, and its key (`description` / `name`) overlaps enough
//! ([`MATCH_THRESHOLD`]). Anything subtler is judge territory and deferred (docs/extract-eval.md §5).
//!
//! Counting, per `(table, field)`:
//! - **tp**: a matched prediction whose value is right;
//! - **hallucinated** (precision denominator): a value the prediction states that no gold row
//!   backs — every field of an unmatched prediction, a wrong value on a matched one, or a value
//!   where gold has none (a `due` the sentence never stated);
//! - **omitted** (recall denominator): a gold value not recovered — every field of an unmatched
//!   gold row, a wrong or missing value on a matched one;
//! - **wrong_value**: the subset of both that are a matched pair disagreeing, reported so a wrong
//!   enum is not mistaken for a pure hallucination.

use std::collections::{BTreeMap, BTreeSet};

use crate::golden::{BandLabel, GoldItem, SynthDay, Table};

/// Minimum key similarity for a prediction to match a gold row.
pub const MATCH_THRESHOLD: f64 = 0.5;

/// Minimum share of a description's tokens that must occur in the cited event for the citation to
/// count as support, when the description is not a literal span of it (a model may paraphrase).
pub const SUPPORT_COVERAGE: f64 = 0.6;

/// What a prediction offers as its support in the cited event.
#[derive(Debug, Clone, PartialEq)]
pub enum Support {
    /// A commitment / open-loop description: must be a span of, or mostly made of, the event text.
    Text(String),
    /// An entity: the product's own rule (`shogun_memory::knowledge::persist`) — the quote is a
    /// literal span of the event and contains both the name and the context.
    Entity { name: String, context: String, quote: String },
}

/// One state row an extractor proposed from one event, before cross-event dedup.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPrediction {
    pub table: Table,
    pub fields: BTreeMap<String, String>,
    pub confidence: f64,
    /// The event this row cites as evidence.
    pub event_id: i64,
    pub support: Support,
}

/// Hallucination / omission counts for one table or field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub tp: usize,
    pub hallucinated: usize,
    pub omitted: usize,
    pub wrong_value: usize,
}

fn ratio(num: usize, den: usize) -> Option<f64> {
    (den > 0).then(|| num as f64 / den as f64)
}

impl Counts {
    pub fn precision(&self) -> Option<f64> {
        ratio(self.tp, self.tp + self.hallucinated)
    }
    pub fn recall(&self) -> Option<f64> {
        ratio(self.tp, self.tp + self.omitted)
    }
    pub fn f1(&self) -> Option<f64> {
        let (p, r) = (self.precision()?, self.recall()?);
        Some(if p + r == 0.0 { 0.0 } else { 2.0 * p * r / (p + r) })
    }
    fn add(&mut self, o: &Counts) {
        self.tp += o.tp;
        self.hallucinated += o.hallucinated;
        self.omitted += o.omitted;
        self.wrong_value += o.wrong_value;
    }
}

/// Provenance validity over every citation (FR-ST-02: every state row links to its evidence).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProvenanceCounts {
    pub citations: usize,
    pub valid: usize,
    /// Cites an event id that does not exist in the day.
    pub dangling: usize,
    /// Cites a real event whose text does not support the row.
    pub unsupported: usize,
}

impl ProvenanceCounts {
    pub fn validity(&self) -> Option<f64> {
        ratio(self.valid, self.citations)
    }
}

/// One calibration bucket (one confidence band).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bucket {
    pub n: usize,
    pub sum_confidence: f64,
    pub correct: usize,
}

impl Bucket {
    pub fn mean_confidence(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum_confidence / self.n as f64)
    }
    pub fn accuracy(&self) -> Option<f64> {
        ratio(self.correct, self.n)
    }
}

/// Per-template diagnostics: where the hallucinations and omissions come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TemplateCounts {
    pub gold: usize,
    pub matched: usize,
    pub omitted: usize,
    pub hallucinated: usize,
}

/// The score for one runner over one or more days. Every part is a count, so days add up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Score {
    pub days: usize,
    pub events: usize,
    pub gold: usize,
    pub predicted: usize,
    pub items: BTreeMap<Table, Counts>,
    pub fields: BTreeMap<(Table, &'static str), Counts>,
    pub provenance: ProvenanceCounts,
    pub calibration: BTreeMap<BandLabel, Bucket>,
    /// Matched pairs by `(expected band, predicted band)`.
    pub band_agreement: BTreeMap<(BandLabel, BandLabel), usize>,
    /// Entities of a kind the eval does not score yet (topic, tool, organization, …).
    pub unscored_entities: usize,
    pub by_template: BTreeMap<String, TemplateCounts>,
}

impl Score {
    pub fn merge(&mut self, o: &Score) {
        self.days += o.days;
        self.events += o.events;
        self.gold += o.gold;
        self.predicted += o.predicted;
        for (k, v) in &o.items {
            self.items.entry(*k).or_default().add(v);
        }
        for (k, v) in &o.fields {
            self.fields.entry(*k).or_default().add(v);
        }
        self.provenance.citations += o.provenance.citations;
        self.provenance.valid += o.provenance.valid;
        self.provenance.dangling += o.provenance.dangling;
        self.provenance.unsupported += o.provenance.unsupported;
        for (k, v) in &o.calibration {
            let b = self.calibration.entry(*k).or_default();
            b.n += v.n;
            b.sum_confidence += v.sum_confidence;
            b.correct += v.correct;
        }
        for (k, v) in &o.band_agreement {
            *self.band_agreement.entry(*k).or_default() += v;
        }
        self.unscored_entities += o.unscored_entities;
        for (k, v) in &o.by_template {
            let t = self.by_template.entry(k.clone()).or_default();
            t.gold += v.gold;
            t.matched += v.matched;
            t.omitted += v.omitted;
            t.hallucinated += v.hallucinated;
        }
    }

    /// Item-level counts over every table.
    pub fn items_total(&self) -> Counts {
        let mut c = Counts::default();
        for v in self.items.values() {
            c.add(v);
        }
        c
    }

    /// Field-level counts over every table and field (micro average).
    pub fn fields_total(&self) -> Counts {
        let mut c = Counts::default();
        for v in self.fields.values() {
            c.add(v);
        }
        c
    }

    pub fn item(&self, t: Table) -> Counts {
        self.items.get(&t).copied().unwrap_or_default()
    }

    pub fn field(&self, t: Table, f: &str) -> Counts {
        self.fields
            .iter()
            .find(|((tt, ff), _)| *tt == t && *ff == f)
            .map(|(_, c)| *c)
            .unwrap_or_default()
    }

    /// Expected calibration error over the band buckets: Σ (n_b / N) · |accuracy_b − confidence_b|.
    /// Three buckets on the product's band edges, not ten equal-width bins — the question FR-ST-20
    /// asks is whether each *band* keeps its promise.
    pub fn ece(&self) -> Option<f64> {
        let n: usize = self.calibration.values().map(|b| b.n).sum();
        if n == 0 {
            return None;
        }
        Some(
            self.calibration
                .values()
                .filter_map(|b| Some(b.n as f64 / n as f64 * (b.accuracy()? - b.mean_confidence()?).abs()))
                .sum(),
        )
    }

    /// Bands whose predictions are right less often than [`BandLabel::promised_accuracy`] says.
    /// Empty buckets promise nothing.
    pub fn band_promise_violations(&self) -> Vec<BandLabel> {
        self.calibration
            .iter()
            .filter(|(band, b)| match (band.promised_accuracy(), b.accuracy()) {
                (Some(p), Some(a)) => a < p,
                _ => false,
            })
            .map(|(band, _)| *band)
            .collect()
    }

    /// Matched predictions whose band is stronger than the gold row's expected band.
    pub fn overconfident(&self) -> usize {
        self.band_agreement.iter().filter(|((e, p), _)| p > e).map(|(_, n)| n).sum()
    }

    /// Matched predictions whose band is weaker than the gold row's expected band.
    pub fn underconfident(&self) -> usize {
        self.band_agreement.iter().filter(|((e, p), _)| p < e).map(|(_, n)| n).sum()
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x309F   // hiragana
        | 0x30A0..=0x30FF // katakana
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xFF66..=0xFF9F // halfwidth katakana
    )
}

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "to", "for", "of", "on", "in", "at", "by", "and", "or", "i", "ll", "we", "you",
    "it", "is", "be", "with", "s", "this", "that", "will",
];

/// Content tokens: lowercased alphanumeric words minus a small stopword list, and character
/// bigrams over CJK runs (Japanese has no spaces). Hidden format characters are stripped first.
pub fn tokens(s: &str) -> Vec<String> {
    let stripped = shogun_redact::strip_hidden(s);
    let lower = stripped.text.to_lowercase();
    let mut out = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() && !STOPWORDS.contains(&word.as_str()) {
            out.push(std::mem::take(word));
        }
        word.clear();
    };
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<String>| {
        match run.len() {
            0 => {}
            1 => out.push(run.iter().collect()),
            _ => out.extend(run.windows(2).map(|w| w.iter().collect::<String>())),
        }
        run.clear();
    };
    for c in lower.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            run.push(c);
        } else if c.is_alphanumeric() {
            flush_run(&mut run, &mut out);
            word.push(c);
        } else {
            flush_word(&mut word, &mut out);
            flush_run(&mut run, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_run(&mut run, &mut out);
    out
}

/// Multiset token F1 between two strings.
pub fn token_f1(a: &str, b: &str) -> f64 {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let mut pool: BTreeMap<&str, usize> = BTreeMap::new();
    for t in &tb {
        *pool.entry(t.as_str()).or_default() += 1;
    }
    let mut overlap = 0usize;
    for t in &ta {
        if let Some(n) = pool.get_mut(t.as_str()) {
            if *n > 0 {
                *n -= 1;
                overlap += 1;
            }
        }
    }
    if overlap == 0 {
        return 0.0;
    }
    let p = overlap as f64 / ta.len() as f64;
    let r = overlap as f64 / tb.len() as f64;
    2.0 * p * r / (p + r)
}

/// Lowercase, hidden characters stripped, whitespace collapsed.
pub fn normalize(s: &str) -> String {
    let stripped = shogun_redact::strip_hidden(s);
    stripped.text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Name similarity: 1.0 for the same name, 0.6 when one is a whole-word part of the other
/// ("Oren" in "Oren Vale" — the same person, but a partial name), else token F1.
pub fn name_similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize(a), normalize(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    if crate::generate::mentions_name(&na, &nb) || crate::generate::mentions_name(&nb, &na) {
        return 0.6;
    }
    token_f1(a, b)
}

fn key_similarity(table: Table, a: &str, b: &str) -> f64 {
    match table {
        Table::People | Table::Projects => name_similarity(a, b),
        Table::Commitments | Table::OpenLoops => token_f1(a, b),
    }
}

fn field_correct(field: &str, pred: &str, gold: &str) -> bool {
    match field {
        "description" => token_f1(pred, gold) >= MATCH_THRESHOLD,
        _ => normalize(pred) == normalize(gold),
    }
}

/// Whether `event_text` supports the prediction (pure-code provenance check).
pub fn supports(support: &Support, event_text: &str) -> bool {
    match support {
        Support::Entity { name, context, quote } => {
            !quote.is_empty() && event_text.contains(quote.as_str()) && quote.contains(name.as_str()) && quote.contains(context.as_str())
        }
        Support::Text(desc) => {
            let nd = normalize(desc);
            if nd.is_empty() {
                return false;
            }
            if normalize(event_text).contains(&nd) {
                return true;
            }
            let dt: BTreeSet<String> = tokens(desc).into_iter().collect();
            if dt.is_empty() {
                return false;
            }
            let et: BTreeSet<String> = tokens(event_text).into_iter().collect();
            dt.iter().filter(|t| et.contains(*t)).count() as f64 / dt.len() as f64 >= SUPPORT_COVERAGE
        }
    }
}

/// A prediction after cross-event dedup (the product keeps one row per key, with every citation).
#[derive(Debug, Clone)]
struct Prediction {
    table: Table,
    fields: BTreeMap<String, String>,
    confidence: f64,
    evidence: BTreeSet<i64>,
}

fn dedup(raw: &[RawPrediction]) -> Vec<Prediction> {
    let mut out: Vec<Prediction> = Vec::new();
    let mut index: BTreeMap<(Table, String), usize> = BTreeMap::new();
    for r in raw {
        let key = normalize(r.fields.get(r.table.key_field()).map(String::as_str).unwrap_or(""));
        match index.get(&(r.table, key.clone())) {
            Some(&i) => {
                if let Some(p) = out.get_mut(i) {
                    p.confidence = p.confidence.max(r.confidence);
                    p.evidence.insert(r.event_id);
                }
            }
            None => {
                index.insert((r.table, key), out.len());
                out.push(Prediction {
                    table: r.table,
                    fields: r.fields.clone(),
                    confidence: r.confidence,
                    evidence: std::iter::once(r.event_id).collect(),
                });
            }
        }
    }
    out
}

/// One-to-one greedy matching, best similarity first. Returns `pred index -> gold index`.
fn match_items(preds: &[Prediction], gold: &[GoldItem]) -> BTreeMap<usize, usize> {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (pi, p) in preds.iter().enumerate() {
        for (gi, g) in gold.iter().enumerate() {
            if p.table != g.table || !g.evidence.iter().any(|e| p.evidence.contains(e)) {
                continue;
            }
            let key = p.table.key_field();
            let (Some(pk), Some(gk)) = (p.fields.get(key), g.fields.get(key)) else { continue };
            let sim = key_similarity(p.table, pk, gk);
            if sim >= MATCH_THRESHOLD {
                pairs.push((sim, pi, gi));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let (mut used_p, mut used_g) = (BTreeSet::new(), BTreeSet::new());
    let mut out = BTreeMap::new();
    for (_, pi, gi) in pairs {
        if used_p.contains(&pi) || used_g.contains(&gi) {
            continue;
        }
        used_p.insert(pi);
        used_g.insert(gi);
        out.insert(pi, gi);
    }
    out
}

fn template_of(day: &SynthDay, event_id: i64) -> String {
    day.event(event_id).map_or_else(|| "(dangling)".to_string(), |e| e.template.clone())
}

/// Score one day's predictions. `stored_text` maps an event to the text the evidence row holds
/// (what `event_log` stores); provenance is checked against it.
pub fn score_day(
    day: &SynthDay,
    raw: &[RawPrediction],
    unscored_entities: usize,
    stored_text: impl Fn(&str) -> String,
) -> Score {
    let mut s = Score { days: 1, events: day.events.len(), gold: day.gold.len(), unscored_entities, ..Score::default() };

    // Provenance, per citation, before dedup.
    for r in raw {
        s.provenance.citations += 1;
        match day.event(r.event_id) {
            None => s.provenance.dangling += 1,
            Some(e) if supports(&r.support, &stored_text(&e.text)) => s.provenance.valid += 1,
            Some(_) => s.provenance.unsupported += 1,
        }
    }

    let preds = dedup(raw);
    s.predicted = preds.len();
    let matched = match_items(&preds, &day.gold);
    let matched_gold: BTreeSet<usize> = matched.values().copied().collect();

    for t in Table::ALL {
        s.items.entry(t).or_default();
        for f in t.fields() {
            s.fields.entry((t, f)).or_default();
        }
    }

    for (pi, p) in preds.iter().enumerate() {
        let band = BandLabel::of(p.confidence);
        let bucket = s.calibration.entry(band).or_default();
        bucket.n += 1;
        bucket.sum_confidence += p.confidence;
        match matched.get(&pi).and_then(|gi| day.gold.get(*gi)) {
            Some(g) => {
                bucket.correct += 1;
                s.items.entry(p.table).or_default().tp += 1;
                *s.band_agreement.entry((g.expected_band, band)).or_default() += 1;
                for f in p.table.fields() {
                    let c = s.fields.entry((p.table, f)).or_default();
                    match (p.fields.get(*f), g.fields.get(*f)) {
                        (Some(pv), Some(gv)) if field_correct(f, pv, gv) => c.tp += 1,
                        (Some(_), Some(_)) => {
                            c.hallucinated += 1;
                            c.omitted += 1;
                            c.wrong_value += 1;
                        }
                        (Some(_), None) => c.hallucinated += 1,
                        (None, Some(_)) => c.omitted += 1,
                        (None, None) => {}
                    }
                }
            }
            None => {
                s.items.entry(p.table).or_default().hallucinated += 1;
                for f in p.table.fields() {
                    if p.fields.contains_key(*f) {
                        s.fields.entry((p.table, f)).or_default().hallucinated += 1;
                    }
                }
                if let Some(first) = p.evidence.iter().next() {
                    s.by_template.entry(template_of(day, *first)).or_default().hallucinated += 1;
                }
            }
        }
    }

    for (gi, g) in day.gold.iter().enumerate() {
        // Entities are evidenced by many events; attribute them to a pseudo-template so the
        // per-template table stays about the templates' own rows.
        let tkey = match g.table {
            Table::People => "(people)".to_string(),
            Table::Projects => "(projects)".to_string(),
            _ => g.evidence.first().map_or_else(String::new, |e| template_of(day, *e)),
        };
        let tc = s.by_template.entry(tkey).or_default();
        tc.gold += 1;
        if matched_gold.contains(&gi) {
            tc.matched += 1;
            continue;
        }
        tc.omitted += 1;
        s.items.entry(g.table).or_default().omitted += 1;
        for f in g.table.fields() {
            if g.fields.contains_key(*f) {
                s.fields.entry((g.table, f)).or_default().omitted += 1;
            }
        }
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::golden::{Event, GeneratorInfo, Persona};

    fn day() -> SynthDay {
        let ev = |id: i64, text: &str| Event {
            id,
            ts_ms: 0,
            local_time: "09:00".into(),
            source: "capture".into(),
            app: "Notes".into(),
            window: "Notes".into(),
            text: text.into(),
            template: "t".into(),
        };
        let gold = |id: &str, table, fields: &[(&str, &str)], ev: Vec<i64>| GoldItem {
            id: id.into(),
            table,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            expected_band: BandLabel::Medium,
            evidence: ev,
        };
        SynthDay {
            format: crate::golden::FORMAT.into(),
            id: "unit".into(),
            generator: GeneratorInfo { seed: 0, template_set: "unit".into(), required_templates: vec![] },
            persona: Persona {
                id: "p".into(),
                name: "Unit Persona".into(),
                role: "r".into(),
                org: "o".into(),
                lang: "en".into(),
                utc_offset_secs: 0,
            },
            date: "2026-03-02".into(),
            events: vec![
                ev(1, "I'll send the plan by Friday."),
                ev(2, "Still waiting on Oren for the numbers."),
            ],
            gold: vec![
                gold("g1", Table::Commitments, &[("direction", "mine"), ("description", "I'll send the plan by Friday"), ("due", "2026-03-06")], vec![1]),
                gold("g2", Table::OpenLoops, &[("kind", "waiting_on_them"), ("description", "Still waiting on Oren for the numbers")], vec![2]),
            ],
        }
    }

    fn pred(table: Table, fields: &[(&str, &str)], ev: i64, conf: f64) -> RawPrediction {
        let fields: BTreeMap<String, String> =
            fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let desc = fields.get("description").cloned().unwrap_or_default();
        RawPrediction { table, fields, confidence: conf, event_id: ev, support: Support::Text(desc) }
    }

    fn score(raw: &[RawPrediction]) -> Score {
        score_day(&day(), raw, 0, |t| t.to_string())
    }

    #[test]
    fn perfect_predictions_score_one() {
        let s = score(&[
            pred(Table::Commitments, &[("direction", "mine"), ("description", "I'll send the plan by Friday"), ("due", "2026-03-06")], 1, 0.6),
            pred(Table::OpenLoops, &[("kind", "waiting_on_them"), ("description", "Still waiting on Oren for the numbers")], 2, 0.6),
        ]);
        assert_eq!(s.items_total(), Counts { tp: 2, ..Counts::default() });
        assert_eq!(s.fields_total().f1(), Some(1.0));
        assert_eq!(s.provenance.valid, 2);
        assert_eq!(s.band_promise_violations(), vec![]);
    }

    #[test]
    fn wrong_enum_counts_as_both_and_as_wrong_value() {
        let s = score(&[pred(
            Table::Commitments,
            &[("direction", "theirs"), ("description", "I'll send the plan by Friday"), ("due", "2026-03-06")],
            1,
            0.6,
        )]);
        let d = s.field(Table::Commitments, "direction");
        assert_eq!(d, Counts { tp: 0, hallucinated: 1, omitted: 1, wrong_value: 1 });
        // The item itself still matched: the row exists, one of its fields is wrong.
        assert_eq!(s.item(Table::Commitments).tp, 1);
        // The unmatched open loop is an omission, not a hallucination.
        assert_eq!(s.item(Table::OpenLoops), Counts { omitted: 1, ..Counts::default() });
    }

    #[test]
    fn a_prediction_citing_the_wrong_event_does_not_match() {
        let s = score(&[pred(
            Table::Commitments,
            &[("direction", "mine"), ("description", "I'll send the plan by Friday")],
            2,
            0.6,
        )]);
        assert_eq!(s.item(Table::Commitments).hallucinated, 1);
        assert_eq!(s.item(Table::Commitments).omitted, 1);
        assert_eq!(s.provenance.unsupported, 1);
    }

    #[test]
    fn dangling_citation_is_reported() {
        let s = score(&[pred(Table::OpenLoops, &[("kind", "follow_up"), ("description", "x")], 99, 0.3)]);
        assert_eq!(s.provenance.dangling, 1);
        assert_eq!(s.provenance.validity(), Some(0.0));
    }

    #[test]
    fn stated_due_where_gold_has_none_is_a_hallucinated_field() {
        let mut d = day();
        d.gold[0].fields.remove("due");
        let s = score_day(
            &d,
            &[pred(Table::Commitments, &[("direction", "mine"), ("description", "I'll send the plan by Friday"), ("due", "2026-03-06")], 1, 0.6)],
            0,
            |t| t.to_string(),
        );
        assert_eq!(s.field(Table::Commitments, "due").hallucinated, 1);
        assert_eq!(s.field(Table::Commitments, "due").omitted, 0);
    }

    #[test]
    fn calibration_uses_the_fusion_band_edges() {
        let s = score(&[
            pred(Table::Commitments, &[("direction", "mine"), ("description", "I'll send the plan by Friday")], 1, 0.35),
            pred(Table::OpenLoops, &[("kind", "follow_up"), ("description", "Unrelated hallucinated thing")], 2, 0.9),
        ]);
        assert_eq!(s.calibration.get(&BandLabel::Low).map(|b| b.correct), Some(1));
        assert_eq!(s.calibration.get(&BandLabel::High).map(|b| b.correct), Some(0));
        // A High prediction that is wrong breaks the High promise (used as fact downstream).
        assert_eq!(s.band_promise_violations(), vec![BandLabel::High]);
        // (1/2)|1 - 0.35| + (1/2)|0 - 0.9| = 0.775
        assert!((s.ece().unwrap_or(0.0) - 0.775).abs() < 1e-9);
    }

    #[test]
    fn band_edges_are_read_from_fusion() {
        assert!((crate::golden::lowest_confidence_in(BandLabel::Medium) - 0.5).abs() < 1e-9);
        assert!((crate::golden::lowest_confidence_in(BandLabel::High) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn duplicates_across_events_merge_into_one_row() {
        let mut d = day();
        d.gold[0].evidence = vec![1, 2];
        let p = |ev| pred(Table::Commitments, &[("direction", "mine"), ("description", "I'll send the plan by Friday")], ev, 0.3);
        let s = score_day(&d, &[p(1), p(2)], 0, |t| t.to_string());
        assert_eq!(s.predicted, 1); // one row, two citations
        assert_eq!(s.item(Table::Commitments).tp, 1);
        assert_eq!(s.provenance.citations, 2);
    }

    #[test]
    fn entity_support_follows_the_product_rule() {
        let ok = Support::Entity { name: "Oren".into(), context: "waiting on Oren".into(), quote: "Still waiting on Oren".into() };
        assert!(supports(&ok, "Still waiting on Oren for the numbers."));
        let bad = Support::Entity { name: "Oren".into(), context: "Oren's team".into(), quote: "Still waiting on Oren".into() };
        assert!(!supports(&bad, "Still waiting on Oren for the numbers."));
    }

    #[test]
    fn paraphrase_support_needs_most_tokens() {
        assert!(supports(&Support::Text("Send the plan by Friday".into()), "I'll send the plan by Friday."));
        assert!(!supports(&Support::Text("Prepare slides for the board".into()), "I'll send the plan by Friday."));
    }

    #[test]
    fn japanese_tokens_are_bigrams() {
        assert_eq!(tokens("返信待ち"), vec!["返信", "信待", "待ち"]);
        assert!(token_f1("見積書は金曜日までに送ります", "真野: 佐伯さん、見積書は金曜日までに送ります") > 0.8);
    }
}
