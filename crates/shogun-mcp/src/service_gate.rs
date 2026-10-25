//! The composed first-layer integration gate (§6.9.2). One decision that layers everything
//! a service operation must satisfy *before* it runs, on top of the raw scope table
//! ([`crate::scope::authorize`]):
//!
//! 1. **Wave release** (FR-INT-03) — an operation on an unreleased wave is denied outright.
//! 2. **Scope table** — unknown / not-implemented operations are denied.
//! 3. **Connection state** (FR-INT-06/07) — a disconnected service can do nothing; an *amber*
//!    (needs-reauth) service serves cached **reads** but no writes (the token is invalid).
//! 4. **Composio opt-in consent** (FR-C2-02, issue #80 §2-A0) — Gmail is served entirely through
//!    Composio (the 2026-07-30 decision), so *every* Gmail operation — reads included — crosses a
//!    third-party boundary. None of them may run before the three-disclosure consent; without it
//!    the mailbox is treated as if it were not connected (未同意 = 未接続扱い).
//! 5. **Draft-stop mode** — the Gmail "draft-stop" setting (§6.10) blocks Gmail send entirely, even
//!    though send would otherwise route to Composio.
//! 6. **Plan entitlement** (issue #97) — reads need an active plan (`first_layer_reads`: Standard
//!    and up, Gmail-read-via-Composio included per the 2026-07-30 decision); writes and sends are
//!    agent execution (`agent_execution`: any paid plan / active trial), and the Composio-routed Gmail send
//!    additionally needs the send unlock. An expired trial is denied everything here.
//!
//! Invariant 4 is preserved end to end: the scope table only ever gates an [`OpClass::ExternalSend`]
//! at L3 / Composio / not-implemented, and this gate maps that faithfully — it can never turn a send
//! into an L1/L2 auto-run (test-asserted).

use shogun_agents::entitlement::Entitlements;
use shogun_agents::permission::Level;

use crate::connection::ConnState;
use crate::scope::{self, Gating, OpClass, Service, Wave};

/// The runtime context the gate needs beyond the static scope table.
#[derive(Debug, Clone, Copy)]
pub struct OpContext {
    /// The highest wave rolled out (FR-INT-03).
    pub highest_released: Wave,
    /// The service's current connection state (FR-INT-06/07).
    pub conn: ConnState,
    /// The global Gmail draft-stop setting (§6.10): when true, Gmail never sends.
    pub draft_stop: bool,
    /// Whether the Composio three-disclosure consent is acknowledged (FR-C2-02). Gmail runs
    /// entirely through Composio, so this gates *all* Gmail operations, reads included — resolved
    /// by the caller from the persisted policy, never re-derived here. Callers that already fold
    /// "no consent" into [`ConnState::Disconnected`] (the tool layer, `tool_catalog::ServiceState`)
    /// pass `true`; this field exists so the sync/runtime layer cannot skip the check.
    pub composio_consent: bool,
    /// The plan entitlements in force (issue #97), resolved by the caller from onboarding +
    /// billing state — never re-derived here.
    pub plan: Entitlements,
}

/// Why an operation was refused by the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The service's wave is not rolled out yet (FR-INT-03).
    UnreleasedWave,
    /// The operation is not in the service's scope table.
    UnknownOp,
    /// The operation is explicitly out of v1 scope (the "—" rows).
    NotImplemented,
    /// The service is not connected (no token).
    NotConnected,
    /// The service is amber (needs reauth) and this is a write — the token is invalid.
    NeedsReauth,
    /// Gmail draft-stop is on, so send is blocked (§6.10).
    DraftStop,
    /// The Composio three-disclosure consent has not been granted, and this service runs through
    /// Composio (Gmail): nothing — reads included — may cross the third-party boundary (FR-C2-02).
    ConsentRequired,
    /// The plan does not cover this operation (issue #97): reads need an active plan (Standard+),
    /// writes/sends need agent execution (Pro / active trial).
    PlanNotEntitled,
}

/// The gate's verdict for one service operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpDecision {
    /// Allowed as a background read (no per-action confirm).
    Background,
    /// Allowed after a confirm at this permission level.
    RequiresLevel(Level),
    /// A first-layer-denied send routed to Composio (second layer, itself L3) — Gmail send with
    /// draft-stop OFF.
    RequiresComposio,
    /// Refused, with the reason.
    Denied(DenyReason),
}

