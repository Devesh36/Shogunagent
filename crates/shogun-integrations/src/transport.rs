//! [`RemoteMcpTransport`] — the first-layer connector adapter.
//!
//! It composes the pure pieces into the effect the daemon needs: given a service, pick its Google
//! MCP tool ([`crate::toolmap`]), call it over the [`McpRpc`] seam, and normalize the reply into
//! [`FetchedItem`]s ([`crate::result`]). It implements [`shogun_mcp::sync::IntegrationTransport`], so
//! it drops straight into the existing gate → fetch → normalize ingest ([`shogun_mcp::sync`]).
//!
//! Policy is **not** re-decided here: the daemon runs [`shogun_mcp::service_gate::authorize_op`]
//! before calling us (reads via [`shogun_mcp::sync::collect_sync`], writes via [`Self::execute`]).
//! This module only maps ops to tools and moves bytes.

use serde_json::{json, Value};
use shogun_mcp::scope::Service;
use shogun_mcp::sync::{FetchPage, FetchedItem, IntegrationTransport, SyncCursor};

use crate::rpc::McpRpc;
use crate::toolmap;

/// A first-layer transport backed by an [`McpRpc`] (a fake in tests, the live HTTPS client in
/// production).
pub struct RemoteMcpTransport<R: McpRpc> {
    rpc: R,
    /// How many recent items a background read-sync requests per service.
    page_size: u32,
}

impl<R: McpRpc> RemoteMcpTransport<R> {
    pub fn new(rpc: R) -> Self {
        Self { rpc, page_size: 25 }
    }

    /// The underlying RPC, for a provider call outside the op map (the Gmail account's own
    /// address after a sync).
    pub fn rpc(&self) -> &R {
        &self.rpc
    }

    /// Set the read-sync page size (default 25).
    pub fn with_page_size(mut self, n: u32) -> Self {
        self.page_size = n;
        self
    }

    /// The plain read-sync request: a page of recent items, and for Google Calendar the
    /// schedule window ([`calendar_read_arguments`]) at the wall clock.
    fn recent_window_arguments(&self, service: Service) -> Value {
        if service == Service::GoogleCalendar {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
            return calendar_read_arguments(now_ms, self.page_size);
        }
        json!({ "max_results": self.page_size })
    }

    /// Execute a first-layer **write** op that the daemon has already authorized (an L2 draft or an
    /// L3 create, post-confirmation). Maps the scope op to its Google MCP tool and calls it with
    /// `arguments`. Returns the raw `CallToolResult` for the caller to record/inspect. Gmail aliases
    /// are handled by the direct REST adapter; an unmapped operation is refused.
    pub fn execute(&self, service: Service, op_name: &str, arguments: Value) -> Result<Value, String> {
        let tool = if service == Service::Slack && op_name == "read_on_demand"
            && arguments.get("message_ts").is_some() {
            Some("slack_read_thread")
        } else { toolmap::tool_for(service, op_name) }
            .ok_or_else(|| format!("{}::{op_name} has no Google MCP tool", service.source_str()))?;
        self.rpc.call_tool(service, tool, arguments)
    }
}

/// The write half of the transport, as a seam so the runtime's confirmed-write path
/// ([`crate::runtime::ConnectorRuntime::execute_write`]) is testable without a live RPC.
pub trait WriteExecutor {
    /// Execute an already-confirmed first-layer write op (see [`RemoteMcpTransport::execute`]).
    fn execute(&self, service: Service, op_name: &str, arguments: Value) -> Result<Value, String>;
}

impl<R: McpRpc> WriteExecutor for RemoteMcpTransport<R> {
    fn execute(&self, service: Service, op_name: &str, arguments: Value) -> Result<Value, String> {
        RemoteMcpTransport::execute(self, service, op_name, arguments)
    }
}

impl<R: McpRpc> IntegrationTransport for RemoteMcpTransport<R> {
    fn read_sync(&self, service: Service) -> Result<Vec<FetchedItem>, String> {
        if matches!(service, Service::GitHub | Service::Linear) {
            return Ok(self.read_sync_page(service, &SyncCursor::default())?.items);
        }
        let tool = toolmap::read_sync_tool(service)
            .ok_or_else(|| format!("{} has no read_sync MCP tool", service.source_str()))?;
        // This provisional recent-window request still needs live schema verification before
        // rollout. Providers may reject unknown fields; developer opt-in is not proof of support.
        let args = self.recent_window_arguments(service);
        let result = self.rpc.call_tool(service, tool, args)?;
        crate::result::parse_items(&result)
    }

