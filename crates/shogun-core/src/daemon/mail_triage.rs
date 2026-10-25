//! Mail triage (issue #319) — the daemon's effect layer over [`shogun_memory::mail_triage`].
//!
//! Two halves:
//! - **Ingest-time Stage L** ([`Db::triage_mail_locked`]): called by `ingest_integration` for every
//!   synced Gmail thread while it holds the connection. Writes `mail_messages`, runs the pure
//!   classifier over the thread + world signals, upserts the estimate, and keeps the thread's
//!   `open_loops` row in step (FR-TG-12..15). No network, no model.
//! - **The Db surface** the three API faces and the Tauri commands share (invariant 6): list /
//!   get / user change / labels / sender rules / counts / cursors / the archive and label-sync
//!   candidate queries the write-back job consumes.

use super::*;
use shogun_memory::mail_triage::{
    self as mt, ArchiveStatus, BatchVerdict, FeedbackOrigin, LabelFlags, LabelRow, SenderRule, Triage,
    TriageCounts, TriageFilter, TriageRow, UserChange,
};

/// The grace period a freshly classified thread waits before Auto Archive may act on it
/// (FR-TG-22 (e)): lets a Stage B refinement or a user's own Gmail action land first.
pub const AUTO_ARCHIVE_GRACE_MS: i64 = 10 * 60 * 1000;
/// Max Auto Archive executions per tick (FR-TG-22).
pub const AUTO_ARCHIVE_PER_TICK: usize = 50;

