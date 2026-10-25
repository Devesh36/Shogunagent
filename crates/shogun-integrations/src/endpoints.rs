//! Service → official first-party remote MCP endpoint + OAuth scopes.
//!
//! First-layer connections go **directly** to each vendor's own first-party MCP server (§6.9,
//! FR-INT-01/02): user→service OAuth, no third party in the data path (unlike Composio, the second
//! layer). Only services that actually ship an official remote MCP server are mappable here.
//!
//! Coverage (doc-checked 2026-07; the Google entries are not yet confirmed against a live
//! `tools/list` — issue #80 does that for Calendar / Drive):
//! - Calendar→ `calendarmcp.googleapis.com` (Google Workspace Developer Preview)
//! - Drive → `drivemcp.googleapis.com` (same; also the read path for Google Docs/Sheets content, which have no dedicated MCP server of their own)
//! - Slack → `mcp.slack.com` (Wave 2; OPEN-03 resolved — Slack ships an official remote MCP, JSON-RPC 2.0 over Streamable HTTP, workspace-admin approved)
//!
//! **Gmail has no first-layer entry** (CLAUDE.md 連携実装ルール, 2026-07-30 decision; issue #80
//! §2-A0): all Gmail traffic — reads, drafts, sends — runs through Composio (the second layer),
//! so [`endpoint`] returns `None` and the first-layer transport is structurally unable to reach a
//! mailbox with any scope. If the official Gmail MCP reaches GA and the decision is revisited,
//! the entry goes back here and nowhere else.
//!
//! Notion / GitHub / Linear are Wave 3 and unverified — [`endpoint`] returns `None` for any other
//! service without an official remote MCP server.

use shogun_mcp::scope::Service;

/// An official remote MCP endpoint and the OAuth scopes needed for the operations we call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpEndpoint {
    /// The MCP Streamable-HTTP JSON-RPC URL.
    pub url: &'static str,
    /// The minimal OAuth scopes for this service's read + first-layer-write operations. Requested
    /// least-privilege (FR-INT-05).
    pub scopes: &'static [&'static str],
}

const CALENDAR: McpEndpoint = McpEndpoint {
    url: "https://calendarmcp.googleapis.com/mcp/v1",
    scopes: &[
        "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
        "https://www.googleapis.com/auth/calendar.events.readonly",
        "https://www.googleapis.com/auth/calendar.events.freebusy",
    ],
};

const DRIVE: McpEndpoint = McpEndpoint {
    url: "https://drivemcp.googleapis.com/mcp/v1",
    scopes: &[
        "https://www.googleapis.com/auth/drive.readonly",
        // per-file access created by the app — the least-privilege write scope for file_create.
        "https://www.googleapis.com/auth/drive.file",
    ],
};

const SLACK: McpEndpoint = McpEndpoint {
    url: "https://mcp.slack.com/mcp",
    // User-token scopes for the Wave-2 op set (read_sync / post_message / reaction). Per-tool
    // scopes are Slack-documented; confirm the final set against the Slack app config at wire-up.
    scopes: &["search:read.public", "search:read.users", "chat:write"],
};

// Wave 3 official remote MCP servers (verified 2026-07)
// Endpoints are confirmed; the exact OAuth scope strings are provisional (each vendor's least-
// privilege set) — confirm against each server's auth config at wire-up.
const NOTION: McpEndpoint = McpEndpoint {
    url: "https://mcp.notion.com/mcp",
    scopes: &["default"],
};
const GITHUB: McpEndpoint = McpEndpoint {
    url: "https://api.githubcopilot.com/mcp/",
    // GitHub connects with a user-supplied PAT; OAuth scopes are never requested here.
    scopes: &[],
};
const LINEAR: McpEndpoint = McpEndpoint {
    url: "https://mcp.linear.app/mcp/readonly",
    scopes: &["read"],
};
const GRANOLA: McpEndpoint = McpEndpoint {
    url: "https://mcp.granola.ai/mcp",
    // offline_access: Granola issues the refresh token SHOGUN requires only when asked.
    scopes: &["mcp", "offline_access"],
};

