//! Plan entitlement (issue #97) — the Rust-core source of truth for what the current plan may do.
//!
//! CLAUDE.md プラン構成:
//! - No Free plan. 7-day full trial (Pro-equivalent) → Standard / Pro.
//! - **Standard**: capture + memory + search + Notch UI + first-layer reads (**including Gmail
//!   read via Composio**) + Dream Cycle + Morning Brief + **agent execution (L1/L2/L3: context
//!   actions, chat, drafts, Scribe)**. The Select KK key covers the observation side; agent
//!   execution runs on the user's own credentials (delegation / BYOK), which is why it costs
//!   nothing to include (owner decision 2026-09-24: a plan without chat and actions is not worth
//!   paying for).
//! - **Pro**: + Memory API (MCP/CLI/REST) + the Composio second-layer gate, which means **send
//!   unlock only** (real sending with draft-stop OFF). Reads are never Pro-gated.
//! - Meeting notes are available on every plan (only the Memory-API view, FR-MT-22, rides the
//!   Memory API gate). The Composio 3-disclosure read consent is orthogonal to plans.
//!
//! This module is **pure**: `now_ms` is always a parameter (repo convention — pure logic never
//! reads the clock), so the day-7 boundary is deterministic under test. The desktop layer owns
//! the inputs (onboarding.json's `trial_started_at`, the server-signed trial end and the verified
//! licence, see [`PlanInputs`]) and feeds them in through [`resolve_plan_from`]; every enforcement
//! point takes the resulting [`Entitlements`] value. Plan gating lives here in the Rust core —
//! webview gating is display only (CLAUDE.md: プラン判定はRustコア側で行う).
//!
//! ## Expired-trial posture (owner decision 2026-10-05, issue #940)
//!
//! CLAUDE.md says trial後は全員課金 (no Free fallback). When the 7-day trial has ended and there
//! is no active paid plan (a licence that verifies, including its existing offline grace):
//! - **Capture stops.** No new AX / OCR / screen / meeting capture is recorded
//!   ([`Entitlements::local_capture`] is false). Nothing already saved is deleted.
//! - **Memory access is locked** ([`Entitlements::memory_access`] is false): local keyword search
//!   and AI search, browsing and every read-only view of memory (Today, timeline, Visual recall,
//!   meeting history, chat over memory, context actions, Morning Brief, Dream), and export. There
//!   is no keyword fallback behind the lock.
//! - Also locked: agent execution, the Memory API, every send path, first-layer
//!   reads/sync, Dream Cycle / Morning Brief.
//! - **Still reachable** (never gated here): plan purchase and licence entry, settings and
//!   privacy controls, explicit data deletion, capture exclusions, onboarding.
//! - Buying a plan resolves to `Standard`/`Pro` on the next read, which restores everything with
//!   the saved data intact.
//!
//! Every locked entry point answers [`PLAN_REQUIRED`], before any provider call or result.

/// The stable error code every plan-locked entry point answers with (desktop commands, MCP,
/// REST, CLI). The webview maps it to the "Get a plan to continue" screen.
pub const PLAN_REQUIRED: &str = "plan_required";

/// Length of the full trial: 7 days, in milliseconds.
pub const TRIAL_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How far in the future a trial stamp may sit and still count (clock skew between the process
/// that stamped it and the one reading it). Beyond this the stamp is not trusted: a stamp in the
/// future would otherwise run the trial until `stamp + 7d`, e.g. year 5138 (issue #777).
pub const TRIAL_CLOCK_SKEW_MS: u64 = 10 * 60 * 1000;

/// The plan the device is currently on, as resolved from billing + the trial stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// A trial stamped on first account sign-in (or setup completion as a fallback).
    /// A missing clock grants no paid capabilities.
    ///
    /// `started_at_ms` is the trial's *effective* start: the local stamp, pulled earlier when the
    /// server's signed end of the trial comes sooner than stamp + 7 days
    /// ([`effective_trial_start`]). Without that signed end there is no running trial, so a
    /// re-stamped or deleted `onboarding.json` cannot start or outlive one (issue #1031).
    Trial { started_at_ms: Option<u64> },
    /// Paid Standard.
    Standard,
    /// Paid Standard bought while the device's trial still runs *according to the signed licence
    /// token* (`trial_until`). Gets the trial's Pro-equivalent capabilities until then, then plain
    /// Standard: buying early must not take away the days the trial promised (issue #777). The
    /// local stamp never grants this: deleting a file would re-grant it weekly.
    StandardInTrial { trial_until_ms: u64 },
    /// Paid Pro.
    Pro,
}