    /// One page of an incremental read-sync (FR-TG-09). Gmail narrows the fetch with an
    /// `after:` query derived from the cursor watermark ([`gmail_after_secs`]: a short overlap
    /// behind it is re-fetched and de-duplicated by content hash); other services keep their
    /// plain recent-window fetch. The provider's page token, when returned, rides along for the
    /// next call.
    fn read_sync_page(&self, service: Service, cursor: &SyncCursor) -> Result<FetchPage, String> {
        let tool = toolmap::read_sync_tool(service)
            .ok_or_else(|| format!("{} has no read_sync MCP tool", service.source_str()))?;
        let provider_specific = matches!(service, Service::GitHub | Service::Linear);
        let (mut args, login) = if provider_specific {
            crate::provider_read::arguments(&self.rpc, service, self.page_size, cursor)?
        } else {
            (self.recent_window_arguments(service), None)
        };
        if service == Service::Gmail && cursor.watermark_ms > 0 {
            args["query"] = Value::String(format!("after:{}", gmail_after_secs(cursor.watermark_ms)));
        }
        if let Some(t) = cursor
            .page_token
            .as_deref()
            .filter(|t| !t.is_empty() && service == Service::Gmail)
        {
            args["page_token"] = Value::String(t.to_string());
        }
        let result = self.rpc.call_tool(service, tool, args)?;
        let items = crate::result::parse_items(&result)?;
        let next_page_token = if provider_specific {
            crate::provider_read::next_page(
                service,
                &result,
                cursor,
                login.as_deref(),
                items.len(),
                self.page_size,
            )?
        } else if service == Service::Gmail {
            let payload = crate::result::payload(&result).ok();
            result
                .get("nextPageToken")
                .or_else(|| payload.as_ref().and_then(|value| value.get("nextPageToken")))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        } else {
            // ponytail: Calendar/Drive read one recent page per tick. With no time filter, following
            // pages would re-walk the whole history every tick; follow them once a filter exists.
            None
        };
        Ok(FetchPage {
            items,
            next_page_token,
        })
    }

    fn fetch_on_demand(&self, service: Service, query: &str) -> Result<Vec<FetchedItem>, String> {
        let tool = toolmap::tool_for(service, "read_on_demand")
            .ok_or_else(|| format!("{} has no read_on_demand MCP tool", service.source_str()))?;
        // `id` carries the thread/file id (or search string). The exact per-tool arg name
        // (thread_id / file_id / query) is confirmed against live tools/list at wire-up — same
        // caveat as read_sync's page arg.
        let args = match service {
            Service::Gmail => json!({ "thread_id": query }),
            Service::GoogleDrive => json!({ "file_id": query }),
            Service::Notion | Service::Slack => json!({ "query": query }),
            Service::Granola => json!({ "meeting_ids": [query] }),
            _ => json!({ "id": query }),
        };
        let result = self.rpc.call_tool(service, tool, args)?;
        if service == Service::Granola {
            return Ok(vec![crate::result::granola_note(&result, query)?]);
        }
        crate::result::parse_items(&result)
    }
}

/// How far back a Google Calendar read-sync starts: yesterday's events still feed meeting
/// linking and the Evening Wrap.
pub const CALENDAR_LOOKBACK_MS: i64 = 24 * 60 * 60 * 1000;
/// How far ahead it reaches: the schedule surfaces show a week, with room to spare.
pub const CALENDAR_LOOKAHEAD_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// The most events one calendar read asks for (Google's own default page size).
pub const CALENDAR_MAX_RESULTS: u32 = 250;

