//! Drive each scenario through the real write path and read back what it became.
//!
//! The path is the one `shogun_core::daemon::Ingest::capture_and_extract` runs, call for call:
//! `event_log::insert_or_touch` (a dedup-touch skips extraction), then
//! `extract::extract_untrusted` over the body, then `extract::persist_candidates_deduped`. Batch
//! entities go through `knowledge::persist`, which is where the Dream Cycle sends them. Nothing
//! here re-implements a dedup rule, so every number describes the product's own decision.
//!
//! Candidates are persisted **one at a time**. `persist_candidates_deduped` already processes its
//! slice candidate by candidate with no shared state between iterations, so this is the same
//! decision sequence — and it maps each returned row id to exactly the candidate (and therefore the
//! labelled sentence) that produced it, instead of guessing the alignment after skipped candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use rusqlite::Connection;
use serde::Serialize;
use shogun_memory::extract::{self, Candidate};
use shogun_memory::knowledge::Kind;
use shogun_memory::{event_log, knowledge};

use crate::scenario::{Capture, Mechanism, Scenario, Step, Table};

/// Scenario clock origin: 2026-09-21 (an arbitrary, fixed instant so runs are reproducible).
const T0_MS: i64 = 1_790_000_000_000;

/// A row, qualified by its table (ids are per-table).
pub type RowRef = (Table, i64);

/// Identity text for matching an extracted sentence to its declared label. Case, layout whitespace
/// and terminal punctuation carry no meaning for *which sentence* it was.
pub fn sentence_key(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!', '?'])
        .to_lowercase()
}

fn table_of(c: &Candidate) -> Table {
    match c {
        Candidate::Commitment { .. } => Table::Commitments,
        Candidate::OpenLoop { .. } => Table::OpenLoops,
        Candidate::Entity(e) => entity_table(e.kind),
    }
}

fn entity_table(kind: Kind) -> Table {
    match kind {
        Kind::Person => Table::People,
        Kind::Project => Table::Projects,
        _ => Table::Entities,
    }
}

fn description_of(c: &Candidate) -> Option<&str> {
    match c {
        Candidate::Commitment { description, .. } | Candidate::OpenLoop { description, .. } => {
            Some(description)
        }
        Candidate::Entity(_) => None,
    }
}

/// What one label (one real memory) became.
#[derive(Debug, Clone, Serialize)]
pub struct LabelOutcome {
    pub label: &'static str,
    pub table: Table,
    /// Distinct rows this memory was written to. 1 is correct; 0 means it was never extracted.
    pub rows: usize,
}

/// What one scenario produced.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub id: &'static str,
    pub mechanism: Mechanism,
    pub summary: &'static str,
    /// Real memories in the scenario.
    pub expected: usize,
    /// Of those, how many reached at least one row (extraction recall, not dedup's job).
    pub found: usize,
    /// Rows written for labelled memories.
    pub rows: usize,
    /// Rows beyond one per memory: `sum(rows_for_label - 1)`. The repetition.
    pub extra_rows: usize,
    /// Rows that absorbed two or more distinct memories. The opposite failure, and the worse one:
    /// a duplicate is visible and fixable, a wrong merge silently loses a memory.
    pub wrong_merges: usize,
    /// Events that `insert_or_touch` collapsed before extraction ran.
    pub collapsed_at_insert: usize,
    /// Rows written from sentences no label claims — extraction noise, reported, never scored.
    pub unlabelled_rows: usize,
    pub per_label: Vec<LabelOutcome>,
    /// Carried from the scenario: an expected, documented failure.
    pub known_limitation: Option<&'static str>,
}

impl Outcome {
    /// The scenario's own verdict. Precision scenarios fail on a wrong merge; the rest fail on
    /// either a duplicate or a wrong merge.
    pub fn passed(&self) -> bool {
        self.wrong_merges == 0 && (self.mechanism.is_precision() || self.extra_rows == 0)
    }

    /// What the CI gate asserts: a scenario passes, or fails in exactly the way it documents.
    pub fn acceptable(&self) -> bool {
        self.passed() || self.known_limitation.is_some()
    }
}

