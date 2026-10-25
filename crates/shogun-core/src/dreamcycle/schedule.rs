//! Dream Cycle scheduling glue (§6.7, feature `db`). The *when* and *over-what* of a nightly
//! run — the pure decisions that sit between the macOS trigger (a timer + wall-clock read, on-device)
//! and [`run::decide_and_run`](super::run).
//!
//! The macOS scheduler wakes on a timer, reads the wall clock / idle / power state into
//! [`RunConditions`](super::gate::RunConditions), then calls [`DreamScheduler::tick`]. This module
//! owns the input-window math (what event range a cycle consumes) and the cycle-id derivation, so
//! that logic stays Linux-testable; the adapter contributes only the platform reads it alone can do.

use crate::daemon::{Db, DreamRunLease};
use std::sync::atomic::{AtomicU64, Ordering};

use super::gate::{decide, RunConditions, RunDecision, SkipReason};
use super::outcome::{
    classify_batch_failure, classify_local_failure, yyyymmdd_from_days, DreamReason, FailurePhase,
    RecordKind, ALREADY_RUNNING, POLL_BUDGET_EXHAUSTED, PROVIDER_CREDENTIAL_REQUIRED,
};
use super::jobs::{
    batch_items_with_roster, build_batch_sessions_with_policy, parse_batch_classification, Classifier,
    DbDreamRunner, DreamSummarizer, LocalExtractiveSummarizer, LocalRuleClassifier,
    PrecomputedClassifier,
};
use super::plan::{CycleKind, JobKind, JobState};
use super::run::{run_cycle, run_cycle_fenced, CycleReport, GatedRun};

/// The event-time window `[from_ts, to_ts)` a cycle should consume: from the high-water mark of the
/// last completed consolidation up to `now`. On the very first run (no prior consolidation) it falls
/// back to `now - default_lookback_ms` so an install with history still gets a bounded first window.
pub fn input_range(
    last_consolidated_to: Option<i64>,
    now_ms: i64,
    default_lookback_ms: i64,
) -> (i64, i64) {
    let from = last_consolidated_to.unwrap_or_else(|| now_ms - default_lookback_ms);
    // never invert: a clock skew (last > now) collapses to an empty range at `now`.
    (from.min(now_ms), now_ms)
}

/// The job name Consolidation's send telemetry is recorded under (V54 `dream_batch_send_stats.job`).
pub const CONSOLIDATION_JOB: &str = "consolidation";

/// Default first-run lookback if no cycle has ever completed: one day.
pub const DEFAULT_LOOKBACK_MS: i64 = 24 * 60 * 60 * 1000;

static DREAM_RUN_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

fn dream_run_owner() -> String {
    let sequence = DREAM_RUN_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

struct OwnedDreamRun<'a> {
    db: &'a Db,
    cycle_id: String,
    lease: DreamRunLease,
}

impl OwnedDreamRun<'_> {
    fn renew(&self) -> Result<(), String> {
        self.db
            .try_renew_dream_cycle_run(&self.cycle_id, &self.lease)
    }
}

impl Drop for OwnedDreamRun<'_> {
    fn drop(&mut self) {
        let _ = self
            .db
            .try_release_dream_cycle_run(&self.cycle_id, &self.lease);
    }
}

fn claim_owned_run<'a>(db: &'a Db, cycle_id: &str) -> Result<OwnedDreamRun<'a>, String> {
    let lease = db
        .try_claim_dream_cycle_run(cycle_id, dream_run_owner())?
        .ok_or_else(|| ALREADY_RUNNING.to_string())?;
    Ok(OwnedDreamRun {
        db,
        cycle_id: cycle_id.to_string(),
        lease,
    })
}

fn claim_and_read_cycle<'a>(
    db: &'a Db,
    cycle_id: &str,
) -> Result<
    (
        OwnedDreamRun<'a>,
        shogun_memory::dream_cycles::DreamCycleRow,
    ),
    String,
> {
    claim_and_read_cycle_with(db, cycle_id, || {})
}

fn claim_and_read_cycle_with<'a, F>(
    db: &'a Db,
    cycle_id: &str,
    after_claim: F,
) -> Result<
    (
        OwnedDreamRun<'a>,
        shogun_memory::dream_cycles::DreamCycleRow,
    ),
    String,
>
where
    F: FnOnce(),
{
    let owned = claim_owned_run(db, cycle_id)?;
    after_claim();
    let cycle = db
        .try_dream_cycle(cycle_id)?
        .ok_or_else(|| "leased Dream cycle disappeared".to_string())?;
    Ok((owned, cycle))
}

// The window is local wall-clock (FR-DC-01: default 02:00–06:00 local). The adapter can read the
// clock and the zone offset but must not own the calendar math, so all of it is here and tested.

/// Default nightly window (FR-DC-01), local hours: `[START, END)`.
pub const DEFAULT_WINDOW_START_HOUR: u32 = 2;
pub const DEFAULT_WINDOW_END_HOUR: u32 = 6;

/// Local wall-clock date and hour, derived from a UTC instant plus the zone's offset. The adapter
/// supplies both (macOS `localtime_r` gives them together); everything downstream is pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// `YYYYMMDD`, local.
    pub yyyymmdd: u32,
    /// Local hour, 0..=23.
    pub hour: u32,
}

/// Local date/hour for a UTC instant. `gmt_offset_secs` is seconds east of UTC (`-25200` for
/// UTC-7); the adapter reads it from the OS, so DST is already folded in.
pub fn local_time(unix_secs: i64, gmt_offset_secs: i32) -> LocalTime {
    let local = unix_secs + gmt_offset_secs as i64;
    LocalTime {
        yyyymmdd: yyyymmdd_from_days(local.div_euclid(86_400)),
        hour: (local.rem_euclid(86_400) / 3_600) as u32,
    }
}

/// Where the local hour sits relative to the nightly window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPosition {
    /// Inside the window: a satisfied gate runs the full cycle.
    pub within: bool,
    /// The window passed without a full run: the next idle runs the degraded catch-up.
    pub elapsed: bool,
}

/// Position of `hour` relative to `[start, end)`, handling a window that wraps midnight (a user who
/// sets 22:00–04:00). Before the window, both flags are false — there is nothing to do yet.
pub fn window_position(hour: u32, start_hour: u32, end_hour: u32) -> WindowPosition {
    if start_hour <= end_hour {
        WindowPosition {
            within: hour >= start_hour && hour < end_hour,
            elapsed: hour >= end_hour,
        }
    } else {
        // Wrapping: the window is [start, 24) ∪ [0, end). "Elapsed" is the daytime gap between them.
        let within = hour >= start_hour || hour < end_hour;
        WindowPosition {
            within,
            elapsed: !within && hour >= end_hour,
        }
    }
}

// the submit slot (FR-SVC-01)
// Issue #29: each device gets a fixed slot inside the window, derived from its licence device id,
// and treats the window as opening at its slot, so devices in one time zone do not all submit the
// moment the window opens (02:00) and poll in lockstep. Idle / power rules (FR-DC-01) are
// unchanged; only the moment the full cycle may start moves.

/// The tail of the window no slot lands in. The driver re-evaluates every five minutes and the
/// user must also be idle, so a slot in the window's last minutes would miss the night almost
/// every time (and a fixed slot misses it *every* night). Thirty minutes is six chances.
pub const SUBMIT_TAIL_RESERVE_SECS: u32 = 30 * 60;

/// Length of `[start, end)` in seconds, wrapping midnight. `start == end` is an empty window.
pub fn window_len_secs(start_hour: u32, end_hour: u32) -> u32 {
    ((end_hour % 24 + 24 - start_hour % 24) % 24) * 3_600
}

/// How much of the window slots spread over: all of it but the tail reserve (a window shorter
/// than twice the reserve spreads over its first half instead).
pub fn submit_jitter_span_secs(window_len_secs: u32) -> u32 {
    if window_len_secs > 2 * SUBMIT_TAIL_RESERVE_SECS {
        window_len_secs - SUBMIT_TAIL_RESERVE_SECS
    } else {
        window_len_secs / 2
    }
}

/// This device's submit slot: seconds after the window opens (FR-SVC-01,
/// `hash(device_id) mod span`). Deterministic and stable per device — xxh64 with a fixed seed, not
/// the std hasher, whose output is not promised to survive a toolchain update. An empty id (no
/// licence yet, so no shared service to protect) gets slot 0: the window's own start.
pub fn submit_slot_secs(device_id: &str, start_hour: u32, end_hour: u32) -> u32 {
    use std::hash::Hasher;
    let span = submit_jitter_span_secs(window_len_secs(start_hour, end_hour));
    let id = device_id.trim();
    if span == 0 || id.is_empty() {
        return 0;
    }
    let mut h = twox_hash::XxHash64::with_seed(0);
    h.write(id.as_bytes());
    (h.finish() % u64::from(span)) as u32
}

/// [`window_position`] with this device's submit slot applied: inside the window but before the
/// slot reads as "not yet" (neither within nor elapsed — the gate skips as before the window);
/// from the slot to the window's end reads as within. A device that is busy at its slot therefore
/// still runs later that night, the moment it goes idle — the slot is a lower bound, not an
/// appointment. Local wall-clock, like the window itself.
pub fn window_position_with_slot(
    unix_secs: i64,
    gmt_offset_secs: i32,
    start_hour: u32,
    end_hour: u32,
    slot_secs: u32,
) -> WindowPosition {
    let base = window_position(
        local_time(unix_secs, gmt_offset_secs).hour,
        start_hour,
        end_hour,
    );
    if !base.within || slot_secs == 0 {
        return base;
    }
    let second_of_day = (unix_secs + i64::from(gmt_offset_secs)).rem_euclid(86_400);
    let window_start = i64::from(start_hour % 24) * 3_600;
    let since_open = (second_of_day - window_start).rem_euclid(86_400);
    WindowPosition {
        within: since_open >= i64::from(slot_secs),
        elapsed: false,
    }
}

/// The cycle id (`YYYYMMDD`) for the *night* this instant belongs to — the ledger key that makes
/// "a full cycle already ran tonight" answerable (FR-DC-01, FR-DC-04).
///
/// With a window that wraps midnight, 01:00 belongs to the night that started the previous evening,
/// so it must not get a fresh id — otherwise the cycle would restart halfway through.
pub fn cycle_id(unix_secs: i64, gmt_offset_secs: i32, start_hour: u32, end_hour: u32) -> String {
    let local = local_time(unix_secs, gmt_offset_secs);
    let belongs_to_previous_day = start_hour > end_hour && local.hour < end_hour;
    let at = if belongs_to_previous_day {
        unix_secs - 86_400
    } else {
        unix_secs
    };
    format!("{:08}", local_time(at, gmt_offset_secs).yyyymmdd)
}

/// Drives one scheduled evaluation against the real DB. Holds the shared handle and the injected
/// classifier (invariant 5: only a Batch/Select-KK classifier may touch a model; the Linux build
/// injects the local-rule one).
pub struct DreamScheduler<'a, C: Classifier> {
    db: &'a Db,
    classifier: &'a C,
}

impl<'a, C: Classifier> DreamScheduler<'a, C> {
    pub fn new(db: &'a Db, classifier: &'a C) -> Self {
        Self { db, classifier }
    }

    /// Evaluate the run conditions and, if the gate allows, run (or resume) the cycle for `cycle_id`
    /// over the input window derived from the ledger. `now_ms` is the adapter's wall clock. Returns
    /// the gated outcome (Skipped with a reason, or Ran with a report).
    pub fn tick(&self, conditions: &RunConditions, cycle_id: &str, now_ms: i64) -> GatedRun {
        // The store's fault count before this attempt: a failure that moved it is a storage
        // failure, and the morning says so (issue #285).
        let faults = self.db.dream_fault_mark();
        let decision = match scheduled_decision(self.db, conditions) {
            Ok(decision) => decision,
            Err(error) => {
                note_pre_claim_failure(self.db, cycle_id, &error, faults);
                return GatedRun::Ran {
                    cycle: CycleKind::Full,
                    report: failed_report(JobKind::Consolidation, error),
                };
            }
        };
        match decision {
            RunDecision::Full => {
                let cycle = match prepare_full_cycle(self.db, cycle_id, now_ms) {
                    Ok(cycle) => cycle,
                    Err(error) => {
                        note_pre_claim_failure(self.db, cycle_id, &error.to_string(), faults);
                        return GatedRun::Ran {
                            cycle: CycleKind::Full,
                            report: failed_report(JobKind::Consolidation, error.to_string()),
                        };
                    }
                };
                let (owned, cycle) = match claim_and_read_cycle(self.db, &cycle.cycle_id) {
                    Ok(claimed) => claimed,
                    Err(error) => {
                        note_pre_claim_failure(self.db, &cycle.cycle_id, &error, faults);
                        return GatedRun::Ran {
                            cycle: CycleKind::Full,
                            report: failed_report(JobKind::Consolidation, error),
                        };
                    }
                };
                let consolidation_done = match consolidation_done(self.db, &cycle.cycle_id) {
                    Ok(done) => done,
                    Err(error) => {
                        note_local_failure(self.db, &cycle.cycle_id, &error, faults);
                        return GatedRun::Ran {
                            cycle: CycleKind::Full,
                            report: failed_report(JobKind::Consolidation, error),
                        };
                    }
                };
                if !consolidation_done
                    && cycle.batch_stage == shogun_memory::dream_cycles::BatchStage::Pending
                {
                    if let Err(error) = owned.renew().and_then(|()| {
                        self.db
                            .try_mark_dream_batch_local(&cycle.cycle_id, &owned.lease)
                    }) {
                        note_local_failure(self.db, &cycle.cycle_id, &error, faults);
                        let _ = self
                            .db
                            .try_mark_dream_cycle_failed(&cycle.cycle_id, &owned.lease);
                        return GatedRun::Ran {
                            cycle: CycleKind::Full,
                            report: failed_report(JobKind::Consolidation, error),
                        };
                    }
                } else if !consolidation_done
                    && cycle.batch_stage != shogun_memory::dream_cycles::BatchStage::Local
                {
                    self.db
                        .note_dream_failure(&cycle.cycle_id, DreamReason::PlanInactive);
                    let _ = self
                        .db
                        .try_mark_dream_cycle_failed(&cycle.cycle_id, &owned.lease);
                    return GatedRun::Ran {
                        cycle: CycleKind::Full,
                        report: failed_report(
                            JobKind::Consolidation,
                            PROVIDER_CREDENTIAL_REQUIRED.to_string(),
                        ),
                    };
                }
                let summarizer = LocalExtractiveSummarizer;
                let runner =
                    DbDreamRunner::new(self.db, self.classifier, &summarizer, cycle.created_at)
                        .with_frozen_cycle(&cycle.cycle_id);
                let report = run_cycle_fenced(
                    self.db,
                    &runner,
                    &cycle.cycle_id,
                    CycleKind::Full,
                    cycle.input_from_ts,
                    cycle.input_to_ts,
                    &owned.lease,
                    || owned.renew(),
                );
                GatedRun::Ran {
                    cycle: CycleKind::Full,
                    report: finish_full_cycle(self.db, &cycle.cycle_id, &owned, report, faults),
                }
            }
            RunDecision::Degraded => {
                let range = self
                    .db
                    .try_last_consolidated_to()
                    .map(|last| input_range(last, now_ms, DEFAULT_LOOKBACK_MS));
                let (from_ts, to_ts) = match range {
                    Ok(range) => range,
                    Err(error) => {
                        return GatedRun::Ran {
                            cycle: CycleKind::Degraded,
                            report: failed_report(JobKind::StateUpdate, error),
                        };
                    }
                };
                let summarizer = LocalExtractiveSummarizer;
                let runner = DbDreamRunner::new(self.db, self.classifier, &summarizer, now_ms);
                GatedRun::Ran {
                    cycle: CycleKind::Degraded,
                    report: run_cycle(
                        self.db,
                        &runner,
                        cycle_id,
                        CycleKind::Degraded,
                        from_ts,
                        to_ts,
                    ),
                }
            }
            skip => GatedRun::Skipped(skip),
        }
    }
}

/// Apply idle/power policy first, but keep unfinished Full cycles eligible for recovery even
/// after today's work completed. Preparation can prioritize a newer disjoint night when the
/// oldest request remains unknown beyond the normal failure budget.
pub fn scheduled_decision(db: &Db, conditions: &RunConditions) -> Result<RunDecision, String> {
    let ordinary = decide(conditions);
    if matches!(
        ordinary,
        RunDecision::Skip(SkipReason::NotIdle | SkipReason::InsufficientPower)
    ) {
        return Ok(ordinary);
    }
    let unfinished = db.try_oldest_unfinished_full_cycle()?.is_some()
        || db.try_oldest_legacy_unfinished_full_cycle()?.is_some();
    if !unfinished {
        return Ok(ordinary);
    }
    let recovery = RunConditions {
        within_window: true,
        window_elapsed: false,
        full_run_done_today: false,
        ..*conditions
    };
    Ok(decide(&recovery))
}

fn consolidation_done(db: &Db, cycle_id: &str) -> Result<bool, String> {
    Ok(db
        .try_cycle_runs(cycle_id)?
        .iter()
        .any(|run| run.kind == JobKind::Consolidation && run.state == JobState::Done))
}

/// A backfill slice the driver wants the next hourly cycle of that id to cover instead of the
/// cursor window: `(cycle_id, from_ms, to_ms)`.
pub static BACKFILL_SLICE: std::sync::Mutex<Option<(String, i64, i64)>> = std::sync::Mutex::new(None);

