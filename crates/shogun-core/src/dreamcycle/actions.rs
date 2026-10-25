//! What the user can do about last night's Dream Cycle, and why a button is off (issue #285). Pure.
//!
//! [`super::outcome`] explains the night; this module turns that explanation into the actions a
//! face may offer, each with an `enabled` flag and, when off, a machine-readable
//! [`DisabledCode`]. The limit is known **before** the press — a cap reached, a window long gone,
//! a battery under the floor — so no button ever silently does nothing. Codes only, never copy:
//! the desktop, MCP, REST and CLI all get the same data and each face maps codes to its own
//! strings (invariant 6).
//!
//! Two actions (the owner's comment on #285):
//!
//! - [`ActionId::RunNow`] — the degraded, state-only cycle (FR-DC-01 縮退版), local rules only. It
//!   returns immediately and ships today (the desktop's "Run now"). It sends nothing to the relay,
//!   so the relay's daily cap, the Batch credential and the window do not limit it.
//! - [`ActionId::RetryFull`] — resubmit (or resume) the night's full cycle; its results arrive
//!   later. Not shipped: whether it ships is an open owner decision, so every
//!   face reports it as [`DisabledCode::NotOffered`] until [`RETRY_FULL_OFFERED`] flips. The rest of
//!   its rules are already decided here and tested, so flipping the switch is the only change.
//!
//! The user's idle state is deliberately not an input: the idle requirement protects an
//! unattended run from disturbing the user, and a press means the user is right there.

use serde::Serialize;

use super::outcome::{DreamReason, NightReport, NightStatus, NightWindow};

/// Whether "Retry tonight's cycle" is offered at all. Off until the owner decides the two-button
/// design ships (issue #285); while off, the action is listed but disabled as `not_offered`.
pub const RETRY_FULL_OFFERED: bool = false;

/// An action a face may offer for the Dream Cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionId {
    /// The degraded, local-only cycle, now.
    RunNow,
    /// The night's full cycle again (a resume when one is unfinished); results arrive later.
    RetryFull,
}

/// When the action's result is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completes {
    /// On return — a local run.
    Now,
    /// Later — the cloud job runs on its own schedule; say so instead of spinning.
    Later,
}

/// Why an action is off. A stable wire code; the UI maps it to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledCode {
    /// The action is not offered in this build (retry, until the owner decides).
    NotOffered,
    /// A cycle is running right now.
    AlreadyRunning,
    /// The plan or licence does not cover the nightly review.
    PlanInactive,
    /// The service refused the night's job; resubmitting the same job would be refused again.
    RejectedByService,
    /// A submitted job is still at the provider; its results are collected later. A retry would
    /// submit the same work twice.
    ResultsPending,
    /// The relay's daily cap is reached; it opens again when the cap resets.
    DailyCapReached,
    /// The night had nothing to review (no captured events, or nothing captured yet).
    NothingToReview,
    /// The night's full cycle already completed.
    AlreadyDone,
    /// The night's window is still open; the cycle runs by itself once the Mac is idle.
    WindowOpen,
    /// The window closed longer ago than the catch-up allowance; the full cycle carries into the
    /// next night (the degraded run is still available).
    OutsideCatchUp,
    /// No credential for the Batch lane (no entitled licence token on this device).
    BatchLaneUnavailable,
    /// Off power with the battery under the FR-DC-01 floor.
    LowBattery,
}

impl DisabledCode {
    /// Every code, for exhaustive tests and catalogues.
    pub const ALL: [DisabledCode; 12] = [
        DisabledCode::NotOffered,
        DisabledCode::AlreadyRunning,
        DisabledCode::PlanInactive,
        DisabledCode::RejectedByService,
        DisabledCode::ResultsPending,
        DisabledCode::DailyCapReached,
        DisabledCode::NothingToReview,
        DisabledCode::AlreadyDone,
        DisabledCode::WindowOpen,
        DisabledCode::OutsideCatchUp,
        DisabledCode::BatchLaneUnavailable,
        DisabledCode::LowBattery,
    ];
}

/// One action as a face renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DreamAction {
    pub id: ActionId,
    pub completes: Completes,
    pub enabled: bool,
    /// Why it is off; `None` exactly when `enabled`.
    pub disabled_reason: Option<DisabledCode>,
}

