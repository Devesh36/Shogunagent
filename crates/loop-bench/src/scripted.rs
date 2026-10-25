//! The **scripted-review arm** (plan E49): ShogunAgent's engine with its Batch lane answered by a
//! deterministic, offline, scripted reviewer instead of a model.
//!
//! What it measures is the *pipeline* a candidate goes through to reach the band ShogunAgent
//! renders (≥ 0.5): the hourly window review turns a local candidate into a typed signal with an
//! exact quote, the loop consolidator re-lists it at the reviewer's confidence (≥ 0.84 or it is
//! refused), and the outcomes later messages report (`loop_updates`, V104) are resolved or
//! re-dated only through a quote the consolidator was shown. Every check on that path is the
//! engine's own code; only the two answers are scripted.
//!
//! **It is not a model and its numbers are not a model's.** The scripted reviewer:
//! - answers the window review (`consolidation_window`) by running the same on-device rules
//!   ([`shogun_memory::extract::extract`]) over each numbered line and anchoring every candidate on
//!   its line, exactly as the answer contract asks (`line`, `anchor`, `anchor_len`): so it finds
//!   what the local rules find, no more;
//! - answers the consolidator (`loops_consolidate`) by re-listing every new signal with its exact
//!   quote at confidence 0.9, resolving every outcome marked `resolved` / `cancelled` with that
//!   outcome's quote, and re-listing a `redated` item citing the update's quote. It reads only the
//!   call's own input; it never sees the gold.
//!
//! So this arm answers "how does a local candidate reach the actionable band, and what does the
//! engine do with the answers", not "how good would a model be". A model arm needs a named model,
//! a cost line and a network run (`docs/benchmarks/open-loop-tracking.md`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use shogun_core::llm::anthropic::{BatchHandle, BatchItem, BatchLane, BatchResult, BatchStatus};
use shogun_core::llm::LlmError;
use shogun_memory::extract::Candidate;

use crate::adapter::{AdapterInfo, ShogunAgentAdapter, SystemAdapter, Usage};
use crate::gen::{Dataset, Event};
use crate::score::Prediction;

/// The confidence the scripted consolidator gives every item: above the consolidator's 0.84 bar.
pub const SCRIPTED_CONFIDENCE: f64 = 0.9;
/// Items one consolidator answer may open or re-list (the engine's `loops::MAX_OPEN`).
const MAX_OPEN: usize = shogun_memory::loops::MAX_OPEN;

/// The data half of a fenced chunk.
fn fenced(chunk: &str) -> &str {
    let start = chunk.find("<<<CONTEXT>>>\n").map_or(0, |at| at + "<<<CONTEXT>>>\n".len());
    let end = chunk.rfind("\n<<<END CONTEXT>>>").filter(|&e| e >= start).unwrap_or(chunk.len());
    &chunk[start..end]
}

/// The window review answer: per session, the local rules over each numbered line, anchored on
/// that line from its start to the end of the candidate's sentence.
pub fn answer_window(chunk: &str) -> Value {
    let mut sessions: Vec<Value> = Vec::new();
    let mut current: Option<(String, Vec<Value>, Vec<Value>)> = None;
    let flush = |current: &mut Option<(String, Vec<Value>, Vec<Value>)>, sessions: &mut Vec<Value>| {
        if let Some((id, commitments, open_loops)) = current.take() {
            if !commitments.is_empty() || !open_loops.is_empty() {
                sessions.push(json!({ "id": id, "commitments": commitments, "open_loops": open_loops, "entities": [] }));
            }
        }
    };
    for line in fenced(chunk).lines() {
        if let Some(id) = line.strip_prefix("Session: ") {
            flush(&mut current, &mut sessions);
            current = Some((id.trim().to_string(), Vec::new(), Vec::new()));
            continue;
        }
        let Some((_, commitments, open_loops)) = current.as_mut() else { continue };
        let Some(rest) = line.strip_prefix('[') else { continue };
        let Some((n, text)) = rest.split_once("] ") else { continue };
        let Ok(n) = n.parse::<usize>() else { continue };
        let anchor: String = text.chars().take(40).collect();
        for candidate in shogun_memory::extract::extract(text) {
            let (description, item) = match &candidate {
                Candidate::Commitment { direction, description, .. } => {
                    (description.clone(), json!({ "direction": direction.as_str(), "description": description }))
                }
                Candidate::OpenLoop { kind, description, .. } => {
                    (description.clone(), json!({ "kind": kind.as_str(), "description": description }))
                }
                Candidate::Entity(_) => continue,
            };
            let Some(at) = text.find(&description) else { continue };
            let anchor_len = text[..at].chars().count() + description.chars().count();
            let mut item = item;
            item["line"] = json!(n);
            item["anchor"] = json!(anchor);
            item["anchor_len"] = json!(anchor_len);
            match candidate {
                Candidate::Commitment { .. } => commitments.push(item),
                _ => open_loops.push(item),
            }
        }
    }
    flush(&mut current, &mut sessions);
    json!({ "sessions": sessions })
}

