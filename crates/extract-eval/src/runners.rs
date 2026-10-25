//! Model-free runners and the adapter from the product's extraction output to scorer input.
//!
//! Every extractor in the product ends in the same shape — per event id, a list of
//! [`shogun_memory::extract::Candidate`] — so one adapter ([`predictions`]) turns any of them into
//! [`RawPrediction`]s the scorer understands. Runners in this crate:
//!
//! - [`run_extract_untrusted`]: the inline ingress pass (`shogun_memory::extract::extract_untrusted`)
//!   over each event's text as captured.
//!
//! The Dream Cycle runners need shogun-core's `db` feature — the real `LocalRuleClassifier` over
//! [`stored_text`], and the Batch replay through `parse_batch_classification` — and live in
//! `crates/shogun-core/tests/extract_eval.rs` and reuse this adapter and the replay loader below.

use std::collections::BTreeMap;

use serde::Deserialize;
use shogun_memory::extract::Candidate;
use shogun_memory::knowledge::Kind;

use crate::golden::{Event, SynthDay, Table};
use crate::score::{RawPrediction, Support};

/// Per event id, what an extractor proposed (the `Classifier::classify` shape).
pub type Classified = Vec<(i64, Vec<Candidate>)>;

/// The text `event_log` stores for an event: hidden characters stripped and secrets masked
/// (`shogun_memory::sanitize::persist_body`, the same call the insert path makes). Provenance and
/// the Dream Cycle runners read this, as the product does.
pub fn stored_text(text: &str) -> String {
    shogun_memory::sanitize::persist_body(text).text.into_owned()
}

/// Inline ingress extraction over each event as captured.
pub fn run_extract_untrusted(day: &SynthDay) -> Classified {
    classify_each(&day.events, |e| shogun_memory::extract::extract_untrusted(&e.text))
}

fn classify_each(events: &[Event], f: impl Fn(&Event) -> Vec<Candidate>) -> Classified {
    events
        .iter()
        .map(|e| (e.id, f(e)))
        .filter(|(_, c)| !c.is_empty())
        .collect()
}

fn fmt_date(d: shogun_memory::due::CivilDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// The deadline the product would store for a commitment: `persist_candidates` reads it out of
/// the description with `shogun_memory::due::due_at`, against when it was said. Here the persona's
/// fixed offset stands in for the host clock so the result does not depend on the machine.
pub fn predicted_due(description: &str, said_at_ms: i64, offset_secs: i32) -> Option<String> {
    let ms = shogun_memory::due::due_at_with_offset(description, said_at_ms, offset_secs)?;
    let local_days = (ms + i64::from(offset_secs) * 1_000).div_euclid(86_400_000);
    Some(fmt_date(shogun_memory::due::civil_from_days(local_days)))
}

/// Convert extractor output into scorer input. Returns the predictions and how many entities were
/// of a kind the eval does not score (topic, tool, organization, …).
pub fn predictions(day: &SynthDay, classified: &[(i64, Vec<Candidate>)]) -> (Vec<RawPrediction>, usize) {
    let mut out = Vec::new();
    let mut unscored = 0;
    for (event_id, cands) in classified {
        let said_at = day.event(*event_id).map(|e| e.ts_ms);
        for c in cands {
            let mut fields = BTreeMap::new();
            let (table, support) = match c {
                Candidate::Commitment { direction, description, .. } => {
                    fields.insert("direction".to_string(), direction.as_str().to_string());
                    fields.insert("description".to_string(), description.clone());
                    if let Some(due) = said_at
                        .and_then(|ts| predicted_due(description, ts, day.persona.utc_offset_secs))
                    {
                        fields.insert("due".to_string(), due);
                    }
                    (Table::Commitments, Support::Text(description.clone()))
                }
                Candidate::OpenLoop { kind, description, .. } => {
                    fields.insert("kind".to_string(), kind.as_str().to_string());
                    fields.insert("description".to_string(), description.clone());
                    (Table::OpenLoops, Support::Text(description.clone()))
                }
                Candidate::Entity(e) => {
                    let table = match e.kind {
                        Kind::Person => Table::People,
                        Kind::Project => Table::Projects,
                        _ => {
                            unscored += 1;
                            continue;
                        }
                    };
                    fields.insert("name".to_string(), e.name.clone());
                    (
                        table,
                        Support::Entity {
                            name: e.name.clone(),
                            context: e.context.clone(),
                            quote: e.quote.clone(),
                        },
                    )
                }
            };
            out.push(RawPrediction { table, fields, confidence: c.confidence(), event_id: *event_id, support });
        }
    }
    (out, unscored)
}

/// Score a runner's output over one day.
pub fn score(day: &SynthDay, classified: &[(i64, Vec<Candidate>)]) -> crate::score::Score {
    let (raw, unscored) = predictions(day, classified);
    crate::score::score_day(day, &raw, unscored, stored_text)
}

/// A recorded Batch consolidation result set for one golden day (`batch-replay/v2`). Synthetic:
/// hand-shaped in the JSON contract the consolidation instruction asks for, not model output.
///
/// v2 (zero-base plan L1/L2): one result per *session* — the product folds a day's cloud-eligible
/// events into one request per (app, window, hour), so `custom_id` is the session id the lane
/// derives from that key, `events` lists the golden events the session was built from (for people
/// reading the file and for the lint; the harness checks it against the rebuilt session), and every
/// item names its support as `line` (the `[n]` number) + `anchor` (the line's first 40 characters,
/// the verification) + `anchor_len` into the session's numbered lines instead of a verbatim quote.
/// `line` is absent on the one deliberately unresolvable anchor; one item carries a deliberately
/// wrong `line` (the anchor overrules it — `AnchorStats::line_mismatch`). The numbers come from
/// `dump_session_lines` in `crates/shogun-core/tests/extract_eval.rs` (`--ignored --nocapture`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFixture {
    pub format: String,
    pub day: String,
    pub note: String,
    pub results: Vec<ReplayResult>,
}