#[derive(Debug)]
pub enum HarnessError {
    Memory(shogun_memory::MemoryError),
    Sqlite(rusqlite::Error),
    Scenario(String),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(e) => write!(f, "memory: {e}"),
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Scenario(e) => write!(f, "scenario: {e}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<shogun_memory::MemoryError> for HarnessError {
    fn from(e: shogun_memory::MemoryError) -> Self {
        Self::Memory(e)
    }
}

impl From<rusqlite::Error> for HarnessError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// Write one capture through the product path; returns the event id, or `None` when the write was
/// a dedup-touch (extraction is skipped exactly as `capture_and_extract` skips it).
fn ingest(conn: &Connection, cap: &Capture) -> Result<Option<(i64, i64)>, HarnessError> {
    let ts = T0_MS + cap.at_min * 60_000;
    let hash = event_log::content_hash(&cap.body);
    let ev = event_log::NewEvent {
        ts,
        source: cap.source,
        kind: if cap.source == "capture" { "text" } else { "message" },
        app_bundle_id: cap.app,
        window_title: cap.title,
        content: &cap.body,
        content_hash: &hash,
        dwell_ms: 0,
        display_id: None,
        window_bounds: None,
    };
    let (id, touched) = event_log::insert_or_touch_with_thread(conn, &ev, cap.thread)?;
    Ok((!touched).then_some((id, ts)))
}

/// Run one scenario against a fresh in-memory database.
pub fn run(scenario: &Scenario) -> Result<Outcome, HarnessError> {
    let mut conn = shogun_memory::open_in_memory()?;
    let mut rows_of: BTreeMap<&'static str, BTreeSet<RowRef>> = BTreeMap::new();
    let mut unlabelled: BTreeSet<RowRef> = BTreeSet::new();
    let mut collapsed = 0usize;

    for step in &scenario.steps {
        match step {
            Step::Capture(cap) => {
                let Some((event_id, ts)) = ingest(&conn, cap)? else {
                    collapsed += 1;
                    continue;
                };
                // Labels are positional per sentence: the Nth candidate carrying a sentence takes the
                // Nth label declared for it. Two people saying the identical words in one capture
                // cannot be told apart by text, and a text-keyed map would give both the first
                // label — hiding exactly the wrong merge that scenario exists to show. The
                // extractor emits candidates in text order, so occurrence order is declaration
                // order. Past the last declared label, the last one is reused.
                let mut says: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
                for (label, sentence) in &cap.says {
                    says.entry(sentence_key(sentence)).or_default().push(*label);
                }
                let mut seen: BTreeMap<String, usize> = BTreeMap::new();
                for candidate in extract::extract_untrusted(&cap.body) {
                    let table = table_of(&candidate);
                    let label = description_of(&candidate).and_then(|d| {
                        let key = sentence_key(d);
                        let labels = says.get(&key)?;
                        let n = seen.entry(key).or_default();
                        let label = labels.get(*n).or_else(|| labels.last()).copied();
                        *n += 1;
                        label
                    });
                    let ids = extract::persist_candidates_deduped(
                        &mut conn,
                        event_id,
                        std::slice::from_ref(&candidate),
                        ts,
                        ts,
                    )?;
                    for id in ids {
                        match label {
                            Some(l) => {
                                rows_of.entry(l).or_default().insert((table, id));
                            }
                            None => {
                                unlabelled.insert((table, id));
                            }
                        }
                    }
                }
                for (label, entity) in &cap.entities {
                    if let Some(id) = knowledge::persist(&mut conn, event_id, entity, ts)? {
                        rows_of.entry(label).or_default().insert((entity_table(entity.kind), id));
                    }
                }
            }
            Step::Complete(label) => {
                let rows = rows_of.get(label).cloned().unwrap_or_default();
                if rows.is_empty() {
                    return Err(HarnessError::Scenario(format!(
                        "{}: cannot complete {label}, it has no row yet",
                        scenario.id
                    )));
                }
                for (table, id) in rows {
                    let status = match table {
                        Table::Commitments => "done",
                        Table::OpenLoops => "closed",
                        _ => continue,
                    };
                    conn.execute(
                        &format!("UPDATE {} SET status = ?1 WHERE id = ?2", table.as_str()),
                        rusqlite::params![status, id],
                    )?;
                }
            }
        }
    }

    // A row claimed by two labels is a wrong merge, counted once per row.
    let mut owners: BTreeMap<RowRef, usize> = BTreeMap::new();
    for rows in rows_of.values() {
        for r in rows {
            *owners.entry(*r).or_default() += 1;
        }
    }
    let wrong_merges = owners.values().filter(|&&n| n > 1).count();

    let per_label: Vec<LabelOutcome> = scenario
        .truths
        .iter()
        .map(|t| LabelOutcome {
            label: t.label,
            table: t.table,
            rows: rows_of.get(t.label).map_or(0, BTreeSet::len),
        })
        .collect();
    let found = per_label.iter().filter(|l| l.rows > 0).count();
    let extra_rows = per_label.iter().map(|l| l.rows.saturating_sub(1)).sum();
    let rows = owners.len();

    // Labels the scenario never declared are a corpus bug, not a measurement.
    for label in rows_of.keys() {
        if !scenario.truths.iter().any(|t| t.label == *label) {
            return Err(HarnessError::Scenario(format!(
                "{}: label {label} is not declared in truths",
                scenario.id
            )));
        }
    }

    Ok(Outcome {
        id: scenario.id,
        mechanism: scenario.mechanism,
        summary: scenario.summary,
        expected: scenario.truths.len(),
        found,
        rows,
        extra_rows,
        wrong_merges,
        collapsed_at_insert: collapsed,
        unlabelled_rows: unlabelled.len(),
        per_label,
        known_limitation: scenario.known_limitation,
    })
}

/// Run every scenario in the corpus.
pub fn run_all() -> Result<Vec<Outcome>, HarnessError> {
    crate::scenario::scenarios().iter().map(run).collect()
}


/// How the rows already in the store relate to the candidate being timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CostLayout {
    /// `true`: every existing row shares the candidate's window. `false`: each row lives in its own
    /// window, so none of them can ever match and any cost they add is pure scan.
    pub same_window: bool,
    /// `true`: rows talk about one of `TOPICS` subjects, so a sentence's most distinctive word is
    /// shared with about one row in `TOPICS`. `false`: every row repeats the same long words, the
    /// worst case for any word-based prefilter.
    pub topical: bool,
}

