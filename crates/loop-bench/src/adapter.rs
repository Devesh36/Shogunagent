//! The system adapter: how a memory system is driven through a stream and read at its end.
//!
//! The harness calls, for one dataset: [`SystemAdapter::reset`] once, then hour by hour
//! [`SystemAdapter::ingest`] with that hour's events (time-ordered) and
//! [`SystemAdapter::advance`] at ten past the next hour (where a system runs its background
//! processing), through to the end of the stream; then [`SystemAdapter::snapshot`] at the end.
//! Each call is timed by the harness. A system that processes on ingest leaves `advance` empty.
//!
//! Three adapters ship: [`ShogunAgentAdapter`] (ingest → local-rules hourly dream tick → state
//! read through the export v2 face), [`WillBaseline`] (every sentence containing "will" / "I'll"
//! is an open commitment) and [`Oracle`] (the gold, to check a pipeline end to end).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gen::{Dataset, Event, ItemKind};
use crate::score::{oracle_predictions, Citation, PredictedStatus, Prediction};

/// What an adapter is, for the results file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub name: String,
    pub version: String,
    /// The model the system calls, if any (`None` = no model).
    pub model: Option<String>,
    pub description: String,
}

/// Model usage of a run. Zero for a system that calls no model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: f64,
}

pub trait SystemAdapter {
    fn info(&self) -> AdapterInfo;
    /// Start an empty memory for `dataset`'s principal. Adapters must not read the gold.
    fn reset(&mut self, dataset: &Dataset) -> Result<(), String>;
    /// Take one hour's events, in time order.
    fn ingest(&mut self, events: &[Event]) -> Result<(), String>;
    /// Background processing up to `now_ms`.
    fn advance(&mut self, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }
    /// The open and closed commitments and open loops the system holds at `now_ms`.
    fn snapshot(&mut self, now_ms: i64) -> Result<Vec<Prediction>, String>;
    /// Model usage since the last reset.
    fn usage(&self) -> Usage {
        Usage::default()
    }
}

/// Sentences of a text: split on `.`, `!`, `?` and newlines, trimmed, non-empty. Each is an
/// exact span of the text.
pub fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?' | '\n') {
            let s = text[start..i + c.len_utf8()].trim();
            if !s.trim_end_matches(['.', '!', '?']).trim().is_empty() {
                out.push(s.trim_end_matches('\n'));
            }
            start = i + c.len_utf8();
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// The trivial baseline: every sentence that contains the word "will" or "I'll" is an open
/// commitment, quoted exactly, at confidence 1. It never closes anything, never reads a date,
/// and has no notion of open loops. A scorer that cannot tell it from a real tracker is broken.
#[derive(Debug, Default)]
pub struct WillBaseline {
    seen: Vec<Prediction>,
}

impl WillBaseline {
    fn hit(sentence: &str) -> bool {
        sentence
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .any(|w| w.eq_ignore_ascii_case("will") || w.eq_ignore_ascii_case("i'll"))
    }
}

impl SystemAdapter for WillBaseline {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "will-baseline".into(),
            version: "1".into(),
            model: None,
            description: "Every sentence containing \"will\" or \"I'll\" is an open commitment; never closes, reads no dates, finds no open loops.".into(),
        }
    }

    fn reset(&mut self, _dataset: &Dataset) -> Result<(), String> {
        self.seen.clear();
        Ok(())
    }

    fn ingest(&mut self, events: &[Event]) -> Result<(), String> {
        for event in events {
            for sentence in sentences(&event.content).into_iter().filter(|s| Self::hit(s)) {
                self.seen.push(Prediction {
                    kind: ItemKind::Commitment,
                    text: sentence.to_string(),
                    status: PredictedStatus::Open,
                    due_ms: None,
                    confidence: 1.0,
                    provenance: vec![Citation { event_id: event.id.clone(), quote: Some(sentence.to_string()) }],
                });
            }
        }
        Ok(())
    }

    fn snapshot(&mut self, _now_ms: i64) -> Result<Vec<Prediction>, String> {
        Ok(self.seen.clone())
    }
}

/// The gold as a system: every metric 1. For checking a pipeline, not a result.
#[derive(Debug, Default)]
pub struct Oracle {
    predictions: Vec<Prediction>,
}

impl SystemAdapter for Oracle {
    fn info(&self) -> AdapterInfo {
        AdapterInfo { name: "oracle".into(), version: "1".into(), model: None, description: "The gold file itself; the scorer's upper bound.".into() }
    }

    fn reset(&mut self, dataset: &Dataset) -> Result<(), String> {
        self.predictions = oracle_predictions(dataset);
        Ok(())
    }

    fn ingest(&mut self, _events: &[Event]) -> Result<(), String> {
        Ok(())
    }

    fn snapshot(&mut self, _now_ms: i64) -> Result<Vec<Prediction>, String> {
        Ok(self.predictions.clone())
    }
}

/// ShogunAgent's engine, in process: a fresh in-memory principal store per dataset; each event
/// through `memory.ingest` (the parser and store path every face uses); the hourly Dream Cycle
/// tick on the on-device **local rules** (`shogun_core::tenancy::dream_tick::tick_principal_local`:
/// no model, no network) after every hour; the state read at the end through `state.export`
/// `format: "export-v2"`, with every row in every band (operating points filter on confidence).
pub struct ShogunAgentAdapter {
    clock: Arc<AtomicI64>,
    db: Option<shogun_core::daemon::Db>,
    /// Store event id → dataset event id (the first one, when a duplicate collapsed).
    ids: HashMap<i64, String>,
    pub stats: ShogunStats,
}