/// A paid plan (what a billing record can grant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidPlan {
    Standard,
    Pro,
}

/// What the billing system knows about this device's subscription. Sourced from the verified
/// FR-BIL-08 licence token (issue #8): the desktop's `billing::mac::state` and
/// `shogun_mcp::plan_source::billing_state_of` derive it from the cached signed token; a device
/// with no token (or one this build cannot verify) stays `Unknown` and resolves to the trial
/// rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BillingState {
    /// No billing record known (no licence token cached — any device that never purchased).
    #[default]
    Unknown,
    /// An active paid subscription.
    Active(PaidPlan),
    /// A subscription that existed but lapsed (cancelled / payment failed). Treated like
    /// `Unknown`: back to the trial rules, which for a stamped 7-day-old trial means expired.
    Lapsed,
}

/// Everything plan resolution reads. The effectful layers fill it from
/// `onboarding.json` (stamp), `trial_relay.json` (the verified trial token's end) and the
/// verified licence token (`billing` + its `trial_until` claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanInputs {
    /// `onboarding.json`'s `trial_started_at`, unix ms. `None` fails closed.
    pub trial_started_at_ms: Option<u64>,
    /// The end of this device's trial that the licence API **signed** (the `period_end` of a
    /// verified, device-bound trial token in `trial_relay.json`, issue #1031), unix ms, already
    /// bounded by what this Mac recorded on its own (the plain `trial_ends_at`, a sign-out).
    /// Required: with `None` no trial runs, whatever the local stamp says. At or past it the trial
    /// is over. `shogun_mcp::trial_relay::trial_end_ms` is the one place that fills it.
    pub server_trial_ends_at_ms: Option<u64>,
    pub billing: BillingState,
    /// The verified licence token's `trial_until`, unix ms — the only source of
    /// [`Plan::StandardInTrial`]. Bounded at the source (`shogun_license::LicenseToken::
    /// trial_until_ms`): a claim further than [`TRIAL_DURATION_MS`] plus the clock tolerance past
    /// the token's `iat` is malformed and arrives here as `None`.
    pub licence_trial_until_ms: Option<u64>,
}

/// The trial's effective start from the local stamp and the server's signed end: the stamp, or
/// earlier when `server_end - 7d` is earlier, so that `trial_expired(start, now)` is true exactly
/// when the local 7 days are up **or** `now >= server_end`. Both are required (fail closed):
/// a server end alone is not proof that the trial started locally on this Mac (the account check
/// before setup can carry one), and a local stamp alone is a file anyone can edit or recreate
/// (issue #1031).
pub fn effective_trial_start(
    trial_started_at_ms: Option<u64>,
    server_trial_ends_at_ms: Option<u64>,
) -> Option<u64> {
    let local = trial_started_at_ms?;
    let server_end = server_trial_ends_at_ms?;
    Some(local.min(server_end.saturating_sub(TRIAL_DURATION_MS)))
}

/// Resolve the effective plan from the trial stamp and the billing state, for a trial the server
/// confirmed for exactly that stamp — [`resolve_plan_from`] with `server_end = stamp + 7d` and
/// no licence `trial_until`. A shorthand for tests and fixed inputs: every live plan source passes
/// the signed end through [`resolve_plan_from`] instead. Billing wins: an active subscription
/// overrides the trial clock (a paying user is never "expired").
///
/// `Lapsed` with **no** trial stamp resolves to an already-expired trial, not a fresh one: a
/// lapsed subscription proves onboarding happened long ago, so a missing/corrupt stamp there is
/// lost state, and "nothing known = full trial" would let deleting onboarding.json re-grant full
/// Pro to a cancelled account forever. Only `Unknown` (genuinely pre-onboarding/pre-purchase)
/// has no billing record. Missing stamps fail closed in either case.
pub fn resolve_plan(trial_started_at_ms: Option<u64>, billing: BillingState) -> Plan {
    resolve_plan_from(PlanInputs {
        trial_started_at_ms,
        server_trial_ends_at_ms: trial_started_at_ms.map(|s| s.saturating_add(TRIAL_DURATION_MS)),
        billing,
        ..PlanInputs::default()
    })
}