fn prepare_full_cycle(
    db: &Db,
    cycle_id: &str,
    now_ms: i64,
) -> Result<shogun_memory::dream_cycles::DreamCycleRow, crate::llm::LlmError> {
    // An hourly cycle (#1066 B) consumes the hour that just closed, aligned to the local clock;
    // a nightly one consumes up to `now`.
    let range = |last: Option<i64>| {
        if let Some((_, from, to)) = BACKFILL_SLICE
            .lock()
            .ok()
            .and_then(|slice| slice.clone())
            .filter(|(id, ..)| id == cycle_id)
        {
            (from, to)
        } else if shogun_memory::jobs::is_hourly_cycle_id(cycle_id) {
            super::hourly::hourly_range(last, now_ms, crate::daemon::os_gmt_offset_secs(now_ms))
        } else {
            input_range(last, now_ms, DEFAULT_LOOKBACK_MS)
        }
    };
    // An hourly window resumes only hourly windows: a nightly cycle (failed, parked on an unknown
    // submission, or held by its own run) is the nightly lane's, never resumed classify-only here.
    let oldest = if shogun_memory::jobs::is_hourly_cycle_id(cycle_id) {
        db.try_oldest_unfinished_hourly_cycle()
    } else {
        db.try_oldest_unfinished_full_cycle()
    };
    if let Some(cycle) = oldest.map_err(crate::llm::LlmError::Provider)? {
        // Preserve unknown paid work and its pinned inputs, but do not let it starve later nights.
        // Freeze excludes arrivals owned by another unfinished cycle; never carry this work over.
        // An hourly window that failed before any provider handle exists (relay down, a 502 on
        // recovery) must not hold back the next hour: that hour gets its own window at once and
        // the failed one keeps retrying on backoff (owned events are excluded from the freeze).
        let blocked_without_handle = if shogun_memory::jobs::is_hourly_cycle_id(cycle_id) {
            cycle.failure_count >= 1
        } else {
            !cycle.expected_custom_ids.is_empty()
                && cycle.failure_count >= shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS
        };
        if cycle.provider_batch_id.is_none() && blocked_without_handle
            && cycle_id > cycle.cycle_id.as_str()
        {
            // Existing actionable nights are already preferred by the shared queue. An existing
            // parked night must not override that queue's rotation through other unknown jobs.
            if db.try_dream_cycle(cycle_id).map_err(crate::llm::LlmError::Provider)?.is_none() {
                let last = db.try_last_consolidated_to().map_err(crate::llm::LlmError::Provider)?;
                let (from_ts, to_ts) = range(Some(last.unwrap_or(cycle.input_to_ts).max(cycle.input_to_ts)));
                db.try_freeze_full_cycle(cycle_id, from_ts, to_ts).map_err(crate::llm::LlmError::Provider)?;
                return db.try_dream_cycle(cycle_id).map_err(crate::llm::LlmError::Provider)?
                    .ok_or_else(|| crate::llm::LlmError::Provider("frozen Dream cycle could not be read".into()));
            }
        }
        return Ok(cycle);
    }
    // Hourly windows are never legacy (pre-ledger) cycles; a stale job-run ledger entry of an
    // abandoned one must not block a new window.
    if let Some((legacy_cycle_id, input_from_ts, input_to_ts)) = db
        .try_oldest_legacy_unfinished_full_cycle()
        .map_err(crate::llm::LlmError::Provider)?
        .filter(|_| !shogun_memory::jobs::is_hourly_cycle_id(cycle_id))
    {
        db.try_freeze_full_cycle(&legacy_cycle_id, input_from_ts, input_to_ts)
            .map_err(crate::llm::LlmError::Provider)?;
        return db
            .try_oldest_unfinished_full_cycle()
            .map_err(crate::llm::LlmError::Provider)?
            .ok_or_else(|| {
                crate::llm::LlmError::Provider("legacy Dream cycle could not be frozen".to_string())
            });
    }
    let last = db
        .try_last_consolidated_to()
        .map_err(crate::llm::LlmError::Provider)?;
    let (from_ts, to_ts) = range(last);
    // A window split for the Batch item ceiling leaves a remainder in the same hour: the id is
    // taken, so the remainder is `<id>.p2`, `.p3`, … (ids sort after their base).
    let mut cycle_id = cycle_id.to_string();
    let mut part = 1;
    while db
        .try_dream_cycle(&cycle_id)
        .map_err(crate::llm::LlmError::Provider)?
        .is_some()
    {
        part += 1;
        cycle_id = format!("{}.p{part}", cycle_id.split(".p").next().unwrap_or(&cycle_id));
    }
    db.try_freeze_full_cycle(&cycle_id, from_ts, to_ts)
        .map_err(crate::llm::LlmError::Provider)?;
    // An hourly window reads back its own row: an older nightly cycle may still be unfinished.
    if shogun_memory::jobs::is_hourly_cycle_id(&cycle_id) {
        db.try_oldest_unfinished_hourly_cycle()
    } else {
        db.try_oldest_unfinished_full_cycle()
    }
    .map_err(crate::llm::LlmError::Provider)?
        .ok_or_else(|| {
            crate::llm::LlmError::Provider("frozen Dream cycle could not be read".to_string())
        })
}

fn failed_report(kind: JobKind, error: String) -> CycleReport {
    CycleReport {
        completed: Vec::new(),
        failed: Some((kind, error)),
    }
}

fn finish_full_cycle(
    db: &Db,
    cycle_id: &str,
    owned: &OwnedDreamRun<'_>,
    mut report: CycleReport,
    faults: u64,
) -> CycleReport {
    let completed_jobs = report.is_complete();
    let state_result = owned.renew().and_then(|()| {
        if completed_jobs {
            db.try_mark_dream_cycle_complete(cycle_id, &owned.lease)
        } else {
            db.try_mark_dream_cycle_failed(cycle_id, &owned.lease)
        }
    });
    if let Err(error) = state_result {
        if completed_jobs {
            let _ = db.try_mark_dream_cycle_failed(cycle_id, &owned.lease);
        }
        report.failed = Some((JobKind::LessonDistillation, error));
    }
    match &report.failed {
        None => db.note_dream_outcome(cycle_id, RecordKind::Completed, None),
        // Everything past the classification step is local work: the jobs, the ledger, the swap.
        Some((_, error)) => note_local_failure(db, cycle_id, error, faults),
    }
    report
}

/// Batch clients for one Dream Cycle.
///
/// `classify` is the durable consolidation batch. `brief` is optional and best-effort
/// (issue #316 / FR-HN-04): a miss still completes the cycle with an extractive Morning Brief.
#[derive(Clone, Copy)]
pub struct DreamBatchClients<'a, B> {
    pub classify: &'a B,
    pub brief: Option<&'a B>,
    /// Explicit application output language, never inherited from the process environment.
    pub output_language: &'a str,
}

impl<'a, B> DreamBatchClients<'a, B> {
    pub fn classify_only(classify: &'a B) -> Self {
        Self {
            classify,
            brief: None,
            output_language: "en",
        }
    }

    pub fn with_brief(classify: &'a B, brief: &'a B) -> Self {
        Self {
            classify,
            brief: Some(brief),
            output_language: "en",
        }
    }

    pub fn with_output_language(mut self, language: &'a str) -> Self {
        self.output_language = language;
        self
    }
}

/// Whether the memory store recorded a fault since `faults` was read.
fn storage_fault_since(db: &Db, faults: u64) -> bool {
    db.dream_fault_mark() > faults
}

/// A failure before this runner held the cycle (reading the ledger, freezing the range, claiming
/// the lease). Recorded only when the store faulted: the other causes here — another runner holds
/// the lease, or tonight's cycle was already abandoned — must not overwrite the reason the night
/// actually failed for.
fn note_pre_claim_failure(db: &Db, cycle_id: &str, error: &str, faults: u64) {
    if error != ALREADY_RUNNING && storage_fault_since(db, faults) {
        db.note_dream_failure(cycle_id, DreamReason::StorageError);
    }
}

/// A failure in local work on a claimed cycle.
fn note_local_failure(db: &Db, cycle_id: &str, error: &str, faults: u64) {
    if let Some(reason) = classify_local_failure(error, storage_fault_since(db, faults)) {
        db.note_dream_failure(cycle_id, reason);
    }
}

/// Where a Batch-backed cycle stood, from its durable stage.
fn failure_phase(stage: shogun_memory::dream_cycles::BatchStage) -> FailurePhase {
    use shogun_memory::dream_cycles::BatchStage;
    match stage {
        BatchStage::Pending | BatchStage::Submitting => FailurePhase::Submit,
        BatchStage::Submitted => FailurePhase::Collect,
        BatchStage::ResultsReady
        | BatchStage::Local
        | BatchStage::Abandoned
        | BatchStage::Complete => FailurePhase::Local,
    }
}

/// Run one **Batch-backed** cycle end-to-end (the on-device model path). This is the async wrapper
/// the macOS scheduler awaits: it gates on `conditions`, and only for a **Full** decision does it
/// pay for the Batch/Select-KK lane — read the window, classify via [`classify_via_batch`], wrap the
/// result in a [`PrecomputedClassifier`], then drive the sync cycle. A **Degraded** catch-up runs
/// the state-only sequence with no model call (FR-DC-01); a **Skip** does nothing.
///
/// Keeping the model call here (async) and the cycle sync means the `DreamJobRunner` never bridges
/// async. Generic over the [`BatchLane`](crate::llm::anthropic::BatchLane), like
/// `classify_via_batch` (docs/batch-relay-design.md for the relay).
pub async fn run_batch_cycle<B, F, Fut>(
    db: &Db,
    clients: DreamBatchClients<'_, B>,
    conditions: &RunConditions,
    cycle_id: &str,
    now_ms: i64,
    max_polls: u32,
    mut sleep: F,
) -> Result<GatedRun, crate::llm::LlmError>
where
    B: crate::llm::anthropic::BatchLane,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_batch_cycle_with_backoff(db, clients, conditions, cycle_id, now_ms, max_polls, |_| sleep())
        .await
}

/// The poll budget of one nightly run: as many backed-off polls as fit in two hours of waiting
/// (1+2+4+8 minutes, then every 15 — eleven polls). A batch still running after that fails the
/// run (FR-DC-05) and a later tick resumes it at the capped cadence, so the night's total stays
/// under [`crate::llm::retry::MAX_POLLS_PER_NIGHT`].
pub const DREAM_MAX_POLLS: u32 =
    crate::llm::retry::polls_within(std::time::Duration::from_secs(2 * 60 * 60));

/// A resumed wait (the batch was submitted by an earlier run) starts at the capped cadence rather
/// than at one minute, so a chain of resumes cannot restart the fast part of the schedule.
/// The relay's per-submission item ceiling (`apps/api` `MAX_ITEMS`).
pub const MAX_BATCH_ITEMS: usize = 1000;

/// An hourly window's ceiling (in events, so at least as many sessions): what its few review calls
/// carry ([`super::jobs::window_review_items`]). A window held after days asleep is cut to this
/// and the remainder becomes the next window (`<id>.p2`, …).
pub const MAX_HOURLY_ITEMS: usize =
    super::jobs::WINDOW_SESSIONS_PER_CALL * super::jobs::WINDOW_MAX_CALLS;

const RESUMED_POLL_INDEX: u32 = crate::llm::retry::POLL_BACKOFF_MINUTES.len() as u32 - 1;

/// The longest single sleep between lease renewals. A server `Retry-After` longer than this is
/// honoured in slices, renewing in between, so the morning view never reads a long, legitimate
/// wait as a stalled run (`outcome::STALL_MS`).
const MAX_SLEEP_BETWEEN_RENEWALS: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// [`run_batch_cycle`] with the shared-service poll policy (issue #29 FR-SVC-02): `sleep` is told
/// how long to wait before each poll — 1 → 2 → 4 → 8 → 15 minutes, or the relay's `Retry-After`
/// when it sends one. This is what the macOS driver awaits.
pub async fn run_batch_cycle_with_backoff<B, F, Fut>(
    db: &Db,
    clients: DreamBatchClients<'_, B>,
    conditions: &RunConditions,
    cycle_id: &str,
    now_ms: i64,
    max_polls: u32,
    sleep: F,
) -> Result<GatedRun, crate::llm::LlmError>
where
    B: crate::llm::anthropic::BatchLane,
    F: FnMut(std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let faults = db.dream_fault_mark();
    let decision = scheduled_decision(db, conditions).map_err(|error| {
        note_pre_claim_failure(db, cycle_id, &error, faults);
        crate::llm::LlmError::Provider(error)
    })?;
    match decision {
        RunDecision::Full => {
            let cycle = prepare_full_cycle(db, cycle_id, now_ms).inspect_err(|error| {
                note_pre_claim_failure(db, cycle_id, &error.to_string(), faults);
            })?;
            let (owned, cycle) = claim_and_read_cycle(db, &cycle.cycle_id).map_err(|error| {
                note_pre_claim_failure(db, &cycle.cycle_id, &error, faults);
                crate::llm::LlmError::Provider(error)
            })?;
            let lane = clients.classify;
            match run_frozen_batch_cycle(db, clients, &cycle, &owned, max_polls, sleep).await {
                Ok(report) => Ok(GatedRun::Ran {
                    cycle: CycleKind::Full,
                    report: finish_full_cycle(db, &cycle.cycle_id, &owned, report, faults),
                }),
                Err(error) => {
                    if is_shared_service_failure(&error) {
                        // Issue #29 §5: the driver runs the local lane (or local maintenance)
                        // tonight; the traceability screen says so on the relay's row.
                        use crate::llm::traceability::TraceKind;
                        note_shared_service(db, lane, TraceKind::FellBack);
                    }
                    Err(record_full_cycle_failure(db, &cycle, &owned, error, faults))
                }
            }
        }
        RunDecision::Degraded => {
            // No Batch work in a catch-up (FR-DC-01); the classifier is never consulted by the
            // degraded sequence, so the local-rule one is a safe unused placeholder.
            let classifier = LocalRuleClassifier;
            let summarizer = LocalExtractiveSummarizer;
            let runner = DbDreamRunner::new(db, &classifier, &summarizer, now_ms);
            let last = db
                .try_last_consolidated_to()
                .map_err(crate::llm::LlmError::Provider)?;
            let (from_ts, to_ts) = input_range(last, now_ms, DEFAULT_LOOKBACK_MS);
            let report = run_cycle(db, &runner, cycle_id, CycleKind::Degraded, from_ts, to_ts);
            Ok(GatedRun::Ran {
                cycle: CycleKind::Degraded,
                report,
            })
        }
        skip => Ok(GatedRun::Skipped(skip)),
    }
}

/// The relay could not say what became of a create (HTTP 503 `submission_unknown` on recovery).
fn is_recover_unavailable(error: &crate::llm::LlmError) -> bool {
    matches!(error, crate::llm::LlmError::Provider(m) if m == "relay recover HTTP 503")
}

/// The relay answered HTTP 410 for a results read: the provider-side files are gone for good.
fn is_results_gone(error: &crate::llm::LlmError) -> bool {
    matches!(error, crate::llm::LlmError::Provider(m) if m == "relay results HTTP 410")
}

/// Whether a Batch-lane error came from the shared service being unavailable, busy or capped —
/// the cases where the driver falls back to the local lane — as opposed to a local failure (a
/// lost lease, a storage fault, a malformed frozen window).
fn is_shared_service_failure(error: &crate::llm::LlmError) -> bool {
    use crate::llm::LlmError;
    match error {
        LlmError::RateLimited(..)
        | LlmError::QuotaExhausted(_)
        | LlmError::Unauthorized(..)
        | LlmError::Transport(_) => true,
        LlmError::Provider(message) => {
            message == POLL_BUDGET_EXHAUSTED
                || message.starts_with(crate::llm::relay::REVIEW_FAILED)
                || message.contains("submission unknown")
                || crate::llm::retry::is_retryable(error)
        }
        LlmError::InvalidRequest
        | LlmError::NotConfigured
        | LlmError::NeedsUpdate(_)
        | LlmError::Parse(_)
        | LlmError::PolicyStopped
        | LlmError::TraceUnrecorded => false,
    }
}

/// Record a retry / fell-back marker on the lane's shared-service route (issue #29 §5). A lane
/// that is not a shared service (the direct development client, test mocks) records nothing.
fn note_shared_service<B: crate::llm::anthropic::BatchLane>(
    db: &Db,
    lane: &B,
    kind: crate::llm::traceability::TraceKind,
) {
    use crate::llm::traceability::{TraceRecord, TraceabilitySink};
    if let Some((route, destination)) = lane.shared_service() {
        db.traceability_sink()
            .record(TraceRecord::marker(route, CONSOLIDATION_JOB, destination, kind));
    }
}

/// Sleep `wait` in slices of at most [`MAX_SLEEP_BETWEEN_RENEWALS`], renewing the lease after
/// each, so a long server-requested wait never lets the run read as stalled.
async fn wait_renewing<F, Fut>(
    sleep: &mut F,
    wait: std::time::Duration,
    owned: &OwnedDreamRun<'_>,
) -> Result<(), crate::llm::LlmError>
where
    F: FnMut(std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut remaining = wait;
    loop {
        let slice = remaining.min(MAX_SLEEP_BETWEEN_RENEWALS);
        sleep(slice).await;
        remaining = remaining.saturating_sub(slice);
        if remaining.is_zero() {
            return Ok(());
        }
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
    }
}

