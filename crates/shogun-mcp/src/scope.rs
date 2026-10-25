//! The per-service permission scope table (§6.9.2) — the source of truth for first-layer MCP.
//!
//! "以下の表が各サービスの実装範囲の正である。表にない操作を実装しない。" This module encodes those
//! tables in code and gates every service operation through [`authorize`]:
//! - an operation **not in the table** is denied ([`Authorization::DeniedUnknownOp`]);
//! - an explicitly out-of-v1-scope operation is denied ([`Authorization::DeniedNotImplemented`]);
//! - Gmail send is first-layer-denied and routed to Composio ([`Authorization::RequiresComposio`]);
//! - reads sync in the background; everything that writes carries its L2/L3 level.
//!
//! Invariant 4 is enforced over the whole table (see tests): every [`OpClass::ExternalSend`] is
//! gated L3 (or Composio, itself L3) — no service can post/create/react below L3.

use shogun_agents::permission::Level;

/// The first-layer services (§6.9.2), grouped into rollout waves (FR-INT-03).
///
/// `GoogleDrive` was added to the Wave-1 Google Workspace batch by product decision (2026-07-23):
/// Google ships an official first-party remote MCP server for it (`drivemcp.googleapis.com`), so it
/// fits the first-layer "direct official MCP, no third party" model. Google Docs/Sheets have **no**
/// dedicated MCP server, so they are not separate services — their content is read through Drive's
/// `read_file_content` (see [`crate::sync`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Gmail,
    GoogleCalendar,
    GoogleDrive,
    Slack,
    Notion,
    GitHub,
    Linear,
    Granola,
    /// Public web reads for chat (issue #555): a YouTube transcript, an X post, a page — fetched
    /// by SHOGUN itself from key-less public endpoints, never through a third party and never
    /// with a credential. Not an OAuth service: its "connection" is the one-time Web fetch
    /// consent ([`crate::web_consent`]), which the caller folds into the connection state.
    Web,
}

/// Rollout wave (FR-INT-03): Wave 1 = Gmail + Calendar → Wave 2 = Slack → Wave 3 = Notion + GitHub
/// + Linear. Ordered so a service is enabled only once its wave is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wave {
    One = 1,
    Two = 2,
    Three = 3,
}

impl Service {
    /// The wave this service ships in.
    pub fn wave(self) -> Wave {
        match self {
            // Web ships with the wave already out: it adds no new vendor connection.
            Service::Gmail | Service::GoogleCalendar | Service::GoogleDrive | Service::Web => Wave::One,
            Service::Slack => Wave::Two,
            Service::Notion | Service::GitHub | Service::Linear | Service::Granola => Wave::Three,
        }
    }

    /// Whether this service is released given the highest released wave (FR-INT-03). Unreleased
    /// services show as "Coming soon" and must not sync.
    pub fn is_released(self, highest_released: Wave) -> bool {
        self.wave() <= highest_released
    }

    /// The `event_log.source` discriminator for items ingested from this service (§6.9, the source
    /// column values). Synced integration items land in the log tagged with this, so search and
    /// Context Fusion can tell an email from a captured window (FR-INT-05).
    pub fn source_str(self) -> &'static str {
        match self {
            Service::Gmail => "gmail",
            Service::GoogleCalendar => "gcal",
            Service::GoogleDrive => "gdrive",
            Service::Slack => "slack",
            Service::Notion => "notion",
            Service::GitHub => "github",
            Service::Linear => "linear",
            Service::Granola => "granola",
            Service::Web => "web",
        }
    }

    /// Whether the service has trusted structured metadata. Issue #35: calendar title / time /
    /// local event ID rank above AX / search evidence, while its description stays prose and skips
    /// local-rule extraction. Mail, chat, and document bodies remain prose even through MCP.
    pub fn is_structured_fact(self) -> bool {
        matches!(self, Service::GoogleCalendar)
    }
}