/// The official remote MCP endpoint for a service, if the vendor ships one. `None` means the
/// service is not reachable over first-layer MCP today.
pub fn endpoint(service: Service) -> Option<McpEndpoint> {
    match service {
        // Gmail is Composio-relayed end to end (issue #80 §2-A0) — no first-layer endpoint, so
        // the first-layer transport can never reach a mailbox, with any scope.
        Service::Gmail => None,
        Service::GoogleCalendar => Some(CALENDAR),
        Service::GoogleDrive => Some(DRIVE),
        Service::Slack => Some(SLACK),
        Service::Notion => Some(NOTION),
        Service::GitHub => Some(GITHUB),
        Service::Linear => Some(LINEAR),
        Service::Granola => Some(GRANOLA),
        // Web (issue #555) is not an MCP service: SHOGUN fetches public pages itself over the
        // traced transport. No endpoint, no token, no first-layer transport path.
        Service::Web => None,
    }
}

/// Whether a service is reachable over an official first-layer MCP endpoint today.
pub fn has_endpoint(service: Service) -> bool {
    endpoint(service).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_first_layer_service_except_gmail_has_an_https_endpoint_with_scopes() {
        // Every first-layer service ships an official remote MCP server (Waves 1-3) — except
        // Gmail, which is Composio-relayed end to end (issue #80 §2-A0), and Web (issue #555),
        // which is not an MCP service at all: SHOGUN fetches public pages itself.
        for s in shogun_mcp::scope::ALL_SERVICES {
            if matches!(*s, Service::Gmail | Service::Web) {
                assert!(endpoint(*s).is_none(), "{s:?} has no first-layer endpoint");
                continue;
            }
            let ep = endpoint(*s).unwrap_or_else(|| panic!("{s:?} must have an endpoint"));
            assert!(ep.url.starts_with("https://"), "{s:?} url must be https");
            if *s == Service::GitHub {
                assert!(ep.scopes.is_empty(), "GitHub PAT auth does not request OAuth scopes");
            } else {
                assert!(!ep.scopes.is_empty(), "{s:?} must request scopes");
            }
            assert!(has_endpoint(*s));
        }
        // Google servers share the /mcp/v1 path; the others are vendor-specific.
        assert!(endpoint(Service::GoogleCalendar).unwrap().url.ends_with("/mcp/v1"));
        assert_eq!(endpoint(Service::Slack).unwrap().url, "https://mcp.slack.com/mcp");
        assert_eq!(endpoint(Service::GitHub).unwrap().url, "https://api.githubcopilot.com/mcp/");
        assert_eq!(endpoint(Service::Granola), Some(GRANOLA));
    }

    #[test]
    fn gmail_has_no_first_layer_endpoint_at_all() {
        // Invariant 4 / §6.10 / issue #80 §2-A0: the strongest form of "the first layer can never
        // send mail" is that the first layer cannot reach Gmail with ANY scope.
        assert_eq!(endpoint(Service::Gmail), None);
        assert!(!has_endpoint(Service::Gmail));
    }

    #[test]
    fn no_endpoint_requests_a_gmail_scope() {
        // Belt and braces for the same invariant: no service's scope list can smuggle a Gmail
        // scope back into the first layer under another name.
        for s in shogun_mcp::scope::ALL_SERVICES {
            if let Some(ep) = endpoint(*s) {
                assert!(
                    !ep.scopes.iter().any(|scope| scope.contains("/auth/gmail")),
                    "{s:?} requests a Gmail scope on the first layer"
                );
            }
        }
    }

    #[test]
    fn unpublished_calendar_writes_do_not_request_write_scope() {
        let cal = endpoint(Service::GoogleCalendar).unwrap();
        assert!(!cal.scopes.contains(&"https://www.googleapis.com/auth/calendar.events"));
    }
}