/// `"words"` → `words`.
fn unquote(s: &str) -> Option<&str> {
    s.trim().strip_prefix('"')?.strip_suffix('"')
}

/// `E123` → 123.
fn event_ref(s: &str) -> Option<i64> {
    s.trim().strip_prefix('E')?.parse().ok()
}

/// The lines of one section of the consolidator input (between its header and the next header).
fn section<'a>(data: &'a str, header: &str) -> Vec<&'a str> {
    let mut lines = data.lines().skip_while(|l| !l.starts_with(header));
    lines.next();
    lines
        .take_while(|l| {
            !(l.starts_with("Open items") || l.starts_with("Recently closed") || l.starts_with("New signals")
                || l.starts_with("Outcomes noticed") || l.starts_with("Sources:"))
        })
        .filter(|l| !l.trim().is_empty() && l.trim() != "(none)")
        .collect()
}

/// An open item of the input: its id, its text, and its evidence (event, quote) when shown.
type Listed = (String, String, Option<(i64, String)>);

/// The consolidator answer: every new signal re-listed with its exact quote, every outcome
/// applied through its own quote. Reads only the input it was given.
pub fn answer_consolidator(chunk: &str) -> Value {
    let data = fenced(chunk);
    // Open items: id | kind | horizon | due | text | evidence.
    let listed: Vec<Listed> = section(data, "Open items")
        .into_iter()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(6, " | ").collect();
            let [id, _kind, _horizon, _due, text, evidence] = parts.as_slice() else { return None };
            let evidence = evidence.split_once(' ').and_then(|(e, q)| Some((event_ref(e)?, unquote(q)?.to_string())));
            Some((id.to_string(), text.to_string(), evidence))
        })
        .collect();
    let mut open: Vec<Value> = Vec::new();
    let mut resolved: Vec<Value> = Vec::new();
    // Outcomes first: an item done or moved matters more than one more sighting.
    for line in section(data, "Outcomes noticed") {
        let parts: Vec<&str> = line.splitn(4, " | ").collect();
        let [id, kind, event, rest] = parts.as_slice() else { continue };
        let Some(event_id) = event_ref(event) else { continue };
        let Some(quote) = rest.rsplit_once(" | ").and_then(|(q, _)| unquote(q)) else { continue };
        match *kind {
            "resolved" | "cancelled" => resolved.push(json!({ "id": id, "event_id": event_id, "quote": quote })),
            "redated" if open.len() < MAX_OPEN => {
                let text = listed.iter().find(|(l, _, _)| l == id).map(|(_, t, _)| t.clone()).unwrap_or_default();
                open.push(json!({ "id": id, "kind": "commitment", "horizon": "short", "source_event_id": event_id,
                                  "source_quote": quote, "confidence": SCRIPTED_CONFIDENCE, "text": text }));
            }
            _ => {}
        }
    }
    // New signals: kind | event | text | "quote".
    for line in section(data, "New signals") {
        if open.len() == MAX_OPEN {
            break;
        }
        let Some((head, quote)) = line.rsplit_once(" | ") else { continue };
        let Some(quote) = unquote(quote) else { continue };
        let parts: Vec<&str> = head.splitn(3, " | ").collect();
        let [kind, event, text] = parts.as_slice() else { continue };
        let Some(event_id) = event_ref(event) else { continue };
        let kind = match *kind {
            "commitment" => "commitment",
            "followup" => "followup",
            _ => "loop",
        };
        // Already listed on these words: re-list it in place.
        let id = listed
            .iter()
            .find(|(_, _, ev)| ev.as_ref().is_some_and(|(e, q)| *e == event_id && (q.contains(quote) || quote.contains(q.as_str()))))
            .map(|(id, _, _)| id.clone());
        open.push(json!({ "id": id, "kind": kind, "horizon": "short", "source_event_id": event_id,
                          "source_quote": quote, "confidence": SCRIPTED_CONFIDENCE, "text": text }));
    }
    json!({ "open": open, "resolved": resolved, "duplicates": [] })
}

