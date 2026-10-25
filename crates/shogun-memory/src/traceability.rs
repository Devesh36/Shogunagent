//! Traceability-log repository (§6.14, FR-TR-01/04). The storage side of the traceability record:
//! shogun-memory owns the `traceability_log` table (invariant 1 — data gravity in the Rust core),
//! so the write/read of trace rows lives here, not in the caller.
//!
//! The table (V1__init.sql) stores the chunk's byte length + an xxh64 digest, never the sent text
//! (privacy rule / G8). No column holds a body. The one free-text column, `url` (V71, issue #928),
//! is the masked address of a public web fetch, filled only on `route = 'web'` rows; V1's comment
//! predates it and is frozen. `route` is constrained by a CHECK to the known routes; [`Route`] mirrors that
//! set (V1 + additive migrations), so an insert always produces a valid value and a read always
//! parses back.
//!
//! The LLM layer's `TraceRecord` (shogun-core) maps 1:1 onto [`TraceRow`]; the daemon bridges the
//! two when it wires the sink. Keeping the enum here (rather than depending on shogun-core) keeps
//! the storage layer free of an upward dependency.

use rusqlite::{params, Connection};

/// A send route, mirroring the `traceability_log.route` CHECK set exactly (FR-TR-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    BatchApi,
    /// Batch lane via the Select-operated relay (V18; docs/batch-relay-design.md §3.3) — the
    /// shipping Standard/Pro path. Distinct from [`Route::BatchApi`] (direct, development-only)
    /// because the disclosure differs: the chunk crosses the operator's server and the viewer
    /// must say "via operator server". Not third-party — the relay is the operator's own
    /// infrastructure and stores no chunk content (NFR-PRV-04).
    BatchRelay,
    MessagesApi,
    /// Company live AI through the operator gateway.
    LiveRelay,
    Mcp,
    Composio,
    Billing,
    /// Third-party meeting ASR (Deepgram; V13). Always disclosed.
    Asr,
    /// Agent inference delegated to a local, already-signed-in vendor CLI running on the user's own
    /// subscription (Issue #110). Distinct from [`Route::MessagesApi`]: SHOGUN holds no credential
    /// and does not open the socket — a separate local process does, against the user's plan quota.
    LocalAgent,
    /// CS / bug-report intake (support窓口; V21): a report the user explicitly composed and sent
    /// to the operator's server. User-authored text, user-initiated — never third-party.
    Support,
    /// A public web read for chat (issue #555): URL out, host + digest on the row.
    Web,
}

impl Route {
    /// The exact string stored in the column.
    pub fn as_str(self) -> &'static str {
        match self {
            Route::BatchApi => "batch_api",
            Route::BatchRelay => "batch_relay",
            Route::MessagesApi => "messages_api",
            Route::LiveRelay => "live_relay",
            Route::Mcp => "mcp",
            Route::Composio => "composio",
            Route::Billing => "billing",
            Route::Asr => "asr",
            Route::LocalAgent => "local_agent",
            Route::Support => "support",
            Route::Web => "web",
        }
    }

    /// Parse the stored string back. `None` for an unknown value (shouldn't occur — the CHECK
    /// prevents writing one — but reads stay total).
    pub fn parse(s: &str) -> Option<Route> {
        Some(match s {
            "batch_api" => Route::BatchApi,
            "batch_relay" => Route::BatchRelay,
            "messages_api" => Route::MessagesApi,
            "live_relay" => Route::LiveRelay,
            "mcp" => Route::Mcp,
            "composio" => Route::Composio,
            "billing" => Route::Billing,
            "asr" => Route::Asr,
            "local_agent" => Route::LocalAgent,
            "support" => Route::Support,
            "web" => Route::Web,
            _ => return None,
        })
    }

    /// Third-party routes that must surface a disclosure badge (FR-C2-04 + meeting ASR).
    pub fn is_third_party(self) -> bool {
        matches!(self, Route::Composio | Route::Asr)
    }
}

/// What a row records (`traceability_log.kind`, V67 / issue #29 §5). `Egress` is a chunk that
/// left the device; `Retry` and `FellBack` are markers on a shared-service route with no chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceKind {
    #[default]
    Egress,
    Retry,
    FellBack,
}