async fn run_frozen_batch_cycle<B, F, Fut>(
    db: &Db,
    clients: DreamBatchClients<'_, B>,
    cycle: &shogun_memory::dream_cycles::DreamCycleRow,
    owned: &OwnedDreamRun<'_>,
    max_polls: u32,
    mut sleep: F,
) -> Result<CycleReport, crate::llm::LlmError>
where
    B: crate::llm::anthropic::BatchLane,
    F: FnMut(std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use crate::llm::anthropic::BatchStatus;
    use shogun_memory::dream_cycles::BatchStage;

    // Preserve the cycle's original identities before a classification wait. The Brief worker
    // rechecks their current eligibility before sending and after receiving provider output.
    let date = crate::daemon::local_date_string(cycle.created_at);
    // The Morning Brief belongs to the nightly cycle (#1066 B): an hourly cycle's window is one
    // hour, which is no brief. The brief reads the whole day it summarises, not the cycle's
    // (possibly nearly empty, because the hourly lane already took the events) window.
    let hourly = shogun_memory::jobs::is_hourly_cycle_id(&cycle.cycle_id);
    if !hourly && db.try_dream_cycle_inputs_partitioned(&cycle.cycle_id).is_ok() {
        let _ = db.ensure_local_brief(cycle.created_at);
        let (from, to) = crate::daemon::brief_input_window(
            cycle.created_at,
            cycle.input_from_ts,
            cycle.input_to_ts,
        );
        let _ = db.prepare_brief_generation(&date, from, to, clients.output_language, false);
    }

    // A hourly window an older build left waiting on a provider Batch job is retired here: its
    // events are carried into the next (synchronous) window, so nothing is lost and nothing polls.
    if hourly && matches!(cycle.batch_stage, BatchStage::Submitted | BatchStage::ResultsReady) {
        db.try_mark_dream_cycle_abandoned(&cycle.cycle_id, &owned.lease)
            .map_err(crate::llm::LlmError::Provider)?;
        return Err(crate::llm::LlmError::Provider(format!(
            "{}: retired a legacy batch window; its events move to the next window",
            crate::llm::relay::REVIEW_FAILED
        )));
    }

    // An oversized, not-yet-submitted window is cut at a time boundary so each Batch submission
    // stays within the relay's item ceiling; the released events become the next window.
    let item_cap = if hourly { MAX_HOURLY_ITEMS } else { MAX_BATCH_ITEMS };
    let refreshed;
    let cycle = if cycle.batch_stage == BatchStage::Pending
        && cycle.expected_custom_ids.is_empty()
        && db
            .try_split_dream_cycle(&cycle.cycle_id, &owned.lease, item_cap)
            .map_err(crate::llm::LlmError::Provider)?
    {
        refreshed = db
            .try_dream_cycle(&cycle.cycle_id)
            .map_err(crate::llm::LlmError::Provider)?
            .ok_or_else(|| crate::llm::LlmError::Provider("split Dream cycle vanished".into()))?;
        &refreshed
    } else {
        cycle
    };

    if consolidation_done(db, &cycle.cycle_id).map_err(crate::llm::LlmError::Provider)? {
        let summarizer = LocalExtractiveSummarizer;
        let classifier = LocalRuleClassifier;
        let runner = DbDreamRunner::new(db, &classifier, &summarizer, cycle.created_at)
            .with_frozen_cycle(&cycle.cycle_id);
        return Ok(run_cycle_fenced(
            db,
            &runner,
            &cycle.cycle_id,
            CycleKind::Full,
            cycle.input_from_ts,
            cycle.input_to_ts,
            &owned.lease,
            || owned.renew(),
        ));
    }

    let frozen = db
        .try_dream_cycle_inputs_partitioned(&cycle.cycle_id)
        .map_err(crate::llm::LlmError::Provider)?;
    let window = frozen.partitioned;
    if cycle.batch_stage == BatchStage::Local {
        let summarizer = LocalExtractiveSummarizer;
        let classifier = LocalRuleClassifier;
        let runner = DbDreamRunner::new(db, &classifier, &summarizer, cycle.created_at)
            .with_frozen_cycle(&cycle.cycle_id);
        return Ok(run_cycle_fenced(
            db,
            &runner,
            &cycle.cycle_id,
            CycleKind::Full,
            cycle.input_from_ts,
            cycle.input_to_ts,
            &owned.lease,
            || owned.renew(),
        ));
    }
    if cycle.batch_stage == BatchStage::Complete {
        return Err(crate::llm::LlmError::Provider(
            "unfinished Dream cycle has a completed Batch stage".to_string(),
        ));
    }

    let recovered = if cycle.batch_stage == BatchStage::Pending && !cycle.expected_custom_ids.is_empty() {
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        let handle = match clients.classify.recover_submission("consolidation", &cycle.cycle_id).await {
            Ok(handle) => handle,
            // A create that died without an answer (the Mac slept mid-request) is unknown to the
            // relay for good. After the failure budget, ask it to fence the id (it does so only
            // once the reservation's UTC day is over); a fenced id can never be re-sent, so the
            // cycle is abandoned and its events go to the next window.
            Err(error)
                if is_recover_unavailable(&error)
                    && cycle.failure_count >= shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS =>
            {
                if !matches!(clients.classify.retire_submission("consolidation", &cycle.cycle_id).await, Ok(true)) {
                    return Err(error);
                }
                owned.renew().map_err(crate::llm::LlmError::Provider)?;
                db.try_mark_dream_cycle_abandoned(&cycle.cycle_id, &owned.lease)
                    .map_err(crate::llm::LlmError::Provider)?;
                return Err(crate::llm::LlmError::Provider(format!(
                    "{}: retired an unknown submission; its events move to the next window",
                    crate::llm::relay::REVIEW_FAILED
                )));
            }
            Err(error) => return Err(error),
        };
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        handle
    } else {
        None
    };
    let recovering = recovered.is_some();

    // The sessions of the frozen window (zero-base plan L1). Built on every attempt, whether this
    // is the night's submission or a resume after a restart: the snapshot is immutable and the
    // session ids are a function of their keys, so the same sessions come back, and the anchors
    // the model returns are resolved against the very lines it was shown.
    //
    // The runtime source policy (L0(b): `gmail` rows are Stage B's while mail triage is on) is
    // read now, not frozen with the snapshot (no column for it; migrations are append-only). A
    // resume after the switch moved sees one of two shapes: triage turned on since the
    // submission → the gmail sessions the model answered are simply not rebuilt, and their items
    // count as unresolved (never misattributed); triage turned off → extra gmail sessions the
    // model never saw would fail `submitted_ids_still_valid`, so the resume falls back to the
    // policy under which the ids do match before failing the night closed.
    let policy = db.consolidation_session_policy();
    let already_submitted = recovering || cycle.batch_stage != BatchStage::Pending;
    let built = build_batch_sessions_with_policy(&window.cloud, &window.cloud_meta, &policy);
    let (sessions, mut send_stats) = if already_submitted
        && !built.0.iter().all(|s| cycle.expected_custom_ids.contains(&s.custom_id))
    {
        let other = crate::dreamcycle::jobs::SessionPolicy::for_mail_triage(policy.skip_sources.is_empty());
        build_batch_sessions_with_policy(&window.cloud, &window.cloud_meta, &other)
    } else {
        built
    };
    // Hourly windows (owner decision 2026-10-14): no Batch job. One plain synchronous call per
    // session, answered inline; the cycle stays Pending until it completes, and any failure
    // leaves it Pending to retry on the next tick.
    let mut sync_results: Option<Vec<crate::llm::anthropic::BatchResult>> = None;
    let (provider_batch_id, expected_custom_ids, submitted_ended, pending_item_ids) =
        if cycle.batch_stage == BatchStage::Pending {
            // The known people and projects ride in the shared instruction, so the model maps a
            // mention ("mikel", a handle, a first name) to an existing row instead of a new one.
            let roster = db.entity_roster(&window.cloud);
            let owners = db.owner_names();
            let mut items = batch_items_with_roster(&sessions, &roster, &owners);
            // The relay refuses more than MAX_BATCH_ITEMS per submission, deterministically. A
            // night is ~50 sessions, so this binds only on a backlog window: its first sessions
            // go, the rest keep their local-rule candidates instead of failing the window forever
            // (the expected ids below are taken from `items`, so the cut is exact).
            items.truncate(item_cap);
            if items.is_empty() && !recovering {
                owned.renew().map_err(crate::llm::LlmError::Provider)?;
                db.try_mark_dream_batch_local(&cycle.cycle_id, &owned.lease)
                    .map_err(crate::llm::LlmError::Provider)?;
                // Nothing survived the pre-filter: still a night of telemetry (0 sent), and the
                // night's one counts-only log line.
                db.record_batch_send_stats(&cycle.cycle_id, CONSOLIDATION_JOB, &send_stats);
                db.record_batch_night_counters(&cycle.cycle_id, CONSOLIDATION_JOB, &send_stats);
                let summarizer = LocalExtractiveSummarizer;
                let classifier = LocalRuleClassifier;
                let runner = DbDreamRunner::new(db, &classifier, &summarizer, cycle.created_at)
                    .with_frozen_cycle(&cycle.cycle_id);
                return Ok(run_cycle_fenced(
                    db,
                    &runner,
                    &cycle.cycle_id,
                    CycleKind::Full,
                    cycle.input_from_ts,
                    cycle.input_to_ts,
                    &owned.lease,
                    || owned.renew(),
                ));
            }
            if hourly && !recovering {
                // One call for the whole window (at most WINDOW_MAX_CALLS), one after another,
                // the lease renewed between: a call can sit in rate-limit waits for minutes.
                let (calls, ids) = super::jobs::window_review_items(&sessions, &roster, &owners);
                let mut answers = Vec::with_capacity(calls.len());
                for call in &calls {
                    owned.renew().map_err(crate::llm::LlmError::Provider)?;
                    answers.extend(clients.classify.review(std::slice::from_ref(call)).await?);
                }
                sync_results = Some(super::jobs::split_window_review(&answers, &ids)?);
                owned.renew().map_err(crate::llm::LlmError::Provider)?;
                db.record_batch_send_stats(&cycle.cycle_id, CONSOLIDATION_JOB, &send_stats);
                (String::new(), ids.clone(), false, ids)
            } else {
            // Save ids before egress, so a lost response plus deletion cannot change the
            // expected results of the already-accepted batch. No input text is retained here.
            let expected: Vec<String> = if recovering {
                cycle.expected_custom_ids.clone()
            } else {
                items.iter().map(|item| item.custom_id.clone()).collect()
            };
            if !db
                .try_claim_dream_submission(&cycle.cycle_id, &owned.lease, &expected)
                .map_err(crate::llm::LlmError::Provider)?
            {
                return Err(crate::llm::LlmError::Provider(
                    "Dream provider submission is already owned".to_string(),
                ));
            }
            owned.renew().map_err(crate::llm::LlmError::Provider)?;
            let handle = match recovered {
                Some(handle) => handle,
                None => clients.classify.submit(&items, Some(cycle.cycle_id.as_str())).await?,
            };
            owned.renew().map_err(crate::llm::LlmError::Provider)?;
            db.try_mark_dream_batch_submitted(&cycle.cycle_id, &owned.lease, &handle.id, &expected)
                .map_err(crate::llm::LlmError::Provider)?;
            // Telemetry for what this binary just sent. A recovered handle was submitted by an
            // earlier attempt (possibly an older binary with a different filter), so its counts
            // would describe the wrong request — that attempt's row, if any, stands.
            if !recovering {
                db.record_batch_send_stats(&cycle.cycle_id, CONSOLIDATION_JOB, &send_stats);
            }
            (
                handle.id,
                expected.clone(),
                handle.status.is_ended(),
                expected,
            )
            }
        } else {
            let provider_batch_id = cycle.provider_batch_id.clone().ok_or_else(|| {
                crate::llm::LlmError::Provider(
                    "submitted Dream cycle is missing provider Batch id".to_string(),
                )
            })?;
            if cycle.expected_custom_ids.is_empty() {
                return Err(crate::llm::LlmError::Provider(
                    "submitted Dream cycle is missing expected Batch ids".to_string(),
                ));
            }
            (
                provider_batch_id,
                cycle.expected_custom_ids.clone(),
                false,
                Vec::new(),
            )
        };

    let rebuilt_session_ids: std::collections::HashSet<String> =
        sessions.iter().map(|s| s.custom_id.clone()).collect();
    if !submitted_ids_still_valid(
        recovering || cycle.batch_stage != BatchStage::Pending,
        &pending_item_ids,
        &expected_custom_ids,
        &rebuilt_session_ids,
    ) {
        return Err(crate::llm::LlmError::Provider(
            "frozen Dream inputs do not match submitted Batch ids".to_string(),
        ));
    }

    let results = if let Some(results) = sync_results.take() {
        results
    } else if cycle.batch_stage == BatchStage::ResultsReady || submitted_ended {
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        let results = crate::llm::retry::batch_read(|| clients.classify.results(&provider_batch_id)).await?;
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        results
    } else {
        use crate::llm::retry::{poll_delay, SharedThrottle, TRANSIENT_THROTTLE_LIMIT};

        let mut ended = false;
        // Issue #29 FR-SVC-02: 1 → 2 → 4 → 8 → 15 minutes between polls; a wait resumed from an
        // earlier run starts at the cap.
        let first_index = if cycle.batch_stage == BatchStage::Submitted {
            RESUMED_POLL_INDEX
        } else {
            0
        };
        // A cycle resumed from `Submitted` was already paid for and may long be done: ask now,
        // not after another 15-minute wait.
        let mut next_wait = if cycle.batch_stage == BatchStage::Submitted {
            std::time::Duration::ZERO
        } else {
            poll_delay(first_index)
        };
        let mut transient_streak = 0u32;
        for poll_no in 0..max_polls {
            wait_renewing(&mut sleep, next_wait, owned).await?;
            owned.renew().map_err(crate::llm::LlmError::Provider)?;
            // Quick retries stay for a network blip; a throttled answer goes straight to the
            // backoff below instead of being re-asked seconds later.
            let polled = crate::llm::retry::batch_read_unless(
                || clients.classify.poll(&provider_batch_id),
                |_| clients.classify.last_throttle().is_some(),
            )
            .await;
            owned.renew().map_err(crate::llm::LlmError::Provider)?;
            let handle = match polled {
                Ok(handle) => {
                    transient_streak = 0;
                    handle
                }
                Err(error) => match clients.classify.last_throttle() {
                    // A momentary overload: wait as told, at most TRANSIENT_THROTTLE_LIMIT in a
                    // row. The daily cap, or a third throttle in a row, ends the night.
                    Some(SharedThrottle::Transient(wait))
                        if transient_streak + 1 < TRANSIENT_THROTTLE_LIMIT =>
                    {
                        transient_streak += 1;
                        note_shared_service(
                            db,
                            clients.classify,
                            crate::llm::traceability::TraceKind::Retry,
                        );
                        // The server's interval is the floor; spread above it so a fleet told
                        // the same `Retry-After` does not come back in the same second.
                        next_wait = crate::llm::retry::spread_retry_after(
                            wait,
                            crate::llm::retry::jitter_unit(),
                        );
                        continue;
                    }
                    _ => return Err(error),
                },
            };
            next_wait = poll_delay(first_index.saturating_add(poll_no).saturating_add(1));
            if handle.id != provider_batch_id {
                return Err(crate::llm::LlmError::Parse(
                    "Dream Batch poll returned a different provider id".to_string(),
                ));
            }
            match handle.status {
                BatchStatus::Ended => {
                    ended = true;
                    break;
                }
                BatchStatus::InProgress | BatchStatus::Canceling | BatchStatus::Other(_) => {}
            }
        }
        if !ended {
            return Err(crate::llm::LlmError::Provider(
                POLL_BUDGET_EXHAUSTED.to_string(),
            ));
        }
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        let results = crate::llm::retry::batch_read(|| clients.classify.results(&provider_batch_id)).await?;
        owned.renew().map_err(crate::llm::LlmError::Provider)?;
        results
    };
    let deleted: std::collections::HashSet<i64> =
        frozen.deleted_cloud_event_ids.into_iter().collect();
    // Anchors are resolved here against `sessions`; what cannot be resolved is dropped and counted
    // in `parsed.anchors` (never attributed by guess).
    let parsed = parse_batch_classification(&expected_custom_ids, &sessions, &results);
    // A window the model answered unparseably (`LlmError::Parse`) is retried, then abandoned (see
    // `record_full_cycle_failure`).
    let parsed = parsed?;
    db.note_self_names(&parsed.self_names);
    // The night's final counters (sessions, dedup, cap, shadowed rows, anchors): one log line,
    // and what `dream_status` shows next to `last_send`. Counts only; the V54 row is not touched.
    send_stats.anchors = parsed.anchors;
    db.record_batch_night_counters(&cycle.cycle_id, CONSOLIDATION_JOB, &send_stats);
    let classified = parsed
        .by_event
        .into_iter()
        .filter(|(event_id, _)| !deleted.contains(event_id));
    owned.renew().map_err(crate::llm::LlmError::Provider)?;
    if cycle.batch_stage != BatchStage::Pending {
        db.try_mark_dream_batch_results_ready(&cycle.cycle_id, &owned.lease)
            .map_err(crate::llm::LlmError::Provider)?;
    }
    let local = LocalRuleClassifier.classify(&window.local_only);
    let classifier = PrecomputedClassifier::new(classified.chain(local).collect());
    // Brief is best-effort: a timeout or parse miss degrades to extractive prose (FR-MB-04)
    // and must not fail Consolidation (FR-DC-05).
    let summarizer = DreamSummarizer::extractive();
    let runner = DbDreamRunner::new(db, &classifier, &summarizer, cycle.created_at)
        .with_frozen_cycle(&cycle.cycle_id);
    let report = run_cycle_fenced(
        db,
        &runner,
        &cycle.cycle_id,
        CycleKind::Full,
        cycle.input_from_ts,
        cycle.input_to_ts,
        &owned.lease,
        || owned.renew(),
    );
    if report.is_complete() && !hourly {
        if let Some(client) = clients.brief {
            let _ = db.advance_brief_generation(&date, client).await;
        } else {
            let _ = db.brief_credentials_unavailable(&date);
        }
    }
    Ok(report)
}

/// Pending submit: current items must equal the ids we just persisted.
/// Already submitted: resume only against persisted ids. Every session rebuilt from the frozen
/// window must be one that was submitted — a session this binary builds that the submitting one
/// did not (a changed grouping or pre-filter rule after an upgrade) would resolve the model's
/// anchors against lines it never saw, so the night fails instead. A submitted session that is
/// *missing* from the rebuild is tolerated: an explicit deletion emptied it, or an upgraded
/// pre-filter dropped it; its results are shape-checked and resolve nothing, by design.
fn submitted_ids_still_valid(
    already_submitted: bool,
    pending_item_ids: &[String],
    durable_expected: &[String],
    rebuilt_session_ids: &std::collections::HashSet<String>,
) -> bool {
    if !already_submitted {
        let mut frozen = pending_item_ids.to_vec();
        let mut expected = durable_expected.to_vec();
        frozen.sort();
        expected.sort();
        return frozen == expected;
    }
    rebuilt_session_ids
        .iter()
        .all(|id| durable_expected.contains(id))
}

/// An hourly (synchronous, handle-less) window that failed for its content: the relay answered
/// but said the model refused it ([`crate::llm::relay::REVIEW_REFUSED`]), or its answer does not
/// parse against the window contract (`LlmError::Parse`). Nothing else: an unreachable relay, a
/// throttle, a cap, a refused licence and every other HTTP status (an old relay's 400, a 404, the
/// model provider's 5xx) is an outage that keeps the window retrying on backoff, never abandoned.
fn is_poisoned_hourly_window(
    cycle: &shogun_memory::dream_cycles::DreamCycleRow,
    error: &crate::llm::LlmError,
) -> bool {
    use crate::llm::LlmError;
    shogun_memory::jobs::is_hourly_cycle_id(&cycle.cycle_id)
        && cycle.provider_batch_id.is_none()
        && match error {
            LlmError::Parse(_) => true,
            LlmError::Provider(message) => message.starts_with(crate::llm::relay::REVIEW_REFUSED),
            _ => false,
        }
}

