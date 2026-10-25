//! Why last night's Dream Cycle did — or did not — produce a fresh result (issue #285). Pure.
//!
//! The nightly cycle runs while nobody is watching, so "the Brief is empty" and "the Brief is from
//! Friday" arrive with no explanation unless something keeps one. This module owns that
//! explanation:
//!
//! - [`DreamReason`]: the closed vocabulary of causes — asleep through the window, busy or on low
//!   battery through it, interrupted mid-run, the job refused / unreachable / expired / still
//!   running at the provider, the daily cap, an inactive plan, a storage failure, or simply nothing
//!   new to review. A code, never a message: it goes to the UI, the API and a file on disk, and
//!   none of those may carry captured text.
//! - The classifiers, which map what a failed run actually raised ([`LlmError`] plus where the
//!   cycle stood, or a local error string) onto that vocabulary.
//! - [`OutcomeRecord`]: the one reason the ledger cannot reconstruct by itself, persisted as
//!   `dream_outcome.json` next to the other shared app-data files. The job ledger still decides
//!   whether a night completed; the record only says *why* it did not.
//! - [`night_report`]: the staleness rule. The night the user expects a result for is the latest
//!   nightly window that has begun; a result describing any earlier night is marked stale and
//!   carries its own date. It is never presented as today's, and nothing is discarded or re-run
//!   automatically — whether an old result should be dropped is an open owner decision.
//!
//! Everything here is plain data and arithmetic so it is tested on Linux; the daemon supplies the
//! ledger facts, the clock and the zone offset.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::llm::LlmError;

use super::gate::{RunDecision, SkipReason};

// Errors the Dream Cycle raises itself. The raising site and the classifier share these constants,
// so a reworded message cannot silently fall through to `Unknown`.

/// Another runner holds (or took over) the cycle's lease — the run was cut off mid-way.
pub const LEASE_LOST: &str = "Dream cycle lease was lost";
/// A second runner found the cycle already leased. Not a failure of the night.
pub const ALREADY_RUNNING: &str = "Dream cycle is already running";
/// A provider-backed cycle cannot fall back to local rules without the lane's credential.
pub const PROVIDER_CREDENTIAL_REQUIRED: &str =
    "submitted Dream Batch requires its provider credential";
/// The poll budget ran out while the provider was still working on the batch.
pub const POLL_BUDGET_EXHAUSTED: &str = "batch did not end within the poll budget";
/// The provider ended the batch with items it expired or canceled rather than processed.
pub const BATCH_ITEMS_EXPIRED: &str = "Dream Batch items expired or were canceled at the provider";

/// How long a live-looking run may go without renewing its lease before it reads as interrupted.
/// A runner renews at every poll (at most 15 min apart, issue #29) and every job boundary; a
/// crashed one keeps a lease that is valid for hours, and the morning view must not call that
/// "running".
pub const STALL_MS: i64 = 30 * 60 * 1000;

/// The persisted record's file name, in the shared app-data directory.
pub const FILE_NAME: &str = "dream_outcome.json";

/// Why the night the user expects a result for has none (or has an empty one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DreamReason {
    /// The Mac was asleep, off, or SHOGUN was not running through the whole window. Powered off is
    /// a design boundary, not a gap: nothing runs on a Mac that is off.
    Asleep,
    /// The Mac was in use through the window (the cycle waits for 15 idle minutes).
    NotIdle,
    /// Off power with the battery under the floor through the window.
    LowBattery,
    /// The run stopped part-way — a crash, a quit, or sleep mid-run. It resumes from the ledger.
    Interrupted,
    /// The service refused the night's job (a 4xx other than plan/cap, or a policy stop).
    SubmissionRejected,
    /// No connection, or the service was down or busy (transport failure, 5xx).
    RelayUnreachable,
    /// The job expired or was canceled at the provider before it finished.
    BatchExpired,
    /// The job was still processing when the run stopped waiting; its results are collected later.
    BatchPending,
    /// The daily processing cap was reached (HTTP 429 on submission).
    DailyCap,
    /// The plan or licence does not cover the lane (401/403/402, or no credential at all).
    PlanInactive,
    /// A local write or read failed — a full disk, or a database error.
    StorageError,
    /// The run completed, but the night's window held no captured events.
    NoNewData,
    /// The run failed and the cause was not recorded.
    Unknown,
}

impl DreamReason {
    /// Every reason, for exhaustive tests and catalogues.
    pub const ALL: [DreamReason; 13] = [
        DreamReason::Asleep,
        DreamReason::NotIdle,
        DreamReason::LowBattery,
        DreamReason::Interrupted,
        DreamReason::SubmissionRejected,
        DreamReason::RelayUnreachable,
        DreamReason::BatchExpired,
        DreamReason::BatchPending,
        DreamReason::DailyCap,
        DreamReason::PlanInactive,
        DreamReason::StorageError,
        DreamReason::NoNewData,
        DreamReason::Unknown,
    ];