impl DreamAction {
    fn new(id: ActionId, completes: Completes, blocked: Option<DisabledCode>) -> Self {
        Self {
            id,
            completes,
            enabled: blocked.is_none(),
            disabled_reason: blocked,
        }
    }
}

/// The actions for the due night, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamActions {
    pub run_now: DreamAction,
    pub retry_full: DreamAction,
}

impl DreamActions {
    /// Display order: the action that returns now first.
    pub fn list(&self) -> [DreamAction; 2] {
        [self.run_now, self.retry_full]
    }
}

/// Where "now" sits relative to the due night's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPhase {
    /// Inside the window.
    Open,
    /// Closed, but within the catch-up allowance ([`super::sleep_wake::WAKE_CATCH_UP_MS`]): the
    /// night still counts as last night.
    CatchUp,
    /// Closed longer ago than the allowance.
    Closed,
}

/// [`WindowPhase`] for `now_ms` against the due night's window.
pub fn window_phase(now_ms: i64, window: &NightWindow, catch_up_ms: i64) -> WindowPhase {
    if now_ms < window.end_ms {
        WindowPhase::Open
    } else if now_ms <= window.end_ms.saturating_add(catch_up_ms) {
        WindowPhase::CatchUp
    } else {
        WindowPhase::Closed
    }
}

/// An unfinished full cycle in the ledger, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingFull {
    None,
    /// Unfinished with nothing waiting at the provider: a retry resumes it from the ledger.
    Resumable,
    /// Submitted and waiting at the provider: its results are collected later.
    AwaitingResults,
}

/// What the ledger says, alongside the night report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerFacts {
    pub pending_full: PendingFull,
    pub window: WindowPhase,
}

/// What only the process serving the answer can know. `None` is "unknown": an unknown fact never
/// disables an action by itself (a press reports its own failure), but the night's recorded reason
/// still can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFacts {
    /// The plan covers the nightly review (`Entitlements::background_intelligence`).
    pub plan_active: Option<bool>,
    /// An entitled Batch credential is present (the licence token the relay accepts).
    pub batch_lane: Option<bool>,
    /// Chunks left under the relay's daily cap, when the relay has said.
    pub cap_remaining: Option<u32>,
    /// Off power with the battery under the floor, right now.
    pub low_battery: Option<bool>,
    /// Whether the retry action is offered at all ([`RETRY_FULL_OFFERED`]).
    pub retry_offered: bool,
}

impl ExternalFacts {
    /// Nothing known beyond the ledger; the retry offered as this build ships it.
    pub fn unknown() -> Self {
        Self {
            plan_active: None,
            batch_lane: None,
            cap_remaining: None,
            low_battery: None,
            retry_offered: RETRY_FULL_OFFERED,
        }
    }
}

/// The actions for the due night (issue #285). Blockers are checked in a fixed order and the first
/// one wins: offered → running → plan → what the night's reason rules out → the night's state →
/// what is available right now. That order puts the most fundamental blocker in front of the user.
pub fn available(night: &NightReport, ledger: &LedgerFacts, env: &ExternalFacts) -> DreamActions {
    DreamActions {
        run_now: DreamAction::new(
            ActionId::RunNow,
            Completes::Now,
            run_now_blocker(night, env),
        ),
        retry_full: DreamAction::new(
            ActionId::RetryFull,
            Completes::Later,
            retry_blocker(night, ledger, env),
        ),
    }
}

fn run_now_blocker(night: &NightReport, env: &ExternalFacts) -> Option<DisabledCode> {
    if night.status == NightStatus::Running {
        return Some(DisabledCode::AlreadyRunning);
    }
    if env.plan_active == Some(false) {
        return Some(DisabledCode::PlanInactive);
    }
    night
        .reason
        .and_then(|reason| reason_blocks_run_now(reason, env))
}

