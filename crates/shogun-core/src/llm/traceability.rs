//! Traceability recording for every outbound chunk (AR-11 / §6.14, CLAUDE.md invariant 3 & G8).
//!
//! Whenever a processed chunk leaves the device, exactly one [`TraceRecord`] is written. The
//! record holds only the chunk's **byte length** and an **xxh64 digest** — never the text.
//! [`TraceRecord::for_chunk`] hashes-and-drops the chunk, so there is no path by which a sink
//! could persist the sent text. The one exception to "no address beyond the host" is
//! [`TraceRecord::with_url`]: a public web fetch's URL, masked by `web_fetch::ledger_url` first
//! (issue #928). The record's fields line up
//! 1:1 with the `traceability_log` columns (shogun-memory V1__init.sql).

use std::hash::Hasher;

use twox_hash::XxHash64;

/// The route a chunk left by. Matches the `route` CHECK set in `traceability_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Batch API, Select KK key (indexing / classification / Dream Cycle / Morning Brief) —
    /// direct to Anthropic. Development builds only, once the relay ships.
    BatchApi,
    /// Batch lane via the Select-operated relay (docs/batch-relay-design.md §3.3) — the shipping
    /// Standard/Pro path. Distinct from [`Route::BatchApi`] because the disclosure differs: the
    /// chunk crosses the operator's server, and the traceability screen must say so ("via
    /// operator server"), the way Composio sends say "via third party". Not `third_party` — the
    /// relay is the operator's own infrastructure, and it stores no chunk content (NFR-PRV-04).
    BatchRelay,
    /// Messages API, user BYOK key (agent inference / chat / drafts).
    MessagesApi,
    /// Company live AI through the operator gateway.
    LiveRelay,
    /// A remote MCP integration.
    Mcp,
    /// Composio (third-party relay; always surfaced as "via third party" in the UI).
    Composio,
    /// Billing / licensing calls.
    Billing,
    /// Third-party meeting ASR (Deepgram Nova-3; CLAUDE.md 2026-08-05 exception).
    Asr,
    /// Agent inference delegated to a local, already-signed-in vendor CLI on the user's own
    /// subscription (Issue #110). Separate from [`Route::MessagesApi`] because the disclosure is
    /// different: SHOGUN holds no credential and does not open the socket — a local process the
    /// user already installed does, against their plan quota.
    LocalAgent,
    /// CS / bug-report intake (support窓口): a report the user explicitly composed and sent to
    /// the operator's server. Distinct from [`Route::Billing`] because the payload is
    /// user-authored text, not a licence handshake — the ledger row proves the send happened and
    /// what size it was, per invariant 3.
    Support,
    /// A public web read the user asked chat for (issue #555): a YouTube caption track, an X
    /// post's oEmbed, a page. What crosses is the URL; the row is the host, the URL's digest
    /// and its length. Not `third_party`: nothing is relayed, and nothing of the user's goes
    /// with the request.
    Web,
}

impl Route {
    /// The exact string stored in `traceability_log.route`.
    pub fn as_db_str(self) -> &'static str {
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
}

/// What a traceability row records (issue #29 §5, `traceability_log.kind`, V67). Almost every
/// row is an [`TraceKind::Egress`] — a chunk that left the device. The other two are markers on a
/// shared-service route, so the traceability screen can say what happened when the service was
/// busy or down: they carry no chunk (zero bytes, empty digest) and never count as a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceKind {
    /// A chunk left the device (the original and default meaning of a row).
    #[default]
    Egress,
    /// The service asked the device to wait (`Retry-After`) and it will ask again.
    Retry,
    /// The device gave the shared service up for tonight and used the local lane instead.
    FellBack,
}

impl TraceKind {
    /// The exact string stored in `traceability_log.kind`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            TraceKind::Egress => "egress",
            TraceKind::Retry => "retry",
            TraceKind::FellBack => "fell_back",
        }
    }
}