impl Db {
    /// Stage L for one synced thread. Returns whether a triage row was written (false when the
    /// item carried no usable message metadata or a write failed — never panics; the event row
    /// is the durable record and triage is re-derived on the next sync).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn triage_mail_locked(
        &self,
        conn: &mut rusqlite::Connection,
        event_id: i64,
        subject: &str,
        body: &str,
        ts_ms: i64,
        meta: &shogun_mcp::sync::MailMeta,
        now: i64,
    ) -> bool {
        if meta.thread_id.trim().is_empty() || meta.messages.is_empty() {
            return false;
        }
        let thread_key = format!("gmail:{}", meta.thread_id.trim());
        let started = std::time::Instant::now();
        let msgs: Vec<mt::MailMessage> = meta
            .messages
            .iter()
            .filter(|m| !m.message_id.is_empty())
            .map(|m| mt::MailMessage {
                message_id: m.message_id.clone(),
                thread_key: thread_key.clone(),
                event_id,
                from_addr: m.from_addr.clone(),
                from_name: m.from_name.clone(),
                from_domain: mt::domain_of(&m.from_addr),
                to_addrs: m.to_addrs.clone(),
                cc_addrs: m.cc_addrs.clone(),
                is_from_me: m.is_from_me,
                in_reply_to: m.in_reply_to.clone(),
                gmail_label_ids: m.gmail_label_ids.clone(),
                has_list_unsubscribe: m.has_list_unsubscribe,
                has_ics: m.has_ics,
                internal_date_ms: if m.internal_date_ms > 0 { m.internal_date_ms } else { ts_ms },
            })
            .collect();
        if msgs.is_empty() {
            return false;
        }
        if let Err(e) = mt::upsert_messages(conn, &msgs, now) {
            self.note_fault("mail_triage.messages", MemoryFault::Query, e.fault_class());
            return false;
        }
        let signals = match mt::thread_signals(conn, &thread_key, subject, body, now) {
            Ok(Some(s)) => s,
            Ok(None) => return false,
            Err(e) => {
                self.note_fault("mail_triage.signals", MemoryFault::Query, e.fault_class());
                return false;
            }
        };
        let world = match mt::world_signals(conn, &signals.from_addr, subject) {
            Ok(w) => w,
            Err(e) => {
                self.note_fault("mail_triage.world", MemoryFault::Query, e.fault_class());
                return false;
            }
        };
        let rules = mt::list_sender_rules(conn).unwrap_or_default();
        let labels = mt::list_labels(conn).unwrap_or_default();
        let verdict = mt::local::classify(&signals, &world, &rules, &labels);
        let stored = match mt::upsert_local(conn, &thread_key, event_id, &verdict, now) {
            Ok(t) => t,
            Err(e) => {
                self.note_fault("mail_triage.upsert", MemoryFault::Query, e.fault_class());
                return false;
            }
        };
        let counterparty = world.known_person.as_ref().map(|p| p.person_id).filter(|id| *id > 0);
        let evidence_ts = if ts_ms > 0 { ts_ms } else { now };
        if let Err(e) = mt::sync_open_loop(conn, &thread_key, event_id, subject, &stored, counterparty, evidence_ts, now) {
            self.note_fault("mail_triage.open_loop", MemoryFault::Query, e.fault_class());
        }
        crate::metrics::record_global(crate::metrics::Slo::MailTriageLocal, started.elapsed().as_secs_f64() * 1000.0);
        true
    }

    /// The stored incremental-sync cursor for a service (FR-TG-09).
    pub fn sync_cursor(&self, service: &str) -> Option<shogun_mcp::sync::SyncCursor> {
        self.with_conn("mail_triage.cursor", |c| mt::get_cursor(c, service))
            .ok()
            .flatten()
            .map(|c| shogun_mcp::sync::SyncCursor { watermark_ms: c.watermark_ms, page_token: c.page_token })
    }

    /// Persist the cursor after a completed tick.
    pub fn set_sync_cursor(&self, service: &str, cursor: &shogun_mcp::sync::SyncCursor) {
        let now = self.now_ms();
        let stored = mt::SyncCursor { watermark_ms: cursor.watermark_ms, page_token: cursor.page_token.clone() };
        let _ = self.with_conn("mail_triage.cursor.set", |c| mt::set_cursor(c, service, &stored, now));
    }

    /// Register the connected account's own address (from onboarding / the connector's profile)
    /// so `same_domain` and "to me" work before the first SENT message is seen.
    pub fn add_mail_identity(&self, addr: &str) {
        let now = self.now_ms();
        let _ = self.with_conn("mail_triage.identity", |c| mt::add_identity(c, addr, now));
    }

    /// The user's own addresses recorded so far (lowercase, newest first).
    pub fn mail_identities(&self) -> Vec<String> {
        self.with_conn("mail_triage.identities", mt::identities).unwrap_or_default()
    }

    // reads (UI + API, invariant 6)

    /// `mail.triage.list`.
    pub fn mail_triage_list(&self, filter: &TriageFilter) -> Vec<TriageRow> {
        self.with_conn("mail_triage.list", |c| mt::list_triage(c, filter)).unwrap_or_default()
    }

    /// `mail.triage.get`.
    pub fn mail_triage_get(&self, thread_key: &str) -> Option<TriageRow> {
        self.with_conn("mail_triage.get", |c| mt::get_triage(c, thread_key)).ok().flatten()
    }

    /// The triage row whose subject matches an on-screen window title (the Fusion hook).
    pub fn mail_triage_for_title(&self, title: &str) -> Option<TriageRow> {
        self.with_conn("mail_triage.for_title", |c| mt::find_by_subject(c, title)).ok().flatten()
    }

    /// Counts since `since_ms` plus the live needs-reply backlog (Brief / Wrap / Notch).
    pub fn mail_triage_counts(&self, since_ms: i64) -> TriageCounts {
        self.with_conn("mail_triage.counts", |c| mt::counts(c, since_ms)).unwrap_or_default()
    }

    /// Is triage running at all (the V34 master switch)? Gmail keeps syncing into memory either
    /// way — this stops the *sorting*, which is a smaller thing than disconnecting the account.
    pub fn mail_triage_enabled(&self) -> bool {
        // Fails open, as `mt::triage_enabled` does: a database we cannot read is not a user
        // saying "stop sorting my mail". The same answer also decides which lane reads
        // Gmail for commitments (`consolidation_session_policy`): open means "Stage B does", which
        // is the lossy direction only if Stage B itself is broken — consistent with ingestion,
        // which on the same answer runs Stage L and queues the thread for Stage B.
        self.with_conn("mail_triage.enabled", |c| {
            Ok::<_, rusqlite::Error>(mt::triage_enabled(c))
        })
        .unwrap_or(true)
    }

    /// Turn triage on or off. Existing classifications are left alone: turning it back on should
    /// return the user to the inbox they had, not to one that forgot everything it learned.
    pub fn set_mail_triage_enabled(&self, enabled: bool) -> Result<(), String> {
        self.with_conn("mail_triage.set_enabled", move |c| mt::set_triage_enabled(c, enabled))
            .map_err(|_| "mail settings write failed".to_string())
    }

    /// User corrections since `since_ms` (issue #319 FR-TG-24 — the analytics figure only).
    pub fn mail_feedback_count(&self, since_ms: i64) -> i64 {
        self.with_conn("mail_triage.feedback_count", |c| mt::feedback_count_since(c, since_ms))
            .unwrap_or(0)
    }

    /// Unhandled `needs_reply` threads (the Notch counter).
    pub fn mail_needs_reply_count(&self) -> i64 {
        self.with_conn("mail_triage.needs_reply", mt::needs_reply_open_count).unwrap_or(0)
    }

    /// `mail.labels.list`.
    pub fn mail_labels(&self) -> Vec<LabelRow> {
        self.with_conn("mail_triage.labels", mt::list_labels).unwrap_or_default()
    }

    /// `mail.sender_rules.list`.
    pub fn mail_sender_rules(&self) -> Vec<SenderRule> {
        self.with_conn("mail_triage.rules", mt::list_sender_rules).unwrap_or_default()
    }

    // writes (local, L1)

    /// `mail.triage.set` — a Stage U correction (FR-TG-19). Mirrors the relabel into
    /// `feedback_events` (kind `state_resolve`, action `mail_relabel`, label keys only) so the
    /// nightly lesson distillation can cite it. Returns the updated row.
    pub fn mail_triage_set(
        &self,
        thread_key: &str,
        change: &UserChange,
        origin: FeedbackOrigin,
    ) -> Result<TriageRow, String> {
        let now = self.now_ms();
        let key = thread_key.to_string();
        let change = change.clone();
        self.with_conn_mut("mail_triage.set", move |c| {
            let db_err = |e: rusqlite::Error| format!("mail triage write failed: {}", crate::log_redact::scrub(&e.to_string()));
            let before = mt::get_triage(c, &key).map_err(db_err)?.ok_or_else(|| "unknown thread".to_string())?;
            let relabel = change.label_key.as_deref().is_some_and(|l| l != before.label_key)
                || change.bucket.is_some_and(|b| b != before.bucket);
            let feedback_event_id = if relabel {
                let to_label = change.label_key.clone().unwrap_or_else(|| before.label_key.clone());
                let scope_ref = mt::thread_sender_domain(c, &key).map_err(db_err)?;
                shogun_memory::lessons::record_feedback(
                    c,
                    shogun_memory::lessons::FeedbackKind::StateResolve,
                    if scope_ref.is_some() {
                        shogun_memory::lessons::LessonScope::Person
                    } else {
                        shogun_memory::lessons::LessonScope::Global
                    },
                    &shogun_memory::lessons::NewFeedback {
                        ts_ms: now,
                        action_kind: Some("mail_relabel"),
                        scope_ref: scope_ref.as_deref(),
                        before_text: Some(&before.label_key),
                        after_text: Some(&to_label),
                        ..Default::default()
                    },
                )
                .ok()
            } else {
                None
            };
            match mt::apply_user_change(c, &key, &change, origin, feedback_event_id, now).map_err(db_err)? {
                Ok(_) => {}
                Err(mt::UserChangeError::UnknownThread) => return Err("unknown thread".to_string()),
                Err(mt::UserChangeError::UnknownLabel) => return Err("unknown label".to_string()),
            }
            mt::get_triage(c, &key).map_err(db_err)?.ok_or_else(|| "unknown thread".to_string())
        })
        .map_err(|_| "mail triage write failed".to_string())
    }

    /// Create / edit a custom label (FR-TG-06). Returns the key.
    pub fn mail_label_upsert_custom(
        &self,
        key: Option<&str>,
        name: &str,
        definition: &str,
        default_bucket: mt::Bucket,
    ) -> Result<String, String> {
        let now = self.now_ms();
        let key = key.map(str::to_string);
        let (name, definition) = (name.to_string(), definition.to_string());
        self.with_conn("mail_triage.label.upsert", move |c| {
            mt::upsert_custom_label(c, key.as_deref(), &name, &definition, default_bucket, now)
        })
        .map_err(|_| "mail label write failed".to_string())
        .and_then(|r| {
            r.map_err(|e| match e {
                mt::LabelError::TooMany => format!("at most {} custom labels", mt::MAX_CUSTOM_LABELS),
                mt::LabelError::InvalidKey => "custom label keys start with custom:".to_string(),
                mt::LabelError::NotCustom => "built-in labels cannot be edited".to_string(),
                mt::LabelError::Unknown => "unknown label".to_string(),
            })
        })
    }

    /// Set a label's flags. The standing approvals (`auto_archive` / `sync_to_gmail`) are the
    /// FR-TG-21 grant — the caller (UI toggle / API with `confirm_standing`) has shown the
    /// standing-approval text before reaching here.
    pub fn mail_label_set_flags(&self, key: &str, flags: LabelFlags) -> Result<LabelRow, String> {
        let now = self.now_ms();
        let key = key.to_string();
        self.with_conn("mail_triage.label.flags", move |c| {
            if !mt::set_label_flags(c, &key, flags, now)? {
                return Ok(None);
            }
            mt::get_label(c, &key)
        })
        .map_err(|_| "mail label write failed".to_string())
        .and_then(|r| r.ok_or_else(|| "unknown label".to_string()))
    }

    /// Delete a custom label.
    pub fn mail_label_delete(&self, key: &str) -> Result<(), String> {
        let now = self.now_ms();
        let key = key.to_string();
        self.with_conn_mut("mail_triage.label.delete", move |c| mt::delete_custom_label(c, &key, now))
            .map_err(|_| "mail label write failed".to_string())
            .and_then(|r| {
                r.map_err(|e| match e {
                    mt::LabelError::NotCustom => "built-in labels cannot be deleted".to_string(),
                    _ => "unknown label".to_string(),
                })
            })
    }

    /// Add a sender rule by hand (`source = user`).
    pub fn mail_sender_rule_add(
        &self,
        match_kind: mt::SenderRuleMatch,
        match_value: &str,
        label_key: &str,
        bucket: Option<mt::Bucket>,
    ) -> Result<i64, String> {
        let now = self.now_ms();
        let (v, l) = (match_value.to_string(), label_key.to_string());
        self.with_conn("mail_triage.rule.add", move |c| {
            if mt::get_label(c, &l)?.is_none() {
                return Ok(Err("unknown label".to_string()));
            }
            mt::upsert_sender_rule(c, match_kind, &v, &l, bucket, mt::SenderRuleSource::User, now).map(Ok)
        })
        .map_err(|_| "mail rule write failed".to_string())
        .and_then(|r| r)
    }

    /// Delete a sender rule.
    pub fn mail_sender_rule_delete(&self, id: i64) -> Result<bool, String> {
        self.with_conn("mail_triage.rule.delete", |c| mt::delete_sender_rule(c, id))
            .map_err(|_| "mail rule write failed".to_string())
    }

    /// Threads awaiting Batch refinement, respecting the daily cap (FR-TG-03). `cap_remaining`
    /// is computed against submissions since local midnight (`day_start_ms`).
    pub fn mail_batch_pending(&self, day_start_ms: i64, daily_cap: usize) -> Vec<(String, i64)> {
        self.with_conn("mail_triage.batch.pending", |c| {
            let used = mt::submitted_since(c, day_start_ms)?.max(0) as usize;
            let remaining = daily_cap.saturating_sub(used);
            if remaining == 0 {
                return Ok(Vec::new());
            }
            mt::pending_for_batch(c, remaining)
        })
        .unwrap_or_default()
    }

    /// Atomically reserve threads and the daily allowance before provider access (#343).
    pub fn mail_batch_claim(&self, request_id: &str, items: &[(String, String)], day_start_ms: i64, cap: usize) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn("mail_triage.batch.claim", |c| {
            let tx = c.unchecked_transaction()?;
            if mt::submitted_since(&tx, day_start_ms)?.max(0) as usize + items.len() > cap { return Ok(false); }
            for (_, key) in items {
                let claimed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM mail_triage_batch_items WHERE thread_key=?1 AND applied_at_ms IS NULL)", [key], |row| row.get(0))?;
                if claimed { return Ok(false); }
            }
            mt::record_batch_items(&tx, request_id, items, now)?;
            tx.commit()?;
            Ok::<_, rusqlite::Error>(true)
        }).map_err(|_| "mail submission claim failed".to_string())
    }

    /// Persist a new attempt before egress; identical content can retry after terminal work.
    pub fn mail_batch_claim_attempt(&self, fingerprint: &str, items: &[(String, String)], day_start_ms: i64, cap: usize) -> Result<Option<String>, String> {
        let nonce: String = self.with_conn("mail_triage.batch.identity", |c| c.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0)))
            .map_err(|_| "mail submission identity failed".to_string())?;
        let request_id = format!("{fingerprint}:{nonce}");
        self.mail_batch_claim(&format!("request:{request_id}"), items, day_start_ms, cap)
            .map(|claimed| claimed.then_some(request_id))
    }

    /// Bind the saved request without dropping its thread claims or resetting submission time.
    pub fn mail_batch_bind(&self, request_id: &str, batch_id: &str) -> Result<(), String> {
        self.with_conn("mail_triage.batch.bind", |c| {
            c.execute("UPDATE mail_triage_batch_items SET batch_id=?2 WHERE batch_id=?1 AND applied_at_ms IS NULL", rusqlite::params![request_id, batch_id])
        }).map(|_| ()).map_err(|_| "mail submission bind failed".to_string())
    }

    /// Record a submitted batch: `(custom_id, thread_key)` pairs.
    pub fn mail_batch_record(&self, batch_id: &str, items: &[(String, String)]) -> bool {
        let now = self.now_ms();
        self.with_conn("mail_triage.batch.record", |c| mt::record_batch_items(c, batch_id, items, now)).is_ok()
    }

    /// Batches whose results have not been applied yet.
    pub fn mail_batch_unapplied(&self) -> Vec<mt::BatchLedger> {
        self.with_conn("mail_triage.batch.unapplied", mt::unapplied_batches).unwrap_or_default()
    }

    /// Apply Stage B verdicts and close the batch ledger entry. Returns rows changed.
    pub fn mail_batch_apply(&self, batch_id: &str, verdicts: &[BatchVerdict]) -> usize {
        let now = self.now_ms();
        let started = std::time::Instant::now();
        let changed = self
            .with_conn_mut("mail_triage.batch.apply", |c| {
                let mut n = 0;
                for v in verdicts {
                    if mt::apply_batch(c, v, now)? {
                        n += 1;
                    }
                }
                mt::mark_batch_applied(c, batch_id, now)?;
                Ok::<usize, shogun_memory::MemoryError>(n)
            })
            .unwrap_or(0);
        crate::metrics::record_global(crate::metrics::Slo::MailTriageBatchApply, started.elapsed().as_secs_f64() * 1000.0);
        if changed > 0 {
            if let Some(bus) = &self.bus {
                bus.publish(crate::bus::BusEvent::MailTriaged {
                    new: changed as u64,
                    needs_reply: self.mail_needs_reply_count().max(0) as u64,
                });
            }
        }
        changed
    }

    /// Abandon a batch (results unusable): free its threads for resubmission.
    pub fn mail_batch_abandon(&self, batch_id: &str) {
        let now = self.now_ms();
        let _ = self.with_conn("mail_triage.batch.abandon", |c| mt::mark_batch_applied(c, batch_id, now));
    }

    /// The custom labels + built-in definitions + distilled lessons the Stage B prompt needs.
    pub fn mail_batch_prompt_context(&self) -> (Vec<LabelRow>, Vec<String>) {
        self.with_conn("mail_triage.batch.context", |c| {
            let labels = mt::list_labels(c)?;
            let lessons = mt::triage_lessons(c, 10).unwrap_or_default();
            Ok::<_, rusqlite::Error>((labels, lessons))
        })
        .unwrap_or_default()
    }

    /// The prompt inputs for one thread: `(subject, latest body excerpt, sender display name,
    /// sender domain, to_me, gmail categories, stage-L label, reasons)`.
    pub fn mail_batch_thread_input(&self, thread_key: &str) -> Option<mt::BatchThreadInput> {
        self.with_conn("mail_triage.batch.input", |c| mt::batch_thread_input(c, thread_key)).ok().flatten()
    }

    // write-back candidates (FR-TG-20..22)

    /// The explicit per-thread requests awaiting execution: `(row, unarchive)`.
    pub fn mail_explicit_requests(&self) -> Vec<(mt::TriageRow, bool)> {
        self.with_conn("mail_triage.explicit", |c| mt::explicit_requests(c, AUTO_ARCHIVE_PER_TICK))
            .unwrap_or_default()
    }

    /// Threads Auto Archive may act on right now.
    pub fn mail_archive_candidates(&self) -> Vec<TriageRow> {
        let now = self.now_ms();
        self.with_conn("mail_triage.archive.candidates", |c| {
            mt::archive_candidates(c, now, AUTO_ARCHIVE_GRACE_MS, AUTO_ARCHIVE_PER_TICK)
        })
        .unwrap_or_default()
    }

    /// Threads whose Gmail label is out of step with a `sync_to_gmail` label.
    pub fn mail_label_sync_candidates(&self) -> Vec<TriageRow> {
        self.with_conn("mail_triage.label_sync.candidates", |c| mt::label_sync_candidates(c, AUTO_ARCHIVE_PER_TICK))
            .unwrap_or_default()
    }

    /// Record the outcome of an archive / unarchive write.
    pub fn mail_set_archive_status(&self, thread_key: &str, status: ArchiveStatus) -> bool {
        let now = self.now_ms();
        self.with_conn("mail_triage.archive.status", |c| {
            let changed = mt::set_archive_status(c, thread_key, status, now)?;
            match status {
                ArchiveStatus::Done => mt::set_inbox_label_locally(c, thread_key, false)?,
                ArchiveStatus::Undone => mt::set_inbox_label_locally(c, thread_key, true)?,
                _ => {}
            }
            Ok::<bool, rusqlite::Error>(changed)
        })
        .unwrap_or(false)
    }

    /// Record the label now applied on Gmail for a thread.
    pub fn mail_set_synced_label(&self, thread_key: &str, label_key: Option<&str>) {
        let now = self.now_ms();
        let _ = self.with_conn("mail_triage.label_sync.set", |c| mt::set_synced_label(c, thread_key, label_key, now));
    }

    /// Remember the Gmail label id created for a label.
    pub fn mail_set_gmail_label_id(&self, key: &str, gmail_label_id: &str) {
        let now = self.now_ms();
        let _ = self.with_conn("mail_triage.label.gmail_id", |c| mt::set_gmail_label_id(c, key, gmail_label_id, now));
    }

    // execution history (FR-AG-18 / FR-TG-21)

    /// Record one write-back attempt. Returns the history row id (0 on a write failure).
    #[allow(clippy::too_many_arguments)]
    pub fn mail_write_log(
        &self,
        thread_key: &str,
        action_kind: &str,
        add: &[String],
        remove: &[String],
        approval: mt::WriteApproval,
        ok: bool,
        error_class: Option<&str>,
    ) -> i64 {
        let now = self.now_ms();
        self.with_conn("mail_triage.write_log", |c| {
            mt::insert_write_log(c, thread_key, action_kind, add, remove, approval, ok, error_class, now)
        })
        .unwrap_or(0)
    }

    /// Recent write-back history, newest first.
    pub fn mail_write_log_list(&self, limit: usize) -> Vec<mt::WriteLogRow> {
        self.with_conn("mail_triage.write_log.list", |c| mt::list_write_log(c, limit)).unwrap_or_default()
    }

    /// One history row.
    pub fn mail_write_log_get(&self, id: i64) -> Option<mt::WriteLogRow> {
        self.with_conn("mail_triage.write_log.get", |c| mt::get_write_log(c, id)).ok().flatten()
    }

    /// Mark a history row undone.
    pub fn mail_write_log_mark_undone(&self, id: i64) -> bool {
        let now = self.now_ms();
        self.with_conn("mail_triage.write_log.undone", |c| mt::mark_write_undone(c, id, now)).unwrap_or(false)
    }

    /// Undistilled corrections for the nightly lesson distillation (FR-TG-19).
    pub fn mail_feedback_undistilled(&self, limit: usize) -> Vec<mt::FeedbackRow> {
        self.with_conn("mail_triage.feedback.undistilled", |c| mt::undistilled_feedback(c, limit)).unwrap_or_default()
    }

    /// Mark corrections distilled.
    pub fn mail_feedback_mark_distilled(&self, ids: &[i64]) -> bool {
        self.with_conn("mail_triage.feedback.distilled", |c| mt::mark_feedback_distilled(c, ids)).is_ok()
    }

    /// Sender domain of a thread (for distillation scope).
    pub fn mail_thread_sender_domain(&self, thread_key: &str) -> Option<String> {
        self.with_conn("mail_triage.sender_domain", |c| mt::thread_sender_domain(c, thread_key)).ok().flatten()
    }

    /// "Re-run on last N days" (FR-TG-19): non-user rows classified since `since_ms` drop back to
    /// Stage L below the Batch bar, so the next tick resubmits them with the current labels.
    /// Returns the number of rows reset.
    pub fn mail_rerun_since(&self, since_ms: i64) -> usize {
        let now = self.now_ms();
        self.with_conn("mail_triage.rerun", |c| mt::reset_for_rerun(c, since_ms, now)).unwrap_or(0)
    }

    /// A stored verdict for tests / diagnostics.
    pub fn mail_triage_verdict(&self, thread_key: &str) -> Option<Triage> {
        self.mail_triage_get(thread_key).map(|r| Triage {
            label_key: r.label_key,
            bucket: r.bucket,
            attention: r.attention,
            confidence: r.confidence,
            reasons: r.reasons,
            bucket_lock: r.bucket_lock,
        })
    }

    // API-face archive intents (FR-TG-30, issue #779)

    /// Record an archive request as `archive_status='pending'` for the desktop's write-back
    /// tick, which executes it (FR-TG-21). Nothing touches Gmail here. Callers: the desktop,
    /// once the user confirmed an API caller's queued request (`apply_confirmed_setting`), and
    /// [`Self::mail_api_request_archive`] under the label's standing approval. Idempotent on a
    /// row already pending; a thread already out of the inbox is refused so the caller learns
    /// nothing changed.
    pub fn mail_request_archive(&self, thread_key: &str) -> Result<(), String> {
        let now = self.now_ms();
        let key = thread_key.to_string();
        self.with_conn("mail_triage.archive.request", move |c| record_archive_request(c, &key, now))
            .map_err(|_| "mail triage write failed".to_string())
            .and_then(|r| r)
    }

    /// The inverse request. A thread the tick archived (`done`) becomes `unarchive_pending` and
    /// the next tick issues the Gmail write that puts INBOX back; a still-pending archive request
    /// is simply withdrawn. A thread that is in the inbox already is refused.
    pub fn mail_request_unarchive(&self, thread_key: &str) -> Result<(), String> {
        let now = self.now_ms();
        let key = thread_key.to_string();
        self.with_conn("mail_triage.unarchive.request", move |c| record_unarchive_request(c, &key, now))
            .map_err(|_| "mail triage write failed".to_string())
            .and_then(|r| r)
    }

    /// Whether an MCP / REST / CLI `mail.triage.archive` (or `unarchive`) needs the user's
    /// one-tap confirm before anything is recorded (issue #779). `Ok(false)`: the thread's label
    /// carries the user's standing Auto Archive approval (FR-TG-21), or the request changes
    /// nothing on Gmail (already requested, or withdrawing an archive that has not run).
    /// `Ok(true)`: it would change Gmail without any approval from the user. `Err`: the same
    /// refusals the request itself gives (unknown thread, already archived, not archived).
    pub fn mail_archive_needs_approval(&self, thread_key: &str, unarchive: bool) -> Result<bool, String> {
        let key = thread_key.to_string();
        self.with_conn("mail_triage.archive.classify", move |c| api_request_needs_approval(c, &key, unarchive))
            .map_err(|_| "mail triage read failed".to_string())
            .and_then(|r| r)
    }

    /// An API caller's archive / unarchive recorded directly, decided on the same connection as
    /// the write: only what [`Self::mail_archive_needs_approval`] lets through (a standing
    /// approval, or nothing changing on Gmail). Anything else is refused, so a face that skipped
    /// the approval queue fails closed instead of changing Gmail (issue #779).
    pub fn mail_api_request_archive(&self, thread_key: &str, unarchive: bool) -> Result<(), String> {
        let now = self.now_ms();
        let key = thread_key.to_string();
        self.with_conn("mail_triage.archive.api_request", move |c| {
            match api_request_needs_approval(c, &key, unarchive)? {
                Err(refusal) => Ok(Err(refusal)),
                Ok(true) => Ok(Err(MAIL_ARCHIVE_NEEDS_APPROVAL.to_string())),
                Ok(false) if unarchive => record_unarchive_request(c, &key, now),
                Ok(false) => record_archive_request(c, &key, now),
            }
        })
        .map_err(|_| "mail triage write failed".to_string())
        .and_then(|r| r)
    }
}