/// What the night's reason alone rules out for the degraded run. No wildcard: a new reason does
/// not compile until it is placed here.
fn reason_blocks_run_now(reason: DreamReason, env: &ExternalFacts) -> Option<DisabledCode> {
    match reason {
        // Local rules, no relay: none of these limit a state-only run.
        DreamReason::Asleep
        | DreamReason::NotIdle
        | DreamReason::LowBattery
        | DreamReason::Interrupted
        | DreamReason::SubmissionRejected
        | DreamReason::RelayUnreachable
        | DreamReason::BatchExpired
        | DreamReason::BatchPending
        | DreamReason::DailyCap
        | DreamReason::StorageError
        | DreamReason::NoNewData
        | DreamReason::Unknown => None,
        // The night was refused for the plan; a renewal since then (known active) lifts it.
        DreamReason::PlanInactive => {
            (env.plan_active != Some(true)).then_some(DisabledCode::PlanInactive)
        }
    }
}

fn retry_blocker(
    night: &NightReport,
    ledger: &LedgerFacts,
    env: &ExternalFacts,
) -> Option<DisabledCode> {
    if !env.retry_offered {
        return Some(DisabledCode::NotOffered);
    }
    if night.status == NightStatus::Running {
        return Some(DisabledCode::AlreadyRunning);
    }
    if env.plan_active == Some(false) {
        return Some(DisabledCode::PlanInactive);
    }
    if let Some(code) = night
        .reason
        .and_then(|reason| reason_blocks_retry(reason, env))
    {
        return Some(code);
    }
    match night.status {
        NightStatus::Completed => return Some(DisabledCode::AlreadyDone),
        NightStatus::Pending => return Some(DisabledCode::WindowOpen),
        NightStatus::Never => return Some(DisabledCode::NothingToReview),
        NightStatus::Running => return Some(DisabledCode::AlreadyRunning),
        NightStatus::Failed | NightStatus::NotRun => {}
    }
    match ledger.pending_full {
        PendingFull::AwaitingResults => return Some(DisabledCode::ResultsPending),
        // Resuming an unfinished cycle is valid however old it is: the ordinary run resumes it too.
        PendingFull::Resumable => {}
        PendingFull::None if ledger.window == WindowPhase::Closed => {
            return Some(DisabledCode::OutsideCatchUp)
        }
        PendingFull::None => {}
    }
    if env.batch_lane == Some(false) {
        return Some(DisabledCode::BatchLaneUnavailable);
    }
    if env.cap_remaining == Some(0) {
        return Some(DisabledCode::DailyCapReached);
    }
    if env.low_battery == Some(true) {
        return Some(DisabledCode::LowBattery);
    }
    None
}

