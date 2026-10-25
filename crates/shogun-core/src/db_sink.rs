//! The DB-backed traceability sink (daemon wiring, §6.14 / FR-TR-03). Feature `db`.
//!
//! This is the adapter that closes the traceability loop: the LLM egress records every outbound
//! chunk to a [`TraceabilitySink`] ([`crate::llm::traceability`]), and here that sink persists the
//! record to `traceability_log` via the storage layer ([`shogun_memory::traceability`]). Because
//! the single HTTP egress goes through the sink, and the sink writes a row, an external send that
//! reaches the wire always leaves a trace (FR-TR-03) — enforced structurally, not by discipline.
//!
//! A write failure never panics and never stops the daemon (CLAUDE.md crash-resilience): it is
//! counted in `dropped`, which the Notch indicator surfaces, whether the caller used
//! [`TraceabilitySink::record`] (best-effort, `()`) or [`TraceabilitySink::try_record`], which
//! also reports the failure. Every egress site — live ASR audio (issue #831), Gmail, and every
//! Batch / Messages / delegate lane — uses `try_record` and does not send when the row was not
//! written (`LlmError::TraceUnrecorded`). `record` remains for bookkeeping that is not itself a
//! send (retry / fell-back markers).
//!
//! The sink shares the daemon's connection ([`crate::daemon::SharedConn`]) so traceability rows and
//! capture writes hit the same SQLite handle. Meeting audio audit ([`MeetingAuditSink`]) may use
//! its own keyed connection to the same store instead.

use std::sync::atomic::{AtomicU64, Ordering};

use shogun_memory::traceability::{
    insert, list, Filter, Route as DbRoute, TraceKind as DbKind, TraceRow,
};

use crate::daemon::{Clock, SharedConn};
use crate::llm::traceability::{
    RoundTrip, Route, TraceKind, TraceRecord, TraceUnrecorded, TraceabilitySink,
};
use crate::llm::usage::{UsageOutcome, UsageRecord, UsageSink};

/// Map the LLM-layer route onto the storage-layer route (1:1; the DB CHECK is the shared contract).
fn map_route(route: Route) -> DbRoute {
    match route {
        Route::BatchApi => DbRoute::BatchApi,
        Route::BatchRelay => DbRoute::BatchRelay,
        Route::MessagesApi => DbRoute::MessagesApi,
        Route::LiveRelay => DbRoute::LiveRelay,
        Route::Mcp => DbRoute::Mcp,
        Route::Composio => DbRoute::Composio,
        Route::Billing => DbRoute::Billing,
        Route::Asr => DbRoute::Asr,
        Route::LocalAgent => DbRoute::LocalAgent,
        Route::Support => DbRoute::Support,
        Route::Web => DbRoute::Web,
    }
}

/// A [`TraceabilitySink`] that persists every record to `traceability_log`, sharing the daemon's
/// connection and clock (so the row's `ts` is injectable and deterministic under test).
pub struct DbTraceabilitySink {
    conn: SharedConn,
    clock: Clock,
    dropped: AtomicU64,
}

impl DbTraceabilitySink {
    /// Build a sink over the daemon's shared connection.
    pub fn new(conn: SharedConn, clock: Clock) -> Self {
        Self { conn, clock, dropped: AtomicU64::new(0) }
    }

    /// Number of records that failed to persist (surfaced via the indicator, never fatal).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Read back rows for the traceability viewer (FR-TR-02) through the same handle. Returns an
    /// empty vec if the lock or query fails (read-only; never panics).
    pub fn rows(&self, filter: &Filter) -> Vec<TraceRow> {
        self.conn.lock().ok().and_then(|c| list(&c, filter).ok()).unwrap_or_default()
    }

