//! Integration read-sync ingest (§6.9: "同期は `integration.synced` → event log（source列で
//! 区別）→ 検索/Fusionに合流"). The composition that turns a released, connected service into actual
//! memory: gate `read_sync` → fetch via the transport → normalize each item into a **source-tagged**
//! ingest record the daemon appends to the event log.
//!
//! Layering (mirrors the pure/effect split used elsewhere): this module is pure except for the one
//! [`IntegrationTransport`] call, and produces [`IngestItem`]s — it never touches the DB itself
//! (invariant 1, data gravity in the Rust core). The daemon persists the records and emits
//! `IntegrationSynced` on the bus.
//!
//! The gate is the same [`crate::service_gate::authorize_op`] every operation goes through: a sync
//! only proceeds when `read_sync` resolves to [`OpDecision::Background`] (released + connected, or
//! amber serving cached reads). An unreleased / disconnected service can never sync. This module
//! writes no traceability row itself (it has no sink, and [`crate::service_gate::requires_traceability`]
//! is false for reads); the connector runtime asks its ingest sink to record each fetch request
//! before it is sent, because a Composio read still carries the user id and query off the device
//! (issue #782).

use crate::scope::Service;
use crate::service_gate::{authorize_op, DenyReason, OpContext, OpDecision};

/// The operation name a read-sync gates on (the `read_sync` row in every service's scope table).
pub const READ_SYNC: &str = "read_sync";

/// One item fetched from a service, normalized across integrations. The transport (real MCP, or a
/// fake in tests) yields these; v1 carries only text fields — no attachments, no raw payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedItem {
    /// The service-side id (message id, event id, …) — used only for de-duplication upstream; it is
    /// **not** stored as content.
    pub external_id: String,
    /// A short human title (subject / event name / message preview). May be empty.
    pub title: String,
    /// The item's text body — the only thing that becomes searchable memory.
    pub body: String,
    /// The item's own timestamp (unix ms) — when it happened on the service, not when we synced.
    pub ts_ms: i64,
    /// Mail metadata (Gmail only; issue #319). Headers, never bodies or attachments.
    pub mail: Option<MailMeta>,
    /// Structured event facts (Google Calendar only; FR-MT-06).
    pub calendar: Option<CalendarMeta>,
}

/// One attendee of a calendar event (FR-MT-06). `email` never leaves the device; it is the key
/// identity resolution (`people.emails`) matches on.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CalendarAttendee {
    #[serde(default)]
    pub email: Option<String>,
    /// The display name the calendar shows, when it has one.
    #[serde(default)]
    pub name: Option<String>,
    /// The account owner (Google's `self: true`) — never a speaker candidate for "someone else".
    #[serde(default)]
    pub is_self: bool,
    /// `accepted` / `declined` / `tentative` / `needsAction`, as the calendar reports it.
    #[serde(default)]
    pub response: Option<String>,
}

/// The structured facts of one calendar event (FR-MT-06): when it runs, where, and who is invited.
/// Kept as state (`calendar_occurrences`), not as the event log's past-tense record, because it is
/// about the future. `location` can hold a meeting URL and is never sent to a third party.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CalendarMeta {
    /// Start / end, epoch ms. An all-day event runs from 00:00 UTC of its first day to 00:00 UTC
    /// after its last (the calendar gives dates, not instants).
    pub start_ms: i64,
    pub end_ms: i64,
    #[serde(default)]
    pub all_day: bool,
    /// The event's location or its conference link (`hangoutLink`), whichever it has.
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub attendees: Vec<CalendarAttendee>,
    /// The organiser cancelled it (Google `status: "cancelled"`).
    #[serde(default)]
    pub cancelled: bool,
}

