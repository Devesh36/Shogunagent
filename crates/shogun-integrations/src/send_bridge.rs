//! Confirmed-send → first-layer MCP op bridge (WP-F, §6.14).
//!
//! The approval queue hands back a [`SendAction`]; this module decides **where it goes** — which
//! first-layer service + scope-table op performs it, with what arguments — or that it is the
//! second layer's job. The second-layer decision is **service-scoped**: all of Gmail is Composio's
//! (§6.10 / issue #80 §2-A0), not just one operation. Pure and exhaustive over the [`SendAction`]
//! variants, so the routing is Linux-testable and a new variant is a compile error here rather
//! than a silently unroutable send.
//!
//! The gate is NOT applied here: the caller executes via
//! [`crate::runtime::ConnectorRuntime::execute_write`], which re-checks
//! [`shogun_mcp::service_gate::authorize_op`] (the WP-F double gate).

use serde_json::{json, Value};
use shogun_agents::permission::SendAction;
use shogun_mcp::scope::Service;

/// Where a confirmed send is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRoute {
    /// A first-layer MCP write: this service's scope-table op.
    FirstLayer { service: Service, op: &'static str },
    /// The second layer (Composio, §6.10) — v1: Gmail send only. Never first-layer MCP.
    Composio,
    /// No transport at all: confirming applies a local setting (issue #654, turning
    /// writing-style sharing on; issue #657, resuming capture of excluded apps or websites;
    /// issue #782, turning Visual recall on or keeping saved screens longer; issue #779, an API
    /// caller's Gmail archive request, which confirming hands to the desktop's mail write-back
    /// tick rather than to a send transport). Every transport refuses it, so an approval that
    /// reaches one fails closed instead of being sent somewhere.
    LocalSetting,
}

/// Route a confirmed send to its executor. Exhaustive — every [`SendAction`] variant has exactly
/// one route.
///
/// The layer decision keys on the **service**, not the operation (issue #80 §2-A0): Gmail is
/// served entirely through Composio, so any Gmail-targeted variant — today `SendEmail`, and any
/// added later — routes to the second layer by construction. It can never quietly become a
/// first-layer send: the first layer has no Gmail endpoint at all ([`crate::endpoints`]).
pub fn route_send(action: &SendAction) -> SendRoute {
    let Some((service, op)) = service_op(action) else {
        return SendRoute::LocalSetting;
    };
    SendRoute::FirstLayer { service, op }
}

/// The service a send acts on, or `None` for a local setting. The traceability ledger names this
/// service as the send's destination (FR-TR-01: the API category, never the recipient or title).
pub fn service_for(action: &SendAction) -> Option<Service> {
    service_op(action).map(|(service, _)| service)
}

/// The service and scope-table op a confirmed send targets, before the layer decision. `None`
/// only for an approval that has no service at all (a local setting).
fn service_op(action: &SendAction) -> Option<(Service, &'static str)> {
    Some(match action {
        SendAction::SendEmail { .. } => (Service::Gmail, "send"),
        SendAction::PostMessage { .. } => (Service::Slack, "post_message"),
        SendAction::CreateCalendarEvent { .. } => (Service::GoogleCalendar, "event_create"),
        SendAction::PostComment { .. } => (Service::GitHub, "issue_create_or_comment"),
        SendAction::AddReaction { .. } => (Service::Slack, "reaction"),
        SendAction::UpdateCalendarEvent { .. } => (Service::GoogleCalendar, "event_update_delete"),
        SendAction::CreateDocument { .. } => (Service::GoogleDrive, "file_create"),
        SendAction::UpdateDocument { .. } => (Service::Notion, "page_update"),
        SendAction::ChangeIssueStatus { .. } => (Service::Linear, "status_change"),
        SendAction::ShareWritingStyle
        | SendAction::EnableDictionaryAutoAdd
        | SendAction::ResumeCapture { .. }
        | SendAction::UseSubscriptionAgent { .. }
        | SendAction::WidenVisualRecall { .. }
        | SendAction::ArchiveMailThread { .. }
        | SendAction::UnpauseCapture { .. } => return None,
    })
}