    /// The stable wire code (identical to the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            DreamReason::Asleep => "asleep",
            DreamReason::NotIdle => "not_idle",
            DreamReason::LowBattery => "low_battery",
            DreamReason::Interrupted => "interrupted",
            DreamReason::SubmissionRejected => "submission_rejected",
            DreamReason::RelayUnreachable => "relay_unreachable",
            DreamReason::BatchExpired => "batch_expired",
            DreamReason::BatchPending => "batch_pending",
            DreamReason::DailyCap => "daily_cap",
            DreamReason::PlanInactive => "plan_inactive",
            DreamReason::StorageError => "storage_error",
            DreamReason::NoNewData => "no_new_data",
            DreamReason::Unknown => "unknown",
        }
    }

    /// Parse a wire code. An unrecognised code (a newer build wrote it) is `None`.
    pub fn parse(code: &str) -> Option<DreamReason> {
        DreamReason::ALL
            .into_iter()
            .find(|reason| reason.as_str() == code)
    }
}

/// Where the night the user expects a result for stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NightStatus {
    /// No full cycle has ever run, and SHOGUN was not yet capturing when the window opened.
    Never,
    /// The window is open and the cycle has not run yet.
    Pending,
    /// A cycle is running right now.
    Running,
    /// The night's full cycle completed.
    Completed,
    /// The night's cycle ran and did not finish; it resumes or carries into the next night.
    Failed,
    /// The window closed without the cycle running at all.
    NotRun,
}

/// The morning view of the last night: its status, why, and how old the result on screen is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NightReport {
    /// The night a result is expected for (`YYYYMMDD`): the latest nightly window that has begun.
    pub night: String,
    pub status: NightStatus,
    /// Why there is no fresh result (or why it is empty). `None` for a clean completion, a run in
    /// progress, or a window still waiting.
    pub reason: Option<DreamReason>,
    /// The night the newest completed result describes (`YYYYMMDD`), if any.
    pub result_night: Option<String>,
    /// Whole nights between `result_night` and `night`; 0 when the result is last night's.
    pub nights_behind: u32,
    /// The result is older than last night. Show it with its date; never present it as today's.
    pub stale: bool,
}

/// Where a Batch-backed cycle stood when it failed, from its durable `batch_stage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePhase {
    /// Before the provider accepted the job (pending / submitting / recovering the submission).
    Submit,
    /// The job was accepted; the run was polling for or fetching its results.
    Collect,
    /// Results were in hand (or the cycle ran on local rules); a local job failed.
    Local,
}

/// Classify a failed Batch-backed run. `storage_fault` is whether the memory store recorded a
/// fault during the attempt (`MemoryHealth::faults_total` moved), which is how a local read or
/// write failure that surfaced as a plain provider string is told apart from a network one.
/// `None` means the attempt was not a failure of the night (another runner already had it).
pub fn classify_batch_failure(
    error: &LlmError,
    phase: FailurePhase,
    storage_fault: bool,
) -> Option<DreamReason> {
    Some(match error {
        LlmError::Transport(_) => DreamReason::RelayUnreachable,
        // `NeedsUpdate` is a subscription delegate's and never reaches the Batch lane
        // (invariant 5); grouped with the other "this credential cannot run" answers.
        LlmError::Unauthorized(..)
        | LlmError::QuotaExhausted(_)
        | LlmError::NotConfigured
        | LlmError::NeedsUpdate(_) => DreamReason::PlanInactive,
        // 429 on submission is the relay's daily cap. On a poll it is only a throttle: the job is
        // fine, the service asked for a slower cadence.
        LlmError::RateLimited(..) => match phase {
            FailurePhase::Submit => DreamReason::DailyCap,
            FailurePhase::Collect | FailurePhase::Local => DreamReason::RelayUnreachable,
        },
        LlmError::PolicyStopped => DreamReason::SubmissionRejected,
        // The lane refused to send because the local ledger would not take the row: a storage
        // fault on this Mac, not the service.
        LlmError::TraceUnrecorded => DreamReason::StorageError,
        LlmError::InvalidRequest if phase == FailurePhase::Submit => DreamReason::SubmissionRejected,
        LlmError::InvalidRequest => DreamReason::Unknown,
        LlmError::Parse(_) if storage_fault => DreamReason::StorageError,
        LlmError::Parse(_) => DreamReason::Unknown,
        LlmError::Provider(message) => {
            return classify_provider_message(message, phase, storage_fault)
        }
    })
}

fn classify_provider_message(
    message: &str,
    phase: FailurePhase,
    storage_fault: bool,
) -> Option<DreamReason> {
    Some(match message {
        ALREADY_RUNNING => return None,
        LEASE_LOST => DreamReason::Interrupted,
        PROVIDER_CREDENTIAL_REQUIRED => DreamReason::PlanInactive,
        POLL_BUDGET_EXHAUSTED => DreamReason::BatchPending,
        BATCH_ITEMS_EXPIRED => DreamReason::BatchExpired,
        _ => match http_status(message) {
            Some(status) if status >= 500 => DreamReason::RelayUnreachable,
            // The service no longer knows the job it accepted.
            Some(404 | 410) if phase == FailurePhase::Collect => DreamReason::BatchExpired,
            Some(400..=499) if phase == FailurePhase::Submit => DreamReason::SubmissionRejected,
            Some(_) => DreamReason::Unknown,
            None if storage_fault => DreamReason::StorageError,
            None => DreamReason::Unknown,
        },
    })
}