fn record_full_cycle_failure(
    db: &Db,
    cycle: &shogun_memory::dream_cycles::DreamCycleRow,
    owned: &OwnedDreamRun<'_>,
    error: crate::llm::LlmError,
    faults: u64,
) -> crate::llm::LlmError {
    // Why, before anything else touches the row: the stage it stopped at is part of the answer.
    let stage = db
        .try_dream_cycle(&cycle.cycle_id)
        .ok()
        .flatten()
        .map_or(cycle.batch_stage, |row| row.batch_stage);
    if let Some(reason) = classify_batch_failure(
        &error,
        failure_phase(stage),
        storage_fault_since(db, faults),
    ) {
        db.note_dream_failure(&cycle.cycle_id, reason);
    }
    if owned.renew().is_err() {
        return error;
    }
    let ledger = match consolidation_done(db, &cycle.cycle_id) {
        Ok(true) => Ok(()),
        Ok(false) => db.try_record_job_fenced(
            &cycle.cycle_id,
            &owned.lease,
            JobKind::Consolidation,
            JobState::Failed,
            cycle.input_from_ts,
            cycle.input_to_ts,
        ),
        Err(_) => Err("dream job ledger read failed".to_string()),
    };
    let status = if is_results_gone(&error) {
        db.try_mark_dream_cycle_abandoned(&cycle.cycle_id, &owned.lease)
    } else if is_poisoned_hourly_window(cycle, &error) {
        // Counts toward MAX_FAILURE_ATTEMPTS; at the limit the window is abandoned and, unlike
        // an outage, its events are not carried forward, so one bad window is never re-sent.
        db.try_mark_dream_cycle_refused(&cycle.cycle_id, &owned.lease)
    } else if is_shared_service_failure(&error) {
        db.try_mark_dream_cycle_failed_retryable(&cycle.cycle_id, &owned.lease)
    } else {
        db.try_mark_dream_cycle_failed(&cycle.cycle_id, &owned.lease)
    };
    if ledger.is_err() || status.is_err() {
        crate::llm::LlmError::Provider("Dream failure state could not be persisted".to_string())
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::dreamcycle::gate::{RunConditions, IDLE_THRESHOLD_MS};
    use crate::dreamcycle::jobs::LocalRuleClassifier;
    use crate::dreamcycle::run::GatedRun;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{mpsc, Arc, Mutex};
    use tokio::sync::Notify;

    #[test]
    fn submitted_session_ids_tolerate_shrinkage_but_never_a_session_that_was_not_sent() {
        let set = |ids: &[&str]| -> std::collections::HashSet<String> {
            ids.iter().map(|s| s.to_string()).collect()
        };
        let expected: Vec<String> = vec!["s1".into(), "s2".into(), "s3".into()];
        // Resumed: the same sessions come back → valid.
        assert!(submitted_ids_still_valid(true, &[], &expected, &set(&["s1", "s2", "s3"])));
        // Resumed: a session vanished (explicit deletion, or an upgraded pre-filter) → tolerated;
        // its results resolve nothing.
        assert!(submitted_ids_still_valid(true, &[], &expected, &set(&["s1", "s3"])));
        assert!(submitted_ids_still_valid(true, &[], &expected, &set(&[])));
        // Resumed: a session the submitting binary never sent → anchors would be resolved against
        // lines the model never saw → the night fails.
        assert!(!submitted_ids_still_valid(true, &[], &expected, &set(&["s1", "s2", "s3", "s9"])));
        assert!(!submitted_ids_still_valid(true, &[], &expected, &set(&["s1", "s9"])));
        // Pending: the items about to go out must be exactly the ids just persisted.
        let items: Vec<String> = vec!["s3".into(), "s1".into(), "s2".into()];
        assert!(submitted_ids_still_valid(false, &items, &expected, &set(&[])));
        let fewer: Vec<String> = vec!["s1".into(), "s3".into()];
        assert!(!submitted_ids_still_valid(false, &fewer, &expected, &set(&[])));
    }

    #[derive(Default)]
    struct CountingBatchLane {
        brief_result: Option<String>,
        submits: AtomicUsize,
        polls: AtomicUsize,
        results: AtomicUsize,
        ids: Mutex<Vec<String>>,
        client_request_ids: Mutex<Vec<Option<String>>>,
        /// Window review calls: (sessions labelled in the call, the call's data).
        reviews: Mutex<Vec<(usize, String)>>,
    }

    impl CountingBatchLane {
        fn reviewed_sessions(&self) -> usize {
            self.reviews.lock().unwrap().iter().map(|(n, _)| n).sum()
        }
        fn reviewed_text(&self) -> String {
            self.reviews.lock().unwrap().iter().map(|(_, t)| t.as_str()).collect()
        }
    }

    impl crate::llm::anthropic::BatchLane for CountingBatchLane {
        /// One window call per item: counts it, keeps its data, answers "nothing to report".
        fn review(
            &self,
            items: &[crate::llm::anthropic::BatchItem],
        ) -> impl std::future::Future<
            Output = Result<Vec<crate::llm::anthropic::BatchResult>, crate::llm::LlmError>,
        > + Send {
            let calls: Vec<(usize, String)> = items
                .iter()
                .map(|item| (item.chunk.matches("Session: S").count(), item.chunk.clone()))
                .collect();
            let ids = items.iter().map(|item| item.custom_id.clone()).collect::<Vec<_>>();
            async move {
                self.submits.fetch_add(calls.len(), AtomicOrdering::SeqCst);
                self.reviews.lock().unwrap().extend(calls);
                Ok(ids
                    .into_iter()
                    .map(|custom_id| crate::llm::anthropic::BatchResult {
                        custom_id,
                        text: Some(r#"{"sessions":[]}"#.to_string()),
                        error: None,
                    })
                    .collect())
            }
        }

        fn submit(
            &self,
            items: &[crate::llm::anthropic::BatchItem],
            client_request_id: Option<&str>,
        ) -> impl std::future::Future<
            Output = Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError>,
        > + Send {
            let ids = items
                .iter()
                .map(|item| item.custom_id.clone())
                .collect::<Vec<_>>();
            let client_request_id = client_request_id.map(str::to_string);
            async move {
                self.submits.fetch_add(1, AtomicOrdering::SeqCst);
                *self.ids.lock().unwrap() = ids;
                self.client_request_ids.lock().unwrap().push(client_request_id);
                tokio::task::yield_now().await;
                Ok(crate::llm::anthropic::BatchHandle {
                    id: "counted-batch".to_string(),
                    status: crate::llm::anthropic::BatchStatus::Ended,
                })
            }
        }

        async fn poll(
            &self,
            _batch_id: &str,
        ) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.polls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(crate::llm::anthropic::BatchHandle {
                id: "counted-batch".to_string(),
                status: crate::llm::anthropic::BatchStatus::Ended,
            })
        }

        async fn results(
            &self,
            _batch_id: &str,
        ) -> Result<Vec<crate::llm::anthropic::BatchResult>, crate::llm::LlmError> {
            self.results.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self
                .ids
                .lock()
                .unwrap()
                .iter()
                .map(|custom_id| crate::llm::anthropic::BatchResult {
                    custom_id: custom_id.clone(),
                    text: Some(if custom_id.starts_with("morning-brief-") {
                        self.brief_result.clone().unwrap_or_default()
                    } else {
                        r#"{"commitments":[],"open_loops":[]}"#.to_string()
                    }),
                    error: None,
                })
                .collect())
        }
    }

    #[derive(Default)]
    struct LostSubmitResponse {
        lane: CountingBatchLane,
    }

    impl crate::llm::anthropic::BatchLane for LostSubmitResponse {
        async fn submit(
            &self,
            items: &[crate::llm::anthropic::BatchItem],
            request_id: Option<&str>,
        ) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.lane.submit(items, request_id).await?;
            Err(crate::llm::LlmError::Provider(
                "response lost after acceptance".into(),
            ))
        }
        async fn recover_submission(
            &self,
            purpose: &str,
            request_id: &str,
        ) -> Result<Option<crate::llm::anthropic::BatchHandle>, crate::llm::LlmError> {
            assert_eq!(purpose, "consolidation");
            assert_eq!(request_id, "lost-response-night");
            Ok(Some(self.lane.poll("counted-batch").await?))
        }
        async fn poll(
            &self,
            id: &str,
        ) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.lane.poll(id).await
        }
        async fn results(
            &self,
            id: &str,
        ) -> Result<Vec<crate::llm::anthropic::BatchResult>, crate::llm::LlmError> {
            self.lane.results(id).await
        }
    }

    /// The relay lost a create mid-sleep: recovery answers 503 until the id is retired.
    #[derive(Default)]
    struct UnknownUntilRetired {
        lane: CountingBatchLane,
        retires: AtomicUsize,
    }

    impl crate::llm::anthropic::BatchLane for UnknownUntilRetired {
        async fn submit(
            &self,
            items: &[crate::llm::anthropic::BatchItem],
            request_id: Option<&str>,
        ) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.lane.submit(items, request_id).await
        }
        async fn recover_submission(
            &self,
            _purpose: &str,
            _request_id: &str,
        ) -> Result<Option<crate::llm::anthropic::BatchHandle>, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Provider("relay recover HTTP 503".into()))
        }
        async fn retire_submission(&self, purpose: &str, request_id: &str) -> Result<bool, crate::llm::LlmError> {
            assert_eq!((purpose, request_id), ("consolidation", "slept-night"));
            self.retires.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(true)
        }
        async fn poll(&self, id: &str) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.lane.poll(id).await
        }
        async fn results(&self, id: &str) -> Result<Vec<crate::llm::anthropic::BatchResult>, crate::llm::LlmError> {
            self.lane.results(id).await
        }
    }

    #[tokio::test]
    async fn an_unknown_create_is_retired_after_the_budget_and_its_events_resubmitted() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        db.try_freeze_full_cycle("slept-night", now - DEFAULT_LOOKBACK_MS, now).unwrap();
        let session = sid(&db, ids[0]);
        let lane = UnknownUntilRetired::default();
        // Under the failure budget an unknown create is only retried, never retired.
        for attempt in 0..shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS {
            if attempt == 0 {
                let (owned, _) = claim_and_read_cycle(&db, "slept-night").unwrap();
                assert!(db.try_claim_dream_submission("slept-night", &owned.lease, std::slice::from_ref(&session)).unwrap());
                db.try_mark_dream_cycle_failed("slept-night", &owned.lease).unwrap();
            } else {
                assert!(run_batch_cycle(&db, DreamBatchClients::classify_only(&lane), &idle_conditions(), "slept-night", now, 1, || async {}).await.is_err());
            }
        }
        assert_eq!(lane.retires.load(AtomicOrdering::SeqCst), 0);
        assert!(run_batch_cycle(&db, DreamBatchClients::classify_only(&lane), &idle_conditions(), "slept-night", now, 1, || async {}).await.is_err());
        assert_eq!(lane.retires.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(db.try_dream_cycle("slept-night").unwrap().unwrap().status, shogun_memory::dream_cycles::CycleStatus::Abandoned);
        // The same night re-sends its events under a new id.
        let fresh = CountingBatchLane::default();
        assert!(matches!(run_batch_cycle(&db, DreamBatchClients::classify_only(&fresh), &idle_conditions(), "slept-night", now + 60_000, 1, || async {}).await,
            Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert_eq!(fresh.client_request_ids.lock().unwrap().as_slice(), [Some("slept-night.p2".to_string())]);
        assert_eq!(fresh.ids.lock().unwrap().as_slice(), [session]);
    }

    #[tokio::test]
    async fn unknown_dream_does_not_starve_new_nights_or_reassign_paid_inputs() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        db.try_freeze_full_cycle("lost-response-night", now - DEFAULT_LOOKBACK_MS, now).unwrap();
        let lost_session = sid(&db, ids[0]);
        for _ in 0..shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS {
            let (owned, _) = claim_and_read_cycle(&db, "lost-response-night").unwrap();
            assert!(db.try_claim_dream_submission("lost-response-night", &owned.lease, std::slice::from_ref(&lost_session)).unwrap());
            db.try_mark_dream_cycle_failed("lost-response-night", &owned.lease).unwrap();
        }
        let later = now + 86_400_000;
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: later - 1000, source: "capture", kind: "text", app_bundle_id: None,
            window_title: None, content: "New night", content_hash: "new-night", dwell_ms: 0,
            display_id: None, window_bounds: None,
        }).unwrap();
        let newer = prepare_full_cycle(&db, "next-night", later).unwrap();
        assert_eq!(newer.cycle_id, "next-night");
        assert_eq!((newer.input_from_ts, newer.input_to_ts), (now, later));
        let inputs = db.try_dream_cycle_inputs_partitioned("next-night").unwrap();
        assert_eq!(inputs.partitioned.cloud.len(), 1);
        assert_ne!(inputs.partitioned.cloud[0].id, ids[0]);
        let original = db.try_dream_cycle("lost-response-night").unwrap().unwrap();
        assert_eq!(original.expected_custom_ids, std::slice::from_ref(&lost_session));
        assert_eq!(original.status, shogun_memory::dream_cycles::CycleStatus::Failed);
        assert!(original.provider_batch_id.is_none());
        assert_eq!(db.try_dream_cycle_inputs_partitioned("lost-response-night").unwrap().partitioned.cloud[0].id, ids[0]);
        // Resume the newer cycle rather than freezing again; the old identity remains recoverable.
        assert_eq!(prepare_full_cycle(&db, "next-night", later).unwrap().cycle_id, "next-night");
        for _ in 0..shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS {
            let (owned, _) = claim_and_read_cycle(&db, "next-night").unwrap();
            assert!(db.try_claim_dream_submission("next-night", &owned.lease, &[inputs.partitioned.cloud[0].id.to_string()]).unwrap());
            db.try_mark_dream_cycle_failed("next-night", &owned.lease).unwrap();
        }
        // A parked current night must not defeat rotation by always replacing the selected old job.
        assert_eq!(prepare_full_cycle(&db, "next-night", later).unwrap().cycle_id, "lost-response-night");
        let (owned, _) = claim_and_read_cycle(&db, "lost-response-night").unwrap();
        db.try_mark_dream_cycle_failed("lost-response-night", &owned.lease).unwrap();
        let newer_lane = CountingBatchLane::default();
        assert!(matches!(run_batch_cycle(&db, DreamBatchClients::classify_only(&newer_lane), &idle_conditions(), "next-night", later, 1, || async {}).await,
            Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert_eq!(newer_lane.submits.load(AtomicOrdering::SeqCst), 1);
        assert!(!newer_lane.ids.lock().unwrap().contains(&lost_session));
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(later));
        // Once today's work is done, the original request can recover without another create.
        let recovered = LostSubmitResponse::default();
        *recovered.lane.ids.lock().unwrap() = vec![lost_session.clone()];
        assert!(matches!(run_batch_cycle(&db, DreamBatchClients::classify_only(&recovered), &idle_conditions(), "next-night", later, 1, || async {}).await,
            Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert_eq!(recovered.lane.submits.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(db.try_dream_cycle("lost-response-night").unwrap().unwrap().status, shogun_memory::dream_cycles::CycleStatus::Done);
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(later));
    }

    #[test]
    fn a_failed_hourly_window_does_not_hold_back_the_next_hour() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        db.try_freeze_full_cycle("19700101-h08", now - DEFAULT_LOOKBACK_MS, now).unwrap();
        let (owned, _) = claim_and_read_cycle(&db, "19700101-h08").unwrap();
        assert!(db.try_claim_dream_submission("19700101-h08", &owned.lease, &[sid(&db, ids[0])]).unwrap());
        db.try_mark_dream_cycle_failed("19700101-h08", &owned.lease).unwrap(); // one failure, no handle
        let later = now + 3_600_000;
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: later - 1000, source: "capture", kind: "text", app_bundle_id: None,
            window_title: None, content: "New hour", content_hash: "new-hour", dwell_ms: 0,
            display_id: None, window_bounds: None,
        }).unwrap();
        let next = prepare_full_cycle(&db, "19700101-h09", later).unwrap();
        assert_eq!(next.cycle_id, "19700101-h09", "the new hour gets its own window");
        // The failed window stays unfinished (it keeps retrying) and keeps its own events.
        assert_eq!(db.try_dream_cycle("19700101-h08").unwrap().unwrap().status, shogun_memory::dream_cycles::CycleStatus::Failed);
        let old = db.try_dream_cycle("19700101-h08").unwrap().unwrap();
        assert_eq!(db.try_dream_cycle_inputs_partitioned("19700101-h08").unwrap().partitioned.cloud[0].id, ids[0]);
        assert!(old.provider_batch_id.is_none());
    }

    #[test]
    fn a_failed_nightly_never_blocks_or_is_resumed_by_the_hourly_lane() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        // The night's create died mid-sleep: ids claimed, no provider handle, failed once.
        db.try_freeze_full_cycle("19700101", now - DEFAULT_LOOKBACK_MS, now).unwrap();
        let (owned, _) = claim_and_read_cycle(&db, "19700101").unwrap();
        assert!(db.try_claim_dream_submission("19700101", &owned.lease, &[sid(&db, ids[0])]).unwrap());
        db.try_mark_dream_cycle_failed("19700101", &owned.lease).unwrap();
        let night = db.try_dream_cycle("19700101").unwrap().unwrap();
        assert!(!super::super::hourly::nightly_holds_lane(false, night.status == shogun_memory::dream_cycles::CycleStatus::Running, night.run_lease_until, now));
        let later = now + 3_600_000;
        db.capture(&shogun_memory::event_log::NewEvent {
            ts: later - 1000, source: "capture", kind: "text", app_bundle_id: None,
            window_title: None, content: "New hour", content_hash: "new-hour", dwell_ms: 0,
            display_id: None, window_bounds: None,
        }).unwrap();
        // Twice: the second tick of the hour finds its own window, never the nightly cycle.
        for _ in 0..2 {
            assert_eq!(prepare_full_cycle(&db, "19700101-h09", later).unwrap().cycle_id, "19700101-h09");
        }
        assert_eq!(db.try_oldest_unfinished_hourly_cycle().unwrap().unwrap().cycle_id, "19700101-h09");
        let night = db.try_dream_cycle("19700101").unwrap().unwrap();
        assert_eq!(night.status, shogun_memory::dream_cycles::CycleStatus::Failed, "the nightly lane keeps its own cycle");
        assert_eq!(db.try_dream_cycle_inputs_partitioned("19700101").unwrap().partitioned.cloud[0].id, ids[0]);
    }

    #[tokio::test]
    async fn lost_submit_response_recovers_after_frozen_input_deletion() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        for delete_all in [false, true] {
            let (db, ids) = db_with_cloud_events(now, 2);
            let lane = LostSubmitResponse::default();
            assert!(run_batch_cycle(
                &db,
                DreamBatchClients::classify_only(&lane),
                &idle_conditions(),
                "lost-response-night",
                now,
                1,
                || async {}
            )
            .await
            .is_err());
            db.test_delete_dream_input_event(ids[0])?;
            if delete_all {
                db.test_delete_dream_input_event(ids[1])?;
            }
            let retry = run_batch_cycle(
                &db,
                DreamBatchClients::classify_only(&lane),
                &idle_conditions(),
                "next-night",
                now,
                1,
                || async {},
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
            assert!(matches!(retry, GatedRun::Ran { report, .. } if report.is_complete()));
            assert_eq!(lane.lane.submits.load(AtomicOrdering::SeqCst), 1);
            assert_eq!(lane.lane.results.load(AtomicOrdering::SeqCst), 1);
            assert!(db.try_oldest_unfinished_full_cycle()?.is_none());
        }
        Ok(())
    }

    #[derive(Default)]
    struct TakeoverBatchLane {
        submits: AtomicUsize,
        polls: AtomicUsize,
        results: AtomicUsize,
        ids: Mutex<Vec<String>>,
        first_submit_started: Notify,
        release_first_submit: Notify,
    }

    struct PausingCandidateClassifier {
        started: mpsc::SyncSender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl Classifier for PausingCandidateClassifier {
        fn classify(
            &self,
            events: &[shogun_memory::event_log::EventText],
        ) -> Vec<(i64, Vec<shogun_memory::extract::Candidate>)> {
            self.started.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            events
                .first()
                .map(|event| {
                    (
                        event.id,
                        vec![shogun_memory::extract::Candidate::Commitment {
                            direction: shogun_memory::state::CommitmentDirection::Mine,
                            description: "stale owner candidate".to_string(),
                            confidence: 0.6,
                            evidence: None,
                        }],
                    )
                })
                .into_iter()
                .collect()
        }
    }

    struct EmptyClassifier;

    impl Classifier for EmptyClassifier {
        fn classify(
            &self,
            _events: &[shogun_memory::event_log::EventText],
        ) -> Vec<(i64, Vec<shogun_memory::extract::Candidate>)> {
            Vec::new()
        }
    }

    impl crate::llm::anthropic::BatchLane for TakeoverBatchLane {
        fn submit(
            &self,
            items: &[crate::llm::anthropic::BatchItem],
            _client_request_id: Option<&str>,
        ) -> impl std::future::Future<
            Output = Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError>,
        > + Send {
            let ids = items
                .iter()
                .map(|item| item.custom_id.clone())
                .collect::<Vec<_>>();
            async move {
                *self.ids.lock().unwrap() = ids;
                let ordinal = self.submits.fetch_add(1, AtomicOrdering::SeqCst);
                if ordinal == 0 {
                    self.first_submit_started.notify_one();
                    self.release_first_submit.notified().await;
                }
                Ok(crate::llm::anthropic::BatchHandle {
                    id: format!("takeover-batch-{ordinal}"),
                    status: crate::llm::anthropic::BatchStatus::Ended,
                })
            }
        }

        async fn poll(
            &self,
            batch_id: &str,
        ) -> Result<crate::llm::anthropic::BatchHandle, crate::llm::LlmError> {
            self.polls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(crate::llm::anthropic::BatchHandle {
                id: batch_id.to_string(),
                status: crate::llm::anthropic::BatchStatus::Ended,
            })
        }

        async fn results(
            &self,
            _batch_id: &str,
        ) -> Result<Vec<crate::llm::anthropic::BatchResult>, crate::llm::LlmError> {
            self.results.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self
                .ids
                .lock()
                .unwrap()
                .iter()
                .map(|custom_id| crate::llm::anthropic::BatchResult {
                    custom_id: custom_id.clone(),
                    text: Some(r#"{"commitments":[],"open_loops":[]}"#.to_string()),
                    error: None,
                })
                .collect())
        }
    }

    fn db_at(now: i64) -> Db {
        Db::open_in_memory(Arc::new(move || now)).unwrap()
    }

    #[test]
    fn post_claim_read_error_releases_owner_for_immediate_retry() {
        let db = db_at(10_000);
        db.try_freeze_full_cycle("read-error-cycle", 0, 10_000)
            .unwrap();

        let failed = claim_and_read_cycle_with(&db, "read-error-cycle", || {
            db.test_set_dream_batch_items_available(false).unwrap();
        });
        assert!(failed.is_err());

        db.test_set_dream_batch_items_available(true).unwrap();
        let (retry, cycle) = claim_and_read_cycle(&db, "read-error-cycle").unwrap();
        assert_eq!(retry.lease.generation, 2);
        assert_eq!(cycle.run_owner.as_deref(), Some(retry.lease.owner.as_str()));
        retry.renew().unwrap();
    }

    #[tokio::test]
    async fn expired_owner_stops_after_takeover_generation_continues() {
        let now = Arc::new(AtomicI64::new(10_000));
        let clock = Arc::clone(&now);
        let db = Db::open_in_memory(Arc::new(move || clock.load(AtomicOrdering::SeqCst))).unwrap();
        db.capture(&super_ev(9_000, "ship the takeover fix", "takeover-hash"))
            .unwrap();
        let lane = TakeoverBatchLane::default();
        let conditions = idle_conditions();

        let first = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &conditions,
            "takeover-cycle",
            10_000,
            1,
            || async {},
        );
        let second = async {
            lane.first_submit_started.notified().await;
            now.store(10_000 + 4 * 60 * 60 * 1000 + 1, AtomicOrdering::SeqCst);
            let result = run_batch_cycle(
                &db,
                DreamBatchClients::classify_only(&lane),
                &conditions,
                "takeover-cycle",
                now.load(AtomicOrdering::SeqCst),
                1,
                || async {},
            )
            .await;
            lane.release_first_submit.notify_one();
            result
        };

        let (stale, winner) = tokio::join!(first, second);

        assert!(matches!(winner, Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert!(
            matches!(stale, Err(crate::llm::LlmError::Provider(message)) if message.contains("lease"))
        );
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(lane.polls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(lane.results.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(db.try_cycle_runs("takeover-cycle").unwrap().len(), 8);
    }

    #[test]
    fn stale_owner_paused_inside_consolidation_cannot_write_after_takeover() {
        let now = Arc::new(AtomicI64::new(10_000));
        let clock = Arc::clone(&now);
        let db = Db::open_in_memory(Arc::new(move || clock.load(AtomicOrdering::SeqCst))).unwrap();
        db.capture(&super_ev(9_000, "neutral screen text", "effect-fence-hash"))
            .unwrap();
        let conditions = idle_conditions();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let stale_classifier = PausingCandidateClassifier {
            started: started_tx,
            resume: Mutex::new(resume_rx),
        };

        std::thread::scope(|scope| {
            let stale = scope.spawn(|| {
                DreamScheduler::new(&db, &stale_classifier).tick(
                    &conditions,
                    "effect-takeover-cycle",
                    10_000,
                )
            });
            started_rx.recv().unwrap();
            now.store(10_000 + 4 * 60 * 60 * 1000 + 1, AtomicOrdering::SeqCst);

            let winner = DreamScheduler::new(&db, &EmptyClassifier).tick(
                &conditions,
                "effect-takeover-cycle",
                now.load(AtomicOrdering::SeqCst),
            );
            assert!(matches!(winner, GatedRun::Ran { report, .. } if report.is_complete()));
            resume_tx.send(()).unwrap();

            let stale = stale.join().unwrap();
            assert!(matches!(
                stale,
                GatedRun::Ran {
                    report: CycleReport { failed: Some((_, error)), .. },
                    ..
                } if error.contains("lease")
            ));
        });

        assert!(db
            .commitments_due(now.load(AtomicOrdering::SeqCst))
            .iter()
            .all(|commitment| commitment.description != "stale owner candidate"));
    }

    fn idle_conditions() -> RunConditions {
        RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: IDLE_THRESHOLD_MS,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        }
    }

    // issue #29

    #[test]
    fn submit_slot_is_stable_per_device_and_inside_the_spread() {
        let (start, end) = (DEFAULT_WINDOW_START_HOUR, DEFAULT_WINDOW_END_HOUR);
        assert_eq!(window_len_secs(start, end), 4 * 3_600);
        assert_eq!(window_len_secs(22, 4), 6 * 3_600);
        assert_eq!(window_len_secs(3, 3), 0);
        let span = submit_jitter_span_secs(window_len_secs(start, end));
        assert_eq!(span, 4 * 3_600 - SUBMIT_TAIL_RESERVE_SECS);

        let a = submit_slot_secs("device-a", start, end);
        assert_eq!(a, submit_slot_secs("device-a", start, end), "deterministic");
        assert_eq!(a, submit_slot_secs("  device-a ", start, end), "whitespace-insensitive");
        // Pinned: a toolchain or dependency update must not move every device's slot.
        assert_eq!(a, submit_slot_secs("device-a", start, end));
        assert_eq!(submit_slot_secs("", start, end), 0, "no licence → the pre-#29 behaviour");
        assert_eq!(submit_slot_secs("device-a", 3, 3), 0, "empty window → no jitter");

        // A fleet spreads across the whole span instead of landing at 02:00.
        let slots: Vec<u32> = (0..2_000)
            .map(|i| submit_slot_secs(&format!("dev-{i:05}"), start, end))
            .collect();
        assert!(slots.iter().all(|&slot| slot < span));
        let mut per_hour = [0u32; 4];
        for &slot in &slots {
            per_hour[(slot / 3_600) as usize] += 1;
        }
        // 3.5h of span: the first three hours hold ~571 each, the last half hour ~286.
        for (hour, &n) in per_hour.iter().take(3).enumerate() {
            assert!((450..=700).contains(&n), "hour {hour} got {n} of 2000");
        }
        assert!((200..=380).contains(&per_hour[3]), "last half-hour got {}", per_hour[3]);
    }

    #[test]
    fn the_window_opens_at_the_device_slot_and_stays_open_until_its_end() {
        let (start, end) = (DEFAULT_WINDOW_START_HOUR, DEFAULT_WINDOW_END_HOUR);
        let midnight = 1_784_937_600; // 2026-07-25T00:00:00Z
        let at = |h: i64, m: i64| midnight + h * 3_600 + m * 60;
        let slot = 75 * 60; // 03:15
        let pos = |t| window_position_with_slot(t, 0, start, end, slot);
        let not_yet = WindowPosition { within: false, elapsed: false };
        assert_eq!(pos(at(1, 0)), not_yet, "before the window");
        assert_eq!(pos(at(2, 0)), not_yet, "window open, slot not yet reached");
        assert_eq!(pos(at(3, 14)), not_yet);
        assert!(pos(at(3, 15)).within, "the slot itself");
        // Busy at 03:15 → the gate skipped; idle at 05:40 → still tonight's window.
        assert!(pos(at(5, 40)).within, "a missed slot is not a lost night");
        assert_eq!(pos(at(6, 0)), WindowPosition { within: false, elapsed: true });
        // Slot 0 leaves the window unshifted.
        for h in 0..24 {
            assert_eq!(
                window_position_with_slot(at(h, 30), 0, start, end, 0),
                window_position(h as u32, start, end)
            );
        }
        // Local wall-clock: at UTC+9 the same instants shift by nine hours.
        let tokyo = 9 * 3_600;
        assert!(window_position_with_slot(at(3, 15) - i64::from(tokyo), tokyo, start, end, slot).within);
        // A window that wraps midnight counts the slot from its evening start.
        let wrap = |t| window_position_with_slot(t, 0, 22, 4, 3 * 3_600);
        assert!(!wrap(at(23, 30)).within);
        assert!(wrap(at(1, 0)).within, "22:00 + 3h = 01:00");
        assert!(wrap(at(3, 59)).within);
        assert!(wrap(at(4, 0)).elapsed);
    }

    #[test]
    fn the_slot_gates_the_full_cycle_but_not_a_manual_run() {
        use super::super::gate::{decide, RunDecision, SkipReason};
        let midnight = 1_784_937_600;
        let slot = 90 * 60; // 03:30
        let before_slot = window_position_with_slot(midnight + 2 * 3_600, 0, 2, 6, slot);
        let cond = RunConditions {
            within_window: before_slot.within,
            window_elapsed: before_slot.elapsed,
            ..idle_conditions()
        };
        assert_eq!(decide(&cond), RunDecision::Skip(SkipReason::OutsideWindow));
        // "Run now" never consults the slot: its conditions are built without it (desktop
        // `manual_conditions`: outside the window, elapsed → the degraded maintenance pair).
        let manual = RunConditions { within_window: false, window_elapsed: true, ..idle_conditions() };
        assert_eq!(decide(&manual), RunDecision::Degraded);
    }

    #[test]
    fn a_nights_poll_budget_is_eleven_backed_off_polls() {
        assert_eq!(DREAM_MAX_POLLS, 11);
        const { assert!(DREAM_MAX_POLLS <= crate::llm::retry::MAX_POLLS_PER_NIGHT) };
    }

    type RelayLane =
        crate::llm::relay::RelayBatchClient<crate::llm::transport::MockTransport, crate::db_sink::DbTraceabilitySink>;

    fn relay_lane(db: &Db, responses: Vec<crate::llm::transport::HttpResponse>) -> RelayLane {
        use crate::llm::relay::{ModelClass, RelayBatchClient, RelayConfig};
        use crate::llm::{LicenseBearer, Secret};
        RelayBatchClient::new(
            crate::llm::transport::MockTransport::new(responses),
            db.traceability_sink(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            RelayConfig::new(ModelClass::Classify).with_base_url("https://relay.test"),
        )
    }

    /// [`relay_lane`] over a transport the test keeps a handle to, so the submission the lane
    /// actually sent can be read back after the night (the ledger clears the expected ids once
    /// the cycle completes).
    fn relay_lane_on(
        db: &Db,
        transport: Arc<crate::llm::transport::MockTransport>,
    ) -> crate::llm::relay::RelayBatchClient<SharedTransport, crate::db_sink::DbTraceabilitySink> {
        use crate::llm::relay::{ModelClass, RelayBatchClient, RelayConfig};
        use crate::llm::{LicenseBearer, Secret};
        RelayBatchClient::new(
            SharedTransport(transport),
            db.traceability_sink(),
            LicenseBearer::new(Secret::new("v1.license-token-123456")),
            RelayConfig::new(ModelClass::Classify).with_base_url("https://relay.test"),
        )
    }

    /// The `custom_id`s of the one Batch submission `transport` carried, in wire order.
    fn submitted_custom_ids(transport: &crate::llm::transport::MockTransport) -> Vec<String> {
        let submits: Vec<serde_json::Value> = transport
            .sent()
            .iter()
            .filter_map(|r| serde_json::from_str::<serde_json::Value>(r.body.as_deref()?).ok())
            .filter(|b| b.get("items").is_some())
            .collect();
        assert_eq!(submits.len(), 1, "exactly one submission per cycle");
        submits[0]["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|it| it["custom_id"].as_str().expect("custom_id").to_string())
            .collect()
    }

    fn relay(status: u16, body: &str) -> crate::llm::transport::HttpResponse {
        crate::llm::transport::HttpResponse::new(status, body)
    }

    fn relay_ended(session_ids: &[String]) -> crate::llm::transport::HttpResponse {
        let results: Vec<serde_json::Value> = session_ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "custom_id": id,
                    "text": r#"{"commitments":[],"open_loops":[]}"#,
                })
            })
            .collect();
        relay(200, &serde_json::json!({"status":"ended","results":results}).to_string())
    }

    fn markers(db: &Db) -> Vec<shogun_memory::traceability::TraceKind> {
        use shogun_memory::traceability::{Filter, TraceKind};
        let mut kinds: Vec<TraceKind> = db
            .trace_rows(&Filter::default())
            .into_iter()
            .map(|row| row.kind)
            .filter(|kind| *kind != TraceKind::Egress)
            .collect();
        kinds.reverse(); // oldest first
        kinds
    }

    fn recorded_waits() -> (
        Arc<Mutex<Vec<std::time::Duration>>>,
        impl FnMut(std::time::Duration) -> std::future::Ready<()>,
    ) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&waits);
        (waits, move |wait| {
            if let Ok(mut w) = sink.lock() {
                w.push(wait);
            }
            std::future::ready(())
        })
    }

    fn waited_secs(waits: &Mutex<Vec<std::time::Duration>>) -> Vec<u64> {
        waits.lock().map(|w| w.iter().map(|d| d.as_secs()).collect()).unwrap_or_default()
    }

    #[tokio::test]
    async fn dream_polls_back_off_one_two_four_eight_minutes() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let in_progress = r#"{"status":"in_progress"}"#;
        let lane = relay_lane(
            &db,
            vec![
                relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
                relay(200, in_progress),
                relay(200, in_progress),
                relay(200, in_progress),
                relay(200, r#"{"status":"ended"}"#),
                relay_ended(&sids(&db, &ids)),
            ],
        );
        let (waits, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(waited_secs(&waits), vec![60, 120, 240, 480]);
        assert!(markers(&db).is_empty(), "a healthy night records no retry / fell-back");
        // Every relay round trip is timed for `shogun metrics` (submit + 4 polls + results).
        let latency = db.shared_service_latencies(0);
        assert_eq!(latency.len(), 1);
        assert_eq!(latency[0].route, shogun_memory::traceability::Route::BatchRelay);
        assert_eq!((latency[0].count, latency[0].throttled), (6, 0));
        Ok(())
    }

    #[tokio::test]
    async fn a_transient_throttle_is_honoured_and_three_in_a_row_end_the_night() -> Result<(), String> {
        use shogun_memory::traceability::TraceKind;
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _ids) = db_with_cloud_events(now, 1);
        let secs = std::time::Duration::from_secs;
        let busy = r#"{"error":"too many requests"}"#;
        // Exactly one response per request: a quick 1s/2s/4s re-ask would find the queue empty
        // and surface as a transport error instead of the throttle.
        let lane = relay_lane(
            &db,
            vec![
                relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
                relay(429, busy).with_retry_after(secs(30)),
                relay(503, r#"{"error":"metering unavailable"}"#).with_retry_after(secs(1)),
                relay(429, busy).with_retry_after(secs(30)),
            ],
        );
        let (waits, sleep) = recorded_waits();
        let err = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .err()
        .ok_or("the night should end after three throttles")?;
        assert!(matches!(err, crate::llm::LlmError::RateLimited(429, _)), "{err}");
        // The schedule's first minute, then the server's own intervals — spread above them
        // (never below) so a fleet told the same Retry-After does not return in lockstep.
        let waits = waited_secs(&waits);
        assert_eq!(waits.len(), 3);
        assert_eq!(waits[0], 60);
        assert!((30..=60).contains(&waits[1]), "{waits:?}");
        assert!((1..=2).contains(&waits[2]), "{waits:?}");
        assert_eq!(markers(&db), vec![TraceKind::Retry, TraceKind::Retry, TraceKind::FellBack]);
        let latency = db.shared_service_latencies(0);
        assert_eq!((latency[0].count, latency[0].throttled), (4, 3));
        Ok(())
    }

    #[tokio::test]
    async fn a_throttle_then_an_answer_resets_the_streak() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let secs = std::time::Duration::from_secs;
        let busy = r#"{"error":"too many requests"}"#;
        let lane = relay_lane(
            &db,
            vec![
                relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
                relay(429, busy).with_retry_after(secs(45)),
                relay(429, busy).with_retry_after(secs(45)),
                relay(200, r#"{"status":"in_progress"}"#),
                relay(429, busy).with_retry_after(secs(45)),
                relay(429, busy).with_retry_after(secs(45)),
                relay(200, r#"{"status":"ended"}"#),
                relay_ended(&sids(&db, &ids)),
            ],
        );
        let (waits, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        // Throttled attempts still advance the schedule: a busy service gets asked less often.
        let waits = waited_secs(&waits);
        assert_eq!(waits.len(), 6, "{waits:?}");
        assert_eq!((waits[0], waits[3]), (60, 8 * 60));
        for throttled in [waits[1], waits[2], waits[4], waits[5]] {
            assert!((45..=90).contains(&throttled), "{waits:?}");
        }
        assert_eq!(markers(&db).len(), 4, "four retries, no fell-back");
        Ok(())
    }

    #[tokio::test]
    async fn the_daily_cap_ends_the_night_on_the_first_answer() -> Result<(), String> {
        use shogun_memory::traceability::TraceKind;
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _ids) = db_with_cloud_events(now, 1);
        // The relay's cap carries its reason code (and today still a Retry-After to midnight).
        let lane = relay_lane(
            &db,
            vec![relay(429, r#"{"error":"daily chunk cap reached"}"#)
                .with_retry_after(std::time::Duration::from_secs(6_400))],
        );
        let (waits, sleep) = recorded_waits();
        let err = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .err()
        .ok_or("a capped submit must end the night")?;
        assert!(matches!(err, crate::llm::LlmError::RateLimited(429, _)));
        assert!(waited_secs(&waits).is_empty(), "nothing to wait for: the cap refills at midnight");
        assert_eq!(markers(&db), vec![TraceKind::FellBack]);
        Ok(())
    }

    #[tokio::test]
    async fn a_capped_poll_is_not_retried() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _ids) = db_with_cloud_events(now, 1);
        let lane = relay_lane(
            &db,
            vec![
                relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
                // A bare 429 with no interval reads as the cap: never hammer blind.
                relay(429, r#"{"error":"too many requests"}"#),
            ],
        );
        let (waits, sleep) = recorded_waits();
        let err = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .err()
        .ok_or("a capped poll ends the night")?;
        assert!(matches!(err, crate::llm::LlmError::RateLimited(429, _)), "{err}");
        assert_eq!(waited_secs(&waits), vec![60]);
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_window_is_split_and_the_remainder_becomes_the_next_part() -> Result<(), String> {
        use shogun_memory::dream_cycles::CycleStatus;
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, MAX_BATCH_ITEMS + 5);
        db.try_freeze_full_cycle("c1", now - DEFAULT_LOOKBACK_MS, now)?;
        // The window is split by event count (the relay's item ceiling is a backstop for the
        // session lane too); the events of either part fold into the same session (one app, no
        // title, one hour), so each part submits that one session id. The ledger clears the
        // expected ids when a cycle completes, so the submission is read off the wire.
        let session = sids(&db, &ids);
        assert_eq!(session.len(), 1);
        let first_transport = Arc::new(crate::llm::transport::MockTransport::new([
            relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
            relay(200, r#"{"status":"ended"}"#),
            relay_ended(&session),
        ]));
        let first = relay_lane_on(&db, first_transport.clone());
        let (_, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db, DreamBatchClients::classify_only(&first), &idle_conditions(), "c1", now, DREAM_MAX_POLLS, sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(submitted_custom_ids(&first_transport), session);
        let part1 = db.try_dream_cycle("c1")?.ok_or("c1")?;
        assert_eq!(part1.status, CycleStatus::Done);
        assert!(part1.input_to_ts < now, "the window ends where the split fell");
        // The remainder is not lost: the next run freezes it as `c1.p2`.
        let second_transport = Arc::new(crate::llm::transport::MockTransport::new([
            relay(202, r#"{"batch_id":"rb_2","accepted":1}"#),
            relay(200, r#"{"status":"ended"}"#),
            relay_ended(&session),
        ]));
        let second = relay_lane_on(&db, second_transport.clone());
        let (_, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db, DreamBatchClients::classify_only(&second), &idle_conditions(), "c1", now, DREAM_MAX_POLLS, sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(submitted_custom_ids(&second_transport), session);
        let part2 = db.try_dream_cycle("c1.p2")?.ok_or("remainder cycle c1.p2")?;
        assert_eq!(part2.status, CycleStatus::Done);
        assert!(part2.input_from_ts >= part1.input_to_ts, "the remainder starts where the first part ended");
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_window_submits_at_most_the_relay_item_ceiling() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, MAX_BATCH_ITEMS + 5);
        db.try_freeze_full_cycle("c1", now - DEFAULT_LOOKBACK_MS, now)?;
        // Sessions, not events, are the submission's items (L1): the ceiling is read against
        // them, and these events are one session, so the submission is far below it.
        let sent = sids(&db, &ids);
        assert!(sent.len() <= MAX_BATCH_ITEMS);
        let transport = Arc::new(crate::llm::transport::MockTransport::new([
            relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
            relay(200, r#"{"status":"ended"}"#),
            relay_ended(&sent),
        ]));
        let lane = relay_lane_on(&db, transport.clone());
        let (_, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        let submitted = submitted_custom_ids(&transport);
        assert_eq!(submitted, sent);
        assert!(submitted.len() <= MAX_BATCH_ITEMS);
        Ok(())
    }

    #[tokio::test]
    async fn a_resumed_batch_is_polled_at_once_then_at_the_capped_cadence() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        db.try_freeze_full_cycle("c1", now - DEFAULT_LOOKBACK_MS, now)?;
        fixture_submitted(&db, "c1", "rb_1", &sids(&db, &ids));
        let lane = relay_lane(
            &db,
            vec![
                relay(200, r#"{"status":"in_progress"}"#),
                relay(200, r#"{"status":"ended"}"#),
                relay_ended(&sids(&db, &ids)),
            ],
        );
        let (waits, sleep) = recorded_waits();
        let out = run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(waited_secs(&waits), vec![0, 15 * 60]);
        Ok(())
    }

    #[tokio::test]
    async fn a_long_retry_after_is_slept_in_slices_that_renew_the_lease() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let lane = relay_lane(
            &db,
            vec![
                relay(202, r#"{"batch_id":"rb_1","accepted":1}"#),
                relay(503, r#"{"error":"upstream busy"}"#)
                    .with_retry_after(std::time::Duration::from_secs(40 * 60)),
                relay(200, r#"{"status":"ended"}"#),
                relay_ended(&sids(&db, &ids)),
            ],
        );
        let (waits, sleep) = recorded_waits();
        run_batch_cycle_with_backoff(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "c1",
            now,
            DREAM_MAX_POLLS,
            sleep,
        )
        .await
        .map_err(|e| e.to_string())?;
        // 40+ minutes (spread up to 80) in slices of at most 15: never longer than the stall
        // threshold between renewals.
        let waits = waited_secs(&waits);
        assert_eq!(waits[0], 60);
        let slices = &waits[1..];
        assert!(slices.len() >= 3, "{waits:?}");
        assert!(slices.iter().all(|&slice| slice <= 15 * 60), "{waits:?}");
        let total: u64 = slices.iter().sum();
        assert!((40 * 60 - 1..=80 * 60).contains(&total), "{waits:?}");
        Ok(())
    }

    fn succeeded_batch_result(custom_id: &str, classification: &str) -> String {
        serde_json::json!({
            "custom_id": custom_id,
            "result": {
                "type": "succeeded",
                "message": {
                    "content": [{ "type": "text", "text": classification }]
                }
            }
        })
        .to_string()
    }

    fn db_with_cloud_events(now: i64, count: usize) -> (Db, Vec<i64>) {
        let db = db_at(now);
        let ids = (0..count)
            .map(|index| {
                db.capture(&super_ev(
                    now - 1_000 - index as i64,
                    "I will preserve the submitted snapshot through mutation.",
                    &format!("batch-result-{index}"),
                ))
                .unwrap()
                .0
            })
            .collect();
        (db, ids)
    }

    /// The consolidation lane's `custom_id` for the session the event `event_id` falls in — what
    /// a Batch result for that event's text must be keyed by (zero-base plan L1).
    fn sid(db: &Db, event_id: i64) -> String {
        use crate::dreamcycle::session::{session_custom_id, SessionKey};
        let window = db.try_events_in_range_partitioned(0, i64::MAX).unwrap();
        let meta = window
            .cloud_meta
            .iter()
            .find(|m| m.id == event_id)
            .expect("the event is cloud-eligible");
        session_custom_id(&SessionKey::from_meta(meta))
    }

    /// Distinct session ids of `event_ids`, in first-seen order.
    fn sids(db: &Db, event_ids: &[i64]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for id in event_ids {
            let s = sid(db, *id);
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    fn fixture_submitted(db: &Db, cycle_id: &str, batch_id: &str, ids: &[String]) {
        let lease = db
            .try_claim_dream_cycle_run(cycle_id, "fixture-owner".to_string())
            .unwrap()
            .unwrap();
        assert!(db.try_claim_dream_submission(cycle_id, &lease, ids).unwrap());
        db.try_mark_dream_batch_submitted(cycle_id, &lease, batch_id, ids)
            .unwrap();
        db.try_mark_dream_cycle_failed(cycle_id, &lease).unwrap();
    }

    async fn run_ended_batch_response(
        db: &Db,
        now: i64,
        results_body: String,
    ) -> Result<GatedRun, crate::llm::LlmError> {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"batch-results","processing_status":"ended"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: results_body,
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        run_batch_cycle(
            db,
            DreamBatchClients::classify_only(&client),
            &idle_conditions(),
            "result-night",
            now,
            1,
            || async {},
        )
        .await
    }

    #[tokio::test]
    async fn concurrent_full_runners_submit_and_apply_once() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _) = db_with_cloud_events(now, 1);
        let lane = CountingBatchLane::default();
        let conditions = idle_conditions();
        let first = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &conditions,
            "concurrent-night",
            now,
            1,
            || async {},
        );
        let second = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &conditions,
            "concurrent-night",
            now,
            1,
            || async {},
        );

        let (first, second) = tokio::join!(first, second);

        assert_eq!(
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            1,
            "first={first:?}, second={second:?}"
        );
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(lane.polls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(lane.results.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            lane.client_request_ids.lock().unwrap().as_slice(),
            &[Some("concurrent-night".to_string())]
        );
        assert!(db.try_oldest_unfinished_full_cycle().unwrap().is_none());
    }

    #[test]
    fn fenced_full_cycle_never_reaches_long_lived_lock_callers() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let classifier = LocalRuleClassifier;

        let outcome = DreamScheduler::new(&db, &classifier).tick(
            &idle_conditions(),
            "lock-boundary-night",
            now,
        );

        assert!(matches!(outcome, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(db.memory_health().faults_total, 0);
    }

    #[test]
    fn input_range_uses_the_high_water_mark() {
        assert_eq!(input_range(Some(500), 1_000, 9_999), (500, 1_000));
    }

    #[test]
    fn input_range_falls_back_to_lookback_on_first_run() {
        assert_eq!(input_range(None, 1_000, 400), (600, 1_000));
    }

    #[test]
    fn input_range_never_inverts_under_clock_skew() {
        // last consolidated is (impossibly) in the future → clamp to an empty [now, now) range
        assert_eq!(input_range(Some(2_000), 1_000, 400), (1_000, 1_000));
    }

    #[test]
    fn local_time_shifts_by_the_zone_offset() {
        // 2026-07-25T01:30:00Z
        let utc = 1_784_943_000;
        assert_eq!(
            local_time(utc, 0),
            LocalTime {
                yyyymmdd: 20_260_725,
                hour: 1
            }
        );
        // UTC+9: same instant is 10:30 on the 25th
        assert_eq!(
            local_time(utc, 9 * 3600),
            LocalTime {
                yyyymmdd: 20_260_725,
                hour: 10
            }
        );
        // UTC-7: still the 24th, 18:30 — the date must roll back, not just the hour
        assert_eq!(
            local_time(utc, -7 * 3600),
            LocalTime {
                yyyymmdd: 20_260_724,
                hour: 18
            }
        );
    }

    #[test]
    fn civil_dates_survive_leap_days_and_century_rules() {
        // 2024-02-29 (leap), 2000-02-29 (400-year rule), 1900-03-01 (100-year rule: no Feb 29)
        assert_eq!(local_time(1_709_208_000, 0).yyyymmdd, 20_240_229);
        assert_eq!(local_time(951_825_600, 0).yyyymmdd, 20_000_229);
        assert_eq!(local_time(-2_203_848_000, 0).yyyymmdd, 19_000_301);
    }

    #[test]
    fn the_default_window_is_two_to_six_local() {
        let at = |h| window_position(h, DEFAULT_WINDOW_START_HOUR, DEFAULT_WINDOW_END_HOUR);
        // before: nothing to do yet
        assert_eq!(
            at(1),
            WindowPosition {
                within: false,
                elapsed: false
            }
        );
        // inside: end hour is exclusive
        assert!(at(2).within && at(5).within);
        assert!(!at(6).within);
        // after: the catch-up case
        assert_eq!(
            at(6),
            WindowPosition {
                within: false,
                elapsed: true
            }
        );
        assert!(at(23).elapsed);
    }

    #[test]
    fn a_window_that_wraps_midnight_stays_one_window() {
        // 22:00–04:00
        let at = |h| window_position(h, 22, 4);
        assert!(at(22).within && at(23).within && at(0).within && at(3).within);
        assert!(
            !at(4).within,
            "end hour is exclusive on the far side of midnight too"
        );
        // the daytime gap is the elapsed case; the pre-window evening is not
        assert!(at(4).elapsed && at(12).elapsed);
        assert!(!at(0).elapsed, "inside the window is never also elapsed");
    }

    #[test]
    fn a_wrapping_window_keeps_one_cycle_id_across_midnight() {
        // 23:30 local and 01:30 local the next morning are the same night (window 22:00–04:00),
        // so they must share a ledger key — otherwise the cycle restarts halfway through.
        let evening = 1_784_935_800; // 2026-07-24T23:30:00Z
        let small_hours = 1_784_943_200; // 2026-07-25T01:33:20Z
        assert_eq!(cycle_id(evening, 0, 22, 4), "20260724");
        assert_eq!(cycle_id(small_hours, 0, 22, 4), "20260724");
        // with the default (non-wrapping) window, the small hours are simply that day's night
        assert_eq!(
            cycle_id(
                small_hours,
                0,
                DEFAULT_WINDOW_START_HOUR,
                DEFAULT_WINDOW_END_HOUR
            ),
            "20260725"
        );
    }

    #[test]
    fn tick_skips_when_user_is_active() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let clf = LocalRuleClassifier;
        let sched = DreamScheduler::new(&db, &clf);
        let active = RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: 0,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        };
        assert!(matches!(
            sched.tick(&active, "c1", now),
            GatedRun::Skipped(_)
        ));
    }

    #[test]
    fn tick_runs_a_full_cycle_over_the_derived_window() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        // a capture inside the last-day window carries an actionable sentence
        db.capture(&super_ev(now - 1000, "I'll send the report.", "h1"))
            .unwrap();
        let clf = LocalRuleClassifier;
        let sched = DreamScheduler::new(&db, &clf);
        let idle = RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: IDLE_THRESHOLD_MS,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        };
        match sched.tick(&idle, "c1", now) {
            GatedRun::Ran { report, .. } => {
                assert!(report.is_complete());
                // consolidation classified the window → a commitment candidate exists
                assert_eq!(db.commitments_due(now).len(), 1);
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_batch_cycle_full_persists_model_candidates() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        // an event in the window; the model (mocked) classifies it as a commitment
        let (deck_id, _) = db
            .capture(&super_ev(now - 1000, "I'll send the deck.", "h1"))
            .unwrap();

        let transport = MockTransport::new([
            HttpResponse { status: 200, body: r#"{"id":"b","processing_status":"ended"}"#.into(), retry_after: None },
            HttpResponse {
                retry_after: None,
                status: 200,
                // The model names its support as an anchor into the session's lines (L2); the
                // span and the event id are recovered locally.
                body: succeeded_batch_result(
                    &sid(&db, deck_id),
                    r#"{"commitments":[{"direction":"mine","description":"Send the deck","anchor":"I'll send the deck.","anchor_len":19}],"open_loops":[]}"#,
                ),
            },
        ]);
        let client = AnthropicBatchClient::new(
            transport,
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let idle = RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: IDLE_THRESHOLD_MS,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        };
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &idle,
            "c1",
            now,
            3,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(
            out,
            GatedRun::Ran {
                cycle: CycleKind::Full,
                ..
            }
        ));
        // the model's classification is a review signal for the loop consolidator, not a row
        assert!(db.commitments_due(now).iter().all(|c| c.confidence < 0.5));
        assert_eq!(db.loop_signals().len(), 1);
        // #286: the night's send telemetry landed, counts only, and the status surface reads it.
        let sent = db.batch_send_stats(10);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].job, CONSOLIDATION_JOB);
        assert_eq!((sent[0].items_considered, sent[0].items_sent), (1, 1));
        assert!(sent[0].est_tokens_after > 0);
        // One row, one session: the session body's header and line marker cost a few bytes more
        // than the bare row did, so this night shows no reduction (the rate clamps at 0).
        assert!(sent[0].est_tokens_before > 0);
        assert_eq!(db.dream_status("c1", 7).last_send.as_ref(), Some(&sent[0]));
    }

    #[tokio::test]
    async fn run_batch_cycle_brief_success_marks_the_morning_brief_generated() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let lane = CountingBatchLane {
            brief_result: Some(
                serde_json::json!({"what_happened": [{
                    "text": "Planned to preserve the submitted snapshot through mutation.",
                    "source_id": ids[0],
                    "evidence": "I will preserve the submitted snapshot through mutation."
                }]})
                .to_string(),
            ),
            ..Default::default()
        };
        let idle = idle_conditions();
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::with_brief(&lane, &lane),
            &idle,
            "c1",
            now,
            3,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(
            out,
            GatedRun::Ran {
                cycle: CycleKind::Full,
                ..
            }
        ));
        let row = db
            .brief_for(&crate::daemon::local_date_string(now))
            .expect("Morning Brief row");
        assert!(row.generated);
        let payload: shogun_memory::briefs::BriefPayload =
            serde_json::from_str(&row.payload).unwrap();
        assert_eq!(
            payload.what_happened,
            vec!["Planned to preserve the submitted snapshot through mutation.".to_string()]
        );
    }

    #[tokio::test]
    async fn run_batch_cycle_brief_failure_still_completes_with_extractive_brief() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let classify_body = succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#);
        let client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"classify","processing_status":"ended"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: classify_body,
                },
                HttpResponse {
                    retry_after: None,
                    status: 500,
                    body: "brief unavailable".into(),
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::with_brief(&client, &client),
            &idle_conditions(),
            "c1",
            now,
            3,
            || async {},
        )
        .await
        .unwrap();
        assert!(
            matches!(&out, GatedRun::Ran { report, .. } if report.is_complete()),
            "out={out:?}"
        );
        let row = db
            .brief_for(&crate::daemon::local_date_string(now))
            .expect("degraded Morning Brief row");
        assert!(
            !row.generated,
            "a missed Brief batch must not claim generated prose"
        );
    }

    #[tokio::test]
    async fn missing_batch_result_does_not_advance_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        // A second window-hour → a second session, whose result never arrives.
        db.capture(&super_ev(now - 3_600_000 - 1_000, "An earlier hour's unrelated note.", "other-hour"))
            .unwrap();
        let body = succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#);

        let result = run_ended_batch_response(&db, now, body).await;

        assert!(result.is_err());
        assert_eq!(db.last_consolidated_to(), None);
    }

    #[tokio::test]
    async fn duplicate_batch_result_does_not_advance_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let line = succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#);
        let body = format!("{line}\n{line}");

        let result = run_ended_batch_response(&db, now, body).await;

        assert!(result.is_err());
        assert_eq!(db.last_consolidated_to(), None);
    }

    #[tokio::test]
    async fn failed_batch_result_does_not_advance_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let body = serde_json::json!({
            "custom_id": ids[0].to_string(),
            "result": { "type": "errored", "error": { "type": "provider_error" } }
        })
        .to_string();

        let result = run_ended_batch_response(&db, now, body).await;

        assert!(result.is_err());
        assert_eq!(db.last_consolidated_to(), None);
    }

    #[tokio::test]
    async fn unknown_batch_result_does_not_advance_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _ids) = db_with_cloud_events(now, 1);
        let body = succeeded_batch_result("s0000000000000000", r#"{"commitments":[],"open_loops":[]}"#);

        let result = run_ended_batch_response(&db, now, body).await;

        assert!(result.is_err());
        assert_eq!(db.last_consolidated_to(), None);
    }

    #[tokio::test]
    async fn malformed_batch_result_does_not_advance_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, _ids) = db_with_cloud_events(now, 1);

        let result = run_ended_batch_response(&db, now, "not-json".to_string()).await;

        assert!(result.is_err());
        assert_eq!(db.last_consolidated_to(), None);
    }

    #[tokio::test]
    async fn valid_empty_candidate_result_advances_consolidation() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let body = succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#);

        let result = run_ended_batch_response(&db, now, body).await;

        assert!(matches!(
            result,
            Ok(GatedRun::Ran {
                report: CycleReport { failed: None, .. },
                ..
            })
        ));
        assert_eq!(db.last_consolidated_to(), Some(now));
    }

    #[tokio::test]
    async fn run_batch_cycle_skips_without_calling_the_model() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::MockTransport;
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        // no responses queued — a Skip must not call the Batch lane
        let client = AnthropicBatchClient::new(
            MockTransport::new([]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let active = RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: 0,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        };
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &active,
            "c1",
            now,
            3,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(out, GatedRun::Skipped(_)));
    }

    fn super_ev<'a>(
        ts: i64,
        content: &'a str,
        hash: &'a str,
    ) -> shogun_memory::event_log::NewEvent<'a> {
        shogun_memory::event_log::NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content,
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        }
    }

    /// A [`MockTransport`](crate::llm::transport::MockTransport) the test keeps a handle to after
    /// the client takes ownership, so the requests the lane actually sent can be inspected.
    struct SharedTransport(std::sync::Arc<crate::llm::transport::MockTransport>);
    impl crate::llm::transport::HttpTransport for SharedTransport {
        fn send(
            &self,
            req: crate::llm::transport::HttpRequest,
        ) -> impl std::future::Future<
            Output = Result<
                crate::llm::transport::HttpResponse,
                crate::llm::transport::TransportError,
            >,
        > + Send {
            self.0.send(req)
        }
    }

    #[tokio::test]
    async fn restart_after_batch_submit_polls_stored_provider_id_without_second_submit() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, Method, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let path = std::env::temp_dir().join(format!(
            "shogun_dream_batch_restart_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, Arc::new(move || now)).unwrap();
        let event_id = db
            .capture(&super_ev(
                now - 1_000,
                "the deck discussion",
                "restart-event",
            ))
            .unwrap()
            .0;
        let first_transport = Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"provider-batch","processing_status":"in_progress"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 500,
                body: "interrupted".into(),
            },
        ]));
        let first_client = AnthropicBatchClient::new(
            SharedTransport(first_transport),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let first = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&first_client),
            &idle_conditions(),
            "restart-night-1",
            now,
            1,
            || async {},
        )
        .await;
        assert!(first.is_err());
        let frozen = db.try_oldest_unfinished_full_cycle().unwrap().unwrap();
        assert_eq!(frozen.provider_batch_id.as_deref(), Some("provider-batch"));
        drop(db);

        let later = now + 2 * 24 * 60 * 60 * 1000;
        let db = Db::open(&path, Arc::new(move || later)).unwrap();
        let resumed_transport = Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"provider-batch","processing_status":"ended"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: succeeded_batch_result(&sid(&db, event_id), r#"{"commitments":[],"open_loops":[]}"#),
            },
        ]));
        let resumed_client = AnthropicBatchClient::new(
            SharedTransport(resumed_transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&resumed_client),
            &idle_conditions(),
            "restart-night-2",
            later,
            1,
            || async {},
        )
        .await;

        assert!(matches!(resumed, Ok(GatedRun::Ran { .. })));
        assert!(resumed_transport
            .sent()
            .iter()
            .all(|request| request.method == Method::Get));
        assert!(db.cycle_runs("restart-night-2").is_empty());
        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
    }

    #[tokio::test]
    async fn resumed_cycle_keeps_frozen_range_when_now_and_high_water_change() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, Method, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let first_client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"range-batch","processing_status":"in_progress"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 500,
                    body: "interrupted".into(),
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let first = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&first_client),
            &idle_conditions(),
            "range-night-1",
            now,
            1,
            || async {},
        )
        .await;
        assert!(first.is_err());
        let frozen = db.try_oldest_unfinished_full_cycle().unwrap().unwrap();
        let original_range = (frozen.input_from_ts, frozen.input_to_ts);
        assert!(db.record_job(
            "synthetic-newer-watermark",
            JobKind::Consolidation,
            JobState::Done,
            now,
            now + 10_000,
        ));

        let later = now + 2 * 24 * 60 * 60 * 1000;
        let resumed_transport = Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"range-batch","processing_status":"ended"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#),
            },
        ]));
        let resumed_client = AnthropicBatchClient::new(
            SharedTransport(resumed_transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&resumed_client),
            &idle_conditions(),
            "range-night-2",
            later,
            1,
            || async {},
        )
        .await;

        assert!(matches!(resumed, Ok(GatedRun::Ran { .. })));
        assert!(db
            .cycle_runs("range-night-1")
            .iter()
            .all(|run| (run.input_from_ts, run.input_to_ts) == original_range));
        assert!(db.cycle_runs("range-night-2").is_empty());
        assert!(resumed_transport
            .sent()
            .iter()
            .all(|request| request.method == Method::Get));
    }

    #[tokio::test]
    async fn consolidation_done_resumes_later_jobs_without_provider_access() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::MockTransport;
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        db.try_freeze_full_cycle("repair-night", now - DEFAULT_LOOKBACK_MS, now)
            .unwrap();
        fixture_submitted(
            &db,
            "repair-night",
            "expired-provider-handle",
            &[ids[0].to_string()],
        );
        db.try_record_job(
            "repair-night",
            JobKind::Consolidation,
            JobState::Done,
            now - DEFAULT_LOOKBACK_MS,
            now,
        )
        .unwrap();
        db.try_record_job(
            "repair-night",
            JobKind::Compression,
            JobState::Failed,
            now - DEFAULT_LOOKBACK_MS,
            now,
        )
        .unwrap();
        let high_water = db.try_last_consolidated_to().unwrap();
        let transport = Arc::new(MockTransport::new([]));
        let client = AnthropicBatchClient::new(
            SharedTransport(transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let recovery = RunConditions {
            within_window: false,
            window_elapsed: false,
            full_run_done_today: true,
            ..idle_conditions()
        };

        let outcome = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &recovery,
            "new-date-must-not-run",
            now + 86_400_000,
            1,
            || async {},
        )
        .await
        .unwrap();

        assert!(matches!(outcome, GatedRun::Ran { report, .. } if report.is_complete()));
        assert!(
            transport.sent().is_empty(),
            "provider must not be read after Consolidation Done"
        );
        assert_eq!(db.try_last_consolidated_to().unwrap(), high_water);
        assert_eq!(
            db.try_cycle_runs("repair-night")
                .unwrap()
                .into_iter()
                .find(|run| run.kind == JobKind::Consolidation)
                .unwrap()
                .state,
            JobState::Done
        );
        assert!(db
            .try_cycle_runs("new-date-must-not-run")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn explicitly_deleted_submitted_input_is_accounted_and_cycle_completes() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let session = sid(&db, ids[0]);
        let first_client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"delete-batch","processing_status":"in_progress"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 500,
                    body: "interrupted".into(),
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        assert!(run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&first_client),
            &idle_conditions(),
            "delete-night",
            now,
            1,
            || async {},
        )
        .await
        .is_err());
        db.test_delete_dream_input_event(ids[0]).unwrap();
        let transport = Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"delete-batch","processing_status":"ended"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: format!(
                    "{}\n",
                    succeeded_batch_result(
                        &session,
                        r#"{"commitments":[{"description":"must not survive deletion","direction":"mine","anchor":"I will preserve the submitted snapshot thr","anchor_len":56}],"open_loops":[]}"#,
                    )
                ),
            },
        ]));
        let resumed_client = AnthropicBatchClient::new(
            SharedTransport(transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&resumed_client),
            &idle_conditions(),
            "later-night",
            now + 86_400_000,
            1,
            || async {},
        )
        .await;

        assert!(
            matches!(&resumed, Ok(GatedRun::Ran { report, .. }) if report.is_complete()),
            "resumed={resumed:?}"
        );
        assert_eq!(transport.sent().len(), 2);
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(now));
        assert!(db.try_oldest_unfinished_full_cycle().unwrap().is_none());
        assert!(db
            .commitments_due(now + 86_400_000)
            .iter()
            .all(|commitment| commitment.description != "must not survive deletion"));
    }

    #[tokio::test]
    async fn deletion_during_classification_wait_prevents_separate_brief_submission() -> Result<(), Box<dyn std::error::Error>> {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};
        let now = 100 * 86_400_000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let classification = Arc::new(MockTransport::new([
            HttpResponse { status: 200, body: r#"{"id":"classification","processing_status":"in_progress"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: r#"{"id":"classification","processing_status":"ended"}"#.into(), retry_after: None },
            HttpResponse { status: 200, body: succeeded_batch_result(&sid(&db, ids[0]), r#"{"commitments":[],"open_loops":[]}"#), retry_after: None },
        ]));
        let brief_requests = Arc::new(MockTransport::new([]));
        let client = |transport| AnthropicBatchClient::new(SharedTransport(transport), RecordingSink::new(), SelectKkKey::new(Secret::new("fake")), AnthropicConfig::new("fake"));
        let classify = client(classification);
        let brief = client(brief_requests.clone());
        let outcome = run_batch_cycle(&db, DreamBatchClients::with_brief(&classify, &brief), &idle_conditions(), "deleted-before-brief", now, 1, || async {
            assert!(db.test_delete_dream_input_event(ids[0]).is_ok());
        }).await?;
        assert!(matches!(outcome, GatedRun::Ran { report, .. } if report.is_complete()));
        assert!(brief_requests.sent().is_empty());
        assert_eq!(db.brief_generation(&crate::daemon::local_date_string(now))?.ok_or("missing request")?.state, "expired");
        Ok(())
    }

    #[tokio::test]
    async fn deleting_submitted_duplicate_resumes_from_persisted_ids() -> Result<(), String> {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, Method, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let submitted = db
            .capture(&super_ev(now - 3_000, "duplicate screen content", "submitted-twin"))
            .ok_or_else(|| "submitted twin capture failed".to_string())?
            .0;
        let retained = db
            .capture(&super_ev(now - 2_000, "duplicate screen content", "retained-twin"))
            .ok_or_else(|| "retained twin capture failed".to_string())?
            .0;
        let survivor = db
            .capture(&super_ev(now - 1_000, "independent screen content", "survivor"))
            .ok_or_else(|| "survivor capture failed".to_string())?
            .0;
        assert_ne!(submitted, retained);
        db.try_freeze_full_cycle("duplicate-delete-night", now - DEFAULT_LOOKBACK_MS, now)?;
        // One window-hour → one session holding both lines (the retained twin's line is the
        // same line, sent once).
        let session = sid(&db, submitted);
        assert_eq!(session, sid(&db, survivor));
        fixture_submitted(
            &db,
            "duplicate-delete-night",
            "duplicate-delete-batch",
            std::slice::from_ref(&session),
        );
        db.test_delete_dream_input_event(submitted)?;
        let transport = Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"duplicate-delete-batch","processing_status":"ended"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: format!(
                    "{}\n",
                    succeeded_batch_result(
                        &session,
                        r#"{"commitments":[{"description":"anchored on the duplicated line","direction":"mine","anchor":"duplicate screen content","anchor_len":24},{"description":"survives duplicate deletion","direction":"mine","anchor":"independent screen content","anchor_len":26}],"open_loops":[]}"#,
                    ),
                ),
            },
        ]));
        let client = AnthropicBatchClient::new(
            SharedTransport(transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &idle_conditions(),
            "later-night",
            now + 86_400_000,
            1,
            || async {},
        )
        .await;

        assert!(matches!(&resumed, Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert!(transport
            .sent()
            .iter()
            .all(|request| request.method == Method::Get));
        let signals = db.loop_signals();
        assert!(signals.iter().any(|(_, text, _)| text == "survives duplicate deletion"));
        // The duplicated line is still in the window through the retained twin, so the item
        // anchored on it resolves — to the retained event, never to the deleted one.
        let anchored = signals
            .iter()
            .find(|(_, text, _)| text == "anchored on the duplicated line")
            .ok_or_else(|| "anchored signal missing".to_string())?;
        assert_eq!(anchored.2, retained);
        Ok(())
    }

    #[tokio::test]
    async fn old_version_submitted_duplicates_resume_from_persisted_ids() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let first = db
            .capture(&super_ev(now - 3_000, "duplicate screen content", "old-first"))
            .ok_or_else(|| "first old-version capture failed".to_string())?
            .0;
        let second = db
            .capture(&super_ev(now - 2_000, "duplicate screen content", "old-second"))
            .ok_or_else(|| "second old-version capture failed".to_string())?
            .0;
        let third = db
            .capture(&super_ev(now - 1_000, "independent screen content", "old-third"))
            .ok_or_else(|| "third old-version capture failed".to_string())?
            .0;
        db.try_freeze_full_cycle("old-prefilter-night", now - DEFAULT_LOOKBACK_MS, now)?;
        // The submitting binary grouped the window into two sessions; this one folds it into
        // one. The extra submitted session is tolerated (its results resolve nothing); the one
        // this binary rebuilds was submitted, so the resume proceeds without a second egress.
        let _ = (second, third);
        let expected = vec![sid(&db, first), "s00000000000000ff".to_string()];
        fixture_submitted(&db, "old-prefilter-night", "counted-batch", &expected);
        let lane = CountingBatchLane {
            ids: Mutex::new(expected),
            ..Default::default()
        };

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "later-night",
            now + 86_400_000,
            1,
            || async {},
        )
        .await;

        assert!(matches!(resumed, Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn all_dropped_inputs_use_local_fallback_without_submitting() -> Result<(), String> {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        db.capture(&super_ev(now - 1_000, " \t\n ", "all-dropped"))
            .ok_or_else(|| "all-dropped capture failed".to_string())?;
        let lane = CountingBatchLane::default();

        let result = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            "all-dropped-night",
            now,
            1,
            || async {},
        )
        .await;

        assert!(matches!(result, Ok(GatedRun::Ran { report, .. }) if report.is_complete()));
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 0);
        // A night where nothing survived the pre-filter is still a night of telemetry.
        let sent = db.batch_send_stats(10);
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].items_considered, sent[0].items_sent), (1, 0));
        assert_eq!(sent[0].items_dropped_empty, 1);
        assert_eq!(sent[0].est_tokens_after, 0);
        assert!((sent[0].reduction_rate() - 1.0).abs() < 1e-12);
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_mutation_cannot_change_submitted_snapshot_and_resume_completes() {
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let first_client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"mutation-batch","processing_status":"in_progress"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 500,
                    body: "interrupted".into(),
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        assert!(run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&first_client),
            &idle_conditions(),
            "mutation-night",
            now,
            1,
            || async {},
        )
        .await
        .is_err());
        let mutation_error = db.test_mutate_dream_input_event(ids[0]).unwrap_err();
        assert!(mutation_error.contains(shogun_memory::dream_cycles::PINNED_EVENT_ERROR));
        let client = AnthropicBatchClient::new(
            MockTransport::new([
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: r#"{"id":"mutation-batch","processing_status":"ended"}"#.into(),
                },
                HttpResponse {
                    retry_after: None,
                    status: 200,
                    body: format!(
                        "{}\n",
                        succeeded_batch_result(
                            &sid(&db, ids[0]),
                            r#"{"commitments":[{"description":"Preserve submitted snapshot through mutation","direction":"mine","anchor":"I will preserve the submitted snapshot thr","anchor_len":56}],"open_loops":[]}"#,
                        )
                    ),
                },
            ]),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );

        let resumed = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &idle_conditions(),
            "later-night",
            now + 86_400_000,
            1,
            || async {},
        )
        .await;

        assert!(
            matches!(&resumed, Ok(GatedRun::Ran { report, .. }) if report.is_complete()),
            "resumed={resumed:?}"
        );
        assert!(db
            .loop_signals()
            .iter()
            .any(|(_, text, _)| text == "Preserve submitted snapshot through mutation"));
    }

    #[test]
    fn completion_write_repair_precedes_changed_date_and_window() {
        use crate::dreamcycle::plan::FULL_SEQUENCE;

        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let from = now - DEFAULT_LOOKBACK_MS;
        db.try_freeze_full_cycle("old-night", from, now).unwrap();
        for kind in FULL_SEQUENCE {
            db.try_record_job("old-night", *kind, JobState::Done, from, now)
                .unwrap();
        }
        let conditions = RunConditions {
            within_window: false,
            window_elapsed: true,
            full_run_done_today: true,
            ..idle_conditions()
        };
        let classifier = LocalRuleClassifier;

        let outcome =
            DreamScheduler::new(&db, &classifier).tick(&conditions, "new-night", now + 86_400_000);

        assert!(
            matches!(outcome, GatedRun::Ran { cycle: CycleKind::Full, report } if report.is_complete())
        );
        assert!(db.try_oldest_unfinished_full_cycle().unwrap().is_none());
        assert!(db.try_cycle_runs("new-night").unwrap().is_empty());
    }

    #[test]
    fn missing_credential_abandons_bounded_cycle_then_runs_next_with_preserved_range() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let (db, ids) = db_with_cloud_events(now, 1);
        let original_from = now - DEFAULT_LOOKBACK_MS;
        db.try_freeze_full_cycle("submitted-night", original_from, now)
            .unwrap();
        fixture_submitted(
            &db,
            "submitted-night",
            "provider-handle",
            &[ids[0].to_string()],
        );
        let classifier = LocalRuleClassifier;

        for attempt in 1..shogun_memory::dream_cycles::MAX_FAILURE_ATTEMPTS {
            let outcome = DreamScheduler::new(&db, &classifier).tick(
                &idle_conditions(),
                &format!("failed-night-{attempt}"),
                now + 86_400_000,
            );
            assert!(matches!(outcome, GatedRun::Ran { report, .. } if !report.is_complete()));
        }

        let cycle = db.try_dream_cycle("submitted-night").unwrap().unwrap();
        assert_eq!(
            cycle.batch_stage,
            shogun_memory::dream_cycles::BatchStage::Abandoned
        );
        assert_eq!(cycle.provider_batch_id.as_deref(), Some("provider-handle"));
        assert_eq!(
            cycle.status,
            shogun_memory::dream_cycles::CycleStatus::Abandoned
        );
        assert!(db.try_oldest_unfinished_full_cycle().unwrap().is_none());

        let replacement = DreamScheduler::new(&db, &classifier).tick(
            &idle_conditions(),
            "replacement-night",
            now + 86_400_000,
        );
        assert!(
            matches!(replacement, GatedRun::Ran { report, .. } if report.is_complete()),
            "replacement cycle must run after terminal abandonment"
        );
        let replacement = db.try_dream_cycle("replacement-night").unwrap().unwrap();
        assert_eq!(replacement.input_from_ts, original_from);
    }

    #[test]
    fn late_insert_before_high_water_is_processed_by_the_next_cycle() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let classifier = LocalRuleClassifier;
        let scheduler = DreamScheduler::new(&db, &classifier);
        let first = scheduler.tick(&idle_conditions(), "first-night", now);
        assert!(matches!(first, GatedRun::Ran { report, .. } if report.is_complete()));

        db.capture(&super_ev(
            now - 1_000,
            "I will send the late report tomorrow.",
            "late-after-snapshot",
        ))
        .unwrap();
        let later = now + 86_400_000;
        let second = scheduler.tick(&idle_conditions(), "second-night", later);

        assert!(matches!(second, GatedRun::Ran { report, .. } if report.is_complete()));
        assert!(db
            .commitments_due(later)
            .iter()
            .any(|commitment| commitment.description.contains("send the late report")));
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(later));
    }

    /// #1066 B: an hourly cycle consumes only the hours that have closed, in order from the
    /// cursor; the next one starts exactly where it ended, so nothing is sent twice. It never
    /// prepares the Morning Brief and its nightly-only jobs are recorded done without effect.
    #[tokio::test]
    async fn hourly_cycles_catch_up_missed_hours_once_and_in_order() {
        use crate::dreamcycle::hourly::{hour_cycle_id, window_end_ms};
        const HOUR: i64 = 3_600_000;
        let now = 100 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let off = crate::daemon::os_gmt_offset_secs(now);
        let end1 = window_end_ms(now, off);
        let db = db_at(now);
        let missed = db
            .capture(&super_ev(end1 - 5 * HOUR + 600_000, "I'll send the deck.", "h-missed"))
            .unwrap()
            .0;
        let closed = db
            .capture(&super_ev(end1 - 20 * 60_000, "I'll book the venue.", "h-closed"))
            .unwrap()
            .0;
        let open_hour = db
            .capture(&super_ev(end1 + 5 * 60_000, "I'll email Dana the invoice.", "h-open"))
            .unwrap()
            .0;

        let id1 = hour_cycle_id(now.div_euclid(1000), off);
        let lane = CountingBatchLane::default();
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            &id1,
            now,
            1,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        // The missed hours and the closed hour went out together (as their sessions, L1, in one
        // window call); the hour still open did not.
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(lane.reviewed_sessions(), sids(&db, &[missed, closed]).len());
        let sent = lane.reviewed_text();
        assert!(sent.contains("send the deck") && sent.contains("book the venue"));
        assert!(!sent.contains("invoice"));
        let cycle = db.try_dream_cycle(&id1).unwrap().unwrap();
        assert_eq!(cycle.input_to_ts, end1);
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(end1));
        // The brief is the nightly cycle's: no brief work was queued by an hourly one.
        let date = crate::daemon::local_date_string(cycle.created_at);
        assert!(db.brief_for(&date).is_none());
        assert!(db.brief_generation(&date).unwrap().is_none());
        // The night views do not see the hourly cycle; the hourly view does.
        assert!(db.recent_cycles(5).is_empty());
        let hourly = db.recent_hourly_cycles(5);
        assert_eq!(hourly.len(), 1);
        assert!(hourly[0].succeeded);

        // Next hour: the window starts where the last one ended and carries only the new event.
        let later = now + HOUR;
        let id2 = hour_cycle_id(later.div_euclid(1000), off);
        assert_ne!(id1, id2);
        let lane2 = CountingBatchLane::default();
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane2),
            &idle_conditions(),
            &id2,
            later,
            1,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(lane2.submits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(lane2.reviewed_sessions(), sids(&db, &[open_hour]).len());
        assert!(lane2.reviewed_text().contains("invoice"));
        assert_eq!(db.try_dream_cycle(&id2).unwrap().unwrap().input_from_ts, end1);
        assert_eq!(db.try_last_consolidated_to().unwrap(), Some(window_end_ms(later, off)));
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 1, "the first window is never resent");
    }

    /// An outage — the relay unreachable, an old relay's 400, the model provider's 502 — leaves
    /// the hour pending however often it fails. A window whose answer (a 200) does not parse is
    /// abandoned after MAX_FAILURE_ATTEMPTS and its events are NOT carried into the next window,
    /// so one bad window is never re-sent. Neither polls.
    #[tokio::test]
    async fn a_failed_hourly_review_retries_an_outage_but_abandons_a_poisoned_window() {
        use crate::dreamcycle::hourly::hour_cycle_id;
        use crate::llm::anthropic::BatchResult;
        use crate::llm::LlmError;
        use shogun_memory::dream_cycles::{CycleStatus, MAX_FAILURE_ATTEMPTS};
        type Answer = fn() -> Result<Vec<BatchResult>, LlmError>;
        struct Answers(Answer);
        impl crate::llm::anthropic::BatchLane for Answers {
            fn review(
                &self,
                _items: &[crate::llm::anthropic::BatchItem],
            ) -> impl std::future::Future<Output = Result<Vec<BatchResult>, LlmError>> + Send {
                std::future::ready((self.0)())
            }
            async fn submit(&self, _: &[crate::llm::anthropic::BatchItem], _: Option<&str>) -> Result<crate::llm::anthropic::BatchHandle, LlmError> {
                panic!("hourly review must not submit a batch")
            }
            async fn poll(&self, _: &str) -> Result<crate::llm::anthropic::BatchHandle, LlmError> { panic!("no polling") }
            async fn results(&self, _: &str) -> Result<Vec<BatchResult>, LlmError> { panic!("no results") }
        }
        const HOUR: i64 = 3_600_000;
        let now = 100 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let off = crate::daemon::os_gmt_offset_secs(now);
        let end1 = crate::dreamcycle::hourly::window_end_ms(now, off);
        let id = hour_cycle_id(now.div_euclid(1000), off);

        let db = db_at(now);
        db.capture(&super_ev(end1 - 20 * 60_000, "I'll book the venue.", "h-closed")).unwrap();
        let outages: [Answer; 3] = [
            || Err(LlmError::Transport(crate::llm::transport::TransportError::Unreachable("relay".into()))),
            || Err(LlmError::Provider(format!("{}: relay review HTTP 400", crate::llm::relay::REVIEW_FAILED))),
            || Err(LlmError::Provider(format!("{}: relay review HTTP 502", crate::llm::relay::REVIEW_FAILED))),
        ];
        for _ in 0..MAX_FAILURE_ATTEMPTS {
            for outage in outages {
                assert!(run_batch_cycle(&db, DreamBatchClients::classify_only(&Answers(outage)), &idle_conditions(), &id, now, 1, || async {}).await.is_err());
                let cycle = db.try_dream_cycle(&id).unwrap().unwrap();
                assert_eq!(cycle.status, CycleStatus::Failed, "an outage never abandons");
                assert!(!db.try_dream_cycle_inputs_partitioned(&id).unwrap().partitioned.cloud.is_empty(), "events kept");
            }
        }

        let db = db_at(now);
        let venue = db.capture(&super_ev(end1 - 20 * 60_000, "I'll book the venue.", "h-closed")).unwrap().0;
        let unparseable = Answers(|| Ok(vec![BatchResult { custom_id: "window-1".into(), text: Some("not json".into()), error: None }]));
        for attempt in 1..=MAX_FAILURE_ATTEMPTS {
            assert!(run_batch_cycle(&db, DreamBatchClients::classify_only(&unparseable), &idle_conditions(), &id, now, 1, || async {}).await.is_err());
            let want = if attempt < MAX_FAILURE_ATTEMPTS { CycleStatus::Failed } else { CycleStatus::Abandoned };
            assert_eq!(db.try_dream_cycle(&id).unwrap().unwrap().status, want, "attempt {attempt}");
        }
        assert!(db.try_oldest_unfinished_full_cycle().unwrap().is_none(), "nothing left to retry");

        // The next window does not carry the abandoned one's events: only the new hour goes out.
        let later = now + HOUR;
        db.capture(&super_ev(end1 + 20 * 60_000, "I'll email Dana the invoice.", "h-next")).unwrap();
        let lane = CountingBatchLane::default();
        let id2 = hour_cycle_id(later.div_euclid(1000), off);
        let out = run_batch_cycle(&db, DreamBatchClients::classify_only(&lane), &idle_conditions(), &id2, later, 1, || async {}).await.unwrap();
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        let sent = lane.reviewed_text();
        assert!(sent.contains("invoice") && !sent.contains("book the venue"), "{venue}");
        assert_eq!((lane.polls.load(AtomicOrdering::SeqCst), lane.results.load(AtomicOrdering::SeqCst)), (0, 0));
    }

    /// Owner target 2026-10-14 (Goldfish-like volume): an hour of distinct sessions is ONE review
    /// call; a window held after a long sleep is cut to MAX_HOURLY_ITEMS (the remainder is the
    /// next window) and sent in at most WINDOW_MAX_CALLS calls.
    #[tokio::test]
    async fn an_hourly_window_is_one_call_and_an_oversized_one_at_most_three() {
        use crate::dreamcycle::hourly::hour_cycle_id;
        use crate::dreamcycle::jobs::{WINDOW_MAX_CALLS, WINDOW_SESSIONS_PER_CALL};
        const HOUR: i64 = 3_600_000;
        let now = 100 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let off = crate::daemon::os_gmt_offset_secs(now);
        let end1 = crate::dreamcycle::hourly::window_end_ms(now, off);
        for (sessions, max_calls) in [(20, 1), (MAX_HOURLY_ITEMS + 20, WINDOW_MAX_CALLS)] {
            let db = db_at(now);
            let titles: Vec<String> = (0..sessions).map(|i| format!("Doc {i}")).collect();
            let hashes: Vec<String> = (0..sessions).map(|i| format!("h-{i}")).collect();
            let texts: Vec<String> = (0..sessions).map(|i| format!("I'll send deck number {i} to Dana.")).collect();
            for i in 0..sessions {
                let mut ev = super_ev(end1 - 3 * HOUR + i as i64 * 60_000, &texts[i], &hashes[i]);
                ev.window_title = Some(&titles[i]);
                db.capture(&ev).unwrap();
            }
            let id = hour_cycle_id(now.div_euclid(1000), off);
            let lane = CountingBatchLane::default();
            let out = run_batch_cycle(&db, DreamBatchClients::classify_only(&lane), &idle_conditions(), &id, now, 1, || async {}).await.unwrap();
            assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
            let calls = lane.submits.load(AtomicOrdering::SeqCst);
            eprintln!("measured: {sessions} sessions -> {calls} review call(s), {} sessions sent", lane.reviewed_sessions());
            assert_eq!(calls, max_calls, "{sessions} sessions");
            assert!(lane.reviews.lock().unwrap().iter().all(|(n, _)| *n <= WINDOW_SESSIONS_PER_CALL));
            let cycle = db.try_dream_cycle(&id).unwrap().unwrap();
            if sessions > MAX_HOURLY_ITEMS {
                assert_eq!(lane.reviewed_sessions(), MAX_HOURLY_ITEMS);
                assert!(cycle.input_to_ts < end1, "the remainder is left for the next window");
            } else {
                assert_eq!(lane.reviewed_sessions(), sessions);
            }
        }
    }

    #[tokio::test]
    async fn an_hourly_window_with_nothing_in_it_sends_no_batch() {
        use crate::dreamcycle::hourly::{hour_cycle_id, hourly_range};
        const HOUR: i64 = 3_600_000;
        let now = 100 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let off = crate::daemon::os_gmt_offset_secs(now);
        let db = db_at(now);
        let (from, to) = hourly_range(db.try_last_consolidated_to().unwrap(), now, off);
        assert!(!db.has_batch_eligible_events(from, to), "the gate sees an empty window");

        let lane = CountingBatchLane::default();
        let id = hour_cycle_id(now.div_euclid(1000), off);
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&lane),
            &idle_conditions(),
            &id,
            now,
            1,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
        assert_eq!(lane.submits.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_full_run_never_sends_meeting_text_to_the_batch_lane() {
        // The A-2 regression (docs/meeting-text-on-the-search-spine.md): a window holding both a
        // screen capture and an indexed meeting transcript classifies BOTH — but only the capture
        // crosses the wire. This drives the real submit path and reads back what the transport
        // was actually given, so a future rewiring of the window read cannot quietly widen the
        // egress.
        use crate::llm::anthropic::{AnthropicBatchClient, AnthropicConfig};
        use crate::llm::traceability::RecordingSink;
        use crate::llm::transport::{HttpResponse, MockTransport};
        use crate::llm::{Secret, SelectKkKey};

        let now = 100 * 24 * 60 * 60 * 1000;
        let db = db_at(now);
        let (capture_id, _) = db
            .capture(&super_ev(now - 1000, "the deck discussion", "h1"))
            .unwrap();
        // A meeting on the spine, exactly as index_session writes it. "I will send the budget"
        // trips the same local commitment rules inline capture uses.
        db.capture(&shogun_memory::event_log::NewEvent {
            source: "meeting",
            kind: "transcript",
            ..super_ev(now - 900, "Me: I will send the budget tomorrow", "h2")
        })
        .unwrap();

        let transport = std::sync::Arc::new(MockTransport::new([
            HttpResponse {
                retry_after: None,
                status: 200,
                body: r#"{"id":"b","processing_status":"ended"}"#.into(),
            },
            HttpResponse {
                retry_after: None,
                status: 200,
                body: succeeded_batch_result(
                    &sid(&db, capture_id),
                    r#"{"commitments":[{"direction":"mine","description":"send the deck","anchor":"the deck discussion","anchor_len":19}],"open_loops":[]}"#,
                ),
            },
        ]));
        let client = AnthropicBatchClient::new(
            SharedTransport(transport.clone()),
            RecordingSink::new(),
            SelectKkKey::new(Secret::new("kk-123456")),
            AnthropicConfig::new("claude-x"),
        );
        let idle = RunConditions {
            within_window: true,
            window_elapsed: false,
            idle_ms: IDLE_THRESHOLD_MS,
            screen_locked: false,
            power_connected: true,
            battery_pct: 100,
            full_run_done_today: false,
        };
        let out = run_batch_cycle(
            &db,
            DreamBatchClients::classify_only(&client),
            &idle,
            "c1",
            now,
            3,
            || async {},
        )
        .await
        .unwrap();
        assert!(matches!(
            out,
            GatedRun::Ran {
                cycle: CycleKind::Full,
                ..
            }
        ));

        // The wire carried the capture and NOT the meeting — checked on the raw request bodies.
        let sent = transport.sent();
        assert!(!sent.is_empty(), "the batch submit must have gone out");
        for req in &sent {
            let body = req.body.as_deref().unwrap_or("");
            assert!(
                !body.contains("send the budget"),
                "meeting text reached the batch lane: {}",
                req.url
            );
        }
        assert!(
            sent.iter()
                .any(|r| r.body.as_deref().unwrap_or("").contains("deck discussion")),
            "the capture half must still be classified in the cloud"
        );

        // …and the meeting commitment still landed in state, through the LOCAL rules.
        let commitments = db.commitments_due(now + 7 * 24 * 60 * 60 * 1000);
        let descriptions: Vec<&str> = commitments.iter().map(|c| c.description.as_str()).collect();
        assert!(
            descriptions.iter().any(|d| d.contains("send the budget")),
            "the meeting's commitment must still be extracted locally: {descriptions:?}"
        );
    }

    // the morning reason (#285)

    mod morning_reason {
        use super::*;
        use crate::dreamcycle::outcome::{self, DreamReason, NightStatus, RecordKind};
        use crate::llm::anthropic::{BatchHandle, BatchItem, BatchResult, BatchStatus, BatchLane};
        use crate::llm::LlmError;

        const HOUR: i64 = 3_600_000;
        /// 2026-09-25 00:00 UTC; the default window is 02:00–06:00.
        const SEP25: i64 = 1_790_294_400_000;
        const NIGHT: &str = "20260925";

        fn outcome_path(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "shogun-dream-reason-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            dir.join(outcome::FILE_NAME)
        }

        /// A night with two cloud events, recording outcomes at `path`.
        fn night_db(now: i64, path: &std::path::Path) -> Db {
            let (db, _) = db_with_cloud_events(now, 2);
            db.with_dream_outcome_path(Some(path.to_path_buf()))
        }

        fn recorded(path: &std::path::Path) -> (RecordKind, Option<DreamReason>, String) {
            let record = outcome::load_at(path).expect("an outcome was recorded");
            (record.kind, record.reason(), record.cycle_id)
        }

        /// A lane scripted to fail at one step.
        struct ScriptedLane {
            submit_error: Option<fn() -> LlmError>,
            poll_status: BatchStatus,
            item_error: Option<&'static str>,
            ids: Mutex<Vec<String>>,
        }

        impl ScriptedLane {
            fn ok() -> Self {
                Self {
                    submit_error: None,
                    poll_status: BatchStatus::Ended,
                    item_error: None,
                    ids: Mutex::new(Vec::new()),
                }
            }
        }

        impl BatchLane for ScriptedLane {
            async fn submit(
                &self,
                items: &[BatchItem],
                _client_request_id: Option<&str>,
            ) -> Result<BatchHandle, LlmError> {
                if let Some(error) = self.submit_error {
                    return Err(error());
                }
                *self.ids.lock().unwrap() = items.iter().map(|i| i.custom_id.clone()).collect();
                Ok(BatchHandle {
                    id: "scripted".into(),
                    status: self.poll_status.clone(),
                })
            }
            async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
                Ok(BatchHandle {
                    id: batch_id.to_string(),
                    status: self.poll_status.clone(),
                })
            }
            async fn results(&self, _batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
                Ok(self
                    .ids
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|custom_id| BatchResult {
                        custom_id: custom_id.clone(),
                        text: self
                            .item_error
                            .is_none()
                            .then(|| r#"{"commitments":[],"open_loops":[]}"#.to_string()),
                        error: self.item_error.map(str::to_string),
                    })
                    .collect())
            }
        }

        async fn run(db: &Db, lane: &ScriptedLane, now: i64) -> Result<GatedRun, LlmError> {
            run_batch_cycle(
                db,
                DreamBatchClients::classify_only(lane),
                &idle_conditions(),
                NIGHT,
                now,
                1,
                || async {},
            )
            .await
        }

        fn morning(db: &Db, at: i64) -> crate::dreamcycle::outcome::NightReport {
            db.dream_status_at(NIGHT, 7, at, 0).night
        }

        #[tokio::test]
        async fn an_unreachable_service_is_the_mornings_reason() {
            let path = outcome_path("unreachable");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            let lane = ScriptedLane {
                submit_error: Some(|| {
                    LlmError::Transport(crate::llm::transport::TransportError::Io(
                        "offline".into(),
                    ))
                }),
                ..ScriptedLane::ok()
            };
            assert!(run(&db, &lane, now).await.is_err());
            assert_eq!(
                recorded(&path),
                (RecordKind::Failed, Some(DreamReason::RelayUnreachable), NIGHT.into())
            );
            let report = morning(&db, SEP25 + 9 * HOUR);
            assert_eq!(report.night, NIGHT);
            assert_eq!(report.status, NightStatus::Failed);
            assert_eq!(report.reason, Some(DreamReason::RelayUnreachable));
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[tokio::test]
        async fn expired_items_and_an_unfinished_job_are_told_apart() {
            let path = outcome_path("expired");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            let lane = ScriptedLane {
                item_error: Some("expired"),
                ..ScriptedLane::ok()
            };
            assert!(run(&db, &lane, now).await.is_err());
            assert_eq!(recorded(&path).1, Some(DreamReason::BatchExpired));

            let path = outcome_path("pending");
            let db = night_db(now, &path);
            let lane = ScriptedLane {
                poll_status: BatchStatus::InProgress,
                ..ScriptedLane::ok()
            };
            assert!(run(&db, &lane, now).await.is_err());
            assert_eq!(recorded(&path).1, Some(DreamReason::BatchPending));
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[tokio::test]
        async fn an_inactive_plan_is_recorded_and_the_local_fallback_then_completes_the_night() {
            let path = outcome_path("plan");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            let lane = ScriptedLane {
                submit_error: Some(|| LlmError::Unauthorized(401, String::new())),
                ..ScriptedLane::ok()
            };
            assert!(run(&db, &lane, now).await.is_err());
            assert_eq!(recorded(&path).1, Some(DreamReason::PlanInactive));

            // The desktop then runs the local lane (the job was never accepted): the night
            // completes, and the morning shows a fresh result with no failure.
            let local = DreamScheduler::new(&db, &LocalRuleClassifier);
            let out = local.tick(&idle_conditions(), NIGHT, now);
            assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
            assert_eq!(recorded(&path).0, RecordKind::Completed);
            let report = morning(&db, SEP25 + 9 * HOUR);
            assert_eq!((report.status, report.reason), (NightStatus::Completed, None));
            assert_eq!(report.result_night.as_deref(), Some(NIGHT));
            assert!(!report.stale);
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[test]
        fn a_submitted_job_without_its_credential_is_plan_inactive() {
            let path = outcome_path("credential");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            db.try_freeze_full_cycle(NIGHT, 0, now).unwrap();
            fixture_submitted(&db, NIGHT, "accepted-batch", &["1".to_string()]);
            let out = DreamScheduler::new(&db, &LocalRuleClassifier).tick(&idle_conditions(), NIGHT, now);
            assert!(matches!(out, GatedRun::Ran { report, .. } if !report.is_complete()));
            assert_eq!(recorded(&path).1, Some(DreamReason::PlanInactive));
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[tokio::test]
        async fn a_completed_night_is_fresh_and_the_next_missed_one_is_stale() {
            let path = outcome_path("fresh");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            let out = run(&db, &ScriptedLane::ok(), now).await.unwrap();
            assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
            assert_eq!(recorded(&path), (RecordKind::Completed, None, NIGHT.into()));

            let today = morning(&db, SEP25 + 9 * HOUR);
            assert_eq!((today.status, today.reason), (NightStatus::Completed, None));
            assert!(!today.stale);

            // The next morning, with nothing run overnight (lid shut): the 25th's result is
            // shown as the 25th's, never as the 26th's.
            let tomorrow = morning(&db, SEP25 + 33 * HOUR);
            assert_eq!(tomorrow.night, "20260926");
            assert_eq!(tomorrow.status, NightStatus::NotRun);
            assert_eq!(tomorrow.reason, Some(DreamReason::Asleep));
            assert_eq!(tomorrow.result_night.as_deref(), Some(NIGHT));
            assert_eq!(tomorrow.nights_behind, 1);
            assert!(tomorrow.stale);
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[tokio::test]
        async fn a_night_with_nothing_captured_completes_as_no_new_data() {
            let path = outcome_path("empty");
            let now = SEP25 + 3 * HOUR;
            let db = db_at(now).with_dream_outcome_path(Some(path.clone()));
            let out = run(&db, &ScriptedLane::ok(), now).await.unwrap();
            assert!(matches!(out, GatedRun::Ran { report, .. } if report.is_complete()));
            let report = morning(&db, SEP25 + 9 * HOUR);
            assert_eq!(
                (report.status, report.reason),
                (NightStatus::Completed, Some(DreamReason::NoNewData))
            );
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[tokio::test]
        async fn a_store_failure_before_the_claim_is_a_storage_error() {
            let path = outcome_path("storage");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            db.test_set_dream_batch_items_available(false).unwrap();
            assert!(run(&db, &ScriptedLane::ok(), now).await.is_err());
            assert_eq!(recorded(&path).1, Some(DreamReason::StorageError));
            db.test_set_dream_batch_items_available(true).unwrap();
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[test]
        fn a_run_that_stopped_renewing_is_interrupted_not_running() {
            let path = outcome_path("stalled");
            let now = SEP25 + 3 * HOUR;
            let db = night_db(now, &path);
            db.try_freeze_full_cycle(NIGHT, 0, now).unwrap();
            // A runner claims the night and dies without releasing it (crash, quit, sleep).
            let _abandoned = db
                .try_claim_dream_cycle_run(NIGHT, "crashed-owner".into())
                .unwrap()
                .unwrap();
            let soon = morning(&db, now + 5 * 60 * 1000);
            assert_eq!(soon.status, NightStatus::Running);
            let later = morning(&db, now + outcome::STALL_MS + 60_000);
            assert_eq!(
                (later.status, later.reason),
                (NightStatus::Failed, Some(DreamReason::Interrupted))
            );
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[test]
        fn a_busy_window_is_recorded_but_never_hides_a_failure() {
            let path = outcome_path("busy");
            let now = SEP25 + 3 * HOUR;
            let db = db_at(now).with_dream_outcome_path(Some(path.clone()));
            db.note_dream_skip(NIGHT, RunDecision::Skip(SkipReason::OutsideWindow));
            assert!(outcome::load_at(&path).is_none(), "not every skip is a reason");
            db.note_dream_skip(NIGHT, RunDecision::Skip(SkipReason::NotIdle));
            assert_eq!(
                recorded(&path),
                (RecordKind::Skipped, Some(DreamReason::NotIdle), NIGHT.into())
            );
            let report = morning(&db, SEP25 + 9 * HOUR);
            assert_eq!(
                (report.status, report.reason),
                (NightStatus::NotRun, Some(DreamReason::NotIdle))
            );

            db.note_dream_failure(NIGHT, DreamReason::StorageError);
            db.note_dream_skip(NIGHT, RunDecision::Skip(SkipReason::InsufficientPower));
            assert_eq!(recorded(&path).1, Some(DreamReason::StorageError));
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }

        #[test]
        fn without_a_path_nothing_is_written_and_the_ledger_still_answers() {
            let now = SEP25 + 9 * HOUR;
            let db = db_at(now);
            db.note_dream_failure(NIGHT, DreamReason::StorageError);
            assert_eq!(db.dream_outcome_record(), None);
            let report = morning(&db, now);
            // A fresh install that never captured before the window: nothing was missed.
            assert_eq!((report.status, report.reason), (NightStatus::Never, None));
        }

        #[test]
        fn the_record_is_content_free() {
            let path = outcome_path("content");
            let db = night_db(SEP25 + 3 * HOUR, &path);
            db.note_dream_failure(NIGHT, DreamReason::RelayUnreachable);
            let raw = std::fs::read_to_string(&path).unwrap();
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(keys, ["at_ms", "cycle_id", "kind", "reason"]);
            assert!(!raw.contains("deck discussion"));
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }
}