/// Build the MCP tool arguments for a first-layer send: the action's addressing fields plus the
/// confirmed full body. Field names follow the Google/Slack tool schemas as currently documented;
/// confirm against live `tools/list` schemas at wire-up (same caveat as [`crate::toolmap`]).
pub fn args_for_send(action: &SendAction, body: &str) -> Value {
    match action {
        SendAction::SendEmail { to } => {
            let (subject, text) = shogun_mcp::composio::parse_gmail_full_body(body);
            json!({ "to": to, "subject": subject, "body": text })
        }
        SendAction::PostMessage { channel } => json!({ "channel_id": channel, "message": body }),
        SendAction::CreateCalendarEvent { title } => {
            json!({ "summary": title, "description": body })
        }
        SendAction::PostComment { target } => {
            match shogun_agents::permission::github_issue_address(target) {
                Some((owner, repo, issue_number)) => {
                    json!({ "owner": owner, "repo": repo, "issue_number": issue_number, "body": body })
                }
                // Never queued or run (`send_target_refusal`); no address to build.
                None => json!({ "body": body }),
            }
        }
        SendAction::AddReaction { target } => json!({ "target": target, "name": body }),
        SendAction::UpdateCalendarEvent { title } => {
            json!({ "summary": title, "description": body })
        }
        SendAction::CreateDocument { title } => json!({ "name": title, "content": body }),
        SendAction::UpdateDocument { title } => json!({ "title": title, "content": body }),
        SendAction::ChangeIssueStatus { target } => json!({ "id": target, "state": body }),
        // Never sent anywhere (route_send gives it no transport); no arguments to build.
        SendAction::ShareWritingStyle
        | SendAction::EnableDictionaryAutoAdd
        | SendAction::ResumeCapture { .. }
        | SendAction::UseSubscriptionAgent { .. }
        | SendAction::WidenVisualRecall { .. }
        | SendAction::ArchiveMailThread { .. }
        | SendAction::UnpauseCapture { .. } => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_mcp::scope::{self, Gating, OpClass};
    use shogun_agents::permission::Level;

    fn all_actions() -> Vec<SendAction> {
        vec![
            SendAction::SendEmail { to: "a@b.com".into() },
            SendAction::PostMessage { channel: "#general".into() },
            SendAction::CreateCalendarEvent { title: "Sync".into() },
            SendAction::PostComment { target: "pr#12".into() },
            SendAction::AddReaction { target: "msg#1".into() },
            SendAction::UpdateCalendarEvent { title: "Sync".into() },
            SendAction::CreateDocument { title: "Notes".into() },
            SendAction::UpdateDocument { title: "Spec".into() },
            SendAction::ChangeIssueStatus { target: "ENG-1".into() },
        ]
    }

    #[test]
    fn email_send_routes_directly_with_l3_scope() {
        // §6.10 / invariant: the first layer must not grow an email-send path.
        assert_eq!(
            route_send(&SendAction::SendEmail {
                to: "a@b.com".into()
            }),
            SendRoute::FirstLayer {
                service: Service::Gmail,
                op: "send"
            }
        );
    }

    #[test]
    fn all_provider_actions_use_direct_transports() {
        // issue #80 §2-A0: whatever op a Gmail-targeted variant names, it never reaches the first
        // layer — and no other service is ever handed to Composio.
        for action in all_actions() {
            let Some((service, _)) = service_op(&action) else {
                panic!("{action:?} is a send with no service")
            };
            match route_send(&action) {
                SendRoute::LocalSetting => panic!("{action:?} is a real send"),
                SendRoute::Composio => assert_eq!(service, Service::Gmail),
                SendRoute::FirstLayer {
                    service: routed, ..
                } => {
                    assert_eq!(routed, service);
                    assert!(
                        crate::endpoints::has_endpoint(routed) || routed == Service::Gmail,
                        "{routed:?} is routed first-layer but has no endpoint"
                    );
                }
            }
        }
    }

    #[test]
    fn every_first_layer_route_targets_a_real_l3_external_send_op() {
        // Each routed (service, op) must exist in the scope table as an ExternalSend gated L3 —
        // the bridge can never point a send at a read/draft op or an unknown name.
        for action in all_actions() {
            if let SendRoute::FirstLayer { service, op } = route_send(&action) {
                let row = scope::lookup(service, op)
                    .unwrap_or_else(|| panic!("{service:?}::{op} not in the scope table"));
                assert_eq!(row.class, OpClass::ExternalSend, "{service:?}::{op} must be a send");
                assert_eq!(row.gating, Gating::Level(Level::L3), "{service:?}::{op} must be L3");
            }
        }
    }

    #[test]
    fn a_writing_style_approval_has_no_transport() {
        assert_eq!(route_send(&SendAction::ShareWritingStyle), SendRoute::LocalSetting);
        assert_eq!(service_op(&SendAction::ShareWritingStyle), None);
    }

    #[test]
    fn a_capture_resume_approval_has_no_transport() {
        let action = SendAction::ResumeCapture { change: Default::default() };
        assert_eq!(route_send(&action), SendRoute::LocalSetting);
        assert_eq!(service_op(&action), None);
        assert_eq!(args_for_send(&action, "body"), json!({}));
    }

    #[test]
    fn a_capture_unpause_approval_has_no_transport() {
        let action = SendAction::UnpauseCapture { pause: Default::default() };
        assert_eq!(route_send(&action), SendRoute::LocalSetting);
        assert_eq!(service_op(&action), None);
        assert_eq!(args_for_send(&action, "body"), json!({}));
    }

    #[test]
    fn a_subscription_agent_approval_has_no_transport() {
        let action = SendAction::UseSubscriptionAgent { delegate: "claude-code".into() };
        assert_eq!(route_send(&action), SendRoute::LocalSetting);
        assert_eq!(service_op(&action), None);
        assert_eq!(args_for_send(&action, "body"), json!({}));
    }

    #[test]
    fn a_mail_archive_approval_has_no_send_transport() {
        for unarchive in [false, true] {
            let action = SendAction::ArchiveMailThread { thread_key: "gmail:1".into(), unarchive };
            assert_eq!(route_send(&action), SendRoute::LocalSetting);
            assert_eq!(service_op(&action), None);
            assert_eq!(args_for_send(&action, "body"), json!({}));
        }
    }

    #[test]
    fn a_visual_recall_widening_approval_has_no_transport() {
        let action = SendAction::WidenVisualRecall { change: Default::default() };
        assert_eq!(route_send(&action), SendRoute::LocalSetting);
        assert_eq!(service_op(&action), None);
        assert_eq!(args_for_send(&action, "body"), json!({}));
    }

    #[test]
    fn args_carry_the_addressing_field_and_the_full_body() {
        let a = SendAction::PostMessage { channel: "C12345678".into() };
        let v = args_for_send(&a, "the confirmed text");
        assert_eq!(v["channel_id"], "C12345678");
        assert_eq!(v["message"], "the confirmed text");

        let c = SendAction::CreateCalendarEvent { title: "Sync".into() };
        let v = args_for_send(&c, "agenda");
        assert_eq!(v["summary"], "Sync");
        assert_eq!(v["description"], "agenda");
    }

    #[test]
    fn github_comment_address_requires_an_exact_issue_or_pr_url() {
        let action = SendAction::PostComment{target:"https://github.com/octocat/project/issues/42".into()};
        assert_eq!(args_for_send(&action,"confirmed comment"),json!({"owner":"octocat","repo":"project","issue_number":42,"body":"confirmed comment"}));
        let other_host = SendAction::PostComment{target:"https://attacker.invalid/octocat/project/issues/42".into()};
        assert_eq!(args_for_send(&other_host,"confirmed comment"),json!({"body":"confirmed comment"}));
    }

    /// The catalog's proposal tools, by name. The catalog has no iterator by design (its contents
    /// are an implementation detail), so the names are listed here — and
    /// `the_published_list_is_exactly_the_catalogs_proposal_set` pins this list to the catalog in
    /// both directions, so a proposal added there without being added here fails that test rather
    /// than slipping past the round-trip sweep unchecked.
    const PUBLISHED: &[&str] = &[
        "propose_send_email",
        "propose_calendar_event",
        "propose_calendar_event_change",
        "propose_drive_document",
        "propose_chat_message",
        "propose_issue_comment",
        "propose_doc_change",
    ];

    /// Every proposal the model can make must execute against the service it was made for.
    ///
    /// The forward map (a tool the model calls → a [`SendAction`]) lives in
    /// `shogun_mcp::tool_catalog`; the reverse map (an approved action → the service that
    /// performs it) is [`route_send`] here. They are written independently, and one variant can
    /// only route to one service — so if two services ever shared a variant, an approved Linear
    /// comment would be posted to GitHub. This test is what keeps that from being wired: a
    /// published proposal whose round trip does not return to its own service fails here.
    #[test]
    fn every_published_proposal_round_trips_to_its_own_service() {
        use shogun_mcp::tool_catalog::{proposed_action, ToolKind};
        use shogun_mcp::scope::ALL_SERVICES;

        let input = serde_json::json!({
            "to": "a@b.com", "title": "t", "channel": "C0123ABCD",
            "target": "https://github.com/o/r/issues/1", "body": "b"
        });

        let mut covered: Vec<&str> = Vec::new();
        for service in ALL_SERVICES {
            for op in scope::scope(*service).ops {
                // Find the published proposal for this row, if any.
                let Some(entry) = published_proposal_for(*service, op.name) else { continue };
                let Some(action) = proposed_action(entry, &input) else {
                    panic!("{} publishes a proposal that builds no action", entry.name)
                };
                let shogun_agents::permission::Action::Send(send) = action else {
                    panic!("{} built a non-send action", entry.name)
                };
                covered.push(entry.name);
                match route_send(&send) {
                    SendRoute::FirstLayer { service: routed_service, op: routed_op } => assert_eq!(
                        (routed_service, routed_op),
                        (*service, op.name),
                        "{} would execute against the wrong service",
                        entry.name,
                    ),
                    // Gmail send is deliberately not a first-layer op; Composio is its only route.
                    SendRoute::Composio => assert_eq!(
                        (*service, op.name),
                        (Service::Gmail, "send"),
                        "{} routed to Composio but is not the Gmail send",
                        entry.name,
                    ),
                    SendRoute::LocalSetting => {
                        panic!("{} is a proposal with no transport", entry.name)
                    }
                }
                assert_eq!(entry.kind, ToolKind::Propose);
            }
        }
        // Every listed proposal was actually swept — a stale name, or one whose scope row went
        // away, would otherwise shrink the sweep silently.
        for name in PUBLISHED {
            assert!(covered.contains(name), "{name} was never reached by the sweep");
        }
    }

    /// The other half of the coverage guard: [`PUBLISHED`] is a hand-list, so it can drift from
    /// the catalog — and a catalog proposal missing from it would skip the round-trip sweep
    /// entirely. This derives the actually-published proposal set from `tool_definitions` under
    /// the widest context (everything connected, every wave released, full plan, draft-stop off)
    /// and pins the two sets to each other.
    #[test]
    fn the_published_list_is_exactly_the_catalogs_proposal_set() {
        use shogun_agents::entitlement::{entitlements, Plan};
        use shogun_mcp::connection::ConnState;
        use shogun_mcp::scope::{Wave, ALL_SERVICES};
        use shogun_mcp::tool_catalog::{
            catalog_entry, tool_definitions, ServiceState, ToolContext, ToolKind,
        };

        let services: Vec<ServiceState> = ALL_SERVICES
            .iter()
            .map(|s| ServiceState { service: *s, conn: ConnState::Connected { last_sync_ms: 0 } })
            .collect();
        let ctx = ToolContext {
            highest_released: Wave::Three,
            draft_stop: false,
            plan: entitlements(Plan::Pro, 0),
        };
        let offered: Vec<String> = tool_definitions(&services, &ctx)
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .filter(|n| catalog_entry(n).is_some_and(|e| e.kind == ToolKind::Propose))
            .collect();
        for name in PUBLISHED {
            assert!(offered.iter().any(|o| o == name), "{name} is listed but never published");
        }
        for name in &offered {
            assert!(
                PUBLISHED.contains(&name.as_str()),
                "{name} is published but missing from PUBLISHED — the round-trip sweep never \
                 checked it"
            );
        }
    }

    /// The published proposal entry for a scope row, if the catalog has one.
    fn published_proposal_for(
        service: Service,
        op_name: &str,
    ) -> Option<&'static shogun_mcp::tool_catalog::ToolEntry> {
        use shogun_mcp::tool_catalog::{catalog_entry, ToolKind};
        PUBLISHED
            .iter()
            .filter_map(|n| catalog_entry(n))
            .find(|e| e.service == service && e.scope_op == op_name && e.kind == ToolKind::Propose)
    }
}