/// The Google Calendar read-sync request: the events *around now*, not the calendar's oldest.
///
/// Without a window, `list_events` returns events from the start of the calendar's history in
/// no useful order, so 25 of them never reached this week. Recurring events are expanded into
/// their instances (`single_events`), each with its own id and start, and ordered by start time
/// (`order_by`, which Google accepts only together with `single_events`). The page scales with
/// the configured size so the one-item Connect probe stays small.
///
/// Field names are the hub's stable ones; [`crate::provider_read::bind_google_arguments`] binds
/// them to what the server advertises and drops the window fields it does not, so an older
/// schema degrades to the previous unwindowed read instead of failing the sync. **Needs live
/// verification** against the Google Calendar MCP server's `list_events` schema.
pub fn calendar_read_arguments(now_ms: i64, page_size: u32) -> Value {
    json!({
        "max_results": page_size.saturating_mul(10).clamp(1, CALENDAR_MAX_RESULTS),
        "start": rfc3339_utc(now_ms.saturating_sub(CALENDAR_LOOKBACK_MS)),
        "end": rfc3339_utc(now_ms.saturating_add(CALENDAR_LOOKAHEAD_MS)),
        "single_events": true,
        "order_by": "startTime",
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a unix-ms instant (whole seconds, UTC).
fn rfc3339_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// How far behind the watermark each incremental Gmail sync starts. Mail can land with a
/// timestamp slightly older than the newest item already seen (delivery delays); the re-fetched
/// items are dropped by content hash.
pub const GMAIL_AFTER_OVERLAP_SECS: i64 = 3_600;

/// The `after:` bound for a Gmail query, in epoch seconds, one overlap behind a unix-ms
/// watermark. Not a `YYYY/MM/DD` date: Gmail reads a bare date as midnight Pacific time, so a
/// watermark early in the UTC day produced a window that started hours after it, and the
/// watermark then moved past mail that was never fetched (#769). Seconds carry no time zone.
pub fn gmail_after_secs(watermark_ms: i64) -> i64 {
    (watermark_ms.div_euclid(1_000) - GMAIL_AFTER_OVERLAP_SECS).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the last call and returns a canned result (or error).
    struct FakeRpc {
        last: RefCell<Option<(Service, String)>>,
        reply: Result<Value, String>,
    }
    impl FakeRpc {
        fn ok(reply: Value) -> Self {
            Self { last: RefCell::new(None), reply: Ok(reply) }
        }
        fn err(msg: &str) -> Self {
            Self { last: RefCell::new(None), reply: Err(msg.to_string()) }
        }
    }
    impl McpRpc for FakeRpc {
        fn call_tool(&self, service: Service, tool: &str, _args: Value) -> Result<Value, String> {
            *self.last.borrow_mut() = Some((service, tool.to_string()));
            self.reply.clone()
        }
    }

    #[test]
    fn read_sync_calls_the_mapped_tool_and_normalizes() {
        let reply = json!({
            "structuredContent": [ { "id": "m1", "subject": "Hi", "snippet": "hello", "internalDate": 5 } ]
        });
        let rpc = FakeRpc::ok(reply);
        let t = RemoteMcpTransport::new(rpc);
        let items = t.read_sync(Service::Gmail).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "hello");
        // it dispatched to Gmail's read_sync tool
        assert_eq!(t.rpc.last.borrow().as_ref().unwrap(), &(Service::Gmail, "search_threads".to_string()));
    }

    #[test]
    fn the_calendar_read_asks_for_the_schedule_around_now() {
        // 2026-10-13T00:00:00Z.
        let now = 1_791_849_600_000;
        assert_eq!(rfc3339_utc(now), "2026-10-13T00:00:00Z");
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000 + 3_723_000), "2000-02-29T01:02:03Z");
        let args = calendar_read_arguments(now, 25);
        assert_eq!(
            args,
            json!({
                "max_results": 250,
                "start": "2026-10-12T00:00:00Z",
                "end": "2026-10-27T00:00:00Z",
                "single_events": true,
                "order_by": "startTime",
            })
        );
        // The Connect probe's one-item read stays small.
        assert_eq!(calendar_read_arguments(now, 1)["max_results"], 10);
        // Every hub field binds to an advertised Google name.
        let tools = vec![json!({"name":"list_events","inputSchema":{"properties":{
            "calendarId":{"type":"string"},"maxResults":{"type":"integer"},
            "timeMin":{"type":"string"},"timeMax":{"type":"string"},
            "singleEvents":{"type":"boolean"},"orderBy":{"type":"string"}}}})];
        assert_eq!(
            crate::provider_read::bind_google_arguments("list_events", &args, &tools).unwrap(),
            json!({"calendarId":"primary","maxResults":250,"timeMin":"2026-10-12T00:00:00Z",
                "timeMax":"2026-10-27T00:00:00Z","singleEvents":true,"orderBy":"startTime"})
        );
    }

    #[test]
    fn transport_error_propagates() {
        let t = RemoteMcpTransport::new(FakeRpc::err("network down"));
        assert_eq!(t.read_sync(Service::GoogleCalendar).unwrap_err(), "network down");
    }

    #[test]
    fn fetch_on_demand_calls_the_read_on_demand_tool() {
        let reply = json!({ "structuredContent": [ { "id": "t", "subject": "s", "body": "thread text" } ] });
        let t = RemoteMcpTransport::new(FakeRpc::ok(reply));
        let items = t.fetch_on_demand(Service::Gmail, "thread-9").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "thread text");
        assert_eq!(t.rpc.last.borrow().as_ref().unwrap(), &(Service::Gmail, "get_thread".to_string()));
    }

    #[test]
    fn granola_meeting_note_is_fetched_only_on_demand() {
        let rpc = FakeRpc::ok(json!({ "content": [{ "type": "text", "text": "Notes from Granola" }] }));
        let t = RemoteMcpTransport::new(rpc);
        assert!(t.read_sync(Service::Granola).is_err());
        assert!(t.rpc.last.borrow().is_none());
        let notes = t.fetch_on_demand(Service::Granola, "meeting-1").unwrap();
        assert_eq!(notes[0].external_id, "meeting-1");
        assert_eq!(notes[0].body, "Notes from Granola");
        assert_eq!(t.rpc.last.borrow().as_ref().unwrap(), &(Service::Granola, "get_meetings".into()));
    }

    #[test]
    fn fetch_on_demand_errors_when_service_has_no_such_tool() {
        let t = RemoteMcpTransport::new(FakeRpc::ok(json!({})));
        assert!(t.fetch_on_demand(Service::GoogleCalendar, "x").is_err());
        assert!(t.rpc.last.borrow().is_none());
    }

    #[test]
    fn execute_maps_write_op_to_tool() {
        let t = RemoteMcpTransport::new(FakeRpc::ok(json!({ "isError": false, "content": [] })));
        t.execute(Service::GoogleDrive, "file_create", json!({ "name": "Sync", "content":"notes" })).unwrap();
        assert_eq!(
            t.rpc.last.borrow().as_ref().unwrap(),
            &(Service::GoogleDrive, "create_file".to_string())
        );
    }

    #[test]
    fn execute_maps_direct_gmail_send() {
        let t = RemoteMcpTransport::new(FakeRpc::ok(json!({"id":"sent-1"})));
        t.execute(Service::Gmail, "send", json!({})).unwrap();
        assert_eq!(
            t.rpc.last.borrow().as_ref().unwrap(),
            &(Service::Gmail, "send_email".into())
        );
    }

    #[test]
    fn gmail_after_starts_one_overlap_behind_the_watermark_in_seconds() {
        // 2026-09-04T12:00:00Z
        assert_eq!(gmail_after_secs(1_788_523_200_000), 1_788_523_200 - GMAIL_AFTER_OVERLAP_SECS);
        // Early in the UTC day, where a Pacific-midnight date would have started hours after
        // the watermark (#769): 2026-09-01T01:00:00Z → the window opens at 00:00Z.
        assert_eq!(gmail_after_secs(1_788_224_400_000), 1_788_220_800);
        assert!(gmail_after_secs(1_788_224_400_999) * 1_000 < 1_788_224_400_000);
        assert_eq!(gmail_after_secs(1_000), 0, "never a negative bound");
    }

    #[test]
    fn read_sync_page_passes_after_query_and_page_token_and_returns_next_token() {
        struct Recorder {
            args: RefCell<Option<Value>>,
        }
        impl McpRpc for Recorder {
            fn call_tool(&self, _s: Service, _t: &str, args: Value) -> Result<Value, String> {
                *self.args.borrow_mut() = Some(args);
                Ok(json!({ "structuredContent": [ { "threadId": "t", "subject": "s", "body": "b", "ts_ms": 5 } ], "nextPageToken": "p2" }))
            }
        }
        let rpc = Recorder { args: RefCell::new(None) };
        let t = RemoteMcpTransport::new(rpc);
        let page = t
            .read_sync_page(Service::Gmail, &SyncCursor { watermark_ms: 1_788_523_200_000, page_token: Some("p1".into()) })
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.next_page_token.as_deref(), Some("p2"));
        let args = t.rpc.args.borrow().clone().unwrap();
        assert_eq!(args["query"], format!("after:{}", 1_788_523_200 - GMAIL_AFTER_OVERLAP_SECS));
        assert_eq!(args["page_token"], "p1");
    }
}