/// Per-message mail metadata carried alongside a synced Gmail thread (issue #319, FR-TG-04).
/// Header facts only — sender / recipients / Gmail label ids / presence flags. No body, no
/// attachment bytes (invariant 2: only `has_ics`-style flags).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailMessageMeta {
    pub message_id: String,
    pub from_addr: String,
    #[serde(default)]
    pub from_name: Option<String>,
    #[serde(default)]
    pub to_addrs: Vec<String>,
    #[serde(default)]
    pub cc_addrs: Vec<String>,
    /// Gmail marked the message `SENT` (the user wrote it).
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub gmail_label_ids: Vec<String>,
    #[serde(default)]
    pub has_list_unsubscribe: bool,
    #[serde(default)]
    pub has_ics: bool,
    #[serde(default)]
    pub internal_date_ms: i64,
}

/// The mail metadata of one synced thread: its Gmail thread id and its messages, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailMeta {
    pub thread_id: String,
    #[serde(default)]
    pub messages: Vec<MailMessageMeta>,
}

/// An incremental read-sync cursor (FR-TG-09): the newest item timestamp already ingested and,
/// mid-tick, the provider's page token.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncCursor {
    pub watermark_ms: i64,
    pub page_token: Option<String>,
}

/// One page of a paged read-sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchPage {
    pub items: Vec<FetchedItem>,
    /// The token for the next page, or `None` when this was the last one.
    pub next_page_token: Option<String>,
}

/// A normalized, **source-tagged** record ready for the event log. The daemon computes the content
/// hash and appends it (source = the service's [`Service::source_str`]), so a synced email is a
/// first-class event alongside a captured window (FR-INT-05).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestItem {
    /// The `event_log.source` tag (`gmail` / `gcal` / …).
    pub source: &'static str,
    /// The `event_log.kind` (`email` / `calendar_event` / `message` / …).
    pub kind: &'static str,
    /// The item title → `window_title` (the log's short-label column).
    pub title: String,
    /// The item body → `content` (searchable text).
    pub body: String,
    /// The item's own timestamp (unix ms).
    pub ts_ms: i64,
    /// The service's own conversation id (a Gmail thread id) — the `thread_key` is derived from it
    /// (`gmail:<id>`, FR-MEM-40) so one conversation stays one thread whatever its subject does.
    /// Empty when the service has none.
    pub external_id: String,
    /// Mail metadata (Gmail only). Written to `mail_messages`, never to `event_log`.
    pub mail: Option<MailMeta>,
    /// Event facts (Google Calendar only). Written to `calendar_occurrences` (FR-MT-06).
    pub calendar: Option<CalendarMeta>,
}

/// Why a sync could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncFailure {
    /// The gate refused the read (unreleased wave / disconnected / unknown op).
    Denied(DenyReason),
    /// The transport failed to fetch (network / auth). The string is a non-sensitive reason (no
    /// item content) suitable for a log.
    Transport(String),
}

/// A transport that fetches a service's recent items. The real implementation is a remote-MCP
/// client (Category C — needs OAuth tokens); tests inject a fake. Kept as a seam so the whole
/// ingest composition is Linux-testable without a network.
pub trait IntegrationTransport {
    /// Fetch the recent items for a `read_sync`. Returns a non-sensitive error string on failure.
    fn read_sync(&self, service: Service) -> Result<Vec<FetchedItem>, String>;

    /// Fetch a specific item on demand (§6.9 `read_on_demand`, e.g. the Gmail thread currently on
    /// screen). `query` identifies it — a thread/message id, a file id, or a search string. The
    /// default is "not supported" (fakes / read-sync-only transports); the real remote-MCP
    /// transport overrides it. Kept a seam so the whole on-demand composition is Linux-testable.
    fn fetch_on_demand(&self, service: Service, query: &str) -> Result<Vec<FetchedItem>, String> {
        let _ = (service, query);
        Err("on-demand fetch not supported by this transport".to_string())
    }