/// What the night's reason alone rules out for a full retry. No wildcard, as above.
fn reason_blocks_retry(reason: DreamReason, env: &ExternalFacts) -> Option<DisabledCode> {
    match reason {
        // A missed or cut-off night, or a transient/unknown failure: a retry can succeed. Low
        // battery through the window is history; the battery right now is checked separately.
        DreamReason::Asleep
        | DreamReason::NotIdle
        | DreamReason::LowBattery
        | DreamReason::Interrupted
        | DreamReason::RelayUnreachable
        | DreamReason::BatchExpired
        | DreamReason::StorageError
        | DreamReason::Unknown => None,
        DreamReason::SubmissionRejected => Some(DisabledCode::RejectedByService),
        DreamReason::BatchPending => Some(DisabledCode::ResultsPending),
        // The cap was hit on submission. Only a relay that reports room again lifts it.
        DreamReason::DailyCap => {
            (env.cap_remaining.unwrap_or(0) == 0).then_some(DisabledCode::DailyCapReached)
        }
        DreamReason::PlanInactive => {
            (env.plan_active != Some(true)).then_some(DisabledCode::PlanInactive)
        }
        DreamReason::NoNewData => Some(DisabledCode::NothingToReview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreamcycle::outcome::due_night;
    use crate::dreamcycle::sleep_wake::WAKE_CATCH_UP_MS;

    use DisabledCode as D;

    const HOUR: i64 = 3_600_000;
    /// 2026-09-25 00:00 UTC.
    const SEP25: i64 = 1_790_294_400_000;

    fn report(status: NightStatus, reason: Option<DreamReason>) -> NightReport {
        NightReport {
            night: "20260925".into(),
            status,
            reason,
            result_night: Some("20260924".into()),
            nights_behind: 1,
            stale: true,
        }
    }

    /// The status `night_report` pairs with each reason in practice.
    fn status_for(reason: DreamReason) -> NightStatus {
        match reason {
            DreamReason::Asleep | DreamReason::NotIdle | DreamReason::LowBattery => {
                NightStatus::NotRun
            }
            DreamReason::NoNewData => NightStatus::Completed,
            DreamReason::Interrupted
            | DreamReason::SubmissionRejected
            | DreamReason::RelayUnreachable
            | DreamReason::BatchExpired
            | DreamReason::BatchPending
            | DreamReason::DailyCap
            | DreamReason::PlanInactive
            | DreamReason::StorageError
            | DreamReason::Unknown => NightStatus::Failed,
        }
    }

    fn ledger(pending_full: PendingFull, window: WindowPhase) -> LedgerFacts {
        LedgerFacts {
            pending_full,
            window,
        }
    }

    /// The morning after, a couple of hours past the window, nothing pending, retry offered, and
    /// everything that can be known known to be fine.
    fn fine() -> ExternalFacts {
        ExternalFacts {
            plan_active: Some(true),
            batch_lane: Some(true),
            cap_remaining: Some(100),
            low_battery: Some(false),
            retry_offered: true,
        }
    }

    fn offered_unknown() -> ExternalFacts {
        ExternalFacts {
            retry_offered: true,
            ..ExternalFacts::unknown()
        }
    }

    /// `(run_now, retry_full)` disabled codes.
    type Pair = (Option<D>, Option<D>);

    fn codes(actions: &DreamActions) -> Pair {
        (
            actions.run_now.disabled_reason,
            actions.retry_full.disabled_reason,
        )
    }

    /// The table in docs/memory-api-mcp.md (`dream.status` actions), one row per reason: what each action says
    /// on a typical morning (catch-up window, nothing pending) with the environment fine, and with
    /// nothing known beyond the ledger.
    #[test]
    fn every_reason_maps_to_the_documented_actions() {
        #[rustfmt::skip]
        let table: [(DreamReason, Pair, Pair); 13] = [
            // reason                           env fine                          env unknown
            (DreamReason::Asleep,             (None, None),                     (None, None)),
            (DreamReason::NotIdle,            (None, None),                     (None, None)),
            (DreamReason::LowBattery,         (None, None),                     (None, None)),
            (DreamReason::Interrupted,        (None, None),                     (None, None)),
            (DreamReason::SubmissionRejected, (None, Some(D::RejectedByService)), (None, Some(D::RejectedByService))),
            (DreamReason::RelayUnreachable,   (None, None),                     (None, None)),
            (DreamReason::BatchExpired,       (None, None),                     (None, None)),
            (DreamReason::BatchPending,       (None, Some(D::ResultsPending)),  (None, Some(D::ResultsPending))),
            (DreamReason::DailyCap,           (None, None),                     (None, Some(D::DailyCapReached))),
            (DreamReason::PlanInactive,       (None, None),                     (Some(D::PlanInactive), Some(D::PlanInactive))),
            (DreamReason::StorageError,       (None, None),                     (None, None)),
            (DreamReason::NoNewData,          (None, Some(D::NothingToReview)), (None, Some(D::NothingToReview))),
            (DreamReason::Unknown,            (None, None),                     (None, None)),
        ];
        // The table covers every reason exactly once.
        assert_eq!(
            table.iter().map(|row| row.0).collect::<Vec<_>>(),
            DreamReason::ALL.to_vec()
        );
        let facts = ledger(PendingFull::None, WindowPhase::CatchUp);
        for (reason, with_fine, with_unknown) in table {
            let night = report(status_for(reason), Some(reason));
            assert_eq!(
                codes(&available(&night, &facts, &fine())),
                with_fine,
                "{reason:?}, environment fine"
            );
            assert_eq!(
                codes(&available(&night, &facts, &offered_unknown())),
                with_unknown,
                "{reason:?}, environment unknown"
            );
        }
    }

    #[test]
    fn every_reason_in_every_environment_is_consistent() {
        // enabled ⇔ no code; run_now always returns now and retry later; a running cycle blocks
        // both; an inactive plan blocks both; retry not offered is always `not_offered`.
        let windows = [WindowPhase::Open, WindowPhase::CatchUp, WindowPhase::Closed];
        let pendings = [
            PendingFull::None,
            PendingFull::Resumable,
            PendingFull::AwaitingResults,
        ];
        let statuses = [
            NightStatus::Never,
            NightStatus::Pending,
            NightStatus::Running,
            NightStatus::Completed,
            NightStatus::Failed,
            NightStatus::NotRun,
        ];
        let envs = [
            fine(),
            offered_unknown(),
            ExternalFacts::unknown(),
            ExternalFacts {
                plan_active: Some(false),
                ..fine()
            },
            ExternalFacts {
                batch_lane: Some(false),
                ..fine()
            },
            ExternalFacts {
                cap_remaining: Some(0),
                ..fine()
            },
            ExternalFacts {
                low_battery: Some(true),
                ..fine()
            },
        ];
        let reasons = DreamReason::ALL.map(Some);
        for reason in reasons.iter().copied().chain([None]) {
            for status in statuses {
                for window in windows {
                    for pending in pendings {
                        for env in envs {
                            let a =
                                available(&report(status, reason), &ledger(pending, window), &env);
                            for action in a.list() {
                                assert_eq!(action.enabled, action.disabled_reason.is_none());
                            }
                            assert_eq!(a.run_now.completes, Completes::Now);
                            assert_eq!(a.retry_full.completes, Completes::Later);
                            assert_eq!(a.list()[0].id, ActionId::RunNow);
                            if !env.retry_offered {
                                assert_eq!(a.retry_full.disabled_reason, Some(D::NotOffered));
                            }
                            if status == NightStatus::Running {
                                assert_eq!(a.run_now.disabled_reason, Some(D::AlreadyRunning));
                            }
                            if env.plan_active == Some(false) && status != NightStatus::Running {
                                assert_eq!(a.run_now.disabled_reason, Some(D::PlanInactive));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_shipping_build_lists_retry_as_not_offered() {
        let night = report(NightStatus::NotRun, Some(DreamReason::Asleep));
        let facts = ledger(PendingFull::None, WindowPhase::CatchUp);
        let a = available(&night, &facts, &ExternalFacts::unknown());
        assert!(a.run_now.enabled);
        assert_eq!(a.retry_full.disabled_reason, Some(D::NotOffered));
    }

    #[test]
    fn the_degraded_run_ignores_relay_limits() {
        // Cap reached, no Batch credential, low battery, window long gone: the local run still goes.
        let night = report(NightStatus::Failed, Some(DreamReason::DailyCap));
        let env = ExternalFacts {
            batch_lane: Some(false),
            cap_remaining: Some(0),
            low_battery: Some(true),
            ..fine()
        };
        let a = available(
            &night,
            &ledger(PendingFull::None, WindowPhase::Closed),
            &env,
        );
        assert!(a.run_now.enabled);
        assert_eq!(a.retry_full.disabled_reason, Some(D::DailyCapReached));
    }

    #[test]
    fn retry_shows_the_limit_before_the_press() {
        let night = report(NightStatus::NotRun, Some(DreamReason::Asleep));
        let facts = ledger(PendingFull::None, WindowPhase::CatchUp);
        let with = |env: ExternalFacts| available(&night, &facts, &env).retry_full.disabled_reason;
        assert_eq!(with(fine()), None);
        assert_eq!(
            with(ExternalFacts {
                batch_lane: Some(false),
                ..fine()
            }),
            Some(D::BatchLaneUnavailable)
        );
        assert_eq!(
            with(ExternalFacts {
                cap_remaining: Some(0),
                ..fine()
            }),
            Some(D::DailyCapReached)
        );
        assert_eq!(
            with(ExternalFacts {
                low_battery: Some(true),
                ..fine()
            }),
            Some(D::LowBattery)
        );
        // Unknown never disables on its own.
        assert_eq!(with(offered_unknown()), None);
    }

    #[test]
    fn a_window_too_far_behind_carries_over_unless_a_cycle_can_resume() {
        let night = report(NightStatus::NotRun, Some(DreamReason::Asleep));
        let retry = |pending, window| {
            available(&night, &ledger(pending, window), &fine())
                .retry_full
                .disabled_reason
        };
        assert_eq!(retry(PendingFull::None, WindowPhase::Open), None);
        assert_eq!(retry(PendingFull::None, WindowPhase::CatchUp), None);
        assert_eq!(
            retry(PendingFull::None, WindowPhase::Closed),
            Some(D::OutsideCatchUp)
        );
        // An unfinished cycle resumes from the ledger however old: no duplicated work.
        assert_eq!(retry(PendingFull::Resumable, WindowPhase::Closed), None);
        // A job waiting at the provider is collected, never resubmitted.
        assert_eq!(
            retry(PendingFull::AwaitingResults, WindowPhase::CatchUp),
            Some(D::ResultsPending)
        );
    }

    #[test]
    fn nights_without_a_reason_follow_their_status() {
        let facts = ledger(PendingFull::None, WindowPhase::CatchUp);
        let retry = |status| {
            available(&report(status, None), &facts, &fine())
                .retry_full
                .disabled_reason
        };
        assert_eq!(retry(NightStatus::Completed), Some(D::AlreadyDone));
        assert_eq!(retry(NightStatus::Pending), Some(D::WindowOpen));
        assert_eq!(retry(NightStatus::Never), Some(D::NothingToReview));
        assert_eq!(retry(NightStatus::Running), Some(D::AlreadyRunning));
        // Busy through an open window is still "the window is open": it runs once idle.
        let busy = report(NightStatus::Pending, Some(DreamReason::NotIdle));
        assert_eq!(
            available(
                &busy,
                &ledger(PendingFull::None, WindowPhase::Open),
                &fine()
            )
            .retry_full
            .disabled_reason,
            Some(D::WindowOpen)
        );
        // The degraded run is available in every one of these but a running cycle.
        for status in [
            NightStatus::Completed,
            NightStatus::Pending,
            NightStatus::Never,
        ] {
            assert!(
                available(&report(status, None), &facts, &fine())
                    .run_now
                    .enabled
            );
        }
    }

    #[test]
    fn a_renewed_plan_or_a_reset_cap_lifts_the_nights_reason() {
        let facts = ledger(PendingFull::None, WindowPhase::CatchUp);
        let plan = report(NightStatus::Failed, Some(DreamReason::PlanInactive));
        assert_eq!(codes(&available(&plan, &facts, &fine())), (None, None));
        let cap = report(NightStatus::Failed, Some(DreamReason::DailyCap));
        assert_eq!(
            available(&cap, &facts, &offered_unknown())
                .retry_full
                .disabled_reason,
            Some(D::DailyCapReached)
        );
        assert_eq!(
            available(&cap, &facts, &fine()).retry_full.disabled_reason,
            None
        );
    }

    #[test]
    fn window_phase_uses_the_catch_up_allowance() {
        let w = due_night(SEP25 + 3 * HOUR, 0, 2, 6);
        assert_eq!(
            window_phase(SEP25 + 3 * HOUR, &w, WAKE_CATCH_UP_MS),
            WindowPhase::Open
        );
        assert_eq!(
            window_phase(SEP25 + 6 * HOUR, &w, WAKE_CATCH_UP_MS),
            WindowPhase::CatchUp
        );
        assert_eq!(
            window_phase(SEP25 + 6 * HOUR + WAKE_CATCH_UP_MS, &w, WAKE_CATCH_UP_MS),
            WindowPhase::CatchUp
        );
        assert_eq!(
            window_phase(
                SEP25 + 6 * HOUR + WAKE_CATCH_UP_MS + 1,
                &w,
                WAKE_CATCH_UP_MS
            ),
            WindowPhase::Closed
        );
    }

    #[test]
    fn wire_codes_are_stable_snake_case() {
        let a = available(
            &report(NightStatus::Failed, Some(DreamReason::BatchPending)),
            &ledger(PendingFull::AwaitingResults, WindowPhase::CatchUp),
            &fine(),
        );
        assert_eq!(
            serde_json::to_value(a.list()).unwrap(),
            serde_json::json!([
                {"id": "run_now", "completes": "now", "enabled": true, "disabled_reason": null},
                {"id": "retry_full", "completes": "later", "enabled": false, "disabled_reason": "results_pending"},
            ])
        );
        let wire: Vec<String> = DisabledCode::ALL
            .iter()
            .map(|c| {
                serde_json::to_value(c)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            wire,
            [
                "not_offered",
                "already_running",
                "plan_inactive",
                "rejected_by_service",
                "results_pending",
                "daily_cap_reached",
                "nothing_to_review",
                "already_done",
                "window_open",
                "outside_catch_up",
                "batch_lane_unavailable",
                "low_battery",
            ]
        );
    }
}