impl TraceKind {
    /// The exact string stored in the column.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceKind::Egress => "egress",
            TraceKind::Retry => "retry",
            TraceKind::FellBack => "fell_back",
        }
    }

    /// Parse the stored string back. An unknown value reads as `Egress` (the CHECK prevents
    /// writing one; reads stay total).
    pub fn parse(s: &str) -> TraceKind {
        match s {
            "retry" => TraceKind::Retry,
            "fell_back" => TraceKind::FellBack,
            _ => TraceKind::Egress,
        }
    }
}

/// A traceability row: byte length + digest of an outbound chunk, never its text (G8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRow {
    /// Persisted identity; zero for a row that has not been inserted yet.
    pub id: i64,
    pub ts: i64,
    pub route: Route,
    pub purpose: String,
    pub destination: String,
    pub chunk_bytes: i64,
    /// Lower-hex xxh64 of the chunk (a digest, never the text).
    pub chunk_xxh64: String,
    pub third_party: bool,
    /// Egress (default) or a retry / fell-back marker (V67).
    pub kind: TraceKind,
    /// The full URL of a public web fetch, secrets masked (V71, issue #928). `None` otherwise.
    pub url: Option<String>,
}

impl TraceRow {
    /// Build a row for a chunk. `third_party` defaults to the route (Composio ⇒ true) but the
    /// caller may still be explicit via the struct.
    pub fn new(
        ts: i64,
        route: Route,
        purpose: impl Into<String>,
        destination: impl Into<String>,
        chunk_bytes: i64,
        chunk_xxh64: impl Into<String>,
    ) -> Self {
        Self {
            id: 0,
            ts,
            route,
            purpose: purpose.into(),
            destination: destination.into(),
            chunk_bytes,
            chunk_xxh64: chunk_xxh64.into(),
            third_party: route.is_third_party(),
            kind: TraceKind::Egress,
            url: None,
        }
    }
}

/// Append a traceability row (one per external send, AR-11). Returns the new row id.
pub fn insert(conn: &Connection, row: &TraceRow) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO traceability_log
           (ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party, kind, url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.ts,
            row.route.as_str(),
            row.purpose,
            row.destination,
            row.chunk_bytes,
            row.chunk_xxh64,
            row.third_party as i64,
            row.kind.as_str(),
            row.url,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Count traceability rows written at or after `ts` — the number of outbound chunks a Dream Cycle
/// sent during a run (FR-DC-06), given the run's start time. Only egress rows count: a retry /
/// fell-back marker (V67) is not a chunk sent.
pub fn count_since(conn: &Connection, ts: i64) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT count(*) FROM traceability_log WHERE ts >= ?1 AND kind = 'egress'",
        params![ts],
        |r| r.get(0),
    )
}

/// A read-back filter for the viewer (FR-TR-02). `None` fields don't constrain; `destination` is a
/// case-insensitive substring (SQL `LIKE`).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub route: Option<Route>,
    pub purpose: Option<String>,
    pub destination: Option<String>,
    /// Only known read purposes are classified as reads; unknown purposes stay sends.
    pub reads: Option<bool>,
    /// Exclusive (timestamp, id) cursor: new arrivals cannot shift an older page.
    pub before: Option<(i64, i64)>,
}

/// List rows matching `filter`, most-recent first (FR-TR-02 時系列一覧).
pub fn list(conn: &Connection, filter: &Filter) -> Result<Vec<TraceRow>, rusqlite::Error> {
    list_page(conn, filter, i64::MAX, 0)
}

