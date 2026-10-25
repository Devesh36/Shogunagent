//! Gmail write-back for mail triage (issue #319, FR-TG-20..23): label sync and archive as
//! **L2 `ServiceStateAction`s**, executed either under a one-tap confirmation or under the
//! per-label *standing approval*, always with a traceability row (direct Google, no broker) and an
//! execution-history row that can be undone for seven days.
//!
//! Layering: this module owns policy and bookkeeping; the actual Google call goes through the
//! [`MailWriteTransport`] seam — in production the connector runtime's `execute_write_owned`,
//! which re-applies the service gate (double gate), in tests a fake. A send is unrepresentable
//! here: the only inputs are [`ServiceStateAction`]s, whose level is L2 by construction.

use shogun_agents::entitlement::Entitlements;
use shogun_agents::permission::{Action, Level, ServiceStateAction};
use shogun_memory::mail_triage::{ArchiveStatus, WriteApproval, WriteLogRow};

use crate::daemon::Db;
use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};

/// Undo window (FR-AG-05 / FR-TG-21): 7 days.
pub const UNDO_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The Gmail label name prefix SHOGUN owns when syncing labels.
pub const GMAIL_LABEL_PREFIX: &str = "SHOGUN/";

/// The seam that performs the Gmail write. `execute` maps the action onto the Google transport;
/// `ensure_label` returns the Gmail label id for a SHOGUN-owned label name, creating it if
/// missing; each request it sends to Google is recorded on `sink` first (issue #961). Errors are
/// content-free strings.
pub trait MailWriteTransport {
    fn execute(&self, action: &ServiceStateAction) -> Result<(), String>;
    fn ensure_label<S: TraceabilitySink + ?Sized>(&self, name: &str, sink: &S) -> Result<String, String>;

    /// Refuse a write that would never be sent (the service gate), without sending anything.
    /// [`execute`] records traceability only after this passes, so a refusal leaves no row while
    /// every request that can reach Google does (issue #782). The default admits everything.
    fn precheck(&self, action: &ServiceStateAction) -> Result<(), String> {
        let _ = action;
        Ok(())
    }
}

/// The transport over a live connector runtime: routes through `execute_write_owned`, so the
/// service gate (released wave, connected, plan) is re-checked on every write.
pub struct RuntimeMailWriteTransport<'a, T> {
    runtime: &'a std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>,
}

impl<'a, T> RuntimeMailWriteTransport<'a, T> {
    pub fn new(runtime: &'a std::sync::Mutex<shogun_integrations::ConnectorRuntime<T>>) -> Self {
        Self { runtime }
    }
}

/// The `arguments` JSON for a service-state action (the `GmailDirectRpc` contract).
pub fn arguments_for(action: &ServiceStateAction) -> serde_json::Value {
    match action {
        ServiceStateAction::ArchiveThread { thread_id } | ServiceStateAction::UnarchiveThread { thread_id } => {
            serde_json::json!({ "thread_id": thread_id })
        }
        ServiceStateAction::ModifyLabels { thread_id, add, remove } => {
            serde_json::json!({ "thread_id": thread_id, "add_label_ids": add, "remove_label_ids": remove })
        }
    }
}

impl<T> MailWriteTransport for RuntimeMailWriteTransport<'_, T>
where
    T: shogun_mcp::sync::IntegrationTransport + shogun_integrations::WriteExecutor,
{
    fn execute(&self, action: &ServiceStateAction) -> Result<(), String> {
        let mut rt = self.runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
        rt.execute_write_owned(shogun_mcp::scope::Service::Gmail, action.scope_op(), arguments_for(action))
            .map(|_| ())
    }

    fn precheck(&self, action: &ServiceStateAction) -> Result<(), String> {
        let rt = self.runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
        rt.precheck_write(shogun_mcp::scope::Service::Gmail, action.scope_op())
    }

    fn ensure_label<S: TraceabilitySink + ?Sized>(&self, name: &str, sink: &S) -> Result<String, String> {
        let mut rt = self.runtime.lock().map_err(|_| "runtime lock poisoned".to_string())?;
        // `label_sync` is the scope op both list/create ride under (both are label-registry
        // reads/writes on the user's own mailbox, L2 ServiceStateChange). A refused op sends
        // nothing, so it leaves no row; every request that can reach Google does (#782, #961).
        rt.precheck_write(shogun_mcp::scope::Service::Gmail, "label_sync")?;
        sink.record(label_trace("list_labels"));
        let listed = rt.execute_write_owned(
            shogun_mcp::scope::Service::Gmail,
            "label_sync",
            serde_json::json!({ "op": "list_labels" }),
        )?;
        if let Some(id) = listed
            .get("labels")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| {
                a.iter().find_map(|l| {
                    (l.get("name").and_then(serde_json::Value::as_str) == Some(name))
                        .then(|| l.get("id").and_then(serde_json::Value::as_str).map(str::to_string))
                        .flatten()
                })
            })
        {
            return Ok(id);
        }
        sink.record(label_trace(&format!("create_label {name}")));
        let created = rt.execute_write_owned(
            shogun_mcp::scope::Service::Gmail,
            "label_sync",
            serde_json::json!({ "op": "create_label", "name": name }),
        )?;
        created
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "create_label returned no id".to_string())
    }
}