/// Classify a failure raised by local work — the local-rules lane, or the local jobs that follow a
/// Batch's results. No network is involved, so what is not a lease or credential problem is a
/// storage problem when the store recorded a fault, and unknown otherwise.
pub fn classify_local_failure(error: &str, storage_fault: bool) -> Option<DreamReason> {
    Some(match error {
        ALREADY_RUNNING => return None,
        LEASE_LOST => DreamReason::Interrupted,
        PROVIDER_CREDENTIAL_REQUIRED => DreamReason::PlanInactive,
        _ if storage_fault => DreamReason::StorageError,
        _ => DreamReason::Unknown,
    })
}

/// Why the gate declined inside the window, when that is worth telling the user the next morning.
/// Only "busy" and "low battery" are: the rest are not reasons a night went without a result.
pub fn skip_reason(decision: RunDecision) -> Option<DreamReason> {
    match decision {
        RunDecision::Skip(SkipReason::NotIdle) => Some(DreamReason::NotIdle),
        RunDecision::Skip(SkipReason::InsufficientPower) => Some(DreamReason::LowBattery),
        _ => None,
    }
}

/// The HTTP status in a provider message (`"relay create HTTP 503"` → 503), as
/// [`crate::llm::status_error`] formats it.
fn http_status(message: &str) -> Option<u16> {
    let (_, rest) = message.split_once("HTTP ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let status: u16 = digits.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

/// What the record says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Completed,
    Failed,
    /// The gate declined inside the window (busy / low battery).
    Skipped,
}

/// The last Dream outcome with its reason — a date, a code and a time; never captured text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// The cycle the outcome concerns (`YYYYMMDD`, possibly an older night being resumed).
    pub cycle_id: String,
    pub kind: RecordKind,
    /// A [`DreamReason`] code. Kept as text so a code from a newer build reads as `Unknown`
    /// instead of making the whole record unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When it was recorded (unix ms). This, not `cycle_id`, ties the record to a night's window.
    pub at_ms: i64,
}

impl OutcomeRecord {
    pub fn new(cycle_id: &str, kind: RecordKind, reason: Option<DreamReason>, at_ms: i64) -> Self {
        Self {
            cycle_id: cycle_id.to_string(),
            kind,
            reason: reason.map(|r| r.as_str().to_string()),
            at_ms,
        }
    }

    /// The recorded reason; an unrecognised code is [`DreamReason::Unknown`].
    pub fn reason(&self) -> Option<DreamReason> {
        self.reason
            .as_deref()
            .map(|code| DreamReason::parse(code).unwrap_or(DreamReason::Unknown))
    }
}

/// Read the record. Missing or unreadable is `None`: the ledger alone then decides the report.
pub fn load_at(path: &Path) -> Option<OutcomeRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write the record through a temporary file and a rename, so a reader never sees half of it.
pub fn save_at(path: &Path, record: &OutcomeRecord) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let temp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)
}

/// Whether `next` should replace `current`. Completions and failures always do. A skip (busy /
/// low battery) never overwrites a completion or failure from the same window, and is not
/// rewritten every tick when nothing changed.
pub fn should_replace(
    current: Option<&OutcomeRecord>,
    next: &OutcomeRecord,
    window_start_ms: i64,
) -> bool {
    let Some(current) = current else { return true };
    if next.kind != RecordKind::Skipped {
        return true;
    }
    if current.at_ms < window_start_ms {
        return true;
    }
    current.kind == RecordKind::Skipped && current.reason != next.reason
}

/// One night's window in UTC instants, and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NightWindow {
    /// `YYYYMMDD` of the local date the window starts on — the ledger's cycle id for that night.
    pub night: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

const DAY_SECS: i64 = 86_400;

/// The latest night whose window (local `[start_hour, end_hour)`, wrapping midnight when
/// `start_hour > end_hour`) has begun at `now_ms`. Before today's window opens that is yesterday's
/// night, so at 01:00 the result expected is still the one from the previous early morning.
pub fn due_night(now_ms: i64, gmt_offset_secs: i32, start_hour: u32, end_hour: u32) -> NightWindow {
    let local = now_ms.div_euclid(1000) + i64::from(gmt_offset_secs);
    let day = local.div_euclid(DAY_SECS);
    let hour = local.rem_euclid(DAY_SECS) / 3_600;
    let night_day = if hour >= i64::from(start_hour) {
        day
    } else {
        day - 1
    };
    window_for_day(night_day, gmt_offset_secs, start_hour, end_hour)
}

/// The night after `window` (same hours, next local date). Used to look ahead at sleep time.
pub fn next_night(
    window: &NightWindow,
    gmt_offset_secs: i32,
    start_hour: u32,
    end_hour: u32,
) -> NightWindow {
    let local_start = window.start_ms.div_euclid(1000) + i64::from(gmt_offset_secs);
    window_for_day(
        local_start.div_euclid(DAY_SECS) + 1,
        gmt_offset_secs,
        start_hour,
        end_hour,
    )
}