    /// Import durable startup audit records with their original timestamps in one transaction.
    pub fn replay(
        &self,
        records: impl IntoIterator<Item = (TraceRecord, i64)>,
    ) -> Result<(), TraceUnrecorded> {
        let mut conn = self.conn.lock().map_err(|_| TraceUnrecorded)?;
        let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(|_| TraceUnrecorded)?;
        // The journal can be cleared only after a FULL (or stronger) durable WAL commit.
        if conn.pragma_update(None, "synchronous", previous.max(2)).is_err() {
            let _ = conn.pragma_update(None, "synchronous", previous);
            return Err(TraceUnrecorded);
        }
        let committed = (|| {
            let transaction = conn.transaction().map_err(|_| TraceUnrecorded)?;
            for (record, ts) in records {
                insert(&transaction, &trace_row(record, ts)).map_err(|_| TraceUnrecorded)?;
            }
            transaction.commit().map_err(|_| TraceUnrecorded)
        })();
        let restored = conn.pragma_update(None, "synchronous", previous).map_err(|_| TraceUnrecorded);
        committed.and(restored)
    }
}

fn trace_row(rec: TraceRecord, ts: i64) -> TraceRow {
    let chunk_bytes = i64::try_from(rec.chunk_bytes).unwrap_or(i64::MAX);
    let mut row = TraceRow::new(
        ts,
        map_route(rec.route),
        rec.purpose,
        rec.destination,
        chunk_bytes,
        rec.chunk_xxh64,
    );
    // Respect the record's own third-party flag (e.g. a Composio-routed send).
    row.third_party = rec.third_party;
    row.url = rec.url;
    row.kind = match rec.kind {
        TraceKind::Egress => DbKind::Egress,
        TraceKind::Retry => DbKind::Retry,
        TraceKind::FellBack => DbKind::FellBack,
    };

    row
}

struct AuditRequest {
    row: TraceRow,
    committed: std::sync::mpsc::SyncSender<bool>,
}

fn commit_audit_batch(conn: &SharedConn, batch: &[AuditRequest]) -> bool {
    conn.lock().ok().is_some_and(|mut conn| {
        let Ok(transaction) = conn.transaction() else { return false; };
        for request in batch {
            if insert(&transaction, &request.row).is_err() {
                return false;
            }
        }
        transaction.commit().is_ok()
    })
}

/// Meeting-only audit writer. Both audio streams share one bounded metadata queue.
/// A successful reply means the entire transaction committed; audio may then leave the device.
/// No waveform or transcript text is queued here. Drop drains and joins the worker.
pub struct MeetingAuditSink {
    requests: Option<std::sync::mpsc::SyncSender<AuditRequest>>,
    worker: Option<std::thread::JoinHandle<()>>,
    clock: Clock,
}