/// Why a write-back was refused before reaching the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    /// The plan lacks `agent_execution` (Standard / expired trial). Standing approvals stay stored
    /// but stop executing (FR-TG-21).
    PlanNotEntitled,
    /// The thread has no triage row.
    UnknownThread,
    /// The undo window has passed or the row was already undone.
    NotUndoable,
}

/// The outcome of one write-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The write left the device (traced), and the history row id.
    Done { log_id: i64 },
    /// The transport failed; the history row records the failure. The attempt was traced before
    /// it was sent (issue #782): the request may have reached Google before the error.
    Failed { log_id: i64, error: String },
    Refused(WriteRefusal),
}

/// The trace record for one label-registry request (`list_labels`, or `create_label` and the
/// SHOGUN-owned label name). Only its digest and length are stored.
pub fn label_trace(chunk: &str) -> TraceRecord {
    TraceRecord::for_chunk(Route::Mcp, "integration_write", "gmail", chunk, false)
}

/// The trace record for a service-state write: direct Google, no broker, `integration_write`, and
/// a chunk that is only the op + thread id + label ids (never content).
pub fn trace_for(action: &ServiceStateAction) -> TraceRecord {
    let chunk = match action {
        ServiceStateAction::ArchiveThread { thread_id } => format!("archive {thread_id}"),
        ServiceStateAction::UnarchiveThread { thread_id } => format!("unarchive {thread_id}"),
        ServiceStateAction::ModifyLabels { thread_id, add, remove } => {
            format!("labels {thread_id} +{} -{}", add.join(","), remove.join(","))
        }
    };
    TraceRecord::for_chunk(Route::Mcp, "integration_write", "gmail", &chunk, false)
}

/// Execute one service-state action for a triage thread: plan gate → L2 assertion → transport →
/// trace + history + triage bookkeeping. `approval` says which L2 grant authorised it.
pub fn execute<T: MailWriteTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    db: &Db,
    transport: &T,
    sink: &S,
    thread_key: &str,
    action: ServiceStateAction,
    approval: WriteApproval,
    ent: &Entitlements,
) -> WriteOutcome {
    if !ent.agent_execution {
        return WriteOutcome::Refused(WriteRefusal::PlanNotEntitled);
    }
    if db.mail_triage_get(thread_key).is_none() {
        return WriteOutcome::Refused(WriteRefusal::UnknownThread);
    }
    // Structural: a ServiceStateAction is L2 by construction; this is the belt to that braces.
    debug_assert_eq!(Action::ServiceState(action.clone()).required_level(), Level::L2);
    let started = std::time::Instant::now();
    let (add, remove) = match &action {
        ServiceStateAction::ModifyLabels { add, remove, .. } => (add.clone(), remove.clone()),
        ServiceStateAction::ArchiveThread { .. } => (Vec::new(), vec!["INBOX".to_string()]),
        ServiceStateAction::UnarchiveThread { .. } => (vec!["INBOX".to_string()], Vec::new()),
    };
    // Traced before the request leaves (the send_exec discipline, issue #782): a write that
    // times out or fails after reaching Google still crossed the third-party boundary, and a
    // retried write is a second egress with its own row. A gate refusal sends nothing and is
    // booked as a failure without a row.
    let result = transport.precheck(&action).and_then(|()| {
        sink.record(trace_for(&action));
        transport.execute(&action)
    });
    crate::metrics::record_global(crate::metrics::Slo::MailArchiveWrite, started.elapsed().as_secs_f64() * 1000.0);
    match result {
        Ok(()) => {
            match &action {
                ServiceStateAction::ArchiveThread { .. } => {
                    db.mail_set_archive_status(thread_key, ArchiveStatus::Done);
                }
                ServiceStateAction::UnarchiveThread { .. } => {
                    db.mail_set_archive_status(thread_key, ArchiveStatus::Undone);
                }
                ServiceStateAction::ModifyLabels { .. } => {}
            }
            let log_id = db.mail_write_log(thread_key, action.kind(), &add, &remove, approval, true, None);
            WriteOutcome::Done { log_id }
        }
        Err(e) => {
            // Record where the thread actually is now, so a stuck request never loops: a failed
            // archive is `failed` (retryable), a failed unarchive is still `done`.
            match &action {
                ServiceStateAction::ArchiveThread { .. } => {
                    db.mail_set_archive_status(thread_key, ArchiveStatus::Failed);
                }
                ServiceStateAction::UnarchiveThread { .. } => {
                    db.mail_set_archive_status(thread_key, ArchiveStatus::Done);
                }
                ServiceStateAction::ModifyLabels { .. } => {}
            }
            // A content-free error class: scrubbed and clipped, never the transport's payload.
            let class: String = crate::log_redact::scrub(&e).chars().take(80).collect();
            let log_id = db.mail_write_log(thread_key, action.kind(), &add, &remove, approval, false, Some(&class));
            WriteOutcome::Failed { log_id, error: class }
        }
    }
}