/// Resolve the effective plan from every input. Standard bought during the trial
/// keeps the rest of the trial ([`Plan::StandardInTrial`]) only on the strength of the signed
/// token's `trial_until`; Pro already covers it. The trial's start is
/// [`effective_trial_start`], so the server's `trial_ends_at` bounds it.
pub fn resolve_plan_from(inputs: PlanInputs) -> Plan {
    let started_at_ms =
        effective_trial_start(inputs.trial_started_at_ms, inputs.server_trial_ends_at_ms);
    match inputs.billing {
        BillingState::Active(PaidPlan::Pro) => Plan::Pro,
        BillingState::Active(PaidPlan::Standard) => match inputs.licence_trial_until_ms {
            Some(trial_until_ms) => Plan::StandardInTrial { trial_until_ms },
            None => Plan::Standard,
        },
        BillingState::Unknown => Plan::Trial { started_at_ms },
        BillingState::Lapsed => Plan::Trial {
            // Stamp 0 = epoch: expired at any real clock reading.
            started_at_ms: Some(started_at_ms.unwrap_or(0)),
        },
    }
}

/// The user-facing plan status, for the "trial ended" surface and settings display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    /// No trial clock yet; account sign-in and onboarding remain available.
    TrialNotStarted,
    /// Trial running.
    Trial,
    /// Trial over, no paid plan: the locked posture (see module docs).
    TrialExpired,
    Standard,
    Pro,
}

/// What the current plan may do. One value, computed once per decision by [`entitlements`], passed
/// into every gate — the gates never re-derive plan logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entitlements {
    pub status: PlanStatus,
    /// New local capture into the on-device memory (AX, OCR / Visual recall frames, meeting
    /// audio, session import). False once the trial has ended without a paid plan (#940); saved
    /// data is never deleted by this.
    pub local_capture: bool,
    /// Access to saved memory: local keyword and AI search, browsing and read-only views, chat
    /// over memory, context actions, briefs, and export. False once the trial has ended without a
    /// paid plan (#940). Deletion, settings and exclusions are never behind this.
    pub memory_access: bool,
    /// First-layer service reads (Gmail read via Composio included — the 2026-07-30 decision) and
    /// the read-sync that feeds them. Standard and up; locked when expired.
    pub first_layer_reads: bool,
    /// Dream Cycle + Morning Brief (Select KK key lane). Standard and up; locked when expired.
    pub background_intelligence: bool,
    /// Agent execution (L1/L2/L3 engine + first-layer writes). Every paid plan and the trial;
    /// locked only when the trial has expired.
    pub agent_execution: bool,
    /// The Memory API, all three faces (MCP / CLI / REST). Pro / Trial only.
    pub memory_api: bool,
    /// The Composio second-layer **send unlock** — the only thing the Pro "Composio第2層" gate
    /// means (real sending with draft-stop OFF). Pro / Trial only. Composes with (never replaces)
    /// the 3-disclosure consent and draft-stop gates, and the send itself is still L3.
    pub composio_send_unlock: bool,
}

impl Entitlements {
    /// Fail-closed default until a durable trial stamp or paid licence is available: the same
    /// locked posture as an ended trial (#940).
    pub fn trial_not_started() -> Self {
        Self {
            status: PlanStatus::TrialNotStarted,
            ..Self::expired()
        }
    }

