//! Cross-crate invariant-4 guard: **an L1 (auto-executed) path can never reach an external send.**
//!
//! Each crate tests its own slice of this (permission levels, preset tables, MCP scope, Composio).
//! This integration test asserts the guarantee holds *across the whole surface at once*, so a future
//! change that quietly lets a send through at L1 in any one layer fails here even if that crate's
//! own tests were adjusted. shogun-mcp is the natural host: it can see the agents permission model,
//! the preset tables, its own scope table, and the Composio gate together.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use shogun_agents::entitlement::{entitlements, Plan, TRIAL_DURATION_MS};
use shogun_agents::permission::{Action, Level, LocalAction, SendAction};
use shogun_agents::presets::{OpKind, PRESETS};
use shogun_mcp::composio::{grant_consent, ComposioSender, Disclosures, GmailSend};
use shogun_mcp::scope::{authorize, Authorization, Service, ALL_SERVICES};

/// permission model: every send is L3, no local action is a send, no local action is L3.
#[test]
fn permission_model_keeps_sends_at_l3() {
    let sends = [
        SendAction::SendEmail { to: "a@b.com".into() },
        SendAction::PostMessage { channel: "#x".into() },
        SendAction::CreateCalendarEvent { title: "t".into() },
        SendAction::PostComment { target: "r#1".into() },
    ];
    for s in sends {
        let a = Action::Send(s);
        assert_eq!(a.required_level(), Level::L3);
        assert!(!a.is_l1_eligible());
        assert!(a.is_external_send());
    }

    let locals = [
        LocalAction::OpenApp { bundle_id: "x".into() },
        LocalAction::LocalSearch { query: "q".into() },
        LocalAction::SaveDraft { target: "reply" },
        LocalAction::CopyToClipboard { text: "c".into() },
        LocalAction::UpdateState { table: "people", state_id: 1 },
        // #279: drafts a reply and stops. Never a send (FR-TL-14).
        LocalAction::ReplyDraft { language: "ja".into() },
        // #397: a file into a user-chosen folder — L2, local, never a send.
        LocalAction::WriteFile {
            path_in_allowed_root: "/tmp/x.md".into(),
            format: shogun_agents::permission::ExportFormat::Markdown,
            body: String::new(),
        },
    ];
    for l in locals {
        let a = Action::Local(l);
        assert_ne!(a.required_level(), Level::L3, "no local action is L3");
        assert!(!a.is_external_send(), "a local action is never a send");
    }
}

/// preset tables: every ExternalSend op is L3, and no L1 op is a send.
#[test]
fn preset_sends_are_all_l3() {
    for p in PRESETS {
        for op in p.operations {
            if op.kind == OpKind::ExternalSend {
                assert_eq!(op.level, Level::L3, "{}::{} send must be L3", p.name, op.name);
            }
            if op.level == Level::L1 {
                assert!(!op.kind.is_external_send(), "{}::{} L1 must not be a send", p.name, op.name);
            }
        }
    }
}

/// MCP scope tables: no external-send operation is ever authorized below L3 (L3, Composio, or
/// not-implemented only) — never Background, never L1/L2.
#[test]
fn mcp_scope_never_authorizes_a_send_below_l3() {
    for &service in ALL_SERVICES {
        for op in shogun_mcp::scope::scope(service).ops {
            if op.class.is_external_send() {
                match authorize(service, op.name) {
                    Authorization::RequiresLevel(Level::L3)
                    | Authorization::RequiresComposio
                    | Authorization::DeniedNotImplemented => {}
                    other => panic!("{service:?}::{} send authorized as {other:?} (must be L3)", op.name),
                }
            }
        }
    }
    // Direct Gmail still requires explicit L3 approval.
    assert_eq!(
        authorize(Service::Gmail, "send"),
        Authorization::RequiresLevel(Level::L3)
    );
}