/// The refusal of a direct API archive / unarchive that needs the user's confirm (issue #779).
pub const MAIL_ARCHIVE_NEEDS_APPROVAL: &str =
    "invalid archive request: changing Gmail needs the user's approval in ShogunAI";

/// Classify an API caller's request against the thread and its label (issue #779). The outer
/// `Result` is the database; the inner one is the request's own refusal.
fn api_request_needs_approval(
    c: &rusqlite::Connection,
    key: &str,
    unarchive: bool,
) -> Result<Result<bool, String>, rusqlite::Error> {
    let Some(row) = mt::get_triage(c, key)? else {
        return Ok(Err("thread not found".to_string()));
    };
    // The label's standing approval covers threads filed there by SHOGUN or the user. One an API
    // caller moved there itself (`mail.triage.set`) still needs the user's tap: otherwise
    // relabelling first would skip the per-thread confirm.
    let standing = mt::get_label(c, &row.label_key)?.is_some_and(|label| label.auto_archive && label.enabled)
        && mt::last_relabel_origin(c, key)? != Some(FeedbackOrigin::Api);
    Ok(needs_approval(row.archive_status, standing, unarchive))
}

/// The decision itself, pure: `standing` is the thread's label carrying an enabled Auto Archive
/// approval. No standing approval covers putting mail back in the Inbox.
fn needs_approval(status: ArchiveStatus, standing: bool, unarchive: bool) -> Result<bool, String> {
    match (unarchive, status) {
        (false, ArchiveStatus::Done) => Err("invalid archive request: thread is already archived".to_string()),
        // Already requested: recording it again changes nothing.
        (false, ArchiveStatus::Pending) | (true, ArchiveStatus::UnarchivePending) => Ok(false),
        // Withdrawing an archive that has not run: nothing reaches Gmail.
        (true, ArchiveStatus::Pending) => Ok(false),
        (true, ArchiveStatus::Done) => Ok(true),
        (true, ArchiveStatus::None | ArchiveStatus::Failed | ArchiveStatus::Undone) => {
            Err("invalid unarchive request: thread is not archived".to_string())
        }
        (false, ArchiveStatus::UnarchivePending | ArchiveStatus::None | ArchiveStatus::Failed | ArchiveStatus::Undone) => {
            Ok(!standing)
        }
    }
}