impl OpDecision {
    /// Whether the operation may proceed (in any allowed form).
    pub fn is_allowed(self) -> bool {
        !matches!(self, OpDecision::Denied(_))
    }
}

/// Authorize one service operation against the full first-layer policy (§6.9.2).
pub fn authorize_op(service: Service, op_name: &str, ctx: &OpContext) -> OpDecision {
    // 1. Wave release — an unreleased service is entirely unreachable.
    if !service.is_released(ctx.highest_released) {
        return OpDecision::Denied(DenyReason::UnreleasedWave);
    }
    // 2. Scope table — the op must exist and be implemented.
    let Some(op) = scope::lookup(service, op_name) else {
        return OpDecision::Denied(DenyReason::UnknownOp);
    };
    if matches!(op.gating, Gating::NotImplemented) {
        return OpDecision::Denied(DenyReason::NotImplemented);
    }
    // 3. Connection — disconnected does nothing; amber serves cached reads only. Web has no
    //    connection to make: its "disconnected" is the missing Web fetch consent (FR-WV-15),
    //    folded into `conn` by the caller, and the refusal says so rather than "not connected".
    match ctx.conn {
        ConnState::Disconnected if service == Service::Web => {
            return OpDecision::Denied(DenyReason::ConsentRequired)
        }
        ConnState::Disconnected => return OpDecision::Denied(DenyReason::NotConnected),
        ConnState::NeedsReauth { .. } if op.class != OpClass::Read => {
            return OpDecision::Denied(DenyReason::NeedsReauth);
        }
        _ => {}
    }
    // 4. Composio consent — Gmail is served entirely through Composio (2026-07-30 decision), so
    //    every Gmail op, reads included, needs the three-disclosure opt-in first. Checked before
    //    draft-stop and plan: an unconsented mailbox refuses for the consent, never for a setting
    //    further inside a boundary the user has not agreed to cross.
    if service == Service::Gmail && !ctx.composio_consent {
        return OpDecision::Denied(DenyReason::ConsentRequired);
    }
    // 5. Draft-stop — Gmail send (the only Composio-routed op) is blocked while it is on.
    if service == Service::Gmail && op_name == "send" && ctx.draft_stop {
        return OpDecision::Denied(DenyReason::DraftStop);
    }
    // 6. Plan entitlement (issue #97). Reads are Standard-and-up (Gmail read via Composio
    //    included — the 2026-07-30 decision); anything that writes or sends is agent execution
    //    (Pro / active trial), and the Composio-routed send additionally needs the send unlock.
    let entitled = match op.class {
        // A Web read exists for the chat that summarises it — Agent lane, Pro (issue #555 §4-7);
        // there is no Standard-plan surface that could show a fetched transcript.
        OpClass::Read if service == Service::Web => ctx.plan.agent_execution,
        OpClass::Read => ctx.plan.first_layer_reads,
        OpClass::ExternalSend if service == Service::Gmail => {
            ctx.plan.agent_execution && ctx.plan.composio_send_unlock
        }
        _ => ctx.plan.agent_execution,
    };
    if !entitled {
        return OpDecision::Denied(DenyReason::PlanNotEntitled);
    }
    // 7. Map the (surviving) gating to a decision. The table's `Background` for a Web `page`
    //    read is the floor for a link the user wrote; the tool loop raises a model-chosen link
    //    to one tap (`tool_catalog::web_page_read_level`, audit 2026-10-10 M4) before it runs.
    match op.gating {
        Gating::Background => OpDecision::Background,
        Gating::Level(l) => OpDecision::RequiresLevel(l),
        Gating::ComposioOnly => OpDecision::RequiresComposio,
        // NotImplemented is handled in step 2; it cannot reach here.
        Gating::NotImplemented => OpDecision::Denied(DenyReason::NotImplemented),
    }
}