/// One Batch item's terminal result. `response` is re-serialised to the item's text; `raw_text`
/// carries text byte-for-byte when the shape itself is the test (a code fence, broken JSON).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayResult {
    pub custom_id: String,
    /// The golden event ids whose lines make up this session, in time order.
    #[serde(default)]
    pub events: Vec<i64>,
    #[serde(default)]
    pub response: Option<serde_json::Value>,
    #[serde(default)]
    pub raw_text: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl ReplayResult {
    /// The text half of a `BatchResult` (`custom_id`, `text`, `error`).
    pub fn text(&self) -> Option<String> {
        self.raw_text.clone().or_else(|| self.response.as_ref().map(|v| v.to_string()))
    }
}

pub const REPLAY_FORMAT: &str = "batch-replay/v2";

/// The checked-in replay fixtures, one per golden day.
pub const REPLAY_FILES: &[(&str, &str)] = &[
    ("day-01-eng-lead", include_str!("../replay/day-01-eng-lead.json")),
    ("day-02-ops-traps", include_str!("../replay/day-02-ops-traps.json")),
    ("day-03-ja-pm", include_str!("../replay/day-03-ja-pm.json")),
    ("day-04-mixed", include_str!("../replay/day-04-mixed.json")),
    ("day-05-ja-dashboard-noise", include_str!("../replay/day-05-ja-dashboard-noise.json")),
    ("day-06-en-dashboard-noise", include_str!("../replay/day-06-en-dashboard-noise.json")),
];

pub fn replay_fixtures() -> Result<Vec<ReplayFixture>, serde_json::Error> {
    REPLAY_FILES.iter().map(|(_, json)| serde_json::from_str(json)).collect()
}

/// Split a day by the product's Batch source policy (`event_log::BATCH_EXCLUDED_SOURCES`):
/// `(cloud ids, local-only events)`. Meeting transcripts never reach the Batch lane; the Dream
/// Cycle classifies them with local rules.
pub fn partition_for_batch(day: &SynthDay) -> (Vec<i64>, Vec<&Event>) {
    let excluded = shogun_memory::event_log::BATCH_EXCLUDED_SOURCES;
    let mut cloud = Vec::new();
    let mut local = Vec::new();
    for e in &day.events {
        if excluded.contains(&e.source.as_str()) {
            local.push(e);
        } else {
            cloud.push(e.id);
        }
    }
    (cloud, local)
}