fn window_for_day(
    night_day: i64,
    gmt_offset_secs: i32,
    start_hour: u32,
    end_hour: u32,
) -> NightWindow {
    let span_hours = (i64::from(end_hour) + 24 - i64::from(start_hour)).rem_euclid(24);
    let start_local = night_day * DAY_SECS + i64::from(start_hour) * 3_600;
    let start_ms = (start_local - i64::from(gmt_offset_secs)) * 1000;
    NightWindow {
        night: format!("{:08}", yyyymmdd_from_days(night_day)),
        start_ms,
        end_ms: start_ms + span_hours * 3_600 * 1000,
    }
}

/// `YYYYMMDD` for a count of days since the epoch (civil-from-days, Howard Hinnant) — no date
/// dependency, and correct across leap years and century rules.
pub(crate) fn yyyymmdd_from_days(days: i64) -> u32 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y.max(0) as u32) * 10_000 + (m as u32) * 100 + d as u32
}

/// Days since the epoch for the `YYYYMMDD` a cycle id starts with (the inverse of
/// [`yyyymmdd_from_days`]). `None` when the id does not start with a valid date.
pub fn days_from_night(id: &str) -> Option<i64> {
    let digits = id.get(..8)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = digits[..4].parse().ok()?;
    let m: i64 = digits[4..6].parse().ok()?;
    let d: i64 = digits[6..8].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The due night's Full cycle as the lifecycle table sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueCycle {
    /// Completed; `events` is how many captured events its input window held.
    Completed { events: i64 },
    /// Unfinished, and its runner renewed the lease within [`STALL_MS`].
    Running,
    /// Unfinished with nobody running it: a crash, a quit, or sleep mid-run. It resumes.
    Stalled,
    /// Marked failed (retried) or abandoned (its range carries into the next night).
    Failed,
}

/// Classify an unfinished cycle's liveness from its lease and last renewal.
pub fn liveness(
    owner_present: bool,
    lease_until: Option<i64>,
    updated_at: i64,
    now_ms: i64,
) -> DueCycle {
    let lease_live = owner_present && lease_until.is_some_and(|until| until > now_ms);
    if lease_live && now_ms.saturating_sub(updated_at) <= STALL_MS {
        DueCycle::Running
    } else {
        DueCycle::Stalled
    }
}

/// The facts [`night_report`] decides from. The daemon reads them from the ledger and the record.
#[derive(Debug, Clone, Copy)]
pub struct NightInputs<'a> {
    pub window: &'a NightWindow,
    pub now_ms: i64,
    /// The due night's own Full cycle, if one was frozen for it.
    pub due: Option<DueCycle>,
    /// Some Full cycle — possibly an older night being resumed — is running right now.
    pub live_run: bool,
    /// Any Full cycle exists in the ledger, or SHOGUN was capturing before the window opened.
    /// False means a fresh install that has never had a night to run: "hasn't run yet".
    pub had_a_chance: bool,
    /// The newest night with a completed Full cycle.
    pub latest_result: Option<&'a str>,
    /// The persisted record, whatever night it concerns ([`night_report`] filters by time).
    pub record: Option<&'a OutcomeRecord>,
}