/// A tick's Auto Archive report (FR-TG-22).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoArchiveReport {
    pub attempted: usize,
    pub archived: usize,
    pub failed: usize,
}

/// Run one Auto Archive pass under the standing approvals: every candidate the DB query admits
/// (label has `auto_archive`, Other, unlocked, confident, past the grace period, still in the
/// inbox) is archived, up to the per-tick cap. Without `agent_execution` nothing runs and the
/// candidates stay pending (FR-TG-21).
pub fn auto_archive_tick<T: MailWriteTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    db: &Db,
    transport: &T,
    sink: &S,
    ent: &Entitlements,
) -> AutoArchiveReport {
    let mut report = AutoArchiveReport::default();
    if !ent.agent_execution {
        return report;
    }
    for row in db.mail_archive_candidates() {
        report.attempted += 1;
        let action = ServiceStateAction::ArchiveThread { thread_id: row.thread_id.clone() };
        match execute(db, transport, sink, &row.thread_key, action, WriteApproval::L2Standing, ent) {
            WriteOutcome::Done { .. } => report.archived += 1,
            _ => report.failed += 1,
        }
    }
    report
}

/// What one [`explicit_requests_tick`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplicitReport {
    pub archived: usize,
    pub unarchived: usize,
    pub failed: usize,
}

/// Execute the explicit per-thread archive / unarchive requests the API faces recorded
/// (FR-TG-30). A request is recorded only after the user confirmed it in the app, or under the
/// thread's label's standing approval (issue #779), so each one carries the user's L2 grant for
/// that thread — the automatic safety conditions belong to the standing-approval sweep, not
/// here — and only the plan gate applies.
pub fn explicit_requests_tick<T: MailWriteTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    db: &Db,
    transport: &T,
    sink: &S,
    ent: &Entitlements,
) -> ExplicitReport {
    let mut report = ExplicitReport::default();
    if !ent.agent_execution {
        return report;
    }
    for (row, unarchive) in db.mail_explicit_requests() {
        let action = if unarchive {
            ServiceStateAction::UnarchiveThread { thread_id: row.thread_id.clone() }
        } else {
            ServiceStateAction::ArchiveThread { thread_id: row.thread_id.clone() }
        };
        match execute(db, transport, sink, &row.thread_key, action, WriteApproval::L2, ent) {
            WriteOutcome::Done { .. } if unarchive => report.unarchived += 1,
            WriteOutcome::Done { .. } => report.archived += 1,
            _ => report.failed += 1,
        }
    }
    report
}