/// What an operation does — this fixes the gating it is allowed to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// Read the service's data (into the event log).
    Read,
    /// A device-local draft (never written to the service).
    DraftLocal,
    /// A reversible write to the service that is not a send (e.g. a Gmail draft/label/read-state).
    ServiceStateChange,
    /// A send/post/create/react/event-create — leaves the device irreversibly (always L3).
    ExternalSend,
    /// A device-local effect that is not a draft and never leaves the device — opening a URL in
    /// the user's own browser (issue #555 `open_url`). L1 or L2: it can put a tab in front of
    /// the user, so it is confirmable, but nothing is sent anywhere.
    LocalAction,
}

impl OpClass {
    /// Whether this class leaves the device as an irreversible external write (a send).
    pub fn is_external_send(self) -> bool {
        matches!(self, OpClass::ExternalSend)
    }
}

/// How an operation is gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gating {
    /// Background sync — no per-action confirmation (reads).
    Background,
    /// Requires this permission level.
    Level(Level),
    /// Not implemented in v1 (explicitly out of scope — the "—" rows).
    NotImplemented,
    /// First-layer-denied; provided only via Composio (second layer, itself L3). Gmail send.
    ComposioOnly,
}

/// One operation in a service's scope table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedOp {
    pub name: &'static str,
    pub class: OpClass,
    pub gating: Gating,
}

/// A service's full scope table.
#[derive(Debug, Clone, Copy)]
pub struct ServiceScope {
    pub service: Service,
    pub ops: &'static [ScopedOp],
}

use Gating::*;
use OpClass::*;

const fn op(name: &'static str, class: OpClass, gating: Gating) -> ScopedOp {
    ScopedOp { name, class, gating }
}

// Gmail (Wave 1)
const GMAIL: &[ScopedOp] = &[
    op("read_sync", Read, Background),
    op("read_on_demand", Read, Level(Level::L2)),
    op("draft_create_update", ServiceStateChange, Level(Level::L2)),
    // Issue #319 (§6.9.2, 2026-09-04 revision): label sync and archive (= INBOX label removal,
    // reversible) are L2 service-state changes. They may run under a per-label *standing*
    // approval (FR-TG-21) — the level stays L2; only the approval's timing moves earlier.
    op("label_sync", ServiceStateChange, Level(Level::L2)),
    op("archive", ServiceStateChange, Level(Level::L2)),
    op("unarchive", ServiceStateChange, Level(Level::L2)),
    // Send is NOT a first-layer operation — Composio only (§6.10), itself L3.
    op("send", ExternalSend, Level(Level::L3)),
    // Mark-as-read, delete, trash and spam reporting stay out of v1 (#319).
    op("delete_trash", ExternalSend, NotImplemented),
];

// Google Calendar (Wave 1)
const GOOGLE_CALENDAR: &[ScopedOp] = &[
    op("read_sync", Read, Background),
    op("free_busy", Read, Level(Level::L2)),
    op("event_create", ExternalSend, Level(Level::L3)),
    op("event_update_delete", ExternalSend, Level(Level::L3)),
];

// Google Drive (Wave 1)
// Official MCP: drivemcp.googleapis.com (tools: list_recent_files, search_files, read_file_content,
// get_file_metadata, create_file, copy_file). `read_on_demand` also serves Google Docs/Sheets
// content (read_file_content), which have no dedicated MCP server of their own. A create writes
// content out to the service, so — like a Notion page create — it is an ExternalSend at L3.
const GOOGLE_DRIVE: &[ScopedOp] = &[
    op("read_sync", Read, Background),
    op("read_on_demand", Read, Level(Level::L2)),
    op("file_create", ExternalSend, Level(Level::L3)),
    op("delete", ExternalSend, NotImplemented),
];

// Slack (Wave 2)
// No read_sync: search needs an explicit query, so there is no background crawl.
const SLACK: &[ScopedOp] = &[
    op("read_on_demand", Read, Level(Level::L2)),
    op("draft_local", DraftLocal, Level(Level::L2)),
    // FR-INT-30 fallback: no external send, so L2.
    op("copy_to_clipboard", DraftLocal, Level(Level::L2)),
    op("post_message", ExternalSend, Level(Level::L3)),
    op("reaction", ExternalSend, Level(Level::L3)),
];

// Notion (Wave 3)
// No read_sync: search needs an explicit query, so there is no background crawl.
const NOTION: &[ScopedOp] = &[
    op("read_on_demand", Read, Level(Level::L2)),
    op("page_or_row_create", ExternalSend, Level(Level::L3)),
    op("page_update", ExternalSend, Level(Level::L3)),
    op("delete", ExternalSend, NotImplemented),
];