/// Issue #319: the Gmail label / archive rows are `ServiceStateChange` at L2 — never a send,
/// never L1 — and the matching `ServiceStateAction`s carry the same level by construction. A
/// standing approval (FR-TG-21) cannot lower this: the level is a property of the action.
#[test]
fn mail_label_and_archive_are_l2_service_state_changes_not_sends() {
    use shogun_agents::permission::ServiceStateAction;
    use shogun_mcp::scope::OpClass;
    for name in ["label_sync", "archive", "unarchive"] {
        let op = shogun_mcp::scope::lookup(Service::Gmail, name).expect("scope row");
        assert_eq!(op.class, OpClass::ServiceStateChange, "{name}");
        assert!(!op.class.is_external_send(), "{name} is not a send");
        assert_eq!(authorize(Service::Gmail, name), Authorization::RequiresLevel(Level::L2), "{name}");
    }
    // Delete / trash / spam stay unimplemented.
    assert_eq!(authorize(Service::Gmail, "delete_trash"), Authorization::DeniedNotImplemented);
    for a in [
        ServiceStateAction::ArchiveThread { thread_id: "t".into() },
        ServiceStateAction::UnarchiveThread { thread_id: "t".into() },
        ServiceStateAction::ModifyLabels { thread_id: "t".into(), add: vec!["L".into()], remove: vec![] },
    ] {
        let scope_op = a.scope_op();
        let action = Action::ServiceState(a);
        assert_eq!(action.required_level(), Level::L2);
        assert!(!action.is_l1_eligible());
        assert!(!action.is_external_send());
        assert_eq!(authorize(Service::Gmail, scope_op), Authorization::RequiresLevel(Level::L2));
    }
}

/// Composio gate: a prepared send is always an L3 send, and the send path is unreachable while
/// draft-stop is ON (the default) — no capability, so prepare_send cannot be called.
#[test]
fn composio_send_is_l3_and_gated_by_draft_stop() {
    let consent =
        grant_consent(Disclosures { via_third_party: true, data_types: true, revocable: true }).unwrap();
    let mut sender = ComposioSender::new(consent);

    let pro = entitlements(Plan::Pro, 0);
    // default draft-stop ON → no send capability exists, even for Pro.
    assert!(sender.draft_stop());
    assert!(sender.send_capability(&pro).is_none());

    // turning it off yields a capability (on an entitled plan); the prepared action is an L3 send.
    sender.set_draft_stop(false);
    let cap = sender.send_capability(&pro).expect("capability once draft-stop is off");
    let (action, preview) = shogun_mcp::composio::prepare_send(
        cap,
        GmailSend { to: "z@y.com".into(), subject: "s".into(), body: "b".into() },
    );
    assert!(matches!(action, SendAction::SendEmail { .. }));
    assert_eq!(Action::Send(action).required_level(), Level::L3);
    assert_eq!(preview.route, shogun_agents::approval::Route::ViaComposio);
}