fn record_archive_request(c: &rusqlite::Connection, key: &str, now: i64) -> Result<Result<(), String>, rusqlite::Error> {
    let Some(row) = mt::get_triage(c, key)? else {
        return Ok(Err("thread not found".to_string()));
    };
    match row.archive_status {
        ArchiveStatus::Done => Ok(Err("invalid archive request: thread is already archived".to_string())),
        ArchiveStatus::Pending => Ok(Ok(())),
        // Withdraws an opposite request that has not run yet.
        ArchiveStatus::UnarchivePending | ArchiveStatus::None | ArchiveStatus::Failed | ArchiveStatus::Undone => {
            mt::set_archive_status(c, key, ArchiveStatus::Pending, now).map(|_| Ok(()))
        }
    }
}

fn record_unarchive_request(c: &rusqlite::Connection, key: &str, now: i64) -> Result<Result<(), String>, rusqlite::Error> {
    let Some(row) = mt::get_triage(c, key)? else {
        return Ok(Err("thread not found".to_string()));
    };
    match row.archive_status {
        // Archived on Gmail: record the intent for the tick, which issues the write.
        ArchiveStatus::Done => mt::set_archive_status(c, key, ArchiveStatus::UnarchivePending, now).map(|_| Ok(())),
        ArchiveStatus::UnarchivePending => Ok(Ok(())),
        // The archive never ran — withdrawing the request is the whole undo.
        ArchiveStatus::Pending => mt::set_archive_status(c, key, ArchiveStatus::None, now).map(|_| Ok(())),
        ArchiveStatus::None | ArchiveStatus::Failed | ArchiveStatus::Undone => {
            Ok(Err("invalid unarchive request: thread is not archived".to_string()))
        }
    }
}