/// What the engine did with the stream. Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShogunStats {
    pub ingested: usize,
    pub deduplicated: usize,
    pub rejected: usize,
    pub ticks_ran: usize,
    pub ticks_idle: usize,
    pub ticks_failed: usize,
}

impl Default for ShogunAgentAdapter {
    fn default() -> Self {
        Self { clock: Arc::new(AtomicI64::new(0)), db: None, ids: HashMap::new(), stats: ShogunStats::default() }
    }
}

impl ShogunAgentAdapter {
    fn db(&self) -> Result<&shogun_core::daemon::Db, String> {
        self.db.as_ref().ok_or_else(|| "reset was not called".to_string())
    }
}

impl SystemAdapter for ShogunAgentAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "shogunagent-local-rules".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            model: None,
            description: "ShogunAgent engine in process: memory.ingest per event, hourly Dream Cycle tick on the on-device local rules (no model), state read through state.export format export-v2.".into(),
        }
    }

    fn reset(&mut self, dataset: &Dataset) -> Result<(), String> {
        let first = dataset.events.first().map(|e| e.ts_ms).unwrap_or(crate::gen::EPOCH_MS);
        self.clock.store(first, Ordering::SeqCst);
        let clock = self.clock.clone();
        let db = shogun_core::daemon::Db::open_in_memory(Arc::new(move || clock.load(Ordering::SeqCst))).map_err(|_| "store_unavailable".to_string())?;
        self.db = Some(db);
        self.ids.clear();
        self.stats = ShogunStats::default();
        Ok(())
    }

    fn ingest(&mut self, events: &[Event]) -> Result<(), String> {
        for event in events {
            self.clock.store(event.ts_ms, Ordering::SeqCst);
            let request = shogun_mcp::ingest_api::ingest_request(&json!({
                "events": [{ "content": event.content, "ts_ms": event.ts_ms, "kind": event.channel.as_str(), "title": event.author }],
                "source": event.channel.as_str(),
                "actor": "agent:loop-bench",
                "thread_key": event.thread,
                "idempotency_key": event.id,
            }))?;
            let outcome = self.db()?.agent_ingest(&request)?;
            self.stats.ingested += outcome.inserted;
            self.stats.deduplicated += outcome.deduplicated;
            self.stats.rejected += outcome.rejected;
            for id in outcome.event_ids {
                self.ids.entry(id).or_insert_with(|| event.id.clone());
            }
        }
        Ok(())
    }

    fn advance(&mut self, now_ms: i64) -> Result<(), String> {
        self.clock.store(now_ms, Ordering::SeqCst);
        use shogun_core::tenancy::dream_tick::{tick_principal_local, TickOutcome};
        match tick_principal_local(self.db()?, now_ms) {
            TickOutcome::Ran { complete: true } => self.stats.ticks_ran += 1,
            TickOutcome::NothingNew | TickOutcome::AlreadyDone | TickOutcome::Skipped(_) => self.stats.ticks_idle += 1,
            _ => self.stats.ticks_failed += 1,
        }
        Ok(())
    }

    fn snapshot(&mut self, now_ms: i64) -> Result<Vec<Prediction>, String> {
        self.clock.store(now_ms, Ordering::SeqCst);
        use shogun_mcp::export_api::{ExportFormat, ExportRequest};
        let export = self.db()?.export_v1(&ExportRequest { format: ExportFormat::V2, ..ExportRequest::default() })?;
        let mut out = Vec::new();
        for line in export.jsonl.lines().skip(1) {
            let record: Value = serde_json::from_str(line).map_err(|_| "export_unreadable".to_string())?;
            if record["type"] != "fact" || !record["superseded_by"].is_null() {
                continue;
            }
            let kind = if record["kind"] == "commitment" { ItemKind::Commitment } else { ItemKind::OpenLoop };
            let status = match record["status"].as_str() {
                Some("open" | "overdue") => PredictedStatus::Open,
                _ => PredictedStatus::Closed,
            };
            let provenance = record["provenance"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| Citation {
                    event_id: p["event_id"].as_i64().and_then(|id| self.ids.get(&id).cloned()).unwrap_or_else(|| format!("store:{}", p["event_id"])),
                    quote: p["quote"].as_str().map(str::to_string),
                })
                .collect();
            out.push(Prediction {
                kind,
                text: record["text"].as_str().unwrap_or_default().to_string(),
                status,
                due_ms: record["due_at"].as_i64(),
                confidence: record["confidence"].as_f64().unwrap_or(0.0),
                provenance,
            });
        }
        Ok(out)
    }
}

/// Look up an adapter by name.
pub fn by_name(name: &str) -> Option<Box<dyn SystemAdapter>> {
    match name {
        "shogunagent" | "shogunagent-local-rules" => Some(Box::new(ShogunAgentAdapter::default())),
        "will-baseline" | "baseline" => Some(Box::new(WillBaseline::default())),
        "oracle" => Some(Box::new(Oracle::default())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn sentences_are_exact_spans() {
        let text = "Hi Oren,\n\nI'll send the plan by Friday. Thanks!\nok";
        let s = sentences(text);
        assert_eq!(s, vec!["Hi Oren,", "I'll send the plan by Friday.", "Thanks!", "ok"]);
        assert!(s.iter().all(|x| text.contains(x)));
    }

    #[test]
    fn the_baseline_takes_will_sentences_only() {
        assert!(WillBaseline::hit("I'll send it."));
        assert!(WillBaseline::hit("She WILL send it."));
        assert!(!WillBaseline::hit("Willow sent it."));
        assert!(!WillBaseline::hit("Waiting for Oren."));
    }
}