/// Whether an *allowed* operation constitutes an egress that must be recorded in the traceability
/// log (invariant 3): any external send, or a service write that leaves the device. Reads and
/// device-local drafts do not egress. The executor calls this to decide whether to trace.
pub fn requires_traceability(service: Service, op_name: &str) -> bool {
    matches!(
        scope::lookup(service, op_name).map(|o| o.class),
        Some(OpClass::ExternalSend | OpClass::ServiceStateChange)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ReauthReason;

    #[test]
    fn web_reads_need_consent_and_an_active_plan_and_open_url_is_l1() {
        // Issue #555: "disconnected" Web is the missing consent, an expired trial is refused for
        // the reads (they exist for the chat, which needs agent execution), and open_url is a
        // confirmable local effect.
        let no_consent = ctx(ConnState::Disconnected, false);
        assert_eq!(
            authorize_op(Service::Web, "page", &no_consent),
            OpDecision::Denied(DenyReason::ConsentRequired)
        );
        let consented = ctx(connected(), false);
        assert_eq!(authorize_op(Service::Web, "page", &consented), OpDecision::Background);
        assert_eq!(
            authorize_op(Service::Web, "video_transcript", &consented),
            OpDecision::Background
        );
        assert_eq!(
            authorize_op(Service::Web, "open_url", &consented),
            OpDecision::RequiresLevel(Level::L1)
        );
        let standard = OpContext {
            plan: shogun_agents::entitlement::entitlements(
                shogun_agents::entitlement::Plan::Standard,
                0,
            ),
            ..consented
        };
        assert_eq!(authorize_op(Service::Web, "page", &standard), OpDecision::Background);
        let expired = OpContext {
            plan: shogun_agents::entitlement::entitlements(
                shogun_agents::entitlement::Plan::Trial { started_at_ms: Some(0) },
                shogun_agents::entitlement::TRIAL_DURATION_MS,
            ),
            ..consented
        };
        assert_eq!(
            authorize_op(Service::Web, "page", &expired),
            OpDecision::Denied(DenyReason::PlanNotEntitled)
        );
    }

    fn connected() -> ConnState {
        ConnState::Connected { last_sync_ms: 1_000 }
    }
    fn amber() -> ConnState {
        ConnState::NeedsReauth { reason: ReauthReason::TokenExpired, last_sync_ms: 500 }
    }
    fn ctx(conn: ConnState, draft_stop: bool) -> OpContext {
        // Consent granted by default so the pre-existing tests keep exercising the layer they
        // were written for; the consent gate has its own tests below.
        OpContext {
            highest_released: Wave::One,
            conn,
            draft_stop,
            composio_consent: true,
            plan: shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
        }
    }

    #[test]
    fn unreleased_wave_is_denied() {
        // Slack is Wave 2; with only Wave 1 released it is unreachable even for a read.
        let d = authorize_op(Service::Slack, "read_sync", &ctx(connected(), false));
        assert_eq!(d, OpDecision::Denied(DenyReason::UnreleasedWave));
    }

    #[test]
    fn unknown_and_not_implemented_ops_are_denied() {
        assert_eq!(
            authorize_op(Service::Gmail, "nope", &ctx(connected(), false)),
            OpDecision::Denied(DenyReason::UnknownOp)
        );
        assert_eq!(
            authorize_op(Service::Gmail, "delete_trash", &ctx(connected(), false)),
            OpDecision::Denied(DenyReason::NotImplemented)
        );
    }

    #[test]
    fn disconnected_denies_everything() {
        let d = authorize_op(Service::Gmail, "read_sync", &ctx(ConnState::Disconnected, false));
        assert_eq!(d, OpDecision::Denied(DenyReason::NotConnected));
    }

    #[test]
    fn amber_serves_reads_but_not_writes() {
        // read on amber → still allowed (cached), a draft write → denied
        assert_eq!(authorize_op(Service::Gmail, "read_sync", &ctx(amber(), false)), OpDecision::Background);
        assert_eq!(
            authorize_op(Service::Gmail, "draft_create_update", &ctx(amber(), false)),
            OpDecision::Denied(DenyReason::NeedsReauth)
        );
    }

    /// Issue #319 (FR-TG-23): draft-stop gates *sends only*. The label-sync / archive rows are
    /// L2 service-state changes and pass with draft-stop ON; they still need an entitled plan.
    #[test]
    fn draft_stop_does_not_touch_label_sync_or_archive() {
        for op in ["label_sync", "archive", "unarchive"] {
            assert_eq!(
                authorize_op(Service::Gmail, op, &ctx(connected(), true)),
                OpDecision::RequiresLevel(Level::L2),
                "{op} must be L2 with draft-stop ON"
            );
            assert_eq!(
                authorize_op(Service::Gmail, op, &ctx(connected(), false)),
                OpDecision::RequiresLevel(Level::L2)
            );
            assert!(requires_traceability(Service::Gmail, op), "{op} egresses and must trace");
        }
        // Standard plan: reads and the L2 label writes both (agent execution is Standard since
        // 2026-09-24); an expired trial has neither.
        let mut standard = ctx(connected(), false);
        standard.plan = shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Standard, 0);
        assert_eq!(
            authorize_op(Service::Gmail, "archive", &standard),
            OpDecision::RequiresLevel(Level::L2)
        );
        assert_eq!(authorize_op(Service::Gmail, "read_sync", &standard), OpDecision::Background);
        let mut expired = ctx(connected(), false);
        expired.plan = shogun_agents::entitlement::entitlements(
            shogun_agents::entitlement::Plan::Trial { started_at_ms: Some(0) },
            shogun_agents::entitlement::TRIAL_DURATION_MS,
        );
        assert_eq!(
            authorize_op(Service::Gmail, "archive", &expired),
            OpDecision::Denied(DenyReason::PlanNotEntitled)
        );
    }

    #[test]
    fn gmail_send_is_l3_and_draft_stop_blocks_it() {
        // Draft-only OFF allows the dedicated L3 confirmation.
        assert_eq!(
            authorize_op(Service::Gmail, "send", &ctx(connected(), false)),
            OpDecision::RequiresLevel(Level::L3)
        );
        // draft-stop ON → send blocked entirely
        assert_eq!(
            authorize_op(Service::Gmail, "send", &ctx(connected(), true)),
            OpDecision::Denied(DenyReason::DraftStop)
        );
    }

    #[test]
    fn draft_stop_does_not_block_reads_or_drafts_or_calendar() {
        // draft-stop only blocks the Gmail send; everything else is unaffected.
        assert_eq!(authorize_op(Service::Gmail, "read_sync", &ctx(connected(), true)), OpDecision::Background);
        assert_eq!(
            authorize_op(Service::Gmail, "draft_create_update", &ctx(connected(), true)),
            OpDecision::RequiresLevel(Level::L2)
        );
        assert_eq!(
            authorize_op(Service::GoogleCalendar, "event_create", &ctx(connected(), true)),
            OpDecision::RequiresLevel(Level::L3)
        );
    }

    #[test]
    fn gmail_without_consent_is_denied_everything_reads_included() {
        // FR-C2-02 / issue #80 §2-A0: Gmail runs entirely through Composio, so with no
        // three-disclosure consent nothing crosses the boundary — a read-sync must not slip
        // through as Background.
        let no_consent = OpContext { composio_consent: false, ..ctx(connected(), false) };
        for op in ["read_sync", "draft_create_update", "send"] {
            assert_eq!(
                authorize_op(Service::Gmail, op, &no_consent),
                OpDecision::Denied(DenyReason::ConsentRequired),
                "{op} must be consent-denied"
            );
        }
    }

    #[test]
    fn consent_gate_touches_only_gmail() {
        // Calendar is first-layer (direct MCP) — the Composio consent has no bearing on it.
        let no_consent = OpContext { composio_consent: false, ..ctx(connected(), false) };
        assert_eq!(
            authorize_op(Service::GoogleCalendar, "read_sync", &no_consent),
            OpDecision::Background
        );
        assert_eq!(
            authorize_op(Service::GoogleCalendar, "event_create", &no_consent),
            OpDecision::RequiresLevel(Level::L3)
        );
    }

    #[test]
    fn consent_denial_wins_over_draft_stop_and_plan() {
        // Order check: the refusal names the outermost boundary the user has not crossed —
        // consent — not a setting (draft-stop) or billing state further inside it.
        use shogun_agents::entitlement::{entitlements, Plan};
        let no_consent_standard_stop = OpContext {
            highest_released: Wave::One,
            conn: connected(),
            draft_stop: true,
            composio_consent: false,
            plan: entitlements(Plan::Standard, 0),
        };
        assert_eq!(
            authorize_op(Service::Gmail, "send", &no_consent_standard_stop),
            OpDecision::Denied(DenyReason::ConsentRequired)
        );
    }

    #[test]
    fn calendar_event_is_l3() {
        assert_eq!(
            authorize_op(Service::GoogleCalendar, "event_create", &ctx(connected(), false)),
            OpDecision::RequiresLevel(Level::L3)
        );
    }

    #[test]
    fn standard_plan_reads_writes_and_queues_sends_but_composio_sending_stays_pro() {
        // Boundary since 2026-09-24: Standard keeps first-layer reads (Gmail read via Composio
        // included) and has agent execution — drafts and L3 sends reach their level gates. The
        // one thing it lacks is the Composio send unlock: a Gmail send routes to Composio only
        // with `composio_send_unlock`, which is Pro.
        use shogun_agents::entitlement::{entitlements, Plan};
        let standard = OpContext {
            highest_released: Wave::One,
            conn: connected(),
            draft_stop: false,
            composio_consent: true,
            plan: entitlements(Plan::Standard, 0),
        };
        assert_eq!(authorize_op(Service::Gmail, "read_sync", &standard), OpDecision::Background);
        assert_eq!(
            authorize_op(Service::Gmail, "draft_create_update", &standard),
            OpDecision::RequiresLevel(Level::L2)
        );
        assert_eq!(
            authorize_op(Service::Gmail, "send", &standard),
            OpDecision::Denied(DenyReason::PlanNotEntitled),
            "real sending through Composio is the Pro unlock"
        );
        assert_eq!(
            authorize_op(Service::GoogleCalendar, "event_create", &standard),
            OpDecision::RequiresLevel(Level::L3)
        );
    }

    #[test]
    fn expired_trial_is_denied_reads_too() {
        // Trial後は全員課金: an expired trial has no active plan — even the background read-sync
        // stops (it spends the Select KK lane). Local capture/search are not this gate's concern.
        use shogun_agents::entitlement::{entitlements, Plan, TRIAL_DURATION_MS};
        let expired = OpContext {
            highest_released: Wave::One,
            conn: connected(),
            draft_stop: false,
            composio_consent: true,
            plan: entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS),
        };
        for op in ["read_sync", "draft_create_update", "send"] {
            assert_eq!(
                authorize_op(Service::Gmail, op, &expired),
                OpDecision::Denied(DenyReason::PlanNotEntitled),
                "{op} must be plan-denied when the trial has expired"
            );
        }
    }

    #[test]
    fn draft_stop_denial_still_wins_over_plan_denial_for_gmail_send() {
        // Order check: with draft-stop ON, the send reports DraftStop regardless of plan — the
        // stricter, user-set safety gate is never masked by the billing state.
        use shogun_agents::entitlement::{entitlements, Plan};
        let standard_stop = OpContext {
            highest_released: Wave::One,
            conn: connected(),
            draft_stop: true,
            composio_consent: true,
            plan: entitlements(Plan::Standard, 0),
        };
        assert_eq!(
            authorize_op(Service::Gmail, "send", &standard_stop),
            OpDecision::Denied(DenyReason::DraftStop)
        );
    }

    #[test]
    fn no_send_is_ever_auto_run_invariant_4() {
        // Across every Wave-1 op, an allowed decision for an ExternalSend/Composio op must be L3 or
        // Composio — never Background or a sub-L3 confirm.
        for service in [Service::Gmail, Service::GoogleCalendar] {
            for op in scope::scope(service).ops {
                let d = authorize_op(service, op.name, &ctx(connected(), false));
                if op.class == OpClass::ExternalSend {
                    match d {
                        OpDecision::RequiresLevel(l) => assert_eq!(l, Level::L3, "{} send below L3", op.name),
                        OpDecision::RequiresComposio | OpDecision::Denied(_) => {}
                        OpDecision::Background => panic!("send {} auto-ran (invariant 4)", op.name),
                    }
                }
            }
        }
    }

    #[test]
    fn traceability_required_for_writes_not_reads() {
        assert!(requires_traceability(Service::Gmail, "send"));
        assert!(requires_traceability(Service::Gmail, "draft_create_update"));
        assert!(requires_traceability(Service::GoogleCalendar, "event_create"));
        assert!(!requires_traceability(Service::Gmail, "read_sync"));
    }
}