    /// `Ok` when saved memory may be read, searched, browsed or exported; otherwise the
    /// [`PLAN_REQUIRED`] code. Checked before any provider call or any result is assembled.
    pub fn require_memory_access(&self) -> Result<(), &'static str> {
        if self.memory_access {
            Ok(())
        } else {
            Err(PLAN_REQUIRED)
        }
    }

    /// The locked posture: a trial that has ended with no paid
    /// plan. Only plan purchase, settings, deletion, exclusions and onboarding stay reachable,
    /// and none of those are entitlements.
    fn expired() -> Self {
        Self {
            status: PlanStatus::TrialExpired,
            local_capture: false,
            memory_access: false,
            first_layer_reads: false,
            background_intelligence: false,
            agent_execution: false,
            memory_api: false,
            composio_send_unlock: false,
        }
    }

    fn full(status: PlanStatus) -> Self {
        Self {
            status,
            local_capture: true,
            memory_access: true,
            first_layer_reads: true,
            background_intelligence: true,
            agent_execution: true,
            memory_api: true,
            composio_send_unlock: true,
        }
    }
}

/// Whether a stamped trial is expired at `now_ms`: the trial covers exactly
/// [`TRIAL_DURATION_MS`]; the first millisecond of day 8 (`started + 7d`) is expired. A stamp more
/// than [`TRIAL_CLOCK_SKEW_MS`] in the future is treated as expired: it is either a clock that
/// stepped back or an edited file, and neither may extend the trial.
pub fn trial_expired(started_at_ms: u64, now_ms: u64) -> bool {
    started_at_ms > now_ms.saturating_add(TRIAL_CLOCK_SKEW_MS)
        || now_ms.saturating_sub(started_at_ms) >= TRIAL_DURATION_MS
}

/// The pure entitlement function: plan + injected clock → what is allowed. Never reads the clock.
pub fn entitlements(plan: Plan, now_ms: u64) -> Entitlements {
    match plan {
        Plan::Pro => Entitlements::full(PlanStatus::Pro),
        // Agent execution runs on the user's own credentials, so the plan boundary is the API
        // faces and the Composio send unlock.
        Plan::Standard => standard(),
        Plan::StandardInTrial { trial_until_ms } => {
            if now_ms < trial_until_ms {
                Entitlements::full(PlanStatus::Standard)
            } else {
                standard()
            }
        }
        Plan::Trial { started_at_ms } => match started_at_ms {
            // No durable clock is not proof of a trial. This also closes standalone API access
            // when onboarding.json is absent or corrupt, before the desktop can initialize it.
            None => Entitlements::trial_not_started(),
            Some(start) if trial_expired(start, now_ms) => Entitlements::expired(),
            _ => Entitlements::full(PlanStatus::Trial),
        },
    }
}

fn standard() -> Entitlements {
    Entitlements {
        memory_api: false,
        composio_send_unlock: false,
        ..Entitlements::full(PlanStatus::Standard)
    }
}

/// The provider seam the effectful layers implement: the desktop app reads onboarding.json + the
/// (stubbed) billing state; the standalone API/MCP binaries read the shared file. Gates never call
/// this directly — the caller resolves once per request/decision and passes the value in.
pub trait EntitlementSource: Send + Sync {
    /// The entitlements in force at `now_ms`.
    fn entitlements(&self, now_ms: u64) -> Entitlements;
}

/// A fixed-plan source (tests, and the pre-Stripe default of "no billing record").
#[derive(Debug, Clone, Copy)]
pub struct StaticPlan(pub Plan);