/// **An L3 send submitted through the MCP or REST face lands in the ONE shared approval queue**
/// — the same queue the confirm UI drains — labeled with its origin, and is resolvable (confirm /
/// reject) from that queue regardless of origin. A face that constructed a private queue would
/// strand sends no UI ever drains; the constructor injection this exercises prevents that.
#[test]
fn api_and_mcp_sends_land_in_the_one_shared_queue_with_origins() {
    use std::sync::{Arc, Mutex};

    use shogun_agents::approval::{
        ApprovalId, ApprovalOrigin, ApprovalQueue, ConfirmIntent, Decision, RejectCause,
    };
    use shogun_mcp::backend::StubBackend;
    use shogun_mcp::mcp::McpServer;
    use shogun_mcp::rest;

    // The one process-wide queue (what the desktop manages at startup / a bin creates at its
    // composition root). Both faces below receive THIS queue, not their own.
    let shared: Arc<Mutex<ApprovalQueue>> = Arc::new(Mutex::new(ApprovalQueue::new()));

    // 1. MCP face: actions.execute with a send → 202-equivalent pending, enqueued in `shared`.
    // Both faces are told a confirm UI exists — that is the premise of this test (the desktop
    // hosting them). A headless process refuses the send instead; see the test below.
    let server = McpServer::new(
        StubBackend,
        shared.clone(),
        || 1_000,
        || shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
    )
    .with_approval_surface(rest::ApprovalSurface::Present);
    let resp = server
        .handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"actions.execute","arguments":{"kind":"send_email","to":"a@b.com","subject":"s","body":"mcp draft"}}}"#,
        )
        .expect("a response line");
    // The tool result is a JSON string inside the JSON-RPC envelope, so the inner quotes arrive
    // escaped on the wire.
    assert!(resp.contains(r#"\"pending\":true"#), "MCP send must be pending, never executed: {resp}");
    assert!(resp.contains(r#"\"origin\":\"mcp\""#), "the MCP face labels its origin: {resp}");

    // 2. REST face: the same shared queue, Api origin.
    let (status, body) = {
        let mut q = shared.lock().expect("queue");
        rest::act(
            Some(r#"{"kind":"send_email","to":"b@c.com","subject":"s","body":"api draft"}"#),
            1_000,
            &mut q,
            ApprovalOrigin::Api,
            rest::ApprovalSurface::Present,
        )
    };
    assert_eq!(status, 202);
    assert!(body.contains("\"origin\":\"api\""), "{body}");

    // 3. One listing (what the UI's list_approvals drains) shows BOTH, each with its origin.
    let (mcp_id, api_id) = {
        let q = shared.lock().expect("queue");
        let ids = q.pending_ids();
        assert_eq!(ids.len(), 2, "both faces enqueued into the same queue");
        assert_eq!(q.origin(ids[0]), Some(ApprovalOrigin::Mcp));
        assert_eq!(q.origin(ids[1]), Some(ApprovalOrigin::Api));
        // full-content previews are present for the confirm UI (FR-AG-03)
        assert_eq!(q.preview(ids[0]).expect("preview").full_body, "Subject: s\n\nmcp draft");
        (ids[0], ids[1])
    };

    // 4. The UI-side confirm/reject operates on them identically, regardless of origin.
    {
        let mut q = shared.lock().expect("queue");
        assert!(matches!(
            q.confirm(mcp_id, ConfirmIntent::DedicatedButton, 2_000),
            Decision::Confirmed(cs) if cs.preview.full_body == "Subject: s\n\nmcp draft"
        ));
        assert_eq!(
            q.reject(api_id, RejectCause::UserRejected, 2_000),
            Decision::Rejected(RejectCause::UserRejected)
        );
        assert_eq!(q.pending_len(), 0, "both resolved through the one queue");
    }

    // 5. L1-can-never-send stays intact on the same surface: a local action through the same
    //    faces executes locally and never touches the queue.
    let (status, body) = {
        let mut q = shared.lock().expect("queue");
        rest::act(Some(r#"{"kind":"local_search","query":"x"}"#), 1_000, &mut q, ApprovalOrigin::Api, rest::ApprovalSurface::Present)
    };
    // Issue #779: without a backend nothing runs, and the answer says so; still no queue entry.
    assert_eq!(status, 501, "{body}");
    assert!(!body.contains("executed"), "{body}");
    assert_eq!(shared.lock().expect("queue").pending_len(), 0, "a local action never enqueues");
    // and an unknown id is refused, not silently confirmed
    assert_eq!(
        shared.lock().expect("queue").confirm(ApprovalId(999), ConfirmIntent::DedicatedButton, 0),
        Decision::Unknown
    );
}

/// Issue #97 regression: **no send path bypasses BOTH the entitlement gate and the L3/consent
/// gates.** Every surface that could emit an external send is checked with the non-entitled plan
/// (an expired trial — Standard has agent execution since 2026-09-24) — each one must refuse
/// before any send exists — then with Standard, where a send may reach its L3 gate but never
/// Composio (the send unlock is Pro) — and then with an entitled plan, where the L3/consent
/// machinery must still be the only way through.
#[test]
fn no_send_path_bypasses_entitlement_or_l3_gates() {
    let locked_plans = [entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS)];
    let standard = entitlements(Plan::Standard, 0);
    let pro = entitlements(Plan::Pro, 0);

    for locked in &locked_plans {
        // 1. Execution engine: a send (or anything else) submitted on a locked plan is rejected —
        //    it never reaches the effector, let alone the network.
        use shogun_agents::engine::{Disposition, ExecutionEngine, RejectReason};
        struct NoEffect;
        impl shogun_agents::engine::LocalEffector for NoEffect {
            fn run(&self, _a: &Action) -> Result<(), String> {
                panic!("effector must never run on a locked plan");
            }
        }
        struct NoObs;
        impl shogun_agents::engine::ExecutionObserver for NoObs {
            fn on_executed(&self, _i: shogun_agents::engine::ActionId, _a: &Action) {}
            fn on_rejected(&self, _i: shogun_agents::engine::ActionId, _a: &Action, _r: &RejectReason) {}
            fn on_cancelled(&self, _i: shogun_agents::engine::ActionId, _a: &Action) {}
            fn on_expired(&self, _i: shogun_agents::engine::ActionId, _a: &Action) {}
            fn on_failed(&self, _i: shogun_agents::engine::ActionId, _a: &Action, _e: &str) {}
        }
        let mut engine = ExecutionEngine::new(NoEffect, NoObs, 5000);
        let sub = engine.submit(Action::Send(SendAction::SendEmail { to: "a@b.com".into() }), 0, locked);
        assert_eq!(sub.disposition, Disposition::Rejected(RejectReason::PlanNotEntitled));

        // 2. Memory API dispatch: a send on a locked plan is denied BEFORE the approval queue.
        use shogun_agents::approval::{ApprovalQueue, Preview, Route};
        use shogun_mcp::dispatch::{ActionOutcome, Denied, MemoryApi};
        use shogun_mcp::memory_api::TokenRegistry;
        let mut tokens = TokenRegistry::new();
        tokens.issue("t");
        let mut approvals = ApprovalQueue::new();
        let send = SendAction::SendEmail { to: "a@b.com".into() };
        let preview = Preview::for_send(&send, "body", Route::ViaComposio);
        {
            let mut api = MemoryApi::new(&tokens, &mut approvals, *locked);
            assert_eq!(
                api.submit_send(Some("t"), send, preview, 0),
                ActionOutcome::Denied(Denied::PlanNotEntitled)
            );
        }
        assert_eq!(approvals.pending_len(), 0, "nothing may be enqueued on a locked plan");

        // 3. Service gate: every external-send op of every service is plan-denied.
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::service_gate::{authorize_op, DenyReason, OpContext, OpDecision};
        use shogun_mcp::scope::Wave;
        let ctx = OpContext {
            highest_released: Wave::Three,
            conn: ConnState::Connected { last_sync_ms: 0 },
            draft_stop: false,
            composio_consent: true,
            plan: *locked,
        };
        for &service in ALL_SERVICES {
            for op in shogun_mcp::scope::scope(service).ops {
                if op.class.is_external_send() {
                    match authorize_op(service, op.name, &ctx) {
                        OpDecision::Denied(DenyReason::PlanNotEntitled)
                        | OpDecision::Denied(DenyReason::NotImplemented) => {}
                        other => panic!(
                            "{service:?}::{} allowed as {other:?} on a locked plan",
                            op.name
                        ),
                    }
                }
            }
        }

        // 4. Composio: even with full consent AND draft-stop OFF, a locked plan yields no
        //    capability — prepare_send is unreachable.
        let consent = grant_consent(Disclosures { via_third_party: true, data_types: true, revocable: true })
            .unwrap();
        let mut sender = ComposioSender::new(consent);
        sender.set_draft_stop(false);
        assert!(sender.send_capability(locked).is_none());
    }

    // Standard: agent execution, but no Composio send unlock and no Memory API. A send is at
    // most an L3 approval — never Background, L1 or L2 — and never a Composio capability.
    {
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::service_gate::{authorize_op, DenyReason, OpContext, OpDecision};
        use shogun_mcp::scope::Wave;
        let ctx = OpContext {
            highest_released: Wave::Three,
            conn: ConnState::Connected { last_sync_ms: 0 },
            draft_stop: false,
            composio_consent: true,
            plan: standard,
        };
        for &service in ALL_SERVICES {
            for op in shogun_mcp::scope::scope(service).ops {
                if op.class.is_external_send() {
                    match authorize_op(service, op.name, &ctx) {
                        OpDecision::RequiresLevel(Level::L3)
                        | OpDecision::Denied(DenyReason::PlanNotEntitled)
                        | OpDecision::Denied(DenyReason::NotImplemented) => {}
                        other => panic!(
                            "{service:?}::{} allowed as {other:?} on Standard",
                            op.name
                        ),
                    }
                }
            }
        }
        let consent = grant_consent(Disclosures { via_third_party: true, data_types: true, revocable: true })
            .unwrap();
        let mut sender = ComposioSender::new(consent);
        sender.set_draft_stop(false);
        assert!(sender.send_capability(&standard).is_none(), "Composio sending is the Pro unlock");
    }

    // And the converse: an entitled plan does NOT dissolve the older gates — consent and
    // draft-stop still decide, and the prepared send is still L3.
    let consent = grant_consent(Disclosures { via_third_party: true, data_types: true, revocable: true })
        .unwrap();
    let mut sender = ComposioSender::new(consent);
    assert!(sender.send_capability(&pro).is_none(), "draft-stop ON still blocks Pro");
    sender.set_draft_stop(false);
    let cap = sender.send_capability(&pro).expect("Pro + consent + draft-stop OFF");
    let (action, _preview) = shogun_mcp::composio::prepare_send(
        cap,
        GmailSend { to: "z@y.com".into(), subject: "s".into(), body: "b".into() },
    );
    assert_eq!(Action::Send(action).required_level(), Level::L3, "entitled send is still L3");
}

/// The model's edge (issue #81): whatever the connection, wave and plan state, the tools array
/// handed to Claude can never contain an operation that leaves the device.
///
/// This is the newest way the invariant could be broken — not by a gate that says yes, but by a
/// *definition* the model can call before any gate is consulted. The catalog filters through the
/// same `service_gate`, so this asserts the composition rather than the filter alone: for every
/// combination of state, every offered tool is a `Read` in the permission table.
#[test]
fn the_llm_tool_surface_only_exposes_a_send_as_a_proposal() {
    use shogun_mcp::connection::{ConnState, ReauthReason};
    use shogun_mcp::scope::{lookup, OpClass, Wave};
    use shogun_mcp::tool_catalog::{proposed_action, tool_definitions, ServiceState, ToolContext, ToolKind};

    let states = [
        ConnState::Disconnected,
        ConnState::Connected { last_sync_ms: 0 },
        ConnState::NeedsReauth { reason: ReauthReason::TokenExpired, last_sync_ms: 0 },
    ];
    let plans = [
        entitlements(Plan::Pro, 0),
        entitlements(Plan::Standard, 0),
        entitlements(Plan::Trial { started_at_ms: Some(0) }, 0),
        // Expired trial: the locked posture.
        entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS + 1),
    ];

    let mut offered_anything = false;
    for wave in [Wave::One, Wave::Two, Wave::Three] {
        for draft_stop in [true, false] {
            for plan in plans {
                for conn in states {
                    let services: Vec<ServiceState> = ALL_SERVICES
                        .iter()
                        .copied()
                        .map(|service| ServiceState { service, conn })
                        .collect();
                    let ctx = ToolContext { highest_released: wave, draft_stop, plan };
                    for tool in tool_definitions(&services, &ctx) {
                        offered_anything = true;
                        let name = tool["name"].as_str().expect("tool name");
                        let entry = shogun_mcp::tool_catalog::catalog_entry(name)
                            .expect("every offered tool is a catalog entry");
                        let class = lookup(entry.service, entry.scope_op).map(|o| o.class);
                        match entry.kind {
                            // A read must be a read in the table: no send may be published as if
                            // it returned data.
                            ToolKind::Read => assert_eq!(
                                class,
                                Some(OpClass::Read),
                                "tool {name} is published as a read but is not one",
                            ),
                            // A local effect (issue #555 `open_url`) never leaves the device:
                            // its table row is a LocalAction, never a send, and it builds no
                            // action for the approval queue.
                            ToolKind::Local => {
                                assert_eq!(
                                    class,
                                    Some(OpClass::LocalAction),
                                    "tool {name} is published as a local effect but is not one",
                                );
                                assert!(
                                    proposed_action(entry, &serde_json::json!({ "url": "https://x.com/" })).is_none(),
                                    "tool {name} is a local effect that builds an action",
                                );
                            }
                            // A proposal must produce an L3 action — the thing that makes it
                            // impossible to auto-run (invariant 4).
                            ToolKind::Propose => {
                                assert!(
                                    class.is_some_and(OpClass::is_external_send),
                                    "tool {name} proposes something that is not an external send",
                                );
                                let action = proposed_action(
                                    entry,
                                    &serde_json::json!({
                                        "to": "a@b.com", "title": "t", "channel": "#c",
                                        "target": "x", "body": "b"
                                    }),
                                )
                                .expect("a proposal tool must build an action");
                                assert!(action.is_external_send(), "{name} is not a send");
                                assert_eq!(action.required_level(), Level::L3, "{name} is not L3");
                            }
                        }
                        // And the gate agrees it is allowed — the array is a subset of what the
                        // gate would permit, never a superset.
                        assert!(
                            shogun_mcp::service_gate::authorize_op(
                                entry.service,
                                entry.scope_op,
                                &shogun_mcp::service_gate::OpContext {
                                    highest_released: wave,
                                    conn,
                                    draft_stop,
                                    // The tool layer folds a missing Gmail consent into
                                    // Disconnected before the catalog sees it, so the sweep's
                                    // `conn` axis already covers the consent axis.
                                    composio_consent: true,
                                    plan,
                                },
                            )
                            .is_allowed(),
                            "tool {name} was offered but the gate denies it",
                        );
                    }
                }
            }
        }
    }
    assert!(offered_anything, "the sweep must actually offer tools somewhere, or it proves nothing");
}

/// The conversation loop's edge (issue #81 step 2): whatever the model asks for, and whatever the
/// state, the only thing that can reach a service through the loop is a read.
///
/// The previous test proves the model is never *offered* a send. This one proves it cannot get one
/// by asking anyway — the loop resolves every name through the catalog and the gate before a
/// runner is touched, so a hallucinated or stale tool name is refused rather than executed.
#[test]
fn the_conversation_loop_can_only_reach_read_operations() {
    use serde_json::{json, Value};
    use shogun_mcp::connection::{ConnState, ReauthReason};
    use shogun_mcp::scope::{lookup, OpClass, Wave};
    use shogun_mcp::tool_catalog::{ServiceState, ToolContext};
    use shogun_mcp::tool_loop::{
        run_read_loop, LoopLimits, ModelTurn, ModelTurnSource, ProposalSink, ReadToolRunner,
        ToolResult, ToolRunError, ToolUse,
    };

    /// Asks for every name we can think of — published tools, every scope op name in the table
    /// (including sends), and pure inventions — then answers.
    struct AsksForEverything {
        asked: bool,
        /// Drive the async-results path (issue #396) as well as the lock-step one: both share
        /// one classifier, and this proves it.
        async_results: bool,
    }
    impl ModelTurnSource for AsksForEverything {
        fn supports_async_results(&self) -> bool {
            self.async_results
        }
        fn next_turn(&mut self, _r: &[ToolResult]) -> Result<ModelTurn, String> {
            if self.asked {
                return Ok(ModelTurn::Final("done".into()));
            }
            self.asked = true;
            let mut uses: Vec<ToolUse> = Vec::new();
            let mut push = |name: String| {
                uses.push(ToolUse { id: format!("t{}", uses.len()), name, input: json!({}) })
            };
            for service in ALL_SERVICES {
                for op in shogun_mcp::scope::scope(*service).ops {
                    push(op.name.to_string());
                }
            }
            for invented in ["send_email", "delete_everything", "create_event", "post_message"] {
                push(invented.to_string());
            }
            for published in [
                "list_calendar_events",
                "check_calendar_availability",
                "search_mail",
                "get_mail_thread",
                "list_recent_drive_files",
                "read_drive_file",
            ] {
                push(published.to_string());
            }
            Ok(ModelTurn::ToolUses(uses))
        }
    }

    /// Records everything that got through to "a service".
    struct Recorder {
        reached: std::sync::Mutex<Vec<(Service, &'static str)>>,
    }

    /// Records every action that reached the approval queue. Nothing here executes.
    #[derive(Default)]
    struct Proposals {
        queued: Vec<Action>,
    }
    impl ProposalSink for Proposals {
        fn propose(&mut self, action: Action, _body: &str) -> Result<(), String> {
            self.queued.push(action);
            Ok(())
        }
    }
    impl ReadToolRunner for Recorder {
        fn run(
            &self,
            service: Service,
            scope_op: &'static str,
            _input: &Value,
            _timeout_ms: u64,
        ) -> Result<String, ToolRunError> {
            self.reached.lock().unwrap().push((service, scope_op));
            Ok(String::new())
        }
    }

    let states = [
        ConnState::Connected { last_sync_ms: 0 },
        ConnState::NeedsReauth { reason: ReauthReason::TokenExpired, last_sync_ms: 0 },
        ConnState::Disconnected,
    ];
    let plans = [
        entitlements(Plan::Pro, 0),
        entitlements(Plan::Standard, 0),
        entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS + 1),
    ];

    let mut reached_anything = false;
    for wave in [Wave::One, Wave::Two, Wave::Three] {
        for draft_stop in [true, false] {
            for plan in plans {
                for (conn, async_results) in
                    states.into_iter().flat_map(|c| [(c, false), (c, true)])
                {
                    let services: Vec<ServiceState> = ALL_SERVICES
                        .iter()
                        .copied()
                        .map(|service| ServiceState { service, conn })
                        .collect();
                    let ctx = ToolContext { highest_released: wave, draft_stop, plan };
                    let mut runner = Recorder { reached: std::sync::Mutex::new(Vec::new()) };
                    let mut proposals = Proposals::default();
                    // A model that asks for everything, including sends, by name.
                    let _ = run_read_loop(
                        &mut AsksForEverything { asked: false, async_results },
                        &mut runner,
                        &mut proposals,
                        &services,
                        &ctx,
                        LoopLimits::default(),
                    );
                    // Anything that became a proposal is a send, is L3, and was never executed.
                    for action in &proposals.queued {
                        assert!(action.is_external_send(), "a non-send was queued: {action:?}");
                        assert_eq!(action.required_level(), Level::L3, "{action:?} is not L3");
                    }
                    for (service, op) in runner.reached.lock().unwrap().iter() {
                        reached_anything = true;
                        assert_eq!(
                            lookup(*service, op).map(|o| o.class),
                            Some(OpClass::Read),
                            "the loop let {op} on {service:?} reach a service",
                        );
                    }
                }
            }
        }
    }
    assert!(reached_anything, "the sweep must actually reach a service, or it proves nothing");
}

/// Issue #484: a subscription delegate (Claude Code, Codex) reaches SHOGUN's
/// tools only through the chat bridge, and every call it makes there is decided by the same loop. A send it asks for — even
/// by a name that is a real scope op — becomes an L3 item in the approval queue and nothing runs
/// it; only reads reach a service.
///
/// Driven end to end over a real socket: the CLI side is the `--chat-bridge` stdio session
/// (`chat_bridge::serve_stdio`), the desktop side the run's `ChatBridgeHost`, and between them
/// `run_read_loop` with a minimal source that turns each bridged call into a `tool_use` (the
/// production source is `shogun_core::llm::delegate_tools`, which cannot be a dependency here).
#[cfg(unix)]
#[test]
fn a_send_asked_for_through_the_chat_bridge_is_only_ever_an_l3_proposal() {
    use serde_json::{json, Value};
    use shogun_mcp::chat_bridge::{self, BridgeCall, ChatBridgeHost};
    use shogun_mcp::connection::ConnState;
    use shogun_mcp::scope::{lookup, OpClass, Wave};
    use shogun_mcp::tool_catalog::{tool_definitions, ServiceState, ToolContext};
    use shogun_mcp::tool_loop::{
        run_read_loop, LoopLimits, ModelTurn, ModelTurnSource, ProposalSink, ReadToolRunner,
        ToolResult, ToolRunError, ToolUse,
    };
    use std::sync::mpsc;

    enum Ev {
        Call(BridgeCall),
        Done,
    }

    /// The CLI as the loop's model: one bridged call per turn, the end of the session as the
    /// final answer.
    struct BridgeModel {
        rx: mpsc::Receiver<Ev>,
        pending: Vec<(String, BridgeCall)>,
        n: u32,
    }
    impl ModelTurnSource for BridgeModel {
        fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
            for r in results {
                if let Some(at) = self.pending.iter().position(|(id, _)| *id == r.id) {
                    let (_, call) = self.pending.remove(at);
                    call.reply(r.content.clone(), r.is_error);
                }
            }
            match self.rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Ev::Call(call)) => {
                    self.n += 1;
                    let id = format!("b{}", self.n);
                    let use_ = ToolUse { id: id.clone(), name: call.name.clone(), input: call.arguments.clone() };
                    self.pending.push((id, call));
                    Ok(ModelTurn::ToolUses(vec![use_]))
                }
                Ok(Ev::Done) => Ok(ModelTurn::Final("done".into())),
                Err(_) => Err("bridge went quiet".into()),
            }
        }
    }

    #[derive(Default)]
    struct Recorder {
        reached: std::sync::Mutex<Vec<(Service, &'static str)>>,
    }
    impl ReadToolRunner for Recorder {
        fn run(&self, service: Service, op: &'static str, _i: &Value, _t: u64) -> Result<String, ToolRunError> {
            self.reached.lock().unwrap().push((service, op));
            Ok("data".into())
        }
    }
    #[derive(Default)]
    struct Queue {
        queued: Vec<Action>,
    }
    impl ProposalSink for Queue {
        fn propose(&mut self, action: Action, _body: &str) -> Result<(), String> {
            self.queued.push(action);
            Ok(())
        }
    }

    let services = vec![ServiceState { service: Service::GoogleCalendar, conn: ConnState::Connected { last_sync_ms: 0 } }];
    for draft_stop in [true, false] {
        let ctx = ToolContext { highest_released: Wave::One, draft_stop, plan: entitlements(Plan::Pro, 0) };
        let dir = std::env::temp_dir().join(format!("inv4-{}-{draft_stop}", std::process::id()));
        let path = dir.join("b.sock");
        let (tx, rx) = mpsc::channel::<Ev>();
        let host = ChatBridgeHost::start_into(
            &path,
            chat_bridge::mcp_tool_list(&tool_definitions(&services, &ctx)),
            tx.clone(),
            Ev::Call,
        )
        .unwrap();

        // The CLI: list, ask for a proposal, a raw send op by name, an invented send, and a read —
        // one after another, as a model waiting on each result would.
        let calls = [
            json!({ "name": "propose_calendar_event", "arguments": { "title": "Vendor sync", "body": "Tue 3pm" } }),
            json!({ "name": "create_event", "arguments": { "title": "x" } }),
            json!({ "name": "send_email", "arguments": { "to": "a@b.com", "body": "hi" } }),
            json!({ "name": "list_calendar_events", "arguments": {} }),
        ];
        // Claude Code's bridge process reads the socket and nonce from its env; Codex's
        // from the bridge file in its working directory. One pass each, same outcome.
        let (socket, nonce) = if draft_stop {
            (path.clone(), host.nonce().to_string())
        } else {
            let cfg = json!({ "socket": path.to_str().unwrap(), "nonce": host.nonce() }).to_string();
            std::fs::write(dir.join("shogun-chat-bridge.json"), cfg).unwrap();
            chat_bridge::read_config_file(&dir, "shogun-chat-bridge.json").unwrap()
        };
        let cli = std::thread::spawn(move || {
            let mut replies = Vec::new();
            for (i, params) in calls.iter().enumerate() {
                let line = json!({ "jsonrpc": "2.0", "id": i + 1, "method": "tools/call", "params": params });
                let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
                #[derive(Clone)]
                struct W(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().unwrap().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                chat_bridge::serve_stdio(&socket, &nonce, line.to_string().as_bytes(), W(out.clone())).unwrap();
                let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
                replies.push(serde_json::from_str::<Value>(text.trim()).unwrap());
            }
            let _ = tx.send(Ev::Done);
            replies
        });

        let mut model = BridgeModel { rx, pending: Vec::new(), n: 0 };
        let mut runner = Recorder::default();
        let mut queue = Queue::default();
        let out = run_read_loop(&mut model, &mut runner, &mut queue, &services, &ctx, LoopLimits::default()).unwrap();
        let replies = cli.join().unwrap();
        drop(host);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(out.answer, "done");
        // The one proposal is an L3 send in the queue; nothing executed it.
        assert_eq!(queue.queued.len(), 1, "draft_stop={draft_stop}");
        for action in &queue.queued {
            assert!(action.is_external_send());
            assert_eq!(action.required_level(), Level::L3);
        }
        assert!(replies[0]["result"]["content"][0]["text"].as_str().unwrap().contains("approval"));
        // The raw send names were refused before any runner was touched.
        assert_eq!(replies[1]["result"]["isError"], true);
        assert_eq!(replies[2]["result"]["isError"], true);
        assert_eq!(out.refused, 2);
        // Only the read reached a service.
        let reached = runner.reached.lock().unwrap().clone();
        assert_eq!(reached.len(), 1);
        for (service, op) in reached {
            assert_eq!(lookup(service, op).map(|o| o.class), Some(OpClass::Read));
        }
    }
}