    /// Fetch one page of a read-sync from `cursor` (FR-TG-09 incremental sync). The default
    /// ignores the cursor and returns the whole `read_sync` as a single, final page, so a
    /// transport that cannot page keeps working unchanged.
    fn read_sync_page(&self, service: Service, cursor: &SyncCursor) -> Result<FetchPage, String> {
        let _ = cursor;
        Ok(FetchPage { items: self.read_sync(service)?, next_page_token: None })
    }
}

/// The `event_log.kind` a service's items are tagged with.
fn item_kind(service: Service) -> &'static str {
    match service {
        Service::Gmail => "email",
        Service::GoogleCalendar => "calendar_event",
        Service::GoogleDrive => "file",
        Service::Slack => "message",
        Service::Notion => "page",
        Service::GitHub => "issue",
        Service::Linear => "issue",
        Service::Granola => "meeting_note",
        // Web reads (issue #555) are never synced into the log: they are on-demand fetches whose
        // text goes to the model, not to memory. The kind exists only so the match is total.
        Service::Web => "web_page",
    }
}

/// Check that a `read_sync` is allowed right now (FR-INT-03/06). A sync is permitted only when the
/// gate resolves `read_sync` to a background read; every other decision (including any required
/// confirmation, which a background sync must never trigger) refuses the sync.
pub fn authorize_sync(service: Service, ctx: &OpContext) -> Result<(), DenyReason> {
    match authorize_op(service, READ_SYNC, ctx) {
        OpDecision::Background => Ok(()),
        OpDecision::Denied(reason) => Err(reason),
        // read_sync is a Background op in every service table; anything else is a table/gate bug.
        // Refuse rather than silently ingesting under the wrong gating.
        _ => Err(DenyReason::UnknownOp),
    }
}

/// Normalize a fetched item into a source-tagged ingest record, dropping empty-body items (nothing
/// to remember). Empty titles are kept (some items have only a body).
fn normalize(service: Service, item: FetchedItem) -> Option<IngestItem> {
    let body = item.body.trim();
    if body.is_empty() {
        return None;
    }
    Some(IngestItem {
        source: service.source_str(),
        kind: item_kind(service),
        title: item.title,
        body: body.to_string(),
        ts_ms: item.ts_ms,
        external_id: item.external_id,
        mail: item.mail,
        calendar: item.calendar,
    })
}

/// The full ingest composition: gate the sync, fetch via the transport, and normalize the results
/// into source-tagged records the daemon can append. Returns the records (possibly empty) or a
/// [`SyncFailure`]. Does no DB I/O.
pub fn collect_sync<T: IntegrationTransport + ?Sized>(
    service: Service,
    ctx: &OpContext,
    transport: &T,
) -> Result<Vec<IngestItem>, SyncFailure> {
    authorize_sync(service, ctx).map_err(SyncFailure::Denied)?;
    let fetched = transport.read_sync(service).map_err(SyncFailure::Transport)?;
    Ok(fetched.into_iter().filter_map(|it| normalize(service, it)).collect())
}

/// [`collect_sync`] for one page of an incremental sync (FR-TG-09): gate, fetch the page at
/// `cursor`, normalize. Returns the records plus the next page token. Does no DB I/O.
pub fn collect_sync_page<T: IntegrationTransport + ?Sized>(
    service: Service,
    ctx: &OpContext,
    transport: &T,
    cursor: &SyncCursor,
) -> Result<(Vec<IngestItem>, Option<String>), SyncFailure> {
    authorize_sync(service, ctx).map_err(SyncFailure::Denied)?;
    let page = transport.read_sync_page(service, cursor).map_err(SyncFailure::Transport)?;
    let items = page.items.into_iter().filter_map(|it| normalize(service, it)).collect();
    Ok((items, page.next_page_token))
}

/// The op name an on-demand fetch gates on (the `read_on_demand` row; only Gmail + Drive have it).
pub const READ_ON_DEMAND: &str = "read_on_demand";

