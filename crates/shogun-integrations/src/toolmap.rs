//! Our scope-table op name → the Google MCP tool that performs it.
//!
//! The scope table ([`shogun_mcp::scope`]) is service-agnostic (`read_sync`, `draft_create_update`,
//! `event_create`, …). Each Google MCP server exposes its own tool names; this is the single place
//! that bridges the two, so the transport never hard-codes tool strings inline.
//!
//! Tool names are from the Google Workspace MCP server docs (Developer Preview, doc-checked
//! 2026-07 — **PROVISIONAL: not yet confirmed against a live `tools/list`**; issue #80 does that
//! confirmation for Calendar / Drive and drops this marker). A `None` means "this op has no
//! first-layer MCP tool" — either the op is Composio-routed (`gmail::send`) or the service is not
//! a Google endpoint. It is never a silent default.
//!
//! **Gmail is served entirely through Composio** (2026-07-30 decision; issue #80 §2-A0): the
//! names in the Gmail arm below are *internal aliases*, not a live Google endpoint's tools —
//! there is no first-layer Gmail endpoint (`crate::endpoints`). `ComposioReadRpc` (shogun-core
//! `composio_read.rs`) translates them to Composio action slugs, and those slugs are the part
//! that is live-verified.

use shogun_mcp::scope::Service;

/// The Google MCP tool name for `(service, op_name)`, if one exists.
pub fn tool_for(service: Service, op_name: &str) -> Option<&'static str> {
    match (service, op_name) {
        // Gmail (Composio-relayed; these are internal aliases, see the module doc)
        (Service::Gmail, "read_sync") => Some("search_threads"),
        (Service::Gmail, "read_on_demand") => Some("get_thread"),
        (Service::Gmail, "draft_create_update") => Some("create_draft"),
        // Issue #319 write-back — served by the Composio Gmail transport (`ComposioReadRpc`),
        // which handles these tool names alongside `create_draft`.
        (Service::Gmail, "label_sync") => Some("modify_labels"),
        (Service::Gmail, "archive") => Some("archive_thread"),
        (Service::Gmail, "unarchive") => Some("unarchive_thread"),
        // Internal alias handled by GmailDirectRpc rather than an official Gmail MCP server.
        (Service::Gmail, "send") => Some("send_email"),

        // Google Calendar (calendarmcp.googleapis.com)
        (Service::GoogleCalendar, "read_sync") => Some("list_events"),
        (Service::GoogleCalendar, "free_busy") => Some("suggest_time"),
        (Service::GoogleCalendar, "event_create") => Some("create_event"),
        (Service::GoogleCalendar, "event_update_delete") => Some("update_event"),

        // Google Drive (drivemcp.googleapis.com)
        (Service::GoogleDrive, "read_sync") => Some("list_recent_files"),
        // read_file_content also serves Google Docs/Sheets content (no dedicated MCP server).
        (Service::GoogleDrive, "read_on_demand") => Some("read_file_content"),
        (Service::GoogleDrive, "file_create") => Some("create_file"),

        // Slack (mcp.slack.com, Wave 2)
        // PROVISIONAL names from Slack's documented capability list (search / send / react);
        // confirm against the live server's tools/list at Wave-2 wire-up before first use.
        (Service::Slack, "read_on_demand") => Some("slack_search_public"),
        (Service::Slack, "post_message") => Some("slack_send_message"),
        (Service::Slack, "reaction") => Some("slack_add_reaction"),
        // draft_local / copy_to_clipboard are DEVICE-LOCAL by design (FR-INT-30) — never MCP.
        (Service::Slack, "draft_local" | "copy_to_clipboard") => None,

        // Notion (mcp.notion.com, Wave 3) — official names checked 2026-09-27.
        // Search still requires notion-get-tool-access + provider-aware arguments before rollout.
        (Service::Notion, "read_on_demand") => Some("notion-search"),
        (Service::Notion, "page_or_row_create") => Some("notion-create-pages"),
        (Service::Notion, "page_update") => Some("notion-update-page"),

        // GitHub (api.githubcopilot.com, Wave 3) — PROVISIONAL
        (Service::GitHub, "read_sync") => Some("search_issues"),
        (Service::GitHub, "issue_create_or_comment") => Some("add_issue_comment"),
        // comment_draft is DEVICE-LOCAL (L2) — a local draft, never an MCP call.
        (Service::GitHub, "comment_draft") => None,

        // Linear (mcp.linear.app, Wave 3) — PROVISIONAL
        (Service::Linear, "read_sync") => Some("list_issues"),
        (Service::Linear, "issue_create_update_comment") => Some("create_comment"),
        (Service::Linear, "status_change") => Some("update_issue"),
        // issue_draft is DEVICE-LOCAL (L2).
        (Service::Linear, "issue_draft") => None,

        // Granola documents these names; wire fields are checked against tools/list per call.
        (Service::Granola, "list_meetings") => Some("list_meetings"),
        (Service::Granola, "read_on_demand") => Some("get_meetings"),

        // Anything else (unknown op, not-implemented row, or device-local) has no tool.
        _ => None,
    }
}