// Granola's MCP exposes meeting notes on demand. Never crawl a workspace in background.
const GRANOLA: &[ScopedOp] = &[
    op("list_meetings", Read, Level(Level::L2)),
    op("read_on_demand", Read, Level(Level::L2)),
];

// GitHub (Wave 3)
const GITHUB: &[ScopedOp] = &[
    op("read_sync", Read, Background),
    op("comment_draft", DraftLocal, Level(Level::L2)),
    op("issue_create_or_comment", ExternalSend, Level(Level::L3)),
    op("pr_merge_close_branch", ExternalSend, NotImplemented),
];

// Web (issue #555)
// Public reads SHOGUN performs itself over the traced transport (spec §4-6 / §4-7): no OAuth, no
// third party, no cookie. Each read is Background — the fetch of a URL the user named is not a
// per-call confirmation; the one-time Web fetch consent (FR-WV-15) is the gate, folded into the
// connection state by the caller. `open_url` is device-local (the user's own browser); the
// catalog raises it to L2 for hosts outside its allow-list.
const WEB: &[ScopedOp] = &[
    op("video_transcript", Read, Background),
    op("social_post", Read, Background),
    op("page", Read, Background),
    op("open_url", LocalAction, Level(Level::L1)),
];

// Linear (Wave 3)
const LINEAR: &[ScopedOp] = &[
    op("read_sync", Read, Background),
    op("issue_draft", DraftLocal, Level(Level::L2)),
    op("issue_create_update_comment", ExternalSend, Level(Level::L3)),
    op("status_change", ExternalSend, Level(Level::L3)),
];

/// The scope table for a service.
pub fn scope(service: Service) -> ServiceScope {
    let ops = match service {
        Service::Gmail => GMAIL,
        Service::GoogleCalendar => GOOGLE_CALENDAR,
        Service::GoogleDrive => GOOGLE_DRIVE,
        Service::Slack => SLACK,
        Service::Notion => NOTION,
        Service::GitHub => GITHUB,
        Service::Linear => LINEAR,
        Service::Granola => GRANOLA,
        Service::Web => WEB,
    };
    ServiceScope { service, ops }
}

/// Resolve a service from its `source_str` id (`gmail` / `gcal` / `gdrive` / …) — the inverse of
/// [`Service::source_str`]. Used by the UI/command layer to turn a string id back into a service.
pub fn from_source(source: &str) -> Option<Service> {
    ALL_SERVICES.iter().copied().find(|s| s.source_str() == source)
}

/// Every service, for exhaustive iteration in tests / settings.
pub const ALL_SERVICES: &[Service] = &[
    Service::Gmail,
    Service::GoogleCalendar,
    Service::GoogleDrive,
    Service::Slack,
    Service::Notion,
    Service::GitHub,
    Service::Linear,
    Service::Granola,
    Service::Web,
];

/// The result of authorizing a service operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// Allowed as a background sync (reads).
    Background,
    /// Allowed after confirmation at this level.
    RequiresLevel(Level),
    /// The operation is not in the service's table (§6.9.2: "表にない操作を実装しない").
    DeniedUnknownOp,
    /// The operation is explicitly out of v1 scope.
    DeniedNotImplemented,
    /// First-layer-denied; only Composio (second layer, L3) can perform it (Gmail send).
    RequiresComposio,
}

/// Authorize an operation by name against a service's scope table. The only entry point the
/// execution engine should use for MCP operations — an unknown name is denied, never defaulted.
/// Look up an operation's row (class + gating) in a service's scope table, if present. The
/// composed integration gate ([`crate::service_gate`]) uses this to apply the amber read/write
/// rule and the draft-stop rule on top of the raw authorization.
pub fn lookup(service: Service, op_name: &str) -> Option<ScopedOp> {
    scope(service).ops.iter().copied().find(|o| o.name == op_name)
}