/// Check that a `read_on_demand` is allowed right now, without fetching. The same decision
/// [`collect_on_demand`] makes first; callers use it to record the egress before the request
/// leaves (issue #782).
pub fn authorize_on_demand(service: Service, ctx: &OpContext) -> Result<(), DenyReason> {
    // read_on_demand is gated L2 (a read the user's focus context requested); any non-allowed
    // decision refuses the fetch.
    match authorize_op(service, READ_ON_DEMAND, ctx) {
        OpDecision::RequiresLevel(_) => Ok(()),
        OpDecision::Denied(reason) => Err(reason),
        // read_on_demand is an L2 op in every table that has it; anything else is a gate bug —
        // refuse rather than fetch under the wrong gating.
        _ => Err(DenyReason::UnknownOp),
    }
}

/// The on-demand fetch composition (§6.9 `read_on_demand`, L2): gate → fetch a specific item via the
/// transport → normalize. Same shape as [`collect_sync`] but for a targeted fetch rather than the
/// background poll. Denied for a service without a `read_on_demand` op (Slack/Calendar), or when the
/// gate refuses (unreleased / disconnected / amber-write). Does no DB I/O.
pub fn collect_on_demand<T: IntegrationTransport + ?Sized>(
    service: Service,
    query: &str,
    ctx: &OpContext,
    transport: &T,
) -> Result<Vec<IngestItem>, SyncFailure> {
    authorize_on_demand(service, ctx).map_err(SyncFailure::Denied)?;
    let fetched = transport.fetch_on_demand(service, query).map_err(SyncFailure::Transport)?;
    Ok(fetched.into_iter().filter_map(|it| normalize(service, it)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{ConnState, ReauthReason};
    use crate::scope::Wave;

    fn connected() -> ConnState {
        ConnState::Connected { last_sync_ms: 1_000 }
    }
    fn amber() -> ConnState {
        ConnState::NeedsReauth { reason: ReauthReason::TokenExpired, last_sync_ms: 500 }
    }
    fn ctx(highest: Wave, conn: ConnState) -> OpContext {
        OpContext {
            highest_released: highest,
            conn,
            draft_stop: false,
            composio_consent: true,
            plan: shogun_agents::entitlement::entitlements(shogun_agents::entitlement::Plan::Pro, 0),
        }
    }
    fn item(id: &str, title: &str, body: &str, ts: i64) -> FetchedItem {
        FetchedItem { external_id: id.into(), title: title.into(), body: body.into(), ts_ms: ts, mail: None, calendar: None }
    }

    /// A transport that yields a fixed list, or errors.
    struct Fake {
        items: Vec<FetchedItem>,
        err: Option<String>,
    }
    impl IntegrationTransport for Fake {
        fn read_sync(&self, _service: Service) -> Result<Vec<FetchedItem>, String> {
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(self.items.clone()),
            }
        }
        fn fetch_on_demand(&self, _service: Service, _query: &str) -> Result<Vec<FetchedItem>, String> {
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(self.items.clone()),
            }
        }
    }

    #[test]
    fn released_connected_service_ingests_source_tagged_items() {
        let fake = Fake {
            items: vec![
                item("m1", "Roadmap", "Let's ship the deck Friday", 10),
                item("m2", "Invoice", "Payment due next week", 20),
            ],
            err: None,
        };
        let out = collect_sync(Service::Gmail, &ctx(Wave::One, connected()), &fake).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|i| i.source == "gmail" && i.kind == "email"));
        assert_eq!(out[0].body, "Let's ship the deck Friday");
        assert_eq!(out[0].title, "Roadmap");
        assert_eq!(out[0].ts_ms, 10);
    }

    #[test]
    fn unreleased_wave_refuses_sync() {
        // Slack is Wave 2; with only Wave 1 released a sync is denied before any fetch.
        let fake = Fake { items: vec![item("s1", "t", "b", 1)], err: None };
        let err = collect_sync(Service::Slack, &ctx(Wave::One, connected()), &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Denied(DenyReason::UnreleasedWave));
    }

    #[test]
    fn disconnected_service_refuses_sync() {
        let fake = Fake { items: vec![], err: None };
        let err = collect_sync(Service::Gmail, &ctx(Wave::One, ConnState::Disconnected), &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Denied(DenyReason::NotConnected));
    }

    #[test]
    fn gmail_sync_without_composio_consent_never_fetches() {
        // Issue #80 §2-A0: Gmail sync goes through Composio, so with no three-disclosure consent
        // the sync is denied before the transport is touched — zero egress. This pins it in the
        // Linux-tested gate, beyond the macOS shell's poller skip.
        let fake = Fake { items: vec![item("m1", "t", "b", 1)], err: None };
        let no_consent = OpContext { composio_consent: false, ..ctx(Wave::One, connected()) };
        let err = collect_sync(Service::Gmail, &no_consent, &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Denied(DenyReason::ConsentRequired));
        // Calendar (first-layer, no Composio) still syncs with the same context.
        assert!(collect_sync(Service::GoogleCalendar, &no_consent, &fake).is_ok());
    }

    #[test]
    fn amber_service_still_syncs_cached_reads() {
        // FR-INT-06: an amber (needs-reauth) service serves cached reads — a sync is still allowed.
        let fake = Fake { items: vec![item("m1", "t", "body", 5)], err: None };
        let out = collect_sync(Service::Gmail, &ctx(Wave::One, amber()), &fake).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn transport_error_propagates_without_content() {
        let fake = Fake { items: vec![], err: Some("token refresh failed".into()) };
        let err = collect_sync(Service::Gmail, &ctx(Wave::One, connected()), &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Transport("token refresh failed".into()));
    }

    #[test]
    fn empty_body_items_are_dropped() {
        let fake = Fake {
            items: vec![item("m1", "only a title", "   ", 1), item("m2", "", "real body", 2)],
            err: None,
        };
        let out = collect_sync(Service::Gmail, &ctx(Wave::One, connected()), &fake).unwrap();
        assert_eq!(out.len(), 1, "the whitespace-only body is dropped");
        assert_eq!(out[0].body, "real body");
    }

    #[test]
    fn on_demand_fetch_ingests_for_a_service_with_read_on_demand() {
        // Gmail has a read_on_demand (L2) row; a connected, released Gmail fetches on demand.
        let fake = Fake { items: vec![item("t1", "Thread", "the thread body", 7)], err: None };
        let out = collect_on_demand(Service::Gmail, "thread-123", &ctx(Wave::One, connected()), &fake).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "gmail");
        assert_eq!(out[0].body, "the thread body");
    }

    #[test]
    fn on_demand_denied_for_a_service_without_that_op() {
        // Calendar has no read_on_demand row → denied (unknown op), no fetch.
        let fake = Fake { items: vec![item("e", "t", "b", 1)], err: None };
        let err = collect_on_demand(Service::GoogleCalendar, "q", &ctx(Wave::One, connected()), &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Denied(DenyReason::UnknownOp));
    }

    #[test]
    fn on_demand_denied_when_disconnected() {
        let fake = Fake { items: vec![item("t", "t", "b", 1)], err: None };
        let err = collect_on_demand(Service::Gmail, "q", &ctx(Wave::One, ConnState::Disconnected), &fake).unwrap_err();
        assert_eq!(err, SyncFailure::Denied(DenyReason::NotConnected));
    }

    #[test]
    fn calendar_items_get_the_calendar_kind() {
        let fake = Fake { items: vec![item("e1", "Standup", "Daily standup 9am", 1)], err: None };
        let out = collect_sync(Service::GoogleCalendar, &ctx(Wave::One, connected()), &fake).unwrap();
        assert_eq!(out[0].source, "gcal");
        assert_eq!(out[0].kind, "calendar_event");
    }
}