/// One traceability row. Carries no chunk text — only the length and digest (G8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRecord {
    pub route: Route,
    pub purpose: String,
    pub destination: String,
    pub chunk_bytes: usize,
    /// Lower-hex xxh64 of the chunk's UTF-8 bytes. A digest, never the text.
    pub chunk_xxh64: String,
    /// True for Composio-relayed sends (third-party disclosure).
    pub third_party: bool,
    /// Egress, or a retry / fell-back marker (issue #29 §5).
    pub kind: TraceKind,
    /// The full URL of a public web fetch, secrets masked (issue #928). `None` for every other
    /// route: a model or service request has no user-chosen address worth keeping.
    pub url: Option<String>,
}

/// The xxh64 digest recorded for an outbound chunk: 16 lower-hex chars. The only thing derived
/// from the chunk that is allowed to persist.
pub fn digest(chunk: &str) -> String {
    let mut h = XxHash64::with_seed(0);
    h.write(chunk.as_bytes());
    format!("{:016x}", h.finish())
}

impl TraceRecord {
    /// Build a record for a chunk that is about to be sent. Captures only its byte length and
    /// digest; the `chunk` argument is consumed for hashing and never stored.
    pub fn for_chunk(
        route: Route,
        purpose: impl Into<String>,
        destination: impl Into<String>,
        chunk: &str,
        third_party: bool,
    ) -> Self {
        Self {
            route,
            purpose: purpose.into(),
            destination: destination.into(),
            chunk_bytes: chunk.len(),
            chunk_xxh64: digest(chunk),
            third_party,
            kind: TraceKind::Egress,
            url: None,
        }
    }

    /// Attach the full URL of a public web fetch (issue #928). The caller masks secrets first.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// A retry / fell-back marker on a shared-service route (issue #29 §5). No chunk is involved,
    /// so the row has zero bytes and an empty digest; `kind` says what happened.
    pub fn marker(
        route: Route,
        purpose: impl Into<String>,
        destination: impl Into<String>,
        kind: TraceKind,
    ) -> Self {
        Self {
            route,
            purpose: purpose.into(),
            destination: destination.into(),
            chunk_bytes: 0,
            chunk_xxh64: String::new(),
            third_party: false,
            kind,
            url: None,
        }
    }
}

/// One shared-service round trip (issue #29 §4-6): which route, how long from request to full
/// response, and the HTTP status (`0` when no response arrived). Timing and status only — no
/// URL, no id, no content — so it is safe to keep and to show in `shogun metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundTrip {
    pub route: Route,
    pub ms: u64,
    pub status: u16,
}

/// The sink every external send is recorded to (AR-11). The real implementation writes a
/// `traceability_log` row (shogun-memory); tests use [`RecordingSink`]. A sink is handed a
/// [`TraceRecord`] — which has no text field — so it is structurally unable to store the sent
/// content.
/// The ledger row for an egress could not be written (disk full, a poisoned lock). Content-free.
/// A caller that is about to send something stops instead of sending untraced (issue #831 for
/// audio, and for every LLM lane; invariant 3). Converts into
/// [`crate::llm::LlmError::TraceUnrecorded`] so the provider clients can `?` it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceUnrecorded;

pub trait TraceabilitySink: Send + Sync {
    /// Best-effort write. **Not for egress sites**: every send of captured or stored text uses
    /// [`try_record`](Self::try_record) and does not send when it fails (a test in this module
    /// greps `src/llm` for the pattern). Retry / fell-back markers and other bookkeeping that is
    /// not itself a send may stay best-effort.
    fn record(&self, rec: TraceRecord);

    /// [`record`](Self::record), reporting whether the row was actually written. Sinks that cannot
    /// fail (in-memory, test) keep this default; the database sink overrides it.
    fn try_record(&self, rec: TraceRecord) -> Result<(), TraceUnrecorded> {
        self.record(rec);
        Ok(())
    }

    /// Record a shared-service round trip for the `shogun metrics` p95 line (issue #29 §4-6).
    /// Default: dropped — only the DB-backed sink keeps them.
    fn round_trip(&self, _rt: RoundTrip) {}
}