/// Run one label-sync pass under the standing approvals: threads whose label has
/// `sync_to_gmail` get the `SHOGUN/<Name>` Gmail label (created on first use), and lose the
/// previously synced one. Returns the number of threads updated.
pub fn label_sync_tick<T: MailWriteTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    db: &Db,
    transport: &T,
    sink: &S,
    ent: &Entitlements,
) -> usize {
    if !ent.agent_execution {
        return 0;
    }
    let labels = db.mail_labels();
    let mut synced = 0;
    for row in db.mail_label_sync_candidates() {
        let Some(label) = labels.iter().find(|l| l.key == row.label_key) else { continue };
        let gmail_id = match label.gmail_label_id.clone() {
            Some(id) => id,
            None => match transport.ensure_label(&format!("{GMAIL_LABEL_PREFIX}{}", label.name), sink) {
                Ok(id) => {
                    db.mail_set_gmail_label_id(&label.key, &id);
                    id
                }
                Err(_) => continue,
            },
        };
        let remove: Vec<String> = row
            .synced_label_key
            .as_deref()
            .and_then(|k| labels.iter().find(|l| l.key == k))
            .and_then(|l| l.gmail_label_id.clone())
            .into_iter()
            .collect();
        let action = ServiceStateAction::ModifyLabels { thread_id: row.thread_id.clone(), add: vec![gmail_id], remove };
        if let WriteOutcome::Done { .. } = execute(db, transport, sink, &row.thread_key, action, WriteApproval::L2Standing, ent) {
            db.mail_set_synced_label(&row.thread_key, Some(&row.label_key));
            synced += 1;
        }
    }
    synced
}

/// Undo a history row within the window: runs the inverse action under the same L2 grant.
pub fn undo<T: MailWriteTransport + ?Sized, S: TraceabilitySink + ?Sized>(
    db: &Db,
    transport: &T,
    sink: &S,
    log_id: i64,
    now_ms: i64,
    ent: &Entitlements,
) -> WriteOutcome {
    let Some(row) = db.mail_write_log_get(log_id) else {
        return WriteOutcome::Refused(WriteRefusal::NotUndoable);
    };
    if row.status != "ok" || now_ms - row.ts_ms > UNDO_WINDOW_MS {
        return WriteOutcome::Refused(WriteRefusal::NotUndoable);
    }
    let thread_id = shogun_memory::mail_triage::thread_id_of(&row.thread_key).to_string();
    let original = match row.action_kind.as_str() {
        "archive_thread" => ServiceStateAction::ArchiveThread { thread_id },
        "unarchive_thread" => ServiceStateAction::UnarchiveThread { thread_id },
        _ => ServiceStateAction::ModifyLabels { thread_id, add: row.add_label_ids.clone(), remove: row.remove_label_ids.clone() },
    };
    let outcome = execute(db, transport, sink, &row.thread_key, original.inverse(), row.approval, ent);
    if let WriteOutcome::Done { .. } = outcome {
        db.mail_write_log_mark_undone(log_id);
        if matches!(original, ServiceStateAction::ModifyLabels { .. }) {
            // Not `None`: that re-qualifies the thread and the next tick puts the label straight
            // back. The marker records "the user took this label off on purpose"; a later relabel
            // changes it and the thread syncs again.
            let marker = db
                .mail_triage_get(&row.thread_key)
                .map(|t| shogun_memory::mail_triage::label_sync_undone_marker(&t.label_key));
            db.mail_set_synced_label(&row.thread_key, marker.as_deref());
        }
    }
    outcome
}