/// Distinct subjects in the topical layout.
pub const TOPICS: usize = 200;

/// Latency of persisting one new candidate when `existing` unresolved commitments are already in
/// the store. The dedup lookup runs on the capture path once per candidate, so this is the number
/// that says whether it stays inside the capture budget as a year of memory accumulates.
#[derive(Debug, Clone, Serialize)]
pub struct CostPoint {
    pub layout: CostLayout,
    pub existing: usize,
    pub samples: usize,
    pub p50_us: f64,
    pub p95_us: f64,
}

/// Write one screen capture for the cost run and time persisting what it extracts.
fn timed_capture(conn: &mut Connection, layout: CostLayout, n: usize) -> Result<u128, HarnessError> {
    let body = if layout.topical {
        format!("Sam Reed  10:14 AM\nI'll send the project{:03} brief {n} by Friday.", n % TOPICS)
    } else {
        format!("Sam Reed  10:14 AM\nI'll send the follow-up item {n} by Friday.")
    };
    let title = if layout.same_window {
        "launch-plan (Channel) - Acme - Slack".to_string()
    } else {
        format!("channel-{n} (Channel) - Acme - Slack")
    };
    let ts = T0_MS + n as i64 * 60_000;
    let hash = event_log::content_hash(&body);
    let ev = event_log::NewEvent {
        ts,
        source: "capture",
        kind: "text",
        app_bundle_id: Some("com.tinyspeck.slackmacgap"),
        window_title: Some(&title),
        content: &body,
        content_hash: &hash,
        dwell_ms: 0,
        display_id: None,
        window_bounds: None,
    };
    let (event_id, touched) = event_log::insert_or_touch(conn, &ev)?;
    if touched {
        return Ok(0);
    }
    let candidates = extract::extract_untrusted(&body);
    let started = Instant::now();
    extract::persist_candidates_deduped(conn, event_id, &candidates, ts, ts)?;
    Ok(started.elapsed().as_micros())
}

/// Measure [`CostPoint`]s at each size in `sizes` (ascending), growing one database through them.
pub fn match_cost(
    sizes: &[usize],
    samples: usize,
    layout: CostLayout,
) -> Result<Vec<CostPoint>, HarnessError> {
    let mut conn = shogun_memory::open_in_memory()?;
    let mut written = 0usize;
    let mut next = 0usize;
    let mut out = Vec::with_capacity(sizes.len());
    for &size in sizes {
        while written < size {
            timed_capture(&mut conn, layout, next)?;
            next += 1;
            written += 1;
        }
        let mut timings: Vec<f64> = Vec::with_capacity(samples);
        for _ in 0..samples {
            timings.push(timed_capture(&mut conn, layout, next)? as f64);
            next += 1;
            written += 1;
        }
        timings.sort_by(|a, b| a.total_cmp(b));
        let pick = |q: f64| -> f64 {
            let idx = ((timings.len() as f64 - 1.0) * q).round() as usize;
            timings.get(idx).copied().unwrap_or(0.0)
        };
        out.push(CostPoint {
            layout,
            existing: size,
            samples,
            p50_us: pick(0.50),
            p95_us: pick(0.95),
        });
    }
    Ok(out)
}