/// An in-memory sink for tests and offline development. Public so downstream crates can assert on
/// what would have been logged.
#[derive(Default)]
pub struct RecordingSink {
    records: std::sync::Mutex<Vec<TraceRecord>>,
    round_trips: std::sync::Mutex<Vec<RoundTrip>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record written, in order.
    pub fn records(&self) -> Vec<TraceRecord> {
        self.records.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Every round trip recorded, in order.
    pub fn round_trips(&self) -> Vec<RoundTrip> {
        self.round_trips.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl TraceabilitySink for RecordingSink {
    fn record(&self, rec: TraceRecord) {
        if let Ok(mut g) = self.records.lock() {
            g.push(rec);
        }
    }

    fn round_trip(&self, rt: RoundTrip) {
        if let Ok(mut g) = self.round_trips.lock() {
            g.push(rt);
        }
    }
}

/// A sink whose ledger is unavailable: every [`TraceabilitySink::try_record`] fails. For tests
/// that pin the fail-closed contract — with this sink no provider client may reach its transport.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RefusingSink {
    refused: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl RefusingSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// How many rows were turned away.
    pub(crate) fn refused(&self) -> usize {
        self.refused.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
impl TraceabilitySink for RefusingSink {
    fn record(&self, _rec: TraceRecord) {
        self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn try_record(&self, _rec: TraceRecord) -> Result<(), TraceUnrecorded> {
        self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(TraceUnrecorded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_db_strings_match_schema() {
        assert_eq!(Route::BatchApi.as_db_str(), "batch_api");
        assert_eq!(Route::BatchRelay.as_db_str(), "batch_relay");
        assert_eq!(Route::MessagesApi.as_db_str(), "messages_api");
        assert_eq!(Route::Mcp.as_db_str(), "mcp");
        assert_eq!(Route::Composio.as_db_str(), "composio");
        assert_eq!(Route::Billing.as_db_str(), "billing");
        assert_eq!(Route::Asr.as_db_str(), "asr");
        assert_eq!(Route::LocalAgent.as_db_str(), "local_agent");
        assert_eq!(Route::Support.as_db_str(), "support");
    }

    #[test]
    fn digest_is_deterministic_and_hex() {
        let d = digest("the quick brown fox");
        assert_eq!(d.len(), 16);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, digest("the quick brown fox"));
        assert_ne!(d, digest("the quick brown foy"));
    }

    #[test]
    fn record_holds_digest_and_length_never_text() {
        let chunk = "meeting notes: ship the roadmap by friday";
        let rec = TraceRecord::for_chunk(Route::BatchApi, "classify", "api.anthropic.com", chunk, false);
        assert_eq!(rec.chunk_bytes, chunk.len());
        assert_eq!(rec.chunk_xxh64, digest(chunk));
        // The record's own Debug must not surface the chunk text — there is no field for it.
        let dumped = format!("{rec:?}");
        assert!(!dumped.contains("roadmap"), "chunk text must never appear in a trace record");
    }

    #[test]
    fn markers_carry_no_chunk_and_say_what_happened() {
        let retry = TraceRecord::marker(Route::BatchRelay, "dream_cycle", "api.example", TraceKind::Retry);
        assert_eq!(retry.chunk_bytes, 0);
        assert!(retry.chunk_xxh64.is_empty());
        assert!(!retry.third_party);
        assert_eq!(retry.kind.as_db_str(), "retry");
        assert_eq!(TraceKind::FellBack.as_db_str(), "fell_back");
        // An ordinary send stays an egress row.
        let sent = TraceRecord::for_chunk(Route::BatchRelay, "dream_cycle", "api.example", "x", false);
        assert_eq!(sent.kind, TraceKind::Egress);
        assert_eq!(TraceKind::default(), TraceKind::Egress);
    }

    #[test]
    fn recording_sink_captures_in_order() {
        let sink = RecordingSink::new();
        sink.record(TraceRecord::for_chunk(Route::BatchApi, "a", "d", "one", false));
        sink.record(TraceRecord::for_chunk(Route::MessagesApi, "b", "d", "two", false));
        let recs = sink.records();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].purpose, "a");
        assert_eq!(recs[1].route, Route::MessagesApi);
    }

    /// Helpers that build an egress row (a [`TraceRecord::for_chunk`] for a real send). A call to
    /// `.record(` whose first argument is one of these, under any module path, is a send written
    /// best-effort. Markers (`TraceRecord::marker`) are not sends and are not listed.
    const EGRESS_ROW_BUILDERS: &[&str] = &["TraceRecord::for_chunk", "trace_for_send", "trace_for_draft"];

    /// The production files that hold egress sites: every provider client under `src/llm`, plus
    /// the confirmed-send executor.
    fn egress_site_files() -> Vec<std::path::PathBuf> {
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src.join("llm"), &mut files);
        files.push(src.join("send_exec.rs"));
        files
    }

    /// `file:line` of every `.record(` call in `text` whose argument is built by one of
    /// [`EGRESS_ROW_BUILDERS`]. The callee path is read up to its opening parenthesis and matched
    /// on its last segments, so `crate::llm::traceability::TraceRecord::for_chunk(` and a bare
    /// `trace_for_send(` are both caught without listing every spelling.
    fn best_effort_egress_sites(path: &std::path::Path, text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut from = 0usize;
        while let Some(at) = text[from..].find(".record(") {
            let start = from + at + ".record(".len();
            let callee: String = text[start..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, ':' | '_'))
                .collect();
            if EGRESS_ROW_BUILDERS
                .iter()
                .any(|builder| callee == *builder || callee.ends_with(&format!("::{builder}")))
            {
                let line = text[..from + at].matches('\n').count() + 1;
                found.push(format!("{}:{line}", path.display()));
            }
            from = start;
        }
        found
    }

    /// No egress site in `src/llm` or `send_exec.rs` may write its row
    /// best-effort. An egress row (see [`EGRESS_ROW_BUILDERS`]) handed to `record` instead of
    /// `try_record` is the fail-open pattern this test exists to keep out.
    #[test]
    fn no_llm_egress_site_records_best_effort() {
        let files = egress_site_files();
        assert!(files.len() > 1, "src/llm and send_exec.rs must be readable from the test");
        let mut offenders = Vec::new();
        for path in files {
            // Test code may exercise `record` on the in-memory sinks; only production code is
            // an egress site. Separate `tests.rs` files are skipped, and an inline test module
            // ends the scan of its file.
            if path.file_name().is_some_and(|n| n == "tests.rs") {
                continue;
            }
            let full = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(!full.is_empty(), "{} must be readable from the test", path.display());
            let text = match full.find("#[cfg(test)]") {
                Some(at) => &full[..at],
                None => full.as_str(),
            };
            offenders.extend(best_effort_egress_sites(&path, text));
        }
        assert!(offenders.is_empty(), "egress rows written best-effort (use try_record): {offenders:?}");
    }

    #[test]
    fn the_best_effort_scanner_sees_every_spelling_and_ignores_markers() {
        let path = std::path::Path::new("x.rs");
        let hits = |src: &str| best_effort_egress_sites(path, src).len();
        assert_eq!(hits("sink.record(TraceRecord::for_chunk(r, p, d, c, false));"), 1);
        assert_eq!(hits("sink.record(crate::llm::traceability::TraceRecord::for_chunk(r));"), 1);
        assert_eq!(hits("sink.record(trace_for_send(confirmed));"), 1);
        assert_eq!(hits("sink.record(\n    send_exec::trace_for_draft(&body),\n);"), 1);
        assert_eq!(hits("sink.record(TraceRecord::marker(r, p, d, TraceKind::Retry));"), 0);
        assert_eq!(hits("sink.try_record(trace_for_send(confirmed))?;"), 0);
        assert_eq!(hits("let rec = trace_for_send(confirmed); sink.record(rec);"), 0, "a bound row is beyond a grep; the type-level guard is try_record's signature");
    }

    #[test]
    fn refusing_sink_turns_every_row_away_and_converts_to_the_lane_error() {
        let sink = RefusingSink::new();
        let err = sink
            .try_record(TraceRecord::for_chunk(Route::MessagesApi, "p", "d", "x", false))
            .expect_err("the ledger is unavailable");
        assert!(matches!(crate::llm::LlmError::from(err), crate::llm::LlmError::TraceUnrecorded));
        assert_eq!(sink.refused(), 1);
        let shown = crate::llm::LlmError::TraceUnrecorded.to_string();
        assert!(shown.contains("nothing was sent"), "{shown}");
    }
}