/// Whole-log privacy disclosures; paging must never turn these into a reassurance. The second
/// count is every row that crossed the operator's servers: the Batch relay and the live gateway
/// (#1101), both shown as the operator relay on the egress screen.
pub fn disclosure_totals(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT COUNT(CASE WHEN third_party != 0 THEN 1 END),
                COUNT(CASE WHEN third_party = 0 AND route IN ('batch_relay', 'live_relay') THEN 1 END)
         FROM traceability_log",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Bounded viewer page, with stable ordering for equal timestamps.
pub fn list_page(conn: &Connection, filter: &Filter, limit: i64, offset: i64) -> Result<Vec<TraceRow>, rusqlite::Error> {
    Ok(list_page_with_ids(conn, filter, limit, offset)?.into_iter().map(|(_, row)| row).collect())
}

/// Viewer rows retain their database identity across refreshes and page boundaries.
pub fn list_page_with_ids(conn: &Connection, filter: &Filter, limit: i64, offset: i64) -> Result<Vec<(i64, TraceRow)>, rusqlite::Error> {
    // Build the WHERE incrementally with bound params (never string-interpolate user input).
    let mut sql = String::from(
        "SELECT ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party, kind, url, id
         FROM traceability_log",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(r) = filter.route {
        clauses.push(format!("route = ?{}", binds.len() + 1));
        binds.push(r.as_str().to_string().into());
    }
    if let Some(p) = &filter.purpose {
        clauses.push(format!("purpose = ?{}", binds.len() + 1));
        binds.push(p.clone().into());
    }
    if let Some(d) = &filter.destination {
        clauses.push(format!("destination LIKE ?{}", binds.len() + 1));
        binds.push(format!("%{d}%").into());
    }
    if let Some((ts, id)) = filter.before {
        let ts_bind = binds.len() + 1;
        let id_bind = ts_bind + 1;
        clauses.push(format!("(ts < ?{ts_bind} OR (ts = ?{ts_bind} AND id < ?{id_bind}))"));
        binds.push(ts.into());
        binds.push(id.into());
    }
    if let Some(reads) = filter.reads {
        let purposes = "purpose IN ('gmail_read', 'first_layer_read', 'mcp_tool_read', 'gmail_tool_read', 'web_page')";
        clauses.push(if reads { purposes.to_string() } else { format!("NOT ({purposes})") });
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(&format!(" ORDER BY ts DESC, id DESC LIMIT ?{} OFFSET ?{}", binds.len() + 1, binds.len() + 2));
    binds.push(limit.into());
    binds.push(offset.into());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), map_row)?;
    rows.collect()
}

/// Lookup a cursor anchor without loading previous pages.
pub fn find(conn: &Connection, id: i64) -> rusqlite::Result<Option<TraceRow>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT ts, route, purpose, destination, chunk_bytes, chunk_xxh64, third_party, kind, url, id FROM traceability_log WHERE id = ?1", [id], map_row)
        .optional().map(|row| row.map(|(_, row)| row))
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, TraceRow)> {
    let route_str: String = r.get(1)?;
    let id: i64 = r.get(9)?;
    Ok((id, TraceRow {
        id,
        ts: r.get(0)?,
        route: Route::parse(&route_str).unwrap_or(Route::BatchApi),
        purpose: r.get(2)?,
        destination: r.get(3)?,
        chunk_bytes: r.get(4)?,
        chunk_xxh64: r.get(5)?,
        third_party: r.get::<_, i64>(6)? != 0,
        kind: TraceKind::parse(&r.get::<_, String>(7)?),
        url: r.get(8)?,
    }))
}

/// Routine reads are identified by a closed purpose list, never by byte size or route alone.
pub fn routine_read(row: &TraceRow) -> bool {
    row.kind == TraceKind::Egress && matches!(row.purpose.as_str(),
        "gmail_read" | "first_layer_read" | "mcp_tool_read" | "gmail_tool_read" | "web_page")
}

/// Collapse consecutive identical routine reads within one minute of the group's newest row.
/// Sends and retry/fallback markers always retain separate rows.
pub fn collapse_reads(rows: Vec<(i64, TraceRow)>) -> Vec<(i64, TraceRow, usize)> {
    collapse_reads_after(rows, None)
}