/// The morning report for the due night (issue #285).
///
/// Order of evidence: the ledger decides completion and a live run; a stalled run is an
/// interruption whatever was recorded before it; then the record written during this night's
/// window gives the reason; then the ledger's bare "failed"; then absence — a window still open is
/// pending, a closed one with no trace at all means the Mac was not awake for it.
pub fn night_report(inputs: &NightInputs<'_>) -> NightReport {
    let window = inputs.window;
    let record = inputs
        .record
        .filter(|record| record.at_ms >= window.start_ms);
    let window_open = inputs.now_ms < window.end_ms;

    let (status, reason) = match inputs.due {
        Some(DueCycle::Completed { events }) => (
            NightStatus::Completed,
            (events == 0).then_some(DreamReason::NoNewData),
        ),
        _ if inputs.live_run || inputs.due == Some(DueCycle::Running) => {
            (NightStatus::Running, None)
        }
        Some(DueCycle::Stalled) => (NightStatus::Failed, Some(DreamReason::Interrupted)),
        due => match record.map(|record| (record.kind, record.reason())) {
            Some((RecordKind::Failed, reason)) => (
                NightStatus::Failed,
                Some(reason.unwrap_or(DreamReason::Unknown)),
            ),
            // An older night's cycle finished inside this window; the staleness fields say which.
            Some((RecordKind::Completed, _)) if due.is_none() => (NightStatus::Completed, None),
            Some((RecordKind::Skipped, reason)) if due.is_none() => {
                let reason = reason.unwrap_or(DreamReason::Asleep);
                if window_open {
                    (NightStatus::Pending, Some(reason))
                } else {
                    (NightStatus::NotRun, Some(reason))
                }
            }
            _ => match due {
                Some(_) => (NightStatus::Failed, Some(DreamReason::Unknown)),
                None if !inputs.had_a_chance => (NightStatus::Never, None),
                None if window_open => (NightStatus::Pending, None),
                None => (NightStatus::NotRun, Some(DreamReason::Asleep)),
            },
        },
    };

    let result_night = inputs
        .latest_result
        .map(|id| id.get(..8).unwrap_or(id).to_string());
    let nights_behind = match (
        result_night.as_deref().and_then(days_from_night),
        days_from_night(&window.night),
    ) {
        (Some(result), Some(due)) if due > result => {
            u32::try_from(due - result).unwrap_or(u32::MAX)
        }
        _ => 0,
    };
    NightReport {
        night: window.night.clone(),
        status,
        reason,
        result_night,
        nights_behind,
        stale: nights_behind >= 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::transport::TransportError;

    const HOUR: i64 = 3_600_000;
    /// 2026-09-25 00:00 UTC, in ms.
    const SEP25: i64 = 1_790_294_400_000;

    fn window_2_to_6(now: i64) -> NightWindow {
        due_night(now, 0, 2, 6)
    }

    #[test]
    fn transport_failure_is_relay_unreachable_in_every_phase() {
        for phase in [
            FailurePhase::Submit,
            FailurePhase::Collect,
            FailurePhase::Local,
        ] {
            let error = LlmError::Transport(TransportError::Io("connection refused".into()));
            assert_eq!(
                classify_batch_failure(&error, phase, false),
                Some(DreamReason::RelayUnreachable)
            );
        }
    }

    #[test]
    fn server_errors_are_unreachable_and_client_errors_on_submit_are_rejections() {
        let e = |m: &str| LlmError::Provider(m.to_string());
        assert_eq!(
            classify_batch_failure(&e("relay create HTTP 503"), FailurePhase::Submit, false),
            Some(DreamReason::RelayUnreachable)
        );
        assert_eq!(
            classify_batch_failure(&e("relay poll HTTP 502"), FailurePhase::Collect, false),
            Some(DreamReason::RelayUnreachable)
        );
        assert_eq!(
            classify_batch_failure(&e("relay create HTTP 413"), FailurePhase::Submit, false),
            Some(DreamReason::SubmissionRejected)
        );
        assert_eq!(
            classify_batch_failure(&LlmError::InvalidRequest, FailurePhase::Submit, false),
            Some(DreamReason::SubmissionRejected)
        );
        assert_eq!(
            classify_batch_failure(&LlmError::InvalidRequest, FailurePhase::Collect, false),
            Some(DreamReason::Unknown)
        );
        assert_eq!(
            classify_batch_failure(&LlmError::PolicyStopped, FailurePhase::Submit, false),
            Some(DreamReason::SubmissionRejected)
        );
        // A 4xx while collecting is not a rejected submission.
        assert_eq!(
            classify_batch_failure(&e("relay poll HTTP 409"), FailurePhase::Collect, false),
            Some(DreamReason::Unknown)
        );
    }

    #[test]
    fn a_job_the_provider_no_longer_has_or_expired_is_batch_expired() {
        for message in ["relay poll HTTP 404", "relay results HTTP 410"] {
            assert_eq!(
                classify_batch_failure(
                    &LlmError::Provider(message.into()),
                    FailurePhase::Collect,
                    false
                ),
                Some(DreamReason::BatchExpired)
            );
        }
        assert_eq!(
            classify_batch_failure(
                &LlmError::Provider(BATCH_ITEMS_EXPIRED.into()),
                FailurePhase::Collect,
                false
            ),
            Some(DreamReason::BatchExpired)
        );
    }

    #[test]
    fn an_unfinished_job_at_the_provider_is_pending_not_failed() {
        assert_eq!(
            classify_batch_failure(
                &LlmError::Provider(POLL_BUDGET_EXHAUSTED.into()),
                FailurePhase::Collect,
                false
            ),
            Some(DreamReason::BatchPending)
        );
    }

    #[test]
    fn plan_and_cap_failures_are_named() {
        for error in [
            LlmError::Unauthorized(401, String::new()),
            LlmError::Unauthorized(403, "forbidden".into()),
            LlmError::QuotaExhausted("plan does not include the lane".into()),
            LlmError::NotConfigured,
            LlmError::Provider(PROVIDER_CREDENTIAL_REQUIRED.into()),
        ] {
            assert_eq!(
                classify_batch_failure(&error, FailurePhase::Submit, false),
                Some(DreamReason::PlanInactive),
                "{error:?}"
            );
        }
        let cap = LlmError::RateLimited(429, String::new());
        assert_eq!(
            classify_batch_failure(&cap, FailurePhase::Submit, false),
            Some(DreamReason::DailyCap)
        );
        // A 429 while polling is the poll throttle, not the daily cap.
        assert_eq!(
            classify_batch_failure(&cap, FailurePhase::Collect, false),
            Some(DreamReason::RelayUnreachable)
        );
    }

    #[test]
    fn a_lost_lease_is_an_interruption_and_a_concurrent_runner_is_not_a_failure() {
        assert_eq!(
            classify_batch_failure(
                &LlmError::Provider(LEASE_LOST.into()),
                FailurePhase::Collect,
                true
            ),
            Some(DreamReason::Interrupted)
        );
        assert_eq!(
            classify_batch_failure(
                &LlmError::Provider(ALREADY_RUNNING.into()),
                FailurePhase::Submit,
                false
            ),
            None
        );
        assert_eq!(
            classify_local_failure(LEASE_LOST, false),
            Some(DreamReason::Interrupted)
        );
        assert_eq!(classify_local_failure(ALREADY_RUNNING, true), None);
    }

    #[test]
    fn a_store_fault_during_the_attempt_is_a_storage_error() {
        let db_error = LlmError::Provider("dream provider handle write failed".into());
        assert_eq!(
            classify_batch_failure(&db_error, FailurePhase::Submit, true),
            Some(DreamReason::StorageError)
        );
        assert_eq!(
            classify_batch_failure(&db_error, FailurePhase::Submit, false),
            Some(DreamReason::Unknown)
        );
        assert_eq!(
            classify_batch_failure(&LlmError::Parse("bad".into()), FailurePhase::Collect, true),
            Some(DreamReason::StorageError)
        );
        // A network answer outranks a coincident store fault elsewhere in the process.
        assert_eq!(
            classify_batch_failure(
                &LlmError::Provider("relay create HTTP 503".into()),
                FailurePhase::Submit,
                true
            ),
            Some(DreamReason::RelayUnreachable)
        );
        assert_eq!(
            classify_local_failure("dream job ledger write failed", true),
            Some(DreamReason::StorageError)
        );
        assert_eq!(
            classify_local_failure("dream job ledger write failed", false),
            Some(DreamReason::Unknown)
        );
        assert_eq!(
            classify_local_failure(PROVIDER_CREDENTIAL_REQUIRED, false),
            Some(DreamReason::PlanInactive)
        );
    }

    #[test]
    fn only_busy_and_low_battery_skips_are_reasons() {
        use crate::dreamcycle::gate::SkipReason;
        assert_eq!(
            skip_reason(RunDecision::Skip(SkipReason::NotIdle)),
            Some(DreamReason::NotIdle)
        );
        assert_eq!(
            skip_reason(RunDecision::Skip(SkipReason::InsufficientPower)),
            Some(DreamReason::LowBattery)
        );
        for decision in [
            RunDecision::Skip(SkipReason::AlreadyRanToday),
            RunDecision::Skip(SkipReason::OutsideWindow),
            RunDecision::Full,
            RunDecision::Degraded,
        ] {
            assert_eq!(skip_reason(decision), None);
        }
    }

    #[test]
    fn http_status_reads_only_a_real_status() {
        assert_eq!(http_status("relay create HTTP 503"), Some(503));
        assert_eq!(http_status("results HTTP 404: gone"), Some(404));
        assert_eq!(http_status("HTTP 99"), None);
        assert_eq!(http_status("no status here"), None);
    }

    #[test]
    fn reason_codes_round_trip_and_match_serde() {
        for reason in DreamReason::ALL {
            assert_eq!(DreamReason::parse(reason.as_str()), Some(reason));
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(reason.as_str().to_string())
            );
        }
        assert_eq!(DreamReason::parse("from_a_newer_build"), None);
    }

    #[test]
    fn record_round_trips_and_an_unknown_code_reads_as_unknown() {
        let dir = std::env::temp_dir().join(format!("shogun-dream-outcome-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        assert_eq!(load_at(&path), None);
        let record = OutcomeRecord::new(
            "20260925",
            RecordKind::Failed,
            Some(DreamReason::DailyCap),
            7,
        );
        save_at(&path, &record).unwrap();
        assert_eq!(load_at(&path), Some(record.clone()));
        assert_eq!(
            load_at(&path).unwrap().reason(),
            Some(DreamReason::DailyCap)
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw,
            r#"{"cycle_id":"20260925","kind":"failed","reason":"daily_cap","at_ms":7}"#
        );
        std::fs::write(
            &path,
            r#"{"cycle_id":"x","kind":"failed","reason":"new_code","at_ms":1}"#,
        )
        .unwrap();
        assert_eq!(load_at(&path).unwrap().reason(), Some(DreamReason::Unknown));
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_at(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skip_never_overwrites_a_failure_from_the_same_window() {
        let start = 1_000;
        let failed = OutcomeRecord::new(
            "n",
            RecordKind::Failed,
            Some(DreamReason::RelayUnreachable),
            2_000,
        );
        let busy = OutcomeRecord::new("n", RecordKind::Skipped, Some(DreamReason::NotIdle), 3_000);
        let busy_again =
            OutcomeRecord::new("n", RecordKind::Skipped, Some(DreamReason::NotIdle), 4_000);
        let low = OutcomeRecord::new(
            "n",
            RecordKind::Skipped,
            Some(DreamReason::LowBattery),
            5_000,
        );
        assert!(should_replace(None, &busy, start));
        assert!(!should_replace(Some(&failed), &busy, start));
        assert!(
            !should_replace(Some(&busy), &busy_again, start),
            "unchanged skip is not rewritten"
        );
        assert!(should_replace(Some(&busy), &low, start));
        // A failure or completion always wins; an older window's record is always replaced.
        assert!(should_replace(Some(&busy), &failed, start));
        let yesterday = OutcomeRecord::new("m", RecordKind::Failed, None, 500);
        assert!(should_replace(Some(&yesterday), &busy, start));
    }

    #[test]
    fn due_night_before_the_window_is_the_previous_early_morning() {
        // 01:00 on the 25th: the 25th's window has not begun, so the night due is the 24th's.
        let w = window_2_to_6(SEP25 + HOUR);
        assert_eq!(w.night, "20260924");
        assert_eq!(w.start_ms, SEP25 - 22 * HOUR);
        assert_eq!(w.end_ms, SEP25 - 18 * HOUR);
        // 03:00 (inside) and 09:00 (after): the 25th.
        for now in [SEP25 + 3 * HOUR, SEP25 + 9 * HOUR] {
            let w = window_2_to_6(now);
            assert_eq!(w.night, "20260925");
            assert_eq!(w.start_ms, SEP25 + 2 * HOUR);
            assert_eq!(w.end_ms, SEP25 + 6 * HOUR);
        }
    }

    #[test]
    fn due_night_follows_the_local_zone() {
        // 23:00 UTC on the 24th is 08:00 on the 25th in UTC+9: the 25th's window (local 02:00 =
        // 17:00 UTC on the 24th) has closed.
        let w = due_night(SEP25 - HOUR, 9 * 3_600, 2, 6);
        assert_eq!(w.night, "20260925");
        assert_eq!(w.start_ms, SEP25 - 7 * HOUR);
    }

    #[test]
    fn a_wrapping_window_belongs_to_the_evening_it_starts() {
        // 22:00–04:00. At 01:00 on the 25th the night in progress started on the 24th.
        let w = due_night(SEP25 + HOUR, 0, 22, 4);
        assert_eq!(w.night, "20260924");
        assert_eq!(w.end_ms, SEP25 + 4 * HOUR);
        // At noon, the last window that began is still the 24th's.
        assert_eq!(due_night(SEP25 + 12 * HOUR, 0, 22, 4).night, "20260924");
        // At 23:00 the 25th's has begun.
        assert_eq!(due_night(SEP25 + 23 * HOUR, 0, 22, 4).night, "20260925");
        assert_eq!(next_night(&w, 0, 22, 4).night, "20260925");
    }

    #[test]
    fn night_ids_convert_both_ways_across_leap_years() {
        for id in ["19700101", "20240229", "20260925", "20261231", "20000301"] {
            let days = days_from_night(id).unwrap();
            assert_eq!(format!("{:08}", yyyymmdd_from_days(days)), id);
        }
        assert_eq!(
            days_from_night("20260925-manual"),
            days_from_night("20260925")
        );
        assert_eq!(days_from_night("nightly"), None);
        assert_eq!(days_from_night("20261301"), None);
    }

    fn inputs<'a>(window: &'a NightWindow, now_ms: i64) -> NightInputs<'a> {
        NightInputs {
            window,
            now_ms,
            due: None,
            live_run: false,
            had_a_chance: true,
            latest_result: None,
            record: None,
        }
    }

    #[test]
    fn a_completed_night_is_fresh_and_an_empty_one_says_so() {
        let now = SEP25 + 9 * HOUR;
        let w = window_2_to_6(now);
        let report = night_report(&NightInputs {
            due: Some(DueCycle::Completed { events: 42 }),
            latest_result: Some("20260925"),
            ..inputs(&w, now)
        });
        assert_eq!(report.status, NightStatus::Completed);
        assert_eq!(report.reason, None);
        assert!(!report.stale);
        assert_eq!(report.nights_behind, 0);

        let empty = night_report(&NightInputs {
            due: Some(DueCycle::Completed { events: 0 }),
            latest_result: Some("20260925"),
            ..inputs(&w, now)
        });
        assert_eq!(empty.reason, Some(DreamReason::NoNewData));
    }

    #[test]
    fn friday_result_on_monday_is_stale_with_its_date() {
        // Lid shut Friday evening, opened Monday 09:00: the last result is Friday's.
        let friday = "20260918";
        let monday_9am = days_from_night("20260921").unwrap() * 86_400_000 + 9 * HOUR;
        let w = window_2_to_6(monday_9am);
        assert_eq!(w.night, "20260921");
        let report = night_report(&NightInputs {
            latest_result: Some(friday),
            ..inputs(&w, monday_9am)
        });
        assert_eq!(report.status, NightStatus::NotRun);
        assert_eq!(report.reason, Some(DreamReason::Asleep));
        assert_eq!(report.result_night.as_deref(), Some(friday));
        assert_eq!(report.nights_behind, 3);
        assert!(report.stale, "a three-night-old result is never today's");
    }

    #[test]
    fn a_failure_recorded_in_this_window_gives_the_reason() {
        let now = SEP25 + 8 * HOUR;
        let w = window_2_to_6(now);
        let record = OutcomeRecord::new(
            "20260925",
            RecordKind::Failed,
            Some(DreamReason::RelayUnreachable),
            SEP25 + 3 * HOUR,
        );
        let report = night_report(&NightInputs {
            due: Some(DueCycle::Failed),
            latest_result: Some("20260924"),
            record: Some(&record),
            ..inputs(&w, now)
        });
        assert_eq!(report.status, NightStatus::Failed);
        assert_eq!(report.reason, Some(DreamReason::RelayUnreachable));
        assert!(report.stale);
        assert_eq!(report.nights_behind, 1);

        // The same failure recorded before this window opened is yesterday's news.
        let old = OutcomeRecord {
            at_ms: w.start_ms - 1,
            ..record
        };
        let report = night_report(&NightInputs {
            due: Some(DueCycle::Failed),
            record: Some(&old),
            ..inputs(&w, now)
        });
        assert_eq!(report.reason, Some(DreamReason::Unknown));
    }

    #[test]
    fn a_failure_before_any_cycle_was_frozen_still_explains_the_night() {
        // The pre-run backup failed: no cycle row exists, only the record.
        let now = SEP25 + 8 * HOUR;
        let w = window_2_to_6(now);
        let record = OutcomeRecord::new(
            "20260925",
            RecordKind::Failed,
            Some(DreamReason::StorageError),
            SEP25 + 2 * HOUR,
        );
        let report = night_report(&NightInputs {
            record: Some(&record),
            ..inputs(&w, now)
        });
        assert_eq!(report.status, NightStatus::Failed);
        assert_eq!(report.reason, Some(DreamReason::StorageError));
    }

    #[test]
    fn a_stalled_run_is_an_interruption_and_a_live_one_is_running() {
        let now = SEP25 + 7 * HOUR;
        let w = window_2_to_6(now);
        let failed_earlier = OutcomeRecord::new(
            "20260925",
            RecordKind::Failed,
            Some(DreamReason::RelayUnreachable),
            SEP25 + 3 * HOUR,
        );
        let stalled = night_report(&NightInputs {
            due: Some(DueCycle::Stalled),
            record: Some(&failed_earlier),
            ..inputs(&w, now)
        });
        assert_eq!(stalled.status, NightStatus::Failed);
        assert_eq!(stalled.reason, Some(DreamReason::Interrupted));

        let live = night_report(&NightInputs {
            due: Some(DueCycle::Failed),
            live_run: true,
            record: Some(&failed_earlier),
            ..inputs(&w, now)
        });
        assert_eq!(live.status, NightStatus::Running);
        assert_eq!(live.reason, None);
    }

    #[test]
    fn liveness_needs_an_owner_a_valid_lease_and_a_recent_renewal() {
        let now = 10 * HOUR;
        assert_eq!(
            liveness(true, Some(now + HOUR), now - 60_000, now),
            DueCycle::Running
        );
        assert_eq!(
            liveness(true, Some(now + HOUR), now - STALL_MS - 1, now),
            DueCycle::Stalled
        );
        assert_eq!(liveness(true, Some(now - 1), now, now), DueCycle::Stalled);
        assert_eq!(liveness(false, None, now, now), DueCycle::Stalled);
    }

    #[test]
    fn busy_or_low_battery_through_the_window_is_not_asleep() {
        let w = window_2_to_6(SEP25 + 9 * HOUR);
        let busy = OutcomeRecord::new(
            "20260925",
            RecordKind::Skipped,
            Some(DreamReason::NotIdle),
            SEP25 + 5 * HOUR,
        );
        let after = night_report(&NightInputs {
            record: Some(&busy),
            ..inputs(&w, SEP25 + 9 * HOUR)
        });
        assert_eq!(
            (after.status, after.reason),
            (NightStatus::NotRun, Some(DreamReason::NotIdle))
        );
        // While the window is still open, a skip only means "waiting".
        let during = night_report(&NightInputs {
            record: Some(&busy),
            ..inputs(&w, SEP25 + 5 * HOUR)
        });
        assert_eq!(
            (during.status, during.reason),
            (NightStatus::Pending, Some(DreamReason::NotIdle))
        );
    }

    #[test]
    fn an_open_window_with_nothing_yet_is_pending_and_a_fresh_install_has_never_run() {
        let now = SEP25 + 3 * HOUR;
        let w = window_2_to_6(now);
        let pending = night_report(&NightInputs {
            latest_result: Some("20260924"),
            ..inputs(&w, now)
        });
        assert_eq!(
            (pending.status, pending.reason),
            (NightStatus::Pending, None)
        );
        // Yesterday's result is shown as yesterday's while tonight's has not run.
        assert!(pending.stale);

        let fresh = night_report(&NightInputs {
            had_a_chance: false,
            ..inputs(&w, SEP25 + 9 * HOUR)
        });
        assert_eq!((fresh.status, fresh.reason), (NightStatus::Never, None));
        assert!(!fresh.stale);
    }

    #[test]
    fn an_older_cycle_finishing_tonight_is_complete_but_stale() {
        // Friday's cycle resumed and finished inside Saturday's window.
        let saturday_9am = days_from_night("20260919").unwrap() * 86_400_000 + 9 * HOUR;
        let w = window_2_to_6(saturday_9am);
        let done = OutcomeRecord::new("20260918", RecordKind::Completed, None, w.start_ms + HOUR);
        let report = night_report(&NightInputs {
            latest_result: Some("20260918"),
            record: Some(&done),
            ..inputs(&w, saturday_9am)
        });
        assert_eq!(report.status, NightStatus::Completed);
        assert!(report.stale);
        assert_eq!(report.result_night.as_deref(), Some("20260918"));
    }

    #[test]
    fn a_result_from_the_future_is_not_negative_staleness() {
        let now = SEP25 + 9 * HOUR;
        let w = window_2_to_6(now);
        let report = night_report(&NightInputs {
            latest_result: Some("20260930"),
            ..inputs(&w, now)
        });
        assert_eq!(report.nights_behind, 0);
        assert!(!report.stale);
    }
}