/// The offline Batch lane: answers the two review purposes with the scripts above. Counts calls.
#[derive(Debug, Default)]
pub struct ScriptedReviewer {
    pub window_calls: AtomicU64,
    pub consolidator_calls: AtomicU64,
    pub other_calls: AtomicU64,
    submitted: Mutex<Vec<String>>,
}

impl ScriptedReviewer {
    fn answer(&self, item: &BatchItem) -> String {
        match item.purpose.as_str() {
            shogun_core::dreamcycle::jobs::WINDOW_PURPOSE => {
                self.window_calls.fetch_add(1, Ordering::Relaxed);
                answer_window(&item.chunk).to_string()
            }
            shogun_memory::loops::PURPOSE => {
                self.consolidator_calls.fetch_add(1, Ordering::Relaxed);
                answer_consolidator(&item.chunk).to_string()
            }
            _ => {
                self.other_calls.fetch_add(1, Ordering::Relaxed);
                r#"{"commitments":[],"open_loops":[],"entities":[]}"#.to_string()
            }
        }
    }
}

impl BatchLane for ScriptedReviewer {
    async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
        Ok(items
            .iter()
            .map(|item| BatchResult { custom_id: item.custom_id.clone(), text: Some(self.answer(item)), error: None })
            .collect())
    }

    /// Not used by the hourly path (it reviews synchronously); answered with nothing to report.
    async fn submit(&self, items: &[BatchItem], _client_request_id: Option<&str>) -> Result<BatchHandle, LlmError> {
        if let Ok(mut ids) = self.submitted.lock() {
            *ids = items.iter().map(|i| i.custom_id.clone()).collect();
        }
        Ok(BatchHandle { id: "scripted".into(), status: BatchStatus::Ended })
    }

    async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
        Ok(BatchHandle { id: batch_id.into(), status: BatchStatus::Ended })
    }

    async fn results(&self, _batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
        let ids = self.submitted.lock().map(|ids| ids.clone()).unwrap_or_default();
        Ok(ids
            .into_iter()
            .map(|custom_id| BatchResult { custom_id, text: Some(r#"{"commitments":[],"open_loops":[]}"#.into()), error: None })
            .collect())
    }
}

/// ShogunAgent with its Batch lane answered by [`ScriptedReviewer`]: the same ingest and read as
/// [`ShogunAgentAdapter`]; after each hour the hourly review through the lane
/// (`tick_principal_with_lane`) and then the loop consolidator (`consolidate_loops`).
pub struct ScriptedReviewAdapter {
    inner: ShogunAgentAdapter,
    lane: Arc<ScriptedReviewer>,
    runtime: Option<tokio::runtime::Runtime>,
    /// Consolidator calls that applied an answer.
    pub consolidations: usize,
    /// What those answers did, summed over the stream (counts only).
    pub applied: shogun_memory::loops::Applied,
}

impl Default for ScriptedReviewAdapter {
    fn default() -> Self {
        Self { inner: ShogunAgentAdapter::default(), lane: Arc::new(ScriptedReviewer::default()), runtime: None, consolidations: 0, applied: Default::default() }
    }
}

impl ScriptedReviewAdapter {
    pub fn stats(&self) -> crate::adapter::ShogunStats {
        self.inner.stats
    }

    pub fn lane(&self) -> &ScriptedReviewer {
        &self.lane
    }
}

impl SystemAdapter for ScriptedReviewAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "shogunagent-scripted-review".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            model: None,
            description: "ShogunAgent engine in process with its Batch lane answered by an OFFLINE SCRIPTED REVIEWER, not a model: the hourly window review is answered by the on-device rules anchored on each line, the loop consolidator re-lists every signal with its exact quote at 0.9 and applies every outcome through its quote. Measures the pipeline to the >= 0.5 band, not model quality.".into(),
        }
    }

    fn reset(&mut self, dataset: &Dataset) -> Result<(), String> {
        self.lane = Arc::new(ScriptedReviewer::default());
        self.consolidations = 0;
        self.applied = Default::default();
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_current_thread().enable_time().build().map_err(|_| "runtime_unavailable".to_string())?,
            );
        }
        self.inner.reset(dataset)
    }

    fn ingest(&mut self, events: &[Event]) -> Result<(), String> {
        self.inner.ingest(events)
    }

    fn advance(&mut self, now_ms: i64) -> Result<(), String> {
        use shogun_core::tenancy::dream_tick::{tick_principal_with_lane, TickOutcome};
        self.inner.set_clock(now_ms);
        let runtime = self.runtime.as_ref().ok_or("reset was not called")?;
        let db = self.inner.db_ref()?;
        let lane = self.lane.as_ref();
        let (outcome, applied) = runtime.block_on(async {
            let outcome = tick_principal_with_lane(db, lane, now_ms).await;
            let applied = shogun_core::dreamcycle::loops::consolidate_loops(db, lane).await;
            (outcome, applied)
        });
        let stats = &mut self.inner.stats;
        match outcome {
            TickOutcome::Ran { complete: true } => stats.ticks_ran += 1,
            TickOutcome::NothingNew | TickOutcome::AlreadyDone | TickOutcome::Skipped(_) => stats.ticks_idle += 1,
            _ => stats.ticks_failed += 1,
        }
        if let Some(a) = applied {
            self.consolidations += 1;
            self.applied.opened += a.opened;
            self.applied.relisted += a.relisted;
            self.applied.resolved += a.resolved;
            self.applied.merged += a.merged;
            self.applied.dropped += a.dropped;
        }
        Ok(())
    }

    fn snapshot(&mut self, now_ms: i64) -> Result<Vec<Prediction>, String> {
        self.inner.snapshot(now_ms)
    }

    /// No model is called: the scripted answers cost nothing and are not model calls.
    fn usage(&self) -> Usage {
        Usage::default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_window_answer_anchors_each_local_candidate_on_its_line() {
        let chunk = "<<<CONTEXT>>>\nSession: S1\nWindow: agent · email\n[1] Hi Oren,\n[2] Leave the board memo with me; it'll be with Oren Vale by Friday.\n[3] Thanks\nSession: S2\nWindow: agent · chat\n[1] lunch at noon?\n<<<END CONTEXT>>>";
        let answer = answer_window(chunk);
        let sessions = answer["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "a session with nothing to report is omitted: {answer}");
        let item = &sessions[0]["commitments"][0];
        assert_eq!(sessions[0]["id"], "S1");
        assert_eq!(item["line"], 2);
        assert_eq!(item["anchor"], "Leave the board memo with me; it'll be w");
        assert_eq!(item["anchor_len"], "Leave the board memo with me; it'll be with Oren Vale by Friday".chars().count());
    }

    #[test]
    fn the_consolidator_answer_relists_signals_and_applies_outcomes_by_quote() {
        let input = "<<<CONTEXT>>>\nNow: 2026-09-08 10:10\nOpen items (id | kind | horizon | due | text | evidence):\nC4 | commitment | short | due 2026-09-11 23:59 | I'll send the plan | E2 \"I'll send the plan to Oren by Friday\"\nRecently closed (id | status | text):\n(none)\nNew signals (kind | event | text | quote):\nblocker | E7 | Still waiting on Priya for the deck | \"Still waiting on Priya for the deck\"\ncommitment | E2 | I'll send the plan | \"I'll send the plan to Oren by Friday\"\nOutcomes noticed in later messages (id | outcome | event | quote | new due):\nC4 | redated | E9 | \"Update: the plan for Oren moves to Monday\" | due 2026-09-14 23:59\nC5 | resolved | E10 | \"Sent the deck to Kenji\" | -\nSources:\n[E2] x\n<<<END CONTEXT>>>";
        let answer = answer_consolidator(input);
        let open = answer["open"].as_array().unwrap();
        assert_eq!(open.len(), 3, "{answer}");
        assert_eq!(open[0]["id"], "C4");
        assert_eq!(open[0]["source_event_id"], 9);
        assert_eq!(open[0]["source_quote"], "Update: the plan for Oren moves to Monday");
        assert_eq!(open[1]["kind"], "loop");
        assert!(open[1]["id"].is_null());
        assert_eq!(open[2]["id"], "C4", "the listed item re-listed in place");
        assert_eq!(answer["resolved"][0], json!({"id": "C5", "event_id": 10, "quote": "Sent the deck to Kenji"}));
        assert!(open.iter().all(|o| o["confidence"].as_f64() == Some(SCRIPTED_CONFIDENCE)));
    }
}