/// Continue the last group from a previous page without reading its earlier members again.
pub fn collapse_reads_after(rows: Vec<(i64, TraceRow)>, seed: Option<(i64, TraceRow, usize)>) -> Vec<(i64, TraceRow, usize)> {
    let mut groups: Vec<(i64, TraceRow, usize)> = seed.into_iter().collect();
    for (id, row) in rows {
        if let Some((_, previous, count)) = groups.last_mut() {
            if routine_read(&row) && routine_read(previous)
                && row.route == previous.route && row.purpose == previous.purpose
                && row.destination == previous.destination && row.url == previous.url
                && row.third_party == previous.third_party
                && previous.ts.saturating_sub(row.ts) >= 0
                && previous.ts.saturating_sub(row.ts) <= 60_000 {
                *count += 1;
                previous.id = row.id;
                continue;
            }
        }
        groups.push((id, row, 1));
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_filter_runs_before_paging_and_combines_with_route() {
        let conn = crate::open_in_memory().expect("database");
        for ts in 0..205 {
            insert(&conn, &row(ts, Route::Mcp, "gmail_read", "gmail")).expect("read");
        }
        insert(&conn, &row(300, Route::Mcp, "integration_write", "gmail")).expect("send");
        let filter = Filter { reads: Some(true), route: Some(Route::Mcp), ..Default::default() };
        let first = list_page(&conn, &filter, 200, 0).expect("first page");
        assert_eq!(first.len(), 200);
        assert_eq!(first[0].ts, 204);
        assert_eq!(list_page(&conn, &filter, 200, 200).expect("second page").len(), 5);
        let sends = list_page(&conn, &Filter { reads: Some(false), ..Default::default() }, 200, 0).expect("sends");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].purpose, "integration_write");
    }

    #[test]
    fn collapse_is_consecutive_bounded_and_never_combines_sends_or_markers() {
        let read = |id, ts| (id, row(ts, Route::Mcp, "gmail_read", "gmail"));
        let mut other = read(7, 5);
        other.1.destination = "other".into();
        let mut retry = read(8, 4);
        retry.1.kind = TraceKind::Retry;
        let groups = collapse_reads(vec![read(1, 120_000), read(2, 60_000), read(3, 59_999),
            (4, row(50_000, Route::Mcp, "integration_write", "gmail")),
            (5, row(49_999, Route::Mcp, "integration_write", "gmail")), read(6, 6), other, retry]);
        assert_eq!(groups.iter().map(|(_, _, count)| *count).collect::<Vec<_>>(), vec![2, 1, 1, 1, 1, 1, 1]);
        assert_eq!(groups[0].0, 1);
        let groups = collapse_reads(vec![read(1, 10), (2, row(9, Route::Web, "gmail_read", "gmail")), read(3, 8)]);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn read_groups_continue_across_pages_without_losing_sends() {
        let conn = crate::open_in_memory().expect("database");
        for ts in 0..205 {
            insert(&conn, &row(ts, Route::Mcp, "gmail_read", "gmail")).expect("read");
        }
        let first = collapse_reads(list_page_with_ids(&conn, &Filter::default(), 200, 0).expect("first"));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].2, 200);
        let anchor = find(&conn, first[0].0).expect("anchor read").expect("anchor exists");
        let rest = list_page_with_ids(&conn, &Filter::default(), 200, 200).expect("next");
        let merged = collapse_reads_after(rest, Some((first[0].0, anchor, first[0].2)));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0, first[0].0);
        assert_eq!(merged[0].2, 205);
        assert!(find(&conn, -1).expect("missing cursor").is_none());
        let send = row(1, Route::Mcp, "integration_write", "gmail");
        let groups = collapse_reads_after(vec![(2, send.clone())], Some((1, send, 1)));
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn cursor_pages_ignore_new_arrivals_and_break_timestamp_ties() {
        let conn = crate::open_in_memory().expect("database");
        for _ in 0..205 {
            insert(&conn, &row(100, Route::Mcp, "gmail_read", "gmail")).expect("read");
        }
        let first = list_page_with_ids(&conn, &Filter::default(), 200, 0).expect("first");
        let (last_id, last) = first.last().expect("last row");
        insert(&conn, &row(200, Route::Mcp, "gmail_read", "gmail")).expect("new arrival");
        let next = list_page_with_ids(&conn, &Filter { before: Some((last.ts, *last_id)), reads: Some(true), ..Default::default() }, 200, 0).expect("next");
        assert_eq!(next.len(), 5);
        assert!(next.iter().all(|(id, _)| id < last_id));
    }

    fn row(ts: i64, route: Route, purpose: &str, dest: &str) -> TraceRow {
        TraceRow::new(ts, route, purpose, dest, 128, "0123456789abcdef")
    }

    #[test]
    fn disclosure_totals_include_rows_outside_the_page() {
        let conn = crate::open_in_memory().expect("database");
        insert(&conn, &row(1, Route::Composio, "agent", "third.example")).expect("third party");
        insert(&conn, &row(2, Route::BatchRelay, "indexing", "relay.example")).expect("relay");
        insert(&conn, &row(3, Route::LiveRelay, "chat", "gateway.example")).expect("gateway");
        for ts in 4..60 {
            insert(&conn, &row(ts, Route::MessagesApi, "chat", "direct.example")).expect("direct");
        }
        let page = list_page(&conn, &Filter::default(), 50, 0).expect("page");
        assert!(page.iter().all(|row| !row.third_party && !matches!(row.route, Route::BatchRelay | Route::LiveRelay)));
        assert_eq!(disclosure_totals(&conn).expect("totals"), (1, 2));
    }

    #[test]
    fn pages_bound_the_read_and_break_timestamp_ties_by_id() {
        let conn = crate::open_in_memory().expect("test");
        for i in 0..105 {
            insert(&conn, &row(100, Route::MessagesApi, &format!("send-{i}"), "example.com")).expect("test");
        }
        let filter = Filter::default();
        let first = list_page(&conn, &filter, 50, 0).expect("test");
        let next = list_page(&conn, &filter, 50, 50).expect("test");
        assert_eq!((first.len(), next.len()), (50, 50));
        assert_eq!(first[0].purpose, "send-104");
        assert_eq!(next[0].purpose, "send-54");
        assert_eq!(list_page(&conn, &filter, 50, 100).expect("test").len(), 5);
    }

    #[test]
    fn insert_returns_incrementing_ids_and_persists_digest_only() {
        let conn = crate::open_in_memory().unwrap();
        let a = insert(&conn, &row(100, Route::BatchApi, "dream_cycle", "api.anthropic.com")).unwrap();
        let b = insert(&conn, &row(200, Route::MessagesApi, "agent", "api.anthropic.com")).unwrap();
        assert!(b > a);
        let ids: Vec<i64> = list(&conn, &Filter::default()).unwrap().iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![b, a]);
        // the table has no text column — assert only digest+len are stored
        let (bytes, digest): (i64, String) = conn
            .query_row("SELECT chunk_bytes, chunk_xxh64 FROM traceability_log WHERE id = ?1", [a], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(bytes, 128);
        assert_eq!(digest, "0123456789abcdef");
    }

    #[test]
    fn a_web_row_round_trips_its_url_and_others_stay_null() {
        // V71, issue #928.
        let conn = crate::open_in_memory().unwrap();
        let mut web = row(100, Route::Web, "web_page", "www.linkedin.com");
        web.url = Some("https://www.linkedin.com/in/jane-doe".into());
        insert(&conn, &web).unwrap();
        insert(&conn, &row(200, Route::MessagesApi, "agent", "api.anthropic.com")).unwrap();
        // An older binary's INSERT names its columns and leaves the URL NULL.
        conn.execute(
            "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64)
             VALUES (300, 'web', 'web_page', 'example.com', 1, 'x')",
            [],
        )
        .unwrap();
        let urls: Vec<Option<String>> = list(&conn, &Filter::default()).unwrap().into_iter().map(|r| r.url).collect();
        assert_eq!(urls, vec![None, None, Some("https://www.linkedin.com/in/jane-doe".into())]);
    }

    #[test]
    fn markers_round_trip_their_kind_and_do_not_count_as_chunks_sent() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(100, Route::BatchRelay, "dream_cycle", "api.example")).unwrap();
        let mut retry = TraceRow::new(110, Route::BatchRelay, "dream_cycle", "api.example", 0, "");
        retry.kind = TraceKind::Retry;
        insert(&conn, &retry).unwrap();
        let mut fell = TraceRow::new(120, Route::BatchRelay, "dream_cycle", "api.example", 0, "");
        fell.kind = TraceKind::FellBack;
        insert(&conn, &fell).unwrap();

        let rows = list(&conn, &Filter::default()).unwrap();
        let kinds: Vec<TraceKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![TraceKind::FellBack, TraceKind::Retry, TraceKind::Egress]);
        // FR-DC-06 "chunks sent" counts egress only.
        assert_eq!(count_since(&conn, 0).unwrap(), 1);
    }

    #[test]
    fn a_pre_v64_insert_without_kind_reads_as_egress() {
        // An older binary inserts without the column; the default keeps its meaning.
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64)
             VALUES (1, 'batch_relay', 'p', 'd', 1, 'h')",
            [],
        )
        .unwrap();
        let rows = list(&conn, &Filter::default()).unwrap();
        assert_eq!(rows[0].kind, TraceKind::Egress);
        let bad = conn.execute(
            "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64, kind)
             VALUES (1, 'batch_relay', 'p', 'd', 1, 'h', 'sneaky')",
            [],
        );
        assert!(bad.is_err(), "CHECK must reject an unknown kind");
    }

    #[test]
    fn composio_row_defaults_third_party_true() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(1, Route::Composio, "agent", "gmail.com")).unwrap();
        let tp: i64 = conn.query_row("SELECT third_party FROM traceability_log", [], |r| r.get(0)).unwrap();
        assert_eq!(tp, 1);
    }

    #[test]
    fn check_constraint_rejects_an_unknown_route_string() {
        // Route::as_str only produces valid values; a raw bad string must be refused by the CHECK.
        let conn = crate::open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO traceability_log (ts, route, purpose, destination, chunk_bytes, chunk_xxh64)
             VALUES (1, 'sneaky', 'p', 'd', 1, 'h')",
            [],
        );
        assert!(err.is_err(), "CHECK must reject an unknown route");
    }

    #[test]
    fn local_agent_route_is_accepted_and_is_not_third_party() {
        // V12 widened the CHECK. A subscription-delegated send must be storable, and must NOT get
        // the third-party badge: the user's own vendor on the user's own plan is not a relay
        // (unlike Composio, FR-C2-04).
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(1, Route::LocalAgent, "agent", "api.anthropic.com")).unwrap();
        let (route, tp): (String, i64) = conn
            .query_row("SELECT route, third_party FROM traceability_log", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(route, "local_agent");
        assert_eq!(tp, 0);
    }

    #[test]
    fn batch_relay_route_is_accepted_and_is_not_third_party() {
        // V18 widened the CHECK. A relay-routed chunk must be storable and distinguishable from
        // the direct 'batch_api' route (docs/batch-relay-design.md §3.3); the relay is the
        // operator's own server, so it does NOT get the third-party badge (that stays Composio/ASR)
        // — the viewer labels it "via operator server" from the route itself.
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(1, Route::BatchRelay, "consolidation", "api.shogunaios.com")).unwrap();
        let (route, tp): (String, i64) = conn
            .query_row("SELECT route, third_party FROM traceability_log", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(route, "batch_relay");
        assert_eq!(tp, 0);
    }

    #[test]
    fn list_is_most_recent_first() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(100, Route::BatchApi, "indexing", "api.anthropic.com")).unwrap();
        insert(&conn, &row(300, Route::Composio, "agent", "gmail.com")).unwrap();
        insert(&conn, &row(200, Route::MessagesApi, "chat", "api.anthropic.com")).unwrap();
        let all = list(&conn, &Filter::default()).unwrap();
        let ts: Vec<i64> = all.iter().map(|r| r.ts).collect();
        assert_eq!(ts, vec![300, 200, 100]);
    }

    #[test]
    fn list_filters_by_route_and_destination() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(1, Route::BatchApi, "indexing", "api.anthropic.com")).unwrap();
        insert(&conn, &row(2, Route::Composio, "agent", "gmail.com")).unwrap();
        insert(&conn, &row(3, Route::MessagesApi, "chat", "api.anthropic.com")).unwrap();

        let composio = list(&conn, &Filter { route: Some(Route::Composio), ..Default::default() }).unwrap();
        assert_eq!(composio.len(), 1);
        assert!(composio[0].third_party);

        let anthropic = list(
            &conn,
            &Filter { destination: Some("ANTHROPIC".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(anthropic.len(), 2); // case-insensitive substring
    }

    #[test]
    fn list_filters_by_purpose() {
        let conn = crate::open_in_memory().unwrap();
        insert(&conn, &row(1, Route::BatchApi, "dream_cycle", "x")).unwrap();
        insert(&conn, &row(2, Route::BatchApi, "morning_brief", "x")).unwrap();
        let mb = list(&conn, &Filter { purpose: Some("morning_brief".into()), ..Default::default() }).unwrap();
        assert_eq!(mb.len(), 1);
        assert_eq!(mb[0].purpose, "morning_brief");
    }

    #[test]
    fn route_string_roundtrips() {
        for r in [
            Route::BatchApi,
            Route::BatchRelay,
            Route::MessagesApi,
            Route::Mcp,
            Route::Composio,
            Route::Billing,
            Route::Asr,
            Route::LocalAgent,
            Route::Support,
            Route::Web,
        ] {
            assert_eq!(Route::parse(r.as_str()), Some(r));
        }
        assert_eq!(Route::parse("nope"), None);
    }
}