/// Recent history (for the Activity / Settings surfaces).
pub fn history(db: &Db, limit: usize) -> Vec<WriteLogRow> {
    db.mail_write_log_list(limit)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use shogun_agents::entitlement::{entitlements, Plan};
    use std::sync::atomic::{AtomicI64, Ordering};
    use shogun_memory::mail_triage::{Bucket, LabelFlags};
    use shogun_mcp::sync::{IngestItem, MailMessageMeta, MailMeta};
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    struct FakeTransport {
        calls: RefCell<Vec<ServiceStateAction>>,
        fail: bool,
    }
    impl MailWriteTransport for FakeTransport {
        fn execute(&self, action: &ServiceStateAction) -> Result<(), String> {
            self.calls.borrow_mut().push(action.clone());
            if self.fail {
                Err("composio 502".into())
            } else {
                Ok(())
            }
        }
        fn ensure_label<S: TraceabilitySink + ?Sized>(&self, name: &str, sink: &S) -> Result<String, String> {
            sink.record(label_trace("list_labels"));
            Ok(format!("Label_{}", name.len()))
        }
    }

    #[derive(Clone, Default)]
    struct RecSink(Arc<Mutex<Vec<TraceRecord>>>);
    impl TraceabilitySink for RecSink {
        fn record(&self, r: TraceRecord) {
            self.0.lock().unwrap().push(r);
        }
    }

    /// A Db with an advanceable clock (the grace period is clock-driven).
    fn db() -> (Db, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(10_000_000));
        let c = now.clone();
        let db = Db::open_in_memory(Arc::new(move || c.load(Ordering::SeqCst))).unwrap();
        (db, now)
    }

    fn pro() -> Entitlements {
        entitlements(Plan::Pro, 0)
    }

    fn seed_marketing_thread(db: &Db, id: &str) {
        db.ingest_integration(&[IngestItem {
            source: "gmail",
            kind: "email",
            title: format!("50% off {id}"),
            body: "Shop now".into(),
            ts_ms: 1_000,
            external_id: id.into(),
            calendar: None,
            mail: Some(MailMeta {
                thread_id: id.into(),
                messages: vec![MailMessageMeta {
                    message_id: format!("m-{id}"),
                    from_addr: "deals@shop.example".into(),
                    to_addrs: vec!["me@select.co.jp".into()],
                    gmail_label_ids: vec!["INBOX".into(), "CATEGORY_PROMOTIONS".into()],
                    has_list_unsubscribe: true,
                    internal_date_ms: 1_000,
                    ..Default::default()
                }],
            }),
        }]);
    }

    #[test]
    fn standing_approval_off_means_no_write_at_all() {
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        let r = auto_archive_tick(&db, &t, &sink, &pro());
        assert_eq!(r, AutoArchiveReport::default());
        assert!(t.calls.borrow().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn standing_approval_archives_traces_and_can_be_undone() {
        let (db, now) = db();
        seed_marketing_thread(&db, "t1");
        db.mail_label_set_flags("marketing", LabelFlags { auto_archive: Some(true), ..Default::default() }).unwrap();
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        // Inside the grace period nothing runs (classified at 10_000_000, now is the same).
        assert_eq!(auto_archive_tick(&db, &t, &sink, &pro()).attempted, 0);
        // Move the clock past the grace period.
        now.store(10_000_000 + 11 * 60 * 1000, Ordering::SeqCst);
        let db2 = &db;
        let r = auto_archive_tick(db2, &t, &sink, &pro());
        assert_eq!(r, AutoArchiveReport { attempted: 1, archived: 1, failed: 0 });
        assert_eq!(t.calls.borrow()[0], ServiceStateAction::ArchiveThread { thread_id: "t1".into() });
        let traces = sink.0.lock().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].route, Route::Mcp);
        assert_eq!(traces[0].purpose, "integration_write");
        assert!(!traces[0].third_party);
        drop(traces);
        let row = db2.mail_triage_get("gmail:t1").unwrap();
        assert_eq!(row.archive_status, ArchiveStatus::Done);
        let hist = history(db2, 10);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].approval, WriteApproval::L2Standing);
        assert_eq!(hist[0].status, "ok");
        // A second tick does not re-archive (INBOX is gone locally).
        assert_eq!(auto_archive_tick(db2, &t, &sink, &pro()).attempted, 0);
        // Undo within the window runs the inverse and marks the row.
        let out = undo(db2, &t, &sink, hist[0].id, 10_000_000 + 12 * 60 * 1000, &pro());
        assert!(matches!(out, WriteOutcome::Done { .. }));
        assert_eq!(t.calls.borrow()[1], ServiceStateAction::UnarchiveThread { thread_id: "t1".into() });
        assert_eq!(db2.mail_triage_get("gmail:t1").unwrap().archive_status, ArchiveStatus::Undone);
        assert_eq!(history(db2, 10)[1].status, "undone");
        // Undo again → refused.
        assert_eq!(
            undo(db2, &t, &sink, hist[0].id, 10_000_000 + 13 * 60 * 1000, &pro()),
            WriteOutcome::Refused(WriteRefusal::NotUndoable)
        );
    }

    #[test]
    fn no_entitlement_keeps_the_grant_but_stops_execution() {
        let (db, now) = db();
        seed_marketing_thread(&db, "t1");
        db.mail_label_set_flags("marketing", LabelFlags { auto_archive: Some(true), ..Default::default() }).unwrap();
        now.store(10_000_000 + 11 * 60 * 1000, Ordering::SeqCst);
        let db2 = &db;
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        // The one status without agent execution is an expired trial (Standard has it since
        // 2026-09-24).
        let standard = entitlements(
            Plan::Trial { started_at_ms: Some(0) },
            shogun_agents::entitlement::TRIAL_DURATION_MS,
        );
        assert_eq!(auto_archive_tick(db2, &t, &sink, &standard), AutoArchiveReport::default());
        assert!(db2.mail_labels().iter().any(|l| l.key == "marketing" && l.auto_archive), "grant stays stored");
        let out = execute(
            db2,
            &t,
            &sink,
            "gmail:t1",
            ServiceStateAction::ArchiveThread { thread_id: "t1".into() },
            WriteApproval::L2,
            &standard,
        );
        assert_eq!(out, WriteOutcome::Refused(WriteRefusal::PlanNotEntitled));
        assert!(t.calls.borrow().is_empty());
    }

    #[test]
    fn a_failed_write_is_logged_traced_and_retried_once() {
        let (db, now) = db();
        seed_marketing_thread(&db, "t1");
        db.mail_label_set_flags("marketing", LabelFlags { auto_archive: Some(true), ..Default::default() }).unwrap();
        now.store(10_000_000 + 11 * 60 * 1000, Ordering::SeqCst);
        let db2 = &db;
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: true };
        let sink = RecSink::default();
        let r = auto_archive_tick(db2, &t, &sink, &pro());
        assert_eq!(r, AutoArchiveReport { attempted: 1, archived: 0, failed: 1 });
        // Issue #782: the request may have reached Google before the 502, so it is traced.
        assert_eq!(sink.0.lock().unwrap().len(), 1, "the failed attempt is on the ledger");
        assert_eq!(db2.mail_triage_get("gmail:t1").unwrap().archive_status, ArchiveStatus::Failed);
        assert_eq!(history(db2, 10)[0].status, "failed");
        // `failed` rows are candidates again (one retry), per FR-TG-22.
        assert_eq!(auto_archive_tick(db2, &t, &sink, &pro()).attempted, 1);
        // The retry is a second egress with its own row.
        let traces = sink.0.lock().unwrap();
        assert_eq!(traces.len(), 2, "one row per attempt");
        assert!(traces
            .iter()
            .all(|r| r.route == Route::Mcp && !r.third_party && r.purpose == "integration_write"));
    }

    #[test]
    fn a_write_the_gate_refuses_is_booked_failed_without_a_row() {
        struct Refusing;
        impl MailWriteTransport for Refusing {
            fn execute(&self, _action: &ServiceStateAction) -> Result<(), String> {
                Err("must not be sent".into())
            }
            fn ensure_label<S: TraceabilitySink + ?Sized>(&self, _name: &str, _sink: &S) -> Result<String, String> {
                Err("refused".into())
            }
            fn precheck(&self, _action: &ServiceStateAction) -> Result<(), String> {
                Err("write not authorized".into())
            }
        }
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        let sink = RecSink::default();
        let out = execute(
            &db,
            &Refusing,
            &sink,
            "gmail:t1",
            ServiceStateAction::ArchiveThread { thread_id: "t1".into() },
            WriteApproval::L2,
            &pro(),
        );
        assert!(matches!(out, WriteOutcome::Failed { ref error, .. } if error.contains("not authorized")));
        assert!(sink.0.lock().unwrap().is_empty(), "nothing was sent, so nothing is on the ledger");
    }

    #[test]
    fn an_explicit_request_executes_without_a_standing_approval_and_unarchives_for_real() {
        use shogun_memory::mail_triage::UserChange;
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        // Locked to Important at low confidence: the sweep would refuse this row outright.
        db.mail_triage_set(
            "gmail:t1",
            &UserChange { label_key: None, bucket: Some(Bucket::Important), handled: None },
            shogun_memory::mail_triage::FeedbackOrigin::Ui,
        )
        .unwrap();
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        assert_eq!(auto_archive_tick(&db, &t, &sink, &pro()), AutoArchiveReport::default());

        db.mail_request_archive("gmail:t1").unwrap();
        let report = explicit_requests_tick(&db, &t, &sink, &pro());
        assert_eq!(report, ExplicitReport { archived: 1, unarchived: 0, failed: 0 });
        assert_eq!(t.calls.borrow()[0], ServiceStateAction::ArchiveThread { thread_id: "t1".into() });
        assert_eq!(db.mail_triage_get("gmail:t1").unwrap().archive_status, ArchiveStatus::Done);
        assert_eq!(history(&db, 10)[0].approval, WriteApproval::L2, "an explicit ask is a one-tap, not a standing grant");
        // The request is consumed, not replayed every tick.
        assert_eq!(explicit_requests_tick(&db, &t, &sink, &pro()), ExplicitReport::default());

        // Unarchive must reach Gmail — clearing local state alone leaves the thread archived
        // while SHOGUN reports it back in the inbox.
        db.mail_request_unarchive("gmail:t1").unwrap();
        assert_eq!(
            db.mail_triage_get("gmail:t1").unwrap().archive_status,
            ArchiveStatus::UnarchivePending,
            "still archived until the write lands"
        );
        let report = explicit_requests_tick(&db, &t, &sink, &pro());
        assert_eq!(report, ExplicitReport { archived: 0, unarchived: 1, failed: 0 });
        assert_eq!(t.calls.borrow()[1], ServiceStateAction::UnarchiveThread { thread_id: "t1".into() });
        assert_eq!(db.mail_triage_get("gmail:t1").unwrap().archive_status, ArchiveStatus::Undone);
        assert_eq!(sink.0.lock().unwrap().len(), 2, "both writes are traced");
    }

    #[test]
    fn a_failed_unarchive_settles_back_to_archived() {
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        let ok = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        db.mail_request_archive("gmail:t1").unwrap();
        explicit_requests_tick(&db, &ok, &sink, &pro());
        db.mail_request_unarchive("gmail:t1").unwrap();
        let bad = FakeTransport { calls: RefCell::new(vec![]), fail: true };
        assert_eq!(
            explicit_requests_tick(&db, &bad, &sink, &pro()),
            ExplicitReport { archived: 0, unarchived: 0, failed: 1 }
        );
        assert_eq!(db.mail_triage_get("gmail:t1").unwrap().archive_status, ArchiveStatus::Done);
        assert_eq!(explicit_requests_tick(&db, &bad, &sink, &pro()), ExplicitReport::default(), "no retry loop");
    }

    #[test]
    fn label_sync_creates_the_gmail_label_once_and_applies_it() {
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        seed_marketing_thread(&db, "t2");
        db.mail_label_set_flags("marketing", LabelFlags { sync_to_gmail: Some(true), ..Default::default() }).unwrap();
        let t = FakeTransport { calls: RefCell::new(vec![]), fail: false };
        let sink = RecSink::default();
        assert_eq!(label_sync_tick(&db, &t, &sink, &pro()), 2);
        let label = db.mail_labels().into_iter().find(|l| l.key == "marketing").unwrap();
        assert_eq!(label.gmail_label_id.as_deref(), Some("Label_16")); // "SHOGUN/Marketing".len()
        let calls = t.calls.borrow();
        assert!(calls.iter().all(|c| matches!(c, ServiceStateAction::ModifyLabels { add, .. } if add == &vec!["Label_16".to_string()])));
        drop(calls);
        assert_eq!(db.mail_triage_get("gmail:t1").unwrap().synced_label_key.as_deref(), Some("marketing"));
        // Nothing left to sync.
        assert_eq!(label_sync_tick(&db, &t, &sink, &pro()), 0);

        // Undoing a label sync is not a suggestion: the next tick must not put the label back.
        let log_id = history(&db, 10).iter().find(|r| r.action_kind == "modify_labels").map(|r| r.id).expect("a label write");
        assert!(matches!(undo(&db, &t, &sink, log_id, 10_000_100, &pro()), WriteOutcome::Done { .. }));
        assert_eq!(label_sync_tick(&db, &t, &sink, &pro()), 0, "an undo the job reverses is not an undo");
        // A relabel to a synced label swaps the Gmail labels; a relabel to an unsynced one is left.
        db.mail_triage_set(
            "gmail:t1",
            &shogun_memory::mail_triage::UserChange { label_key: Some("news".into()), bucket: Some(Bucket::Other), handled: None },
            shogun_memory::mail_triage::FeedbackOrigin::Ui,
        )
        .unwrap();
        assert_eq!(label_sync_tick(&db, &t, &sink, &pro()), 0);
    }

    #[test]
    fn label_registry_requests_are_traced_before_they_are_sent() {
        use shogun_mcp::scope::{Service, Wave};
        use std::sync::atomic::AtomicUsize;
        struct Exec(Arc<AtomicUsize>);
        impl shogun_mcp::sync::IntegrationTransport for Exec {
            fn read_sync(&self, _service: Service) -> Result<Vec<shogun_mcp::sync::FetchedItem>, String> {
                Ok(Vec::new())
            }
        }
        impl shogun_integrations::WriteExecutor for Exec {
            fn execute(&self, _service: Service, _op: &str, _args: serde_json::Value) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({ "id": "Label_1", "labels": [] }))
            }
        }
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        db.mail_label_set_flags("marketing", LabelFlags { sync_to_gmail: Some(true), ..Default::default() }).unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let mut rt = shogun_integrations::ConnectorRuntime::new(Exec(sent.clone()), Wave::One, true);
        rt.set_plan(pro());
        rt.set_composio_consent(true);
        rt.mark_connected(Service::Gmail, 1);
        let rt = Mutex::new(rt);
        let sink = RecSink::default();
        assert_eq!(label_sync_tick(&db, &RuntimeMailWriteTransport::new(&rt), &sink, &pro()), 1);
        // list_labels, create_label, then the label write itself: three requests, three rows.
        assert_eq!(sent.load(Ordering::SeqCst), 3);
        let rows = sink.0.lock().unwrap();
        assert_eq!(
            rows.len(),
            3,
            "every request that reached Composio is on the ledger"
        );
        assert!(rows
            .iter()
            .all(|r| r.route == Route::Mcp && !r.third_party && r.purpose == "integration_write"));
        assert_eq!(rows[0].chunk_xxh64, label_trace("list_labels").chunk_xxh64);
        assert_eq!(rows[1].chunk_xxh64, label_trace("create_label SHOGUN/Marketing").chunk_xxh64);
    }

    /// Issue #781: the post-sync write-back can start before Settings has refreshed the runtime
    /// after a revocation. Every write goes through the runtime, which re-reads the persisted
    /// consent first, so neither the label sync nor an archive reaches Gmail or the ledger.
    #[test]
    fn a_write_back_after_revocation_reaches_neither_gmail_nor_the_ledger() {
        use shogun_mcp::scope::{Service, Wave};
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        struct CountingExec(Arc<AtomicUsize>);
        impl shogun_mcp::sync::IntegrationTransport for CountingExec {
            fn read_sync(&self, _service: Service) -> Result<Vec<shogun_mcp::sync::FetchedItem>, String> {
                Ok(Vec::new())
            }
        }
        impl shogun_integrations::WriteExecutor for CountingExec {
            fn execute(&self, _service: Service, _op: &str, _args: serde_json::Value) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({ "id": "Label_1", "labels": [] }))
            }
        }
        let (db, _now) = db();
        seed_marketing_thread(&db, "t1");
        db.mail_label_set_flags("marketing", LabelFlags { sync_to_gmail: Some(true), ..Default::default() }).unwrap();
        db.mail_request_archive("gmail:t1").unwrap();

        let writes = Arc::new(AtomicUsize::new(0));
        let consent = Arc::new(AtomicBool::new(true));
        let mut rt = shogun_integrations::ConnectorRuntime::new(CountingExec(writes.clone()), Wave::One, true);
        rt.set_plan(pro());
        rt.set_composio_consent(true);
        rt.mark_connected(Service::Gmail, 1);
        let probe = consent.clone();
        rt.set_consent_probe(Arc::new(move || probe.load(Ordering::SeqCst)));
        let rt = Mutex::new(rt);
        // The user revokes; the runtime's own copy still says granted.
        consent.store(false, Ordering::SeqCst);

        let transport = RuntimeMailWriteTransport::new(&rt);
        let sink = RecSink::default();
        assert_eq!(label_sync_tick(&db, &transport, &sink, &pro()), 0);
        let explicit = explicit_requests_tick(&db, &transport, &sink, &pro());
        assert_eq!(explicit.archived, 0);
        assert_eq!(writes.load(Ordering::SeqCst), 0, "nothing reached Gmail");
        assert!(sink.0.lock().unwrap().is_empty(), "nothing was sent, so nothing is on the ledger");
    }
}