impl MeetingAuditSink {
    /// Start one worker, batching simultaneous microphone/system-audio audit rows.
    pub fn new(conn: SharedConn, clock: Clock) -> std::io::Result<Self> {
        let (requests, pending) = std::sync::mpsc::sync_channel::<AuditRequest>(32);
        let worker = std::thread::Builder::new()
            .name("meeting-audit".into())
            .spawn(move || {
                while let Ok(first) = pending.recv() {
                    let mut batch = vec![first];
                    // A short fixed window combines dual-stream writes without a 100 ms delay.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
                    while batch.len() < 32 {
                        let Some(remaining) =
                            deadline.checked_duration_since(std::time::Instant::now())
                        else {
                            break;
                        };
                        match pending.recv_timeout(remaining) {
                            Ok(request) => batch.push(request),
                            Err(_) => break,
                        }
                    }
                    let committed = commit_audit_batch(&conn, &batch);
                    for request in batch {
                        let _ = request.committed.send(committed);
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            worker: Some(worker),
            clock,
        })
    }
}

impl TraceabilitySink for MeetingAuditSink {
    fn record(&self, rec: TraceRecord) {
        let _ = self.try_record(rec);
    }

    fn try_record(&self, rec: TraceRecord) -> Result<(), TraceUnrecorded> {
        let (committed, reply) = std::sync::mpsc::sync_channel(1);
        let request = AuditRequest {
            row: trace_row(rec, (self.clock)()),
            committed,
        };
        self.requests
            .as_ref()
            .ok_or(TraceUnrecorded)?
            .send(request)
            .map_err(|_| TraceUnrecorded)?;
        match reply.recv() {
            Ok(true) => Ok(()),
            _ => Err(TraceUnrecorded),
        }
    }
}

impl Drop for MeetingAuditSink {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A [`UsageSink`] that persists every record to `llm_usage` (V25, FR-HN-01 / #313), sharing the
/// daemon's connection and clock — the same wiring shape as [`DbTraceabilitySink`], and the same
/// fire-and-forget contract: a failed metric write counts a drop and never interrupts the user's
/// work.
pub struct DbUsageSink {
    conn: SharedConn,
    clock: Clock,
    dropped: AtomicU64,
}

impl DbUsageSink {
    /// Build a sink over the daemon's shared connection.
    pub fn new(conn: SharedConn, clock: Clock) -> Self {
        Self { conn, clock, dropped: AtomicU64::new(0) }
    }

    /// Number of records that failed to persist (never fatal).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Aggregated counters since `since_ts`, optionally per prompt id — the read side the
    /// delegate-quota meter (FR-HN-08) and any cost view will use.
    pub fn totals_since(
        &self,
        since_ts: i64,
        prompt_id: Option<&str>,
    ) -> shogun_memory::llm_usage::Totals {
        self.conn
            .lock()
            .ok()
            .and_then(|c| shogun_memory::llm_usage::totals_since(&c, since_ts, prompt_id).ok())
            .unwrap_or_default()
    }
}

fn map_outcome(outcome: UsageOutcome) -> shogun_memory::llm_usage::Outcome {
    use shogun_memory::llm_usage::Outcome as DbOutcome;
    match outcome {
        UsageOutcome::Ok => DbOutcome::Ok,
        UsageOutcome::Error => DbOutcome::Error,
        UsageOutcome::RateLimited => DbOutcome::RateLimited,
        UsageOutcome::Unauthorized => DbOutcome::Unauthorized,
        UsageOutcome::Quota => DbOutcome::Quota,
        UsageOutcome::Parse => DbOutcome::Parse,
    }
}

impl UsageSink for DbUsageSink {
    fn record(&self, rec: UsageRecord) {
        let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let row = shogun_memory::llm_usage::UsageRow {
            ts: (self.clock)(),
            route: map_route(rec.route),
            prompt_id: rec.prompt_id.to_string(),
            prompt_version: i64::from(rec.prompt_version),
            model: rec.model,
            input_tokens: clamp(rec.usage.input_tokens),
            output_tokens: clamp(rec.usage.output_tokens),
            cache_read_tokens: clamp(rec.usage.cache_read_tokens),
            cache_write_tokens: clamp(rec.usage.cache_write_tokens),
            estimated: rec.usage.estimated,
            first_token_ms: rec.first_token_ms.map(clamp),
            total_ms: clamp(rec.total_ms),
            outcome: map_outcome(rec.outcome),
        };
        let wrote =
            self.conn.lock().ok().and_then(|c| shogun_memory::llm_usage::insert(&c, &row).ok());
        if wrote.is_none() {
            // Never interrupt the user's work — count the drop and move on.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl TraceabilitySink for DbTraceabilitySink {
    fn record(&self, rec: TraceRecord) {
        // Never interrupt the user's work for a ledger fault — a caller that must not send
        // untraced uses `try_record` and decides for itself.
        let _ = self.try_record(rec);
    }

    fn try_record(&self, rec: TraceRecord) -> Result<(), TraceUnrecorded> {
        let ts = (self.clock)();
        let row = trace_row(rec, ts);

        let wrote = self.conn.lock().ok().and_then(|c| insert(&c, &row).ok());
        if wrote.is_none() {
            // Count the drop for the indicator; the caller learns of it too (issue #831).
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(TraceUnrecorded);
        }
        Ok(())
    }

    fn round_trip(&self, rt: RoundTrip) {
        let ts = (self.clock)();
        let ms = i64::try_from(rt.ms).unwrap_or(i64::MAX);
        let route = map_route(rt.route);
        let wrote = self.conn.lock().ok().and_then(|c| {
            shogun_memory::shared_service_rtt::insert(&c, ts, route, ms, rt.status).ok()
        });
        if wrote.is_none() {
            // A lost timing sample is never worth interrupting anything for.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::Db;
    use crate::llm::traceability::{Route, TraceRecord};

    fn db() -> Db {
        Db::open_in_memory(std::sync::Arc::new(|| 1_000)).expect("in-memory db")
    }

    #[test]
    fn startup_replay_requires_full_and_restores_exact_synchronous_setting() {
        let sink = db().traceability_sink();
        sink.conn.lock().expect("connection").execute_batch(
            "CREATE TEMP TRIGGER startup_audit_durability BEFORE INSERT ON traceability_log
             WHEN (SELECT synchronous FROM pragma_synchronous) < 2
             BEGIN SELECT RAISE(ABORT, 'startup audit needs FULL'); END;"
        ).expect("durability assertion trigger");
        for previous in [0, 1, 2, 3] {
            sink.conn.lock().expect("connection").pragma_update(None, "synchronous", previous).expect("setting");
            sink.replay([(TraceRecord::for_chunk(Route::Billing, "account_refresh", "shogunai.si", "", false), 123)])
                .expect("durable replay");
            let restored: i64 = sink.conn.lock().expect("connection")
                .pragma_query_value(None, "synchronous", |row| row.get(0)).expect("restored mode");
            assert_eq!(restored, previous);
        }
        assert_eq!(sink.rows(&Filter::default()).len(), 4);
    }

    #[test]
    fn startup_replay_rolls_back_and_restores_synchronous_after_insert_failure() {
        let sink = db().traceability_sink();
        sink.conn.lock().expect("connection").execute_batch(
            "PRAGMA synchronous = NORMAL;
             CREATE TEMP TRIGGER reject_startup_audit BEFORE INSERT ON traceability_log
             WHEN NEW.purpose = 'reject' BEGIN SELECT RAISE(ABORT, 'reject fixture'); END;"
        ).expect("failure fixture");
        let record = |purpose| TraceRecord::for_chunk(Route::Billing, purpose, "shogunai.si", "", false);
        assert_eq!(sink.replay([(record("account_refresh"), 123), (record("reject"), 124)]), Err(TraceUnrecorded));
        assert!(sink.rows(&Filter::default()).is_empty());
        let restored: i64 = sink.conn.lock().expect("connection")
            .pragma_query_value(None, "synchronous", |row| row.get(0)).expect("restored mode");
        assert_eq!(restored, 1);
    }

    #[test]
    fn meeting_audit_reply_waits_for_commit_and_preserves_metadata() {
        let db = db();
        let sink = db.meeting_traceability_sink().unwrap();
        let record = TraceRecord::for_chunk(
            Route::Asr,
            "meeting_asr",
            "api.deepgram.com",
            "private waveform marker",
            true,
        );
        let digest = record.chunk_xxh64.clone();
        sink.try_record(record).unwrap();
        let rows = db.traceability_sink().rows(&Filter::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chunk_xxh64, digest);
        assert!(rows[0].third_party);
        assert!(!format!("{:?}", rows[0]).contains("private waveform"));
        drop(sink);
    }

    #[test]
    fn meeting_audit_rejects_failed_commit_without_sending_success() {
        let conn = std::sync::Arc::new(std::sync::Mutex::new(shogun_memory::open_in_memory().unwrap()));
        conn.lock().unwrap().execute_batch("PRAGMA query_only = ON").unwrap();
        let sink = MeetingAuditSink::new(conn.clone(), std::sync::Arc::new(|| 123)).unwrap();
        assert!(sink
            .try_record(TraceRecord::for_chunk(
                Route::Asr,
                "meeting_asr",
                "api.deepgram.com",
                "",
                true
            ))
            .is_err());
        assert!(list(&conn.lock().unwrap(), &Filter::default()).unwrap().is_empty());
    }

    #[test]
    fn meeting_audit_batches_two_streams_atomically() {
        let conn = std::sync::Arc::new(std::sync::Mutex::new(shogun_memory::open_in_memory().unwrap()));
        conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_second BEFORE INSERT ON traceability_log WHEN NEW.purpose = 'reject' BEGIN SELECT RAISE(ABORT, 'test'); END;").unwrap();
        let request = |purpose| {
            let (committed, _) = std::sync::mpsc::sync_channel(1);
            AuditRequest { row: trace_row(TraceRecord::for_chunk(Route::Asr, purpose, "api.deepgram.com", "", true), 123), committed }
        };
        assert!(!commit_audit_batch(&conn, &[request("first"), request("reject")]));
        assert!(list(&conn.lock().unwrap(), &Filter::default()).unwrap().is_empty());
        assert!(commit_audit_batch(&conn, &[request("mic"), request("system")]));
        assert_eq!(list(&conn.lock().unwrap(), &Filter::default()).unwrap().len(), 2);
    }

    #[test]
    fn meeting_audit_connection_waits_out_a_long_main_write() {
        // Maintenance and erase hold the write lock for well over 100 ms. The audit row must
        // wait for it, not fail and tear down both live streams.
        let dir = std::env::temp_dir().join(format!(
            "shogun-meeting-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("memory.sqlite3");
        let db = Db::open(&path, std::sync::Arc::new(|| 1_000))
            .unwrap()
            .with_meeting_audit_connection(shogun_memory::open(&path).unwrap());
        let sink = db.meeting_traceability_sink().unwrap();
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let writer = rusqlite::Connection::open(writer_path).unwrap();
            writer.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            writer.execute_batch("COMMIT").unwrap();
        });
        wait_locked.recv().unwrap();
        sink.try_record(TraceRecord::for_chunk(Route::Asr, "meeting_asr", "api.deepgram.com", "", true))
            .unwrap();
        writer.join().unwrap();
        assert_eq!(db.traceability_sink().rows(&Filter::default()).len(), 1);
        drop((sink, db));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unconfigurable_meeting_audit_connection_keeps_the_shared_writer() {
        // `synchronous` cannot change inside a transaction, so this attach fails. Startup and
        // meeting audit must carry on through the main handle.
        let rejected = shogun_memory::open_in_memory().unwrap();
        rejected.execute_batch("BEGIN").unwrap();
        let db = db().with_meeting_audit_connection(rejected);
        let sink = db.meeting_traceability_sink().unwrap();
        sink.try_record(TraceRecord::for_chunk(Route::Asr, "meeting_asr", "api.deepgram.com", "", true))
            .unwrap();
        assert_eq!(db.traceability_sink().rows(&Filter::default()).len(), 1);
    }

    #[test]
    fn record_persists_a_row_with_digest_only() {
        let db = db();
        let sink = db.traceability_sink();
        // A record carrying obviously-sensitive text; only its digest+length may land in the DB.
        let rec = TraceRecord::for_chunk(Route::BatchApi, "dream_cycle", "api.anthropic.com", "TOP SECRET chunk", false);
        let expected_digest = rec.chunk_xxh64.clone();
        let expected_bytes = rec.chunk_bytes as i64;
        sink.record(rec);

        let rows = sink.rows(&Filter::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].purpose, "dream_cycle");
        assert_eq!(rows[0].route, DbRoute::BatchApi);
        assert_eq!(rows[0].chunk_bytes, expected_bytes);
        assert_eq!(rows[0].chunk_xxh64, expected_digest);
        // The sent text appears nowhere in the persisted row.
        assert!(!format!("{:?}", rows[0]).contains("SECRET"));
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn try_record_reports_a_row_that_could_not_be_written() {
        // Issue #831: `record` swallows a failed insert; `try_record` tells the caller, so a
        // send that must not go untraced can stop.
        let rec = || TraceRecord::for_chunk(Route::Asr, "meeting_asr", "api.deepgram.com", "", true);
        let db = db();
        let sink = db.traceability_sink();
        assert_eq!(sink.try_record(rec()), Ok(()));
        assert_eq!(sink.rows(&Filter::default()).len(), 1);
        assert_eq!(sink.dropped(), 0);

        // A store the row cannot be written to (here: no ledger table at all).
        let broken = rusqlite::Connection::open_in_memory().expect("in-memory connection");
        let sink = DbTraceabilitySink::new(
            std::sync::Arc::new(std::sync::Mutex::new(broken)),
            std::sync::Arc::new(|| 1_000),
        );
        assert_eq!(sink.try_record(rec()), Err(TraceUnrecorded));
        assert_eq!(sink.dropped(), 1);
        // The fire-and-forget path still never panics or returns an error.
        sink.record(rec());
        assert_eq!(sink.dropped(), 2);
    }

    #[test]
    fn markers_and_round_trips_persist_without_content() {
        use crate::llm::traceability::{RoundTrip, TraceKind};
        use shogun_memory::traceability::TraceKind as DbKind;
        let db = db();
        let sink = db.traceability_sink();
        sink.record(TraceRecord::marker(Route::BatchRelay, "dream_cycle", "api.example", TraceKind::Retry));
        sink.record(TraceRecord::marker(Route::BatchRelay, "dream_cycle", "api.example", TraceKind::FellBack));
        sink.round_trip(RoundTrip { route: Route::BatchRelay, ms: 240, status: 200 });
        let rows = sink.rows(&Filter::default());
        let kinds: Vec<DbKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![DbKind::FellBack, DbKind::Retry]);
        assert!(rows.iter().all(|r| r.chunk_bytes == 0 && r.chunk_xxh64.is_empty()));
        let latency = db.shared_service_latencies(0);
        assert_eq!(latency.len(), 1);
        assert_eq!(latency[0].p95_ms, 240.0);
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn a_web_fetch_url_survives_the_sink_and_other_routes_store_none() {
        // Issue #928: the masked URL from `WebFetcher` reaches the row; nothing else gets one.
        let sink = db().traceability_sink();
        let url = "https://example.com/cb?code=fake-grant-value&page=2#frag";
        sink.record(
            TraceRecord::for_chunk(Route::Web, "web_page", "example.com", url, false)
                .with_url(crate::web_fetch::ledger_url(url)),
        );
        sink.record(TraceRecord::for_chunk(Route::MessagesApi, "agent", "api.anthropic.com", "b", false));
        let rows = sink.rows(&Filter::default());
        assert_eq!(rows[0].url, None);
        assert_eq!(rows[1].url.as_deref(), Some("https://example.com/cb?code=[redacted]&page=2"));
        assert!(!format!("{rows:?}").contains("fake-grant"));
    }

    #[test]
    fn composio_record_persists_third_party() {
        let sink = db().traceability_sink();
        sink.record(TraceRecord::for_chunk(Route::Composio, "agent", "gmail.com", "body", true));
        let rows = sink.rows(&Filter::default());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].third_party);
    }

    #[test]
    fn each_send_records_exactly_one_row() {
        let sink = db().traceability_sink();
        for i in 0..5 {
            sink.record(TraceRecord::for_chunk(Route::MessagesApi, "agent", "api.anthropic.com", &format!("c{i}"), false));
        }
        assert_eq!(sink.rows(&Filter::default()).len(), 5);
    }

    #[test]
    fn usage_record_persists_counts_latency_and_identity_never_text() {
        use crate::llm::prompts;
        use crate::llm::usage::{Usage, UsageOutcome, UsageRecord, UsageSink};

        let db = db();
        let sink = db.usage_sink();
        sink.record(UsageRecord::new(
            Route::MessagesApi,
            prompts::INLINE_CHAT,
            "test-model",
            Usage::exact(120, 30, 100, 0),
            Some(180),
            950,
            UsageOutcome::Ok,
        ));

        let totals = sink.totals_since(0, Some(prompts::INLINE_CHAT.id));
        assert_eq!(totals.calls, 1);
        assert_eq!(totals.ok_calls, 1);
        assert_eq!(totals.input_tokens, 120);
        assert_eq!(totals.cache_read_tokens, 100);
        assert_eq!(totals.estimated_calls, 0);
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn estimated_usage_rows_stay_distinguishable_in_totals() {
        use crate::llm::prompts;
        use crate::llm::usage::{Usage, UsageOutcome, UsageRecord, UsageSink};

        let db = db();
        let sink = db.usage_sink();
        sink.record(UsageRecord::new(
            Route::LocalAgent,
            prompts::INLINE_DRAFT,
            "claude",
            Usage::estimated_from_bytes(400, 80),
            None,
            2_000,
            UsageOutcome::Ok,
        ));
        let totals = sink.totals_since(0, None);
        assert_eq!(totals.calls, 1);
        assert_eq!(totals.estimated_calls, 1);
    }

    #[test]
    fn filter_reads_back_through_the_same_handle() {
        let sink = db().traceability_sink();
        sink.record(TraceRecord::for_chunk(Route::BatchApi, "indexing", "api.anthropic.com", "a", false));
        sink.record(TraceRecord::for_chunk(Route::Composio, "agent", "gmail.com", "b", true));
        let composio = sink.rows(&Filter { route: Some(DbRoute::Composio), ..Default::default() });
        assert_eq!(composio.len(), 1);
        assert_eq!(composio[0].destination, "gmail.com");
    }
}