/// The tool used to satisfy a background read-sync for a service, if it has one.
pub fn read_sync_tool(service: Service) -> Option<&'static str> {
    tool_for(service, "read_sync")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_mcp::scope::{self, Gating, OpClass};

    /// Every first-layer service — all ship an official MCP endpoint (Waves 1–3).
    const MAPPED: [Service; 8] = [
        Service::Gmail,
        Service::GoogleCalendar,
        Service::GoogleDrive,
        Service::Slack,
        Service::Notion,
        Service::GitHub,
        Service::Linear,
        Service::Granola,
    ];

    #[test]
    fn slack_notion_and_granola_have_no_background_sync() {
        for s in [Service::Slack, Service::Notion, Service::Granola] {
            assert!(read_sync_tool(s).is_none());
            assert_eq!(shogun_mcp::scope::authorize(s, "read_sync"), shogun_mcp::scope::Authorization::DeniedUnknownOp);
        }
    }

    #[test]
    fn every_mapped_service_read_sync_has_a_tool() {
        // Slack/Notion have no background read_sync (search needs a query).
        for s in MAPPED.into_iter().filter(|s| !matches!(s, Service::Slack | Service::Notion | Service::Granola)) {
            assert!(read_sync_tool(s).is_some(), "{s:?} read_sync must map to a tool");
        }
    }

    #[test]
    fn gmail_send_maps_to_the_direct_rest_alias() {
        assert_eq!(tool_for(Service::Gmail, "send"), Some("send_email"));
    }

    #[test]
    fn device_local_slack_ops_have_no_mcp_tool() {
        // FR-INT-30: the local draft and the clipboard fallback never leave the device.
        assert_eq!(tool_for(Service::Slack, "draft_local"), None);
        assert_eq!(tool_for(Service::Slack, "copy_to_clipboard"), None);
    }

    #[test]
    fn device_local_wave3_drafts_have_no_mcp_tool() {
        // GitHub comment_draft and Linear issue_draft are device-local (L2) — never MCP.
        assert_eq!(tool_for(Service::GitHub, "comment_draft"), None);
        assert_eq!(tool_for(Service::Linear, "issue_draft"), None);
        // Not-implemented rows are unroutable too.
        assert_eq!(tool_for(Service::Notion, "delete"), None);
    }

    #[test]
    fn every_routable_op_on_a_mapped_service_has_a_tool() {
        // For each service with an endpoint, every scope-table op that goes over MCP (implemented,
        // not the Composio-only send, not device-local) must map to a tool — nothing routable is
        // left unroutable, and nothing device-local grows a network path.
        for s in MAPPED {
            for op in scope::scope(s).ops {
                let over_mcp = !matches!(op.gating, Gating::NotImplemented | Gating::ComposioOnly)
                    && op.class != OpClass::DraftLocal;
                assert_eq!(
                    tool_for(s, op.name).is_some(),
                    over_mcp,
                    "{s:?}::{} routing does not match its class/gating",
                    op.name
                );
            }
        }
    }
}