impl EntitlementSource for StaticPlan {
    fn entitlements(&self, now_ms: u64) -> Entitlements {
        entitlements(self.0, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_000_000;

    fn at(plan: Plan, now_ms: u64) -> Entitlements {
        entitlements(plan, now_ms)
    }

    #[test]
    fn trial_is_pro_equivalent_while_running() {
        let e = at(Plan::Trial { started_at_ms: Some(START) }, START + 1);
        assert_eq!(e.status, PlanStatus::Trial);
        assert_eq!(e, Entitlements::full(PlanStatus::Trial));
        assert!(e.agent_execution && e.memory_api && e.composio_send_unlock);
    }

    #[test]
    fn day_7_boundary_last_ms_in_first_ms_out() {
        let plan = Plan::Trial { started_at_ms: Some(START) };
        // one millisecond before the boundary: still full trial
        let last = at(plan, START + TRIAL_DURATION_MS - 1);
        assert_eq!(last.status, PlanStatus::Trial);
        assert!(last.agent_execution);
        // exactly started + 7d: expired
        let first_out = at(plan, START + TRIAL_DURATION_MS);
        assert_eq!(first_out.status, PlanStatus::TrialExpired);
        assert!(!first_out.agent_execution);
    }

    #[test]
    fn unstamped_trial_is_not_started_and_fails_closed() {
        let e = at(Plan::Trial { started_at_ms: None }, u64::MAX);
        assert_eq!(e.status, PlanStatus::TrialNotStarted);
        assert_eq!(e, Entitlements::trial_not_started());
        assert!(!e.memory_api && !e.agent_execution && !e.composio_send_unlock);
    }

    #[test]
    fn expired_posture_stops_capture_and_locks_memory() {
        let e = at(Plan::Trial { started_at_ms: Some(START) }, START + TRIAL_DURATION_MS + 5);
        assert_eq!(e.status, PlanStatus::TrialExpired);
        // capture stops, and search, browsing and export lock
        assert!(!e.local_capture);
        assert!(!e.memory_access);
        assert_eq!(e.require_memory_access(), Err(PLAN_REQUIRED));
        // everything that spends a key or leaves the device stays locked
        assert!(!e.first_layer_reads);
        assert!(!e.background_intelligence);
        assert!(!e.agent_execution);
        assert!(!e.memory_api);
        assert!(!e.composio_send_unlock);
    }

    #[test]
    fn allow_deny_matrix_per_plan() {
        // (plan, now) → [capture, memory, reads, background, agent_exec, memory_api, composio_send]
        let expired_now = START + TRIAL_DURATION_MS;
        let cases: &[(Plan, u64, [bool; 7])] = &[
            (Plan::Trial { started_at_ms: Some(START) }, START, [true; 7]),
            (Plan::Trial { started_at_ms: None }, START, [false; 7]),
            (Plan::Standard, START, [true, true, true, true, true, false, false]),
            (Plan::Standard, expired_now, [true, true, true, true, true, false, false]),
            (Plan::Pro, START, [true; 7]),
            (Plan::Pro, expired_now, [true; 7]),
            (Plan::StandardInTrial { trial_until_ms: expired_now }, START, [true; 7]),
            (
                Plan::StandardInTrial { trial_until_ms: expired_now },
                expired_now,
                [true, true, true, true, true, false, false],
            ),
            (Plan::Trial { started_at_ms: Some(START) }, expired_now, [false; 7]),
        ];
        for (plan, now, [capture, memory, reads, bg, exec, api, send]) in cases {
            let e = at(*plan, *now);
            assert_eq!(e.local_capture, *capture, "{plan:?} capture");
            assert_eq!(e.memory_access, *memory, "{plan:?} memory access");
            assert_eq!(e.require_memory_access().is_ok(), *memory, "{plan:?} memory gate");
            assert_eq!(e.first_layer_reads, *reads, "{plan:?} reads");
            assert_eq!(e.background_intelligence, *bg, "{plan:?} background");
            assert_eq!(e.agent_execution, *exec, "{plan:?} agent execution");
            assert_eq!(e.memory_api, *api, "{plan:?} memory api");
            assert_eq!(e.composio_send_unlock, *send, "{plan:?} composio send");
        }
    }

    #[test]
    fn expiry_locks_everything_and_a_purchase_restores_it() {
        let stamp = Some(START);
        let expired_now = START + TRIAL_DURATION_MS + 1;
        let locked = at(resolve_plan(stamp, BillingState::Unknown), expired_now);
        assert_eq!(locked, Entitlements::expired());
        // A lapsed licence (past its grace) is the same locked posture.
        assert_eq!(at(resolve_plan(stamp, BillingState::Lapsed), expired_now), locked);
        // Buying either plan restores capture and memory on the next read; nothing was deleted,
        // so access simply comes back.
        for paid in [PaidPlan::Standard, PaidPlan::Pro] {
            let e = at(resolve_plan(stamp, BillingState::Active(paid)), expired_now);
            assert!(e.local_capture && e.memory_access, "{paid:?}");
            assert_eq!(e.require_memory_access(), Ok(()));
        }
    }

    #[test]
    fn standard_runs_agents_but_never_the_api_faces_or_composio_send() {
        let e = at(Plan::Standard, u64::MAX);
        assert_eq!(e.status, PlanStatus::Standard);
        // Gmail read via Composio is a first-layer read → Standard
        assert!(e.first_layer_reads);
        // Chat, context actions and drafts run on the user's own credentials → Standard.
        // The API faces and real Composio sending stay Pro.
        assert!(e.agent_execution);
        assert!(!e.memory_api && !e.composio_send_unlock);
    }

    #[test]
    fn billing_overrides_an_expired_trial() {
        let stamp = Some(START);
        let expired_now = START + TRIAL_DURATION_MS + 1;
        // no billing → expired
        assert_eq!(
            at(resolve_plan(stamp, BillingState::Unknown), expired_now).status,
            PlanStatus::TrialExpired
        );
        // an active subscription wins over the trial clock
        assert_eq!(
            resolve_plan(stamp, BillingState::Active(PaidPlan::Pro)),
            Plan::Pro
        );
        // Standard with a local stamp alone is plain Standard: only the token's `trial_until`
        // buys the rest of the trial.
        assert_eq!(resolve_plan(stamp, BillingState::Active(PaidPlan::Standard)), Plan::Standard);
        assert_eq!(
            at(resolve_plan(stamp, BillingState::Active(PaidPlan::Standard)), expired_now),
            standard()
        );
        assert_eq!(resolve_plan(None, BillingState::Active(PaidPlan::Standard)), Plan::Standard);
        // a lapsed subscription falls back to the trial rules (→ expired here)
        assert_eq!(
            at(resolve_plan(stamp, BillingState::Lapsed), expired_now).status,
            PlanStatus::TrialExpired
        );
    }

    #[test]
    fn buying_standard_mid_trial_keeps_the_trial_until_the_tokens_trial_until() {
        let trial_until = START + TRIAL_DURATION_MS;
        let plan = resolve_plan_from(PlanInputs {
            trial_started_at_ms: Some(START),
            billing: BillingState::Active(PaidPlan::Standard),
            licence_trial_until_ms: Some(trial_until),
            ..PlanInputs::default()
        });
        assert_eq!(plan, Plan::StandardInTrial { trial_until_ms: trial_until });
        let day2 = at(plan, START + 2 * 24 * 60 * 60 * 1000);
        assert_eq!(day2.status, PlanStatus::Standard);
        assert!(day2.memory_api && day2.composio_send_unlock && day2.agent_execution);
        let last = at(plan, trial_until - 1);
        assert!(last.memory_api);
        let after = at(plan, trial_until);
        assert_eq!(after, standard());
        assert!(after.agent_execution && !after.memory_api && !after.composio_send_unlock);
    }

    /// The bonus comes from the signed token only. A Standard buyer who deletes
    /// `onboarding.json` weekly and re-stamps gets Standard, not seven more days of Pro.
    #[test]
    fn a_local_stamp_never_grants_standard_the_trial_bonus() {
        let now = START + 1;
        for stamp in [None, Some(START), Some(now), Some(now + 24 * 60 * 60 * 1000)] {
            let plan = resolve_plan_from(PlanInputs {
                trial_started_at_ms: stamp,
                billing: BillingState::Active(PaidPlan::Standard),
                licence_trial_until_ms: None,
                ..PlanInputs::default()
            });
            assert_eq!(plan, Plan::Standard, "{stamp:?}");
            assert_eq!(at(plan, now), standard());
        }
        // With the claim, the stamp is irrelevant too — including a missing one.
        let plan = resolve_plan_from(PlanInputs {
            trial_started_at_ms: None,
            billing: BillingState::Active(PaidPlan::Standard),
            licence_trial_until_ms: Some(now + 1),
            ..PlanInputs::default()
        });
        assert!(at(plan, now).memory_api);
        assert!(!at(plan, now + 1).memory_api);
        // Pro and the trial rules ignore the claim.
        let pro = resolve_plan_from(PlanInputs {
            billing: BillingState::Active(PaidPlan::Pro),
            licence_trial_until_ms: Some(0),
            ..PlanInputs::default()
        });
        assert_eq!(pro, Plan::Pro);
        let unknown = resolve_plan_from(PlanInputs {
            trial_started_at_ms: Some(START),
            server_trial_ends_at_ms: Some(START + TRIAL_DURATION_MS),
            licence_trial_until_ms: Some(u64::MAX),
            ..PlanInputs::default()
        });
        assert_eq!(unknown, Plan::Trial { started_at_ms: Some(START) });
    }

    #[test]
    fn the_servers_trial_end_is_authoritative_over_the_local_stamp() {
        // A real wall clock (2027-01-15), so `server_end - 7d` stays positive.
        const START: u64 = 1_800_000_000_000;
        let server_end = START + 3 * 24 * 60 * 60 * 1000;
        // A fresh local stamp (the file was deleted and onboarding re-run) with the server's
        // older end: expired at the server's end, not seven days after the new stamp.
        let restamped = Some(START + 2 * 24 * 60 * 60 * 1000);
        let plan = resolve_plan_from(PlanInputs {
            trial_started_at_ms: restamped,
            server_trial_ends_at_ms: Some(server_end),
            ..PlanInputs::default()
        });
        assert_eq!(plan, Plan::Trial { started_at_ms: Some(server_end - TRIAL_DURATION_MS) });
        assert_eq!(at(plan, server_end - 1).status, PlanStatus::Trial);
        assert_eq!(at(plan, server_end).status, PlanStatus::TrialExpired);
        // A server end later than stamp + 7d does not extend the local trial.
        let late = resolve_plan_from(PlanInputs {
            trial_started_at_ms: Some(START),
            server_trial_ends_at_ms: Some(START + 30 * 24 * 60 * 60 * 1000),
            ..PlanInputs::default()
        });
        assert_eq!(late, Plan::Trial { started_at_ms: Some(START) });
        assert_eq!(at(late, START + TRIAL_DURATION_MS).status, PlanStatus::TrialExpired);
        // Missing local stamp still fails closed, server end or not.
        let unstamped = resolve_plan_from(PlanInputs {
            server_trial_ends_at_ms: Some(u64::MAX),
            ..PlanInputs::default()
        });
        assert_eq!(unstamped, Plan::Trial { started_at_ms: None });
        assert_eq!(at(unstamped, START).status, PlanStatus::TrialNotStarted);
        // A server end in the past with a lapsed licence: expired.
        let lapsed = resolve_plan_from(PlanInputs {
            trial_started_at_ms: Some(START),
            server_trial_ends_at_ms: Some(START),
            billing: BillingState::Lapsed,
            ..PlanInputs::default()
        });
        assert_eq!(at(lapsed, START + 1).status, PlanStatus::TrialExpired);
        // The pure helper.
        assert_eq!(effective_trial_start(None, Some(server_end)), None);
        assert_eq!(effective_trial_start(Some(START), None), None);
        assert_eq!(effective_trial_start(Some(START), Some(5)), Some(0));
        // The wrapper is the case of a server end confirming the stamp, with no claim.
        assert_eq!(
            resolve_plan(Some(START), BillingState::Unknown),
            resolve_plan_from(PlanInputs {
                trial_started_at_ms: Some(START),
                server_trial_ends_at_ms: Some(START + TRIAL_DURATION_MS),
                ..PlanInputs::default()
            })
        );
    }

    /// Issue #1031: without the server's signed end, no local stamp runs a trial — edited to an
    /// hour ago, rewritten after deleting the file, or untouched. It reads as not started (sign
    /// in), and a lapsed licence stays expired.
    #[test]
    fn a_local_stamp_alone_never_runs_a_trial() {
        const START: u64 = 1_800_000_000_000;
        let now = START + 60 * 60 * 1000;
        for stamp in [START, now - 1, now] {
            let plan = resolve_plan_from(PlanInputs {
                trial_started_at_ms: Some(stamp),
                ..PlanInputs::default()
            });
            assert_eq!(plan, Plan::Trial { started_at_ms: None });
            let e = at(plan, now);
            assert_eq!(e.status, PlanStatus::TrialNotStarted);
            assert!(!e.memory_api && !e.agent_execution && !e.local_capture && !e.memory_access);
            let lapsed = resolve_plan_from(PlanInputs {
                trial_started_at_ms: Some(stamp),
                billing: BillingState::Lapsed,
                ..PlanInputs::default()
            });
            assert_eq!(at(lapsed, now).status, PlanStatus::TrialExpired);
        }
        // The same stamp with the signed end: running until that end.
        let plan = resolve_plan_from(PlanInputs {
            trial_started_at_ms: Some(now),
            server_trial_ends_at_ms: Some(START + TRIAL_DURATION_MS),
            ..PlanInputs::default()
        });
        assert_eq!(at(plan, now).status, PlanStatus::Trial);
        assert_eq!(at(plan, START + TRIAL_DURATION_MS).status, PlanStatus::TrialExpired);
    }

    #[test]
    fn a_trial_stamp_in_the_future_grants_nothing() {
        let now = 1_800_000_000_000;
        // the issue's example: a stamp in the year 5138
        let far = at(Plan::Trial { started_at_ms: Some(99_999_999_999_999) }, now);
        assert_eq!(far.status, PlanStatus::TrialExpired);
        assert!(!far.memory_api && !far.agent_execution && !far.composio_send_unlock);
        // ten days ahead would otherwise run a 17-day trial
        let ahead = at(Plan::Trial { started_at_ms: Some(now + 10 * 24 * 60 * 60 * 1000) }, now);
        assert_eq!(ahead.status, PlanStatus::TrialExpired);
        // nor does a stamp of any kind stretch a mid-trial Standard purchase
        let bought = resolve_plan(Some(now + 10 * 24 * 60 * 60 * 1000), BillingState::Active(PaidPlan::Standard));
        assert_eq!(bought, Plan::Standard);
        assert_eq!(at(bought, now), standard());
    }

    #[test]
    fn small_clock_skew_still_counts_as_a_running_trial() {
        let now = 1_800_000_000_000;
        let e = at(Plan::Trial { started_at_ms: Some(now + TRIAL_CLOCK_SKEW_MS) }, now);
        assert_eq!(e.status, PlanStatus::Trial);
        let e = at(Plan::Trial { started_at_ms: Some(now + TRIAL_CLOCK_SKEW_MS + 1) }, now);
        assert_eq!(e.status, PlanStatus::TrialExpired);
    }

    #[test]
    fn lapsed_without_a_trial_stamp_is_expired_not_full_pro() {
        // A lapsed subscriber whose onboarding.json is missing/corrupt (stamp = None) must not
        // resolve to an eternal not-yet-started trial with full Pro entitlements. Any real wall
        // clock is far past epoch + 7 days; 2026-01-01 in unix ms stands in for one here.
        let real_now = 1_767_225_600_000;
        let e = at(resolve_plan(None, BillingState::Lapsed), real_now);
        assert_eq!(e.status, PlanStatus::TrialExpired);
        assert!(!e.agent_execution && !e.memory_api && !e.composio_send_unlock);
        assert!(!e.first_layer_reads && !e.background_intelligence);
        // the same locked posture as every expired state (#940)
        assert!(!e.local_capture && !e.memory_access);
        // A missing stamp is never an unlimited trial, even without a billing record.
        assert_eq!(
            at(resolve_plan(None, BillingState::Unknown), real_now).status,
            PlanStatus::TrialNotStarted
        );
    }

    #[test]
    fn static_source_is_deterministic() {
        let src = StaticPlan(Plan::Trial { started_at_ms: Some(START) });
        assert_eq!(src.entitlements(START).status, PlanStatus::Trial);
        assert_eq!(src.entitlements(START + TRIAL_DURATION_MS).status, PlanStatus::TrialExpired);
    }
}