#[cfg(test)]
mod archive_request_tests {
    use super::{needs_approval, ArchiveStatus};

    #[test]
    fn an_api_archive_is_direct_only_under_a_standing_approval_or_when_gmail_is_untouched() {
        // Issue #779.
        for status in [ArchiveStatus::None, ArchiveStatus::Failed, ArchiveStatus::Undone, ArchiveStatus::UnarchivePending] {
            assert_eq!(needs_approval(status, false, false), Ok(true), "{status:?}");
            assert_eq!(needs_approval(status, true, false), Ok(false), "{status:?}");
        }
        assert_eq!(needs_approval(ArchiveStatus::Pending, false, false), Ok(false));
        assert!(needs_approval(ArchiveStatus::Done, true, false).is_err());
        // Unarchive: always the user's, except a withdrawal or a repeat.
        assert_eq!(needs_approval(ArchiveStatus::Done, true, true), Ok(true));
        assert_eq!(needs_approval(ArchiveStatus::Pending, false, true), Ok(false));
        assert_eq!(needs_approval(ArchiveStatus::UnarchivePending, false, true), Ok(false));
        for status in [ArchiveStatus::None, ArchiveStatus::Failed, ArchiveStatus::Undone] {
            assert!(needs_approval(status, true, true).is_err(), "{status:?}");
        }
    }
}