pub fn authorize(service: Service, op_name: &str) -> Authorization {
    match scope(service).ops.iter().find(|o| o.name == op_name) {
        None => Authorization::DeniedUnknownOp,
        Some(o) => match o.gating {
            Gating::Background => Authorization::Background,
            Gating::Level(l) => Authorization::RequiresLevel(l),
            Gating::NotImplemented => Authorization::DeniedNotImplemented,
            Gating::ComposioOnly => Authorization::RequiresComposio,
        },
    }
}

/// What a connection lets SHOGUN do with a service, at most — the connections screen's access
/// badge (issue #82). Derived from the scope table, never written down twice, so the badge cannot
/// promise less (or more) than the table enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only.
    Read,
    /// Reads plus drafts or reversible changes on the service; nothing is ever sent.
    ReadDraft,
    /// Can also send / post / create on the service — always behind explicit approval (L3), and
    /// for Gmail additionally behind the Composio opt-in and draft-stop.
    ReadWrite,
}

impl Access {
    /// The stable wire id the UI keys its badge copy off.
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::ReadDraft => "read_draft",
            Access::ReadWrite => "read_write",
        }
    }
}

/// The access ceiling of `service`: the widest thing any IMPLEMENTED operation in its table can do.
/// `NotImplemented` rows do not count — a badge must not advertise what v1 cannot do. Consent,
/// draft-stop and connection state narrow this further at run time; the badge shows the ceiling
/// and the Mail row states its draft-only mode separately.
pub fn access(service: Service) -> Access {
    let live = scope(service)
        .ops
        .iter()
        .filter(|o| !matches!(o.gating, Gating::NotImplemented))
        .filter(|o| !o.class.is_external_send() || crate::tool_catalog::publishes_send(service, o.name));
    let mut widest = Access::Read;
    for o in live {
        let this = match o.class {
            OpClass::ExternalSend => Access::ReadWrite,
            OpClass::DraftLocal | OpClass::ServiceStateChange => Access::ReadDraft,
            OpClass::Read | OpClass::LocalAction => Access::Read,
        };
        widest = match (widest, this) {
            (Access::ReadWrite, _) | (_, Access::ReadWrite) => Access::ReadWrite,
            (Access::ReadDraft, _) | (_, Access::ReadDraft) => Access::ReadDraft,
            _ => Access::Read,
        };
    }
    widest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_badge_follows_the_table() {
        // Only services with published write proposals advertise a write ceiling.
        for service in [
            Service::Gmail,
            Service::GoogleDrive,
            Service::Slack,
            Service::GitHub,
        ] {
            assert_eq!(access(service), Access::ReadWrite, "{service:?}");
        }
        for service in [Service::GoogleCalendar,Service::Notion] { assert_eq!(access(service),Access::Read); }
        assert_eq!(access(Service::Linear),Access::ReadDraft); // device-local issue draft only
        // Web reads and opens URLs in the user's own browser — nothing is written anywhere.
        assert_eq!(access(Service::Web), Access::Read);
        assert_eq!(Access::ReadDraft.as_str(), "read_draft");
    }

    #[test]
    fn not_implemented_rows_never_widen_the_badge() {
        // Every NotImplemented row in the table is an ExternalSend; if those counted, a read-only
        // service with a "—" delete row would be advertised as read & write.
        for &service in ALL_SERVICES {
            let implemented_send = scope(service).ops.iter().any(|o| {
                o.class.is_external_send() && !matches!(o.gating, Gating::NotImplemented) && crate::tool_catalog::publishes_send(service,o.name)
            });
            assert_eq!(access(service) == Access::ReadWrite, implemented_send, "{service:?}");
        }
    }

    #[test]
    fn every_external_send_is_l3_or_composio_never_below() {
        // Invariant 4 across the entire table: a send is never gated Background/L1/L2.
        for &service in ALL_SERVICES {
            for o in scope(service).ops {
                if o.class.is_external_send() {
                    match o.gating {
                        Gating::Level(Level::L3) | Gating::ComposioOnly | Gating::NotImplemented => {}
                        other => panic!("{service:?}::{} send gated {other:?} — must be L3", o.name),
                    }
                }
            }
        }
    }

    #[test]
    fn class_and_gating_are_consistent() {
        for &service in ALL_SERVICES {
            for o in scope(service).ops {
                match (o.class, o.gating) {
                    // Reads sync in the background or are on-demand L2.
                    (OpClass::Read, Gating::Background) => {}
                    (OpClass::Read, Gating::Level(Level::L2)) => {}
                    // Local drafts are L2 (device-local).
                    (OpClass::DraftLocal, Gating::Level(Level::L2)) => {}
                    // A device-local effect (open a URL in the user's browser) is L1 or L2 —
                    // confirmable, never a send.
                    (OpClass::LocalAction, Gating::Level(Level::L1 | Level::L2)) => {}
                    // Reversible service writes (Gmail draft/label) are L2.
                    (OpClass::ServiceStateChange, Gating::Level(Level::L2)) => {}
                    // Sends are L3 / Composio / not-implemented.
                    (OpClass::ExternalSend, Gating::Level(Level::L3)) => {}
                    (OpClass::ExternalSend, Gating::ComposioOnly) => {}
                    (OpClass::ExternalSend, Gating::NotImplemented) => {}
                    pair => panic!("{service:?}::{} has inconsistent (class, gating): {pair:?}", o.name),
                }
            }
        }
    }

    #[test]
    fn web_reads_are_background_and_open_url_is_a_local_l1_effect() {
        // Issue #555: the three public reads carry no per-call level (the one-time consent is
        // the gate), and `open_url` is the only non-read — device-local, never a send.
        for read in ["video_transcript", "social_post", "page"] {
            assert_eq!(authorize(Service::Web, read), Authorization::Background);
            assert_eq!(lookup(Service::Web, read).map(|o| o.class), Some(OpClass::Read));
        }
        let open = lookup(Service::Web, "open_url").expect("open_url is in the table");
        assert_eq!(open.class, OpClass::LocalAction);
        assert_eq!(open.gating, Gating::Level(Level::L1));
        assert!(scope(Service::Web).ops.iter().all(|o| !o.class.is_external_send()));
        assert_eq!(authorize(Service::Web, "search"), Authorization::DeniedUnknownOp);
    }

    #[test]
    fn unknown_operation_is_denied() {
        assert_eq!(authorize(Service::Gmail, "read_all_mailboxes"), Authorization::DeniedUnknownOp);
        assert_eq!(authorize(Service::Slack, "delete_workspace"), Authorization::DeniedUnknownOp);
        assert_eq!(access(Service::Granola), Access::Read);
        assert_eq!(Service::Granola.wave(), Wave::Three);
        assert_eq!(authorize(Service::Granola, "read_sync"), Authorization::DeniedUnknownOp);
        assert_eq!(authorize(Service::Granola, "delete_meeting"), Authorization::DeniedUnknownOp);
        assert_eq!(authorize(Service::Granola, "read_on_demand"), Authorization::RequiresLevel(Level::L2));
    }

    #[test]
    fn gmail_send_requires_l3_on_direct_transport() {
        assert_eq!(
            authorize(Service::Gmail, "send"),
            Authorization::RequiresLevel(Level::L3)
        );
    }

    #[test]
    fn not_implemented_operations_are_denied() {
        assert_eq!(authorize(Service::Gmail, "delete_trash"), Authorization::DeniedNotImplemented);
        assert_eq!(authorize(Service::Notion, "delete"), Authorization::DeniedNotImplemented);
        assert_eq!(authorize(Service::GitHub, "pr_merge_close_branch"), Authorization::DeniedNotImplemented);
    }

    #[test]
    fn reads_sync_in_background_and_on_demand_is_l2() {
        assert_eq!(authorize(Service::GoogleCalendar, "read_sync"), Authorization::Background);
        assert_eq!(authorize(Service::Gmail, "read_on_demand"), Authorization::RequiresLevel(Level::L2));
        assert_eq!(authorize(Service::GoogleCalendar, "free_busy"), Authorization::RequiresLevel(Level::L2));
    }

    #[test]
    fn writes_and_posts_carry_their_levels() {
        assert_eq!(authorize(Service::Gmail, "draft_create_update"), Authorization::RequiresLevel(Level::L2));
        assert_eq!(authorize(Service::GoogleCalendar, "event_create"), Authorization::RequiresLevel(Level::L3));
        assert_eq!(authorize(Service::Slack, "post_message"), Authorization::RequiresLevel(Level::L3));
        assert_eq!(authorize(Service::Slack, "reaction"), Authorization::RequiresLevel(Level::L3));
        assert_eq!(authorize(Service::Notion, "page_or_row_create"), Authorization::RequiresLevel(Level::L3));
        assert_eq!(authorize(Service::Linear, "status_change"), Authorization::RequiresLevel(Level::L3));
    }

    #[test]
    fn slack_fallback_copy_is_l2_no_send() {
        // FR-INT-30: clipboard copy leaves no device — L2, and never a send.
        assert_eq!(authorize(Service::Slack, "copy_to_clipboard"), Authorization::RequiresLevel(Level::L2));
        let o = scope(Service::Slack).ops.iter().find(|o| o.name == "copy_to_clipboard").unwrap();
        assert!(!o.class.is_external_send());
    }

    #[test]
    fn every_service_has_a_read_and_at_least_one_op() {
        for &service in ALL_SERVICES {
            let ops = scope(service).ops;
            assert!(!ops.is_empty(), "{service:?} has no ops");
            assert!(
                ops.iter().any(|o| o.class == OpClass::Read),
                "{service:?} must have a read operation"
            );
        }
    }

    #[test]
    fn from_source_is_the_inverse_of_source_str() {
        for &s in ALL_SERVICES {
            assert_eq!(from_source(s.source_str()), Some(s));
        }
        assert_eq!(from_source("gdrive"), Some(Service::GoogleDrive));
        assert_eq!(from_source("nope"), None);
    }

    #[test]
    fn source_strings_are_distinct_and_stable() {
        // every service maps to a distinct event_log.source tag (FR-INT-05).
        let tags: Vec<&str> = ALL_SERVICES.iter().map(|s| s.source_str()).collect();
        let mut uniq = tags.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), tags.len(), "source tags must be unique: {tags:?}");
        assert_eq!(Service::Gmail.source_str(), "gmail");
        assert_eq!(Service::GoogleCalendar.source_str(), "gcal");
    }

    #[test]
    fn calendar_is_the_only_structured_fact_source() {
        assert!(Service::GoogleCalendar.is_structured_fact());
        for s in ALL_SERVICES.iter().copied().filter(|s| *s != Service::GoogleCalendar) {
            assert!(
                !s.is_structured_fact(),
                "{} is prose (mail/chat/docs), not a structured fact",
                s.source_str()
            );
        }
    }

    #[test]
    fn google_drive_is_wave_one_with_read_and_l3_create() {
        // Drive ships in the Wave-1 Google Workspace batch.
        assert_eq!(Service::GoogleDrive.wave(), Wave::One);
        assert!(Service::GoogleDrive.is_released(Wave::One));
        // read syncs in the background; on-demand read (Docs/Sheets content) is L2.
        assert_eq!(authorize(Service::GoogleDrive, "read_sync"), Authorization::Background);
        assert_eq!(authorize(Service::GoogleDrive, "read_on_demand"), Authorization::RequiresLevel(Level::L2));
        // a create writes content out → L3 (consistent with a Notion page create).
        assert_eq!(authorize(Service::GoogleDrive, "file_create"), Authorization::RequiresLevel(Level::L3));
        assert_eq!(authorize(Service::GoogleDrive, "delete"), Authorization::DeniedNotImplemented);
        assert_eq!(Service::GoogleDrive.source_str(), "gdrive");
    }

    #[test]
    fn wave_rollout_gates_unreleased_services() {
        // At Wave 1, only the Google Workspace batch (Gmail + Calendar + Drive) is released.
        assert!(Service::Gmail.is_released(Wave::One));
        assert!(Service::GoogleCalendar.is_released(Wave::One));
        assert!(Service::GoogleDrive.is_released(Wave::One));
        assert!(!Service::Slack.is_released(Wave::One));
        assert!(!Service::Notion.is_released(Wave::One));
        // At Wave 2, Slack joins; Wave-3 services still gated.
        assert!(Service::Slack.is_released(Wave::Two));
        assert!(!Service::GitHub.is_released(Wave::Two));
        // At Wave 3, everything is released.
        assert!(Service::Linear.is_released(Wave::Three));
    }
}
