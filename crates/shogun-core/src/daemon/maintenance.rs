//! Maintenance-domain notification mapping.

use super::*;
use serde_json::{json, Value};

/// Convert newly overdue commitments into local, non-egress notifications.
pub fn overdue_notifications(
    newly: &[shogun_memory::recompute::NewlyOverdue],
) -> Vec<shogun_agents::permission::Action> {
    use shogun_agents::permission::{Action, LocalAction};

    newly
        .iter()
        .map(|commitment| {
            Action::Local(LocalAction::ShowNotification {
                text: format!("Overdue: {}", commitment.description),
            })
        })
        .collect()
}

/// What one local maintenance pass changed ([`Db::run_local_maintenance`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalMaintenance {
    pub decayed: usize,
    pub corroborated: usize,
    pub overdue: usize,
    pub stale: usize,
    /// The commitments THIS pass flipped `open` → `overdue`. Each appears in exactly one
    /// pass — the status transition is the dedup watermark (see
    /// [`shogun_memory::recompute::recompute_overdue_and_staleness_detailed`]) — so
    /// [`overdue_notifications`] over this list fires once per item, never repeating.
    pub newly_overdue: Vec<shogun_memory::recompute::NewlyOverdue>,
    /// Faded, unresolved candidates removed by the state-table GC this pass.
    pub pruned: usize,
    /// Free-list pages handed back to the filesystem this pass.
    pub reclaimed_pages: i64,
}

/// A completed "Delete everything" ([`Db::delete_all_with_backups`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteAllOutcome {
    /// Rows removed from the live database, per table.
    pub report: shogun_memory::maintenance::DeleteReport,
    /// Snapshot, temp and side files unlinked from the backup directory.
    pub backups_removed: usize,
}

/// Why "Delete everything" did not fully complete. Never silent: every face turns this into an
/// error the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAllError {
    /// The database delete failed. Its transaction rolled back, so the database is untouched and
    /// no backup was removed either: the user can retry from exactly where they were.
    Database,
    /// The database delete committed (every row is gone), but some encrypted snapshots could not
    /// be removed. The database is consistent; retrying removes what is left.
    Backups {
        report: Box<shogun_memory::maintenance::DeleteReport>,
        failure: shogun_memory::backup::WipeFailure,
    },
}

/// State-table GC thresholds: an unresolved candidate below this confidence with no evidence for
/// this long is noise, not memory. 0.05 is far under the Low band's 0.4 cap (one 30-day half-life
/// from 0.4 is 0.2; three are 0.05), so nothing a user could still see is touched.
const PRUNE_MIN_CONFIDENCE: f64 = 0.05;
const PRUNE_SILENCE_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Pages per hourly reclamation pass: 8,192 × 4 KiB = 32 MiB, comfortably above a day's growth
/// and small enough to finish in tens of milliseconds behind the connection lock.
const RECLAIM_PAGES_PER_PASS: i64 = 8_192;
/// Rows per telemetry table per hourly pass — a day's worth of chat/usage rows is a few hundred.
const TELEMETRY_ROWS_PER_PASS: usize = 2_000;
/// Wall time one local-maintenance pass may spend on the V81 gram index (pending rows, then
/// back-fill of older history, newest first). The next pass resumes where this one stopped.
const GRAM_INDEX_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

impl Db {
    /// Export all user data as JSON (FR-SET-07). Local only — never a network send. `None` on a
    /// read failure.
    pub fn export_json(&self) -> Option<String> {
        self.with_conn(
            "maintenance.export_json",
            shogun_memory::maintenance::export_json,
        )
        .ok()
    }

    /// Delete all user data, keeping the schema (FR-SET-07). Returns the per-table deletion report,
    /// or `None` on failure (the transaction leaves the DB untouched).
    pub fn delete_all(&self) -> Option<shogun_memory::maintenance::DeleteReport> {
        // A pinned backup reader would retain pre-wipe encrypted pages in the WAL.
        // Lock order: backup_copy, then conn; release only after erasure and VACUUM.
        let _copy_guard = self.backup_copy.lock().ok()?;
        let report = self
            .with_conn_mut("maintenance.delete_all", |conn| {
                let report = shogun_memory::maintenance::delete_all(conn)?;
                self.activity_erase_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                self.clear_hot_locked();
                Ok::<_, rusqlite::Error>(report)
            })
            .ok()?;
        // "Delete my memory" must also mean the bytes: without this the file stayed at its
        // high-water mark with every freed (encrypted) page still on disk.
        if !self.vacuum_after_wipe() {
            eprintln!("[memory] vacuum after delete_all skipped — file keeps its size until the next pass");
        }
        Some(report)
    }

    /// "Delete everything" including the encrypted snapshots in the backup directory (#647).
    ///
    /// Order is the contract: the database delete runs first, in its transaction. Only once it
    /// has committed are the snapshots unlinked, so a failed database delete never costs the user
    /// their backups, and a failed unlink never leaves the database half-deleted — it is reported
    /// as [`DeleteAllError::Backups`] with the database already empty and consistent.
    pub fn delete_all_with_backups(
        &self,
        backups: &shogun_memory::backup::BackupCoordinator,
    ) -> Result<DeleteAllOutcome, DeleteAllError> {
        let report = self.delete_all().ok_or(DeleteAllError::Database)?;
        match backups.wipe_all() {
            Ok(backups_removed) => Ok(DeleteAllOutcome {
                report,
                backups_removed,
            }),
            Err(failure) => Err(DeleteAllError::Backups {
                report: Box::new(report),
                failure,
            }),
        }
    }

    /// Delete user data at or after `cutoff_ts` (unix ms), sweeping orphaned state (FR-SET-07 /
    /// #28). `None` on failure (the transaction leaves the DB untouched).
    pub fn delete_since(&self, cutoff_ts: i64) -> Option<shogun_memory::maintenance::DeleteReport> {
        self.with_conn_mut("maintenance.delete_since", |c| {
            let report = shogun_memory::maintenance::delete_since(c, cutoff_ts)?;
            self.activity_erase_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.remove_hot_since_locked(cutoff_ts);
            // A meeting's transcript and note rows are dated at its start, so for a meeting that
            // ran across the cutoff the line above does not reach them, although the delete
            // removed them with the meeting (#900).
            for &id in &report.meeting_rows_removed {
                self.remove_hot_locked(id);
            }
            Ok::<_, rusqlite::Error>(report)
        })
        .ok()
    }

    // state writes (deliberate)
    // Unlike capture, state writes are low-frequency and deliberate (Dream Cycle consolidation,
    // API propose). They return the new id or `None` on failure so the caller (e.g. a Dream Cycle
    // job) can mark itself failed rather than silently continuing.

    /// Insert a person with provenance (FR-ST-02).
    pub fn insert_person(&self, p: &NewPerson<'_>, provenance: &[Provenance]) -> Option<i64> {
        self.with_conn_mut("state.insert_person", |c| {
            state::insert_person(c, p, provenance)
        })
        .ok()
    }

    /// Insert a project with provenance.
    pub fn insert_project(&self, p: &NewProject<'_>, provenance: &[Provenance]) -> Option<i64> {
        self.with_conn_mut("state.insert_project", |c| {
            state::insert_project(c, p, provenance)
        })
        .ok()
    }

    /// Insert a commitment with provenance.
    pub fn insert_commitment(
        &self,
        c: &NewCommitment<'_>,
        provenance: &[Provenance],
    ) -> Option<i64> {
        let id = self
            .with_conn_mut("state.insert_commitment", |conn| {
                state::insert_commitment(conn, c, provenance)
            })
            .ok();
        if id.is_some() {
            notify_state_changed();
        }
        id
    }

    /// Insert an open loop with provenance.
    pub fn insert_open_loop(&self, l: &NewOpenLoop<'_>, provenance: &[Provenance]) -> Option<i64> {
        let id = self
            .with_conn_mut("state.insert_open_loop", |c| {
                state::insert_open_loop(c, l, provenance)
            })
            .ok();
        if id.is_some() {
            notify_state_changed();
        }
        id
    }

    // Concrete effects the nightly cycle drives through the `DreamJobRunner` seam (dreamcycle::jobs).
    // Compatibility methods keep their prior safe defaults; Dream execution uses the paired
    // `try_*` methods so a storage failure leaves the job failed and resumable.

    /// Events in `[from_ts, to_ts)` — the window a Consolidation job classifies (FR-DC-03).
    pub fn events_in_range(&self, from_ts: i64, to_ts: i64) -> Vec<event_log::EventText> {
        self.try_events_in_range(from_ts, to_ts).unwrap_or_default()
    }

    /// [`Self::events_in_range`] with storage failure preserved for Dream job decisions.
    pub fn try_events_in_range(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<event_log::EventText>, String> {
        self.with_conn_reported("events.in_range", |conn| {
            event_log::events_in_range(conn, from_ts, to_ts)
                .map_err(|_| "dream event-window read failed".to_string())
        })
    }

    /// Build and atomically publish closed period summaries through the injected summarizer seam.
    pub fn generate_period_summaries(
        &self,
        windows: &[shogun_memory::period_summaries::PeriodWindow],
        summarize: impl Fn(&[event_log::EventText]) -> Option<String>,
        generated: bool,
        generation_model: Option<&str>,
        now_ms: i64,
    ) -> Result<usize, String> {
        self.with_conn_mut_reported("period_summaries.generate", |conn| {
            shogun_memory::period_summaries::generate_closed(
                conn,
                windows,
                summarize,
                generated,
                generation_model,
                now_ms,
            )
            .map_err(|_| "period summary write failed".to_string())
        })
    }

    /// `memory.get_period_summaries`: the stored hour/day/week/month summaries overlapping
    /// `[from_ms, to_ms)`. Level is chosen from the span, falling back to finer levels when the
    /// coarse one is not generated yet (the current day has only closed hours). Bounded: at most
    /// `limit` periods, each text capped, so one call never returns a blob.
    pub fn period_summaries_json(&self, from_ms: i64, to_ms: i64, limit: usize) -> Result<Value, String> {
        use shogun_memory::period_summaries::{overlapping, PeriodLevel};
        const MAX_CHARS: usize = 1_500;
        let span = to_ms.saturating_sub(from_ms);
        let hour = 3_600_000_i64;
        let ladder: &[PeriodLevel] = if span <= 36 * hour {
            &[PeriodLevel::Hour]
        } else if span <= 10 * 24 * hour {
            &[PeriodLevel::Day, PeriodLevel::Hour]
        } else if span <= 45 * 24 * hour {
            &[PeriodLevel::Week, PeriodLevel::Day]
        } else {
            &[PeriodLevel::Month, PeriodLevel::Week, PeriodLevel::Day]
        };
        self.with_conn("memory.period_summaries", |c| {
            for level in ladder {
                let rows = overlapping(c, *level, from_ms, to_ms, limit)?;
                if rows.is_empty() {
                    continue;
                }
                let periods: Vec<Value> = rows
                    .into_iter()
                    .map(|r| {
                        json!({
                            "level": r.level,
                            "start_ms": r.period_start_ms,
                            "end_ms": r.period_end_ms,
                            "title": r.title,
                            "summary": r.content.chars().take(MAX_CHARS).collect::<String>(),
                            "model_written": r.generated,
                            "stale": r.is_dirty,
                        })
                    })
                    .collect();
                return Ok::<_, shogun_memory::MemoryError>(
                    json!({ "level": level.as_str(), "from_ms": from_ms, "to_ms": to_ms, "periods": periods }),
                );
            }
            Ok(json!({ "from_ms": from_ms, "to_ms": to_ms, "periods": [],
                "note": "no summaries generated for this window yet; use memory.search with from_ms/to_ms or memory.get_wrap for today" }))
        })
        .map_err(|_| "memory_unavailable".to_string())
    }

    /// Bounded historical V22 work that must re-enter a later Full compression run.
    pub(crate) fn pending_period_summary_windows(
        &self,
        limit: usize,
    ) -> Result<Vec<shogun_memory::period_summaries::PeriodWindow>, String> {
        self.with_conn_reported("period_summaries.pending", |conn| {
            shogun_memory::period_summaries::pending_windows(conn, limit)
                .map_err(|_| "period summary pending read failed".to_string())
        })
    }

    /// The same window split by destination: `cloud` may go to the Batch lane, `local_only`
    /// (meeting text) is classified on-device (A-2, `docs/meeting-text-on-the-search-spine.md`).
    pub fn events_in_range_partitioned(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> event_log::PartitionedEvents {
        self.try_events_in_range_partitioned(from_ts, to_ts)
            .unwrap_or_default()
    }

    /// Whether the window has anything the Batch lane could read, without materialising it.
    /// Storage failure reads as "nothing" — the callers are status lines, not submissions.
    pub fn has_batch_eligible_events(&self, from_ts: i64, to_ts: i64) -> bool {
        self.with_conn_reported("events.has_batch_eligible", |conn| {
            event_log::has_batch_eligible_in_range(conn, from_ts, to_ts)
                .map_err(|_| "batch eligibility probe failed".to_string())
        })
        .unwrap_or(false)
    }

    /// [`Self::events_in_range_partitioned`] with storage failure preserved before Batch submit.
    pub fn try_events_in_range_partitioned(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<event_log::PartitionedEvents, String> {
        self.with_conn_reported("events.in_range_partitioned", |conn| {
            event_log::events_in_range_partitioned(conn, from_ts, to_ts)
                .map_err(|_| "dream partitioned event-window read failed".to_string())
        })
    }

    /// Threads whose last activity is in `[from_ts, to_ts]` — the window a Compression job
    /// summarises (Issue #63). Empty on a lock/read failure so a hiccup fails the job (leaving the
    /// cycle resumable) rather than crashing the daemon.
    pub fn active_threads_between(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> Vec<shogun_memory::thread::ThreadRow> {
        self.try_active_threads_between(from_ts, to_ts)
            .unwrap_or_default()
    }

    pub fn try_active_threads_between(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<shogun_memory::thread::ThreadRow>, String> {
        self.with_conn_reported("thread.active_between", |conn| {
            shogun_memory::thread::active_between(conn, from_ts, to_ts)
                .map_err(|_| "dream active-thread read failed".to_string())
        })
    }

    /// Every event body in one thread, oldest first — the material the Compression summariser reads
    /// (Issue #63). Empty on a lock/read failure.
    pub fn thread_event_texts(&self, thread_key: &str) -> Vec<event_log::EventText> {
        self.try_thread_event_texts(thread_key).unwrap_or_default()
    }

    pub fn try_thread_event_texts(
        &self,
        thread_key: &str,
    ) -> Result<Vec<event_log::EventText>, String> {
        self.with_conn_reported("thread.event_texts", |conn| {
            shogun_memory::thread::event_texts(conn, thread_key)
                .map_err(|_| "dream thread-event read failed".to_string())
        })
    }

    /// Write a thread's day-summary (Issue #63). Best-effort: a lock/write failure is swallowed so
    /// a hiccup fails the job, not the daemon. Uses the daemon clock for `updated_at`.
    pub fn set_thread_summary(&self, thread_key: &str, summary: &str) {
        let _ = self.try_set_thread_summary(thread_key, summary);
    }

    pub fn try_set_thread_summary(&self, thread_key: &str, summary: &str) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("thread.set_summary", |conn| {
            shogun_memory::thread::set_summary(conn, thread_key, summary, now)
                .map_err(|_| "dream thread-summary write failed".to_string())
        })
    }

    /// Read back a thread's summary (`None` when unset, absent, or on a read failure) — the
    /// Compression job's effect is verified through this, since `ThreadRow` does not carry it.
    pub fn thread_summary(&self, thread_key: &str) -> Option<String> {
        self.try_thread_summary(thread_key).ok().flatten()
    }

    pub fn try_thread_summary(&self, thread_key: &str) -> Result<Option<String>, String> {
        self.with_conn_reported("thread.get_summary", |conn| {
            shogun_memory::thread::get_summary(conn, thread_key)
                .map_err(|_| "dream thread-summary read failed".to_string())
        })
    }

    /// Sessions whose `started_at` is in `[from_ts, to_ts]` — the interval analogue of
    /// [`Self::active_threads_between`], with the same failure behaviour.
    pub fn active_sessions_between(&self, from_ts: i64, to_ts: i64) -> Vec<i64> {
        self.try_active_sessions_between(from_ts, to_ts)
            .unwrap_or_default()
    }

    pub fn try_active_sessions_between(
        &self,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<i64>, String> {
        self.with_conn_reported("session.active_between", |conn| {
            shogun_memory::session::active_between(conn, from_ts, to_ts)
                .map_err(|_| "dream active-session read failed".to_string())
        })
    }

    /// Every event body attached to one session, oldest first — the session analogue of
    /// [`Self::thread_event_texts`].
    pub fn session_event_texts(&self, session_id: i64) -> Vec<event_log::EventText> {
        self.try_session_event_texts(session_id).unwrap_or_default()
    }

    pub fn try_session_event_texts(
        &self,
        session_id: i64,
    ) -> Result<Vec<event_log::EventText>, String> {
        self.with_conn_reported("session.event_texts", |conn| {
            shogun_memory::session::event_texts(conn, session_id)
                .map_err(|_| "dream session-event read failed".to_string())
        })
    }

    /// Write a session's day-summary, best-effort like [`Self::set_thread_summary`].
    pub fn set_session_summary(&self, session_id: i64, summary: &str) {
        let _ = self.try_set_session_summary(session_id, summary);
    }

    pub fn try_set_session_summary(&self, session_id: i64, summary: &str) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("session.set_summary", |conn| {
            shogun_memory::session::set_summary(conn, session_id, summary, now)
                .map_err(|_| "dream session-summary write failed".to_string())
        })
    }

    /// Read back a session's summary (`None` when unset, absent, or on a read failure).
    pub fn session_summary(&self, session_id: i64) -> Option<String> {
        self.try_session_summary(session_id).ok().flatten()
    }

    pub fn try_session_summary(&self, session_id: i64) -> Result<Option<String>, String> {
        self.with_conn_reported("session.get_summary", |conn| {
            shogun_memory::session::get_summary(conn, session_id)
                .map_err(|_| "dream session-summary read failed".to_string())
        })
    }

    /// The DISTINCT sessions owning the given events — the query-time consume path (Issue #63).
    /// Empty on a lock/read failure or empty input.
    pub fn session_ids_for_events(&self, event_ids: &[i64]) -> Vec<i64> {
        self.with_conn("session.session_ids_for_events", |c| {
            shogun_memory::session::session_ids_for_events(c, event_ids)
        })
        .unwrap_or_default()
    }

    /// The saved summaries of the given sessions in ONE query (Issue #63) — `(id, thread_key,
    /// summary)` for the sessions that have a non-null summary, ordered by id. Replaces the
    /// per-session [`Self::session_summary`] N+1 on the consume path. Empty on a lock/read failure
    /// or empty input.
    pub fn session_summaries_for(&self, ids: &[i64]) -> Vec<(i64, String, Option<String>)> {
        self.with_conn("session.summaries_for_sessions", |c| {
            shogun_memory::session::summaries_for_sessions(c, ids)
        })
        .unwrap_or_default()
    }

    /// Descriptions already present in `commitments` + `open_loops`, for consolidation dedup — so a
    /// re-run over the same range (crash-resume, FR-DC-04) doesn't add the same candidate twice.
    /// Distinct hours in `[from_ts, to_ts)` that produced at least one event — the Coverage
    /// numerator (spec §D2). Zero on a read failure so a locked DB degrades to "nothing seen"
    /// rather than taking the window down.
    pub fn hours_covered(&self, from_ts: i64, to_ts: i64) -> i64 {
        self.with_conn("event_log.hours_covered", |c| {
            event_log::hours_covered(c, from_ts, to_ts)
        })
        .unwrap_or(0)
    }

    /// Events recorded in `[from_ts, to_ts)` — the first number in the Yield funnel.
    pub fn events_count(&self, from_ts: i64, to_ts: i64) -> i64 {
        self.with_conn("event_log.count_in_range", |c| {
            event_log::count_in_range(c, from_ts, to_ts)
        })
        .unwrap_or(0)
    }

    /// Whether the event log holds anything at all — the first-run signal for the
    /// context-insufficient verdict (issue #310). A store fault answers `true`: an unreachable
    /// store is reported through `memory_health` (issue #121: a fault is never "empty"), and
    /// "nothing seen yet" over a year of history would send the user to wait for something
    /// that already happened.
    pub fn has_ever_captured(&self) -> bool {
        self.with_conn("event_log.any_since", |c| event_log::any_since(c, 0))
            .unwrap_or(true)
    }

    /// Return descriptions already present in commitments and open loops.
    #[cfg(test)]
    pub fn existing_state_descriptions(&self) -> std::collections::HashSet<String> {
        self.try_existing_state_descriptions().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn try_existing_state_descriptions(
        &self,
    ) -> Result<std::collections::HashSet<String>, String> {
        self.with_conn_reported("state.existing_descriptions", |c| {
            let mut set = std::collections::HashSet::new();
            set.extend(
                shogun_memory::state::list_commitments(c)
                    .map_err(|_| "dream commitment dedup read failed".to_string())?
                    .into_iter()
                    .map(|row| row.description),
            );
            set.extend(
                shogun_memory::state::list_open_loops(c)
                    .map_err(|_| "dream open-loop dedup read failed".to_string())?
                    .into_iter()
                    .map(|row| row.description),
            );
            Ok(set)
        })
    }

    /// Persist extracted candidates linked to `event_id` (FR-ST-02). Returns the new row ids.
    pub fn persist_candidates(
        &self,
        event_id: i64,
        candidates: &[shogun_memory::extract::Candidate],
    ) -> Vec<i64> {
        self.try_persist_candidates(event_id, candidates)
            .unwrap_or_default()
    }

    pub fn try_persist_candidates(
        &self,
        event_id: i64,
        candidates: &[shogun_memory::extract::Candidate],
    ) -> Result<Vec<i64>, String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("extract.persist_candidates", |c| {
            shogun_memory::extract::persist_candidates_deduped(c, event_id, candidates, now, now)
                .map_err(|_| "dream candidate write failed".to_string())
        })
    }

    pub fn try_promote_candidate_for_event(
        &self,
        event_id: i64,
        candidate: &shogun_memory::extract::Candidate,
    ) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("extract.promote_candidate", |conn| {
            shogun_memory::extract::promote_candidate_for_event(
                conn,
                event_id,
                candidate,
                now,
            )
            .map_err(|_| "dream candidate promotion failed".to_string())
        })
    }

    /// Recompute overdue status + open-loop staleness from `now` (FR-ST-21). Returns
    /// `(commitments_flagged, loops_touched)`; `(0,0)` on a lock/write failure.
    pub fn recompute_overdue_and_staleness(&self, now_ms: i64) -> (usize, usize) {
        self.try_recompute_overdue_and_staleness(now_ms)
            .unwrap_or((0, 0))
    }

    pub fn try_recompute_overdue_and_staleness(
        &self,
        now_ms: i64,
    ) -> Result<(usize, usize), String> {
        self.with_conn_mut_reported("recompute.recompute_overdue_and_staleness", |g| {
            shogun_memory::recompute::recompute_overdue_and_staleness(g, now_ms)
                .map_err(|_| "dream state-update write failed".to_string())
        })
    }

    /// Dream Cycle people hygiene ([`shogun_memory::people_quality::sweep_people`]).
    pub fn try_sweep_people(&self, now_ms: i64) -> Result<(usize, usize), String> {
        self.with_conn_mut_reported("people.sweep", |g| {
            shogun_memory::people_quality::sweep_people(g, now_ms)
                .map_err(|_| "dream people sweep failed".to_string())
        })
    }

    /// Record a person identity seen in an event, merging it into the right person
    /// ([`shogun_memory::identity::observe`]).
    ///
    /// The rule this enforces is do-not-mis-merge: only an exact channel identity (same address,
    /// or same handle on the same platform) merges automatically. A name collision is kept
    /// separate and reported, because fusing two real people is disruptive to undo while a missed
    /// merge is easy to fix later.
    ///
    /// Nothing calls this automatically yet — the connectors are what will supply senders and
    /// participants, and they are not live. It is exercised by tests and reachable from the
    /// Memory API in the meantime.
    pub fn observe_identity(
        &self,
        incoming: &shogun_memory::identity::Identity,
        seen_name: Option<&str>,
        event_id: i64,
    ) -> Option<shogun_memory::identity::Observed> {
        let now = self.now_ms();
        self.with_conn_mut("identity.observe", |c| {
            shogun_memory::identity::observe(c, incoming, seen_name, event_id, now)
        })
        .ok()
    }

    /// The maintenance that needs no model call.
    ///
    /// The full Dream Cycle also classifies the day's events through the Batch lane, which needs
    /// the Select KK key (invariant 5) — that half is not wired yet. These passes are the part
    /// that can run today. Distinct Batch-reviewed sources can strengthen a cautious
    /// suggestion. Corroborate before decay so age uses the newest qualifying evidence.
    pub fn run_local_maintenance(&self, now_ms: i64, half_life_ms: i64) -> LocalMaintenance {
        let _ = self.with_conn("dictionary_autoadd.expire", |conn| {
            super::dictionary_autoadd::prune(conn, now_ms)
        });
        let _ = self.with_conn("writing_voice.expire", |conn| {
            shogun_memory::writing_voice::expire(conn, now_ms)
        });
        // One row per real person or project: rows sharing a normalised name or alias fold into one
        // (idempotent; a no-op once folded). Before anything that reads or ages those rows.
        let deduped = self
            .with_conn_mut("state.dedupe_entities", |c| shogun_memory::entity_dedupe::dedupe_exact(c, now_ms))
            .unwrap_or(0);
        // The owner's own person rows (profile or Mac account name) are classed `self`: never listed.
        let _ = self.with_conn("people.mark_owner", shogun_memory::people_quality::mark_owner_rows);
        // Lightly mentioned "your work" project pages from an older instruction get one rewrite
        // under the current one (#1340; a no-op once rewritten).
        let _ = self.with_conn("pages.requeue_stale_user_projects", shogun_memory::entity_pages::requeue_stale_user_projects);
        // The names the user goes by in chat apps, learned from one-to-one chats, sent messages and
        // the hourly review; a person row made for one folds into the owner's own row.
        let owners = shogun_memory::people_quality::owner_names();
        if self
            .with_conn_mut("people.self_names", |c| shogun_memory::self_identity::learn(c, &owners, now_ms))
            .unwrap_or(0)
            > 0
        {
            notify_state_changed();
        }
        // Projects an earlier build renamed to a title line ("X · Y") get their own name back.
        let _ = self.with_conn("state.restore_project_names", |c| {
            shogun_memory::entity_names::restore_project_names(c, now_ms)
        });
        // Fold repeated sightings stored as separate rows (idempotent; a no-op once folded). Before
        // corroboration so evidence lands on the surviving row.
        let collapsed = self
            .with_conn_mut("state.collapse_repeats", |c| shogun_memory::state_collapse::collapse_existing(c, now_ms))
            .map(|r| r.commitments_merged + r.open_loops_merged)
            .unwrap_or(0);
        // Rows the pre-review rules raised for repeated sightings drop back below the dashboard
        // (owner decision 2026-10-13). Before corroboration, which may raise a row again on
        // Batch-reviewed evidence.
        let _ = self.with_conn_mut("recompute.cap_unreviewed_actions", |c| {
            shogun_memory::recompute::cap_unreviewed_actions(c)
        });
        // Review items earlier builds wrote straight into the tables leave the dashboard, and the
        // last week's go back to the loop consolidator as signals (owner decision 2026-10-14). A
        // no-op once done.
        let _ = self.with_conn_mut("loops.retire_unconfirmed", |c| shogun_memory::loops::retire_unconfirmed(c, now_ms));
        let corroborated = self.try_corroborate(now_ms).unwrap_or(0);
        let decayed = self.decay_confidence(now_ms, half_life_ms);
        // The detailed pass reports WHICH commitments flipped open→overdue right now, so the
        // caller can notify each exactly once (the flip itself is the dedup watermark).
        let (newly_overdue, stale) = self
            .with_conn_mut("recompute.overdue_and_staleness_detailed", |c| {
                shogun_memory::recompute::recompute_overdue_and_staleness_detailed(c, now_ms)
            })
            .unwrap_or_default();
        let overdue = newly_overdue.len();
        // Commitments and open loops retire on their horizon timer (1 / 3 / 30 days unseen),
        // after the overdue flip so a past-due row ages like any other.
        let aged = self
            .with_conn("loops.age_by_horizon", |c| shogun_memory::loops::age_by_horizon(c, now_ms))
            .unwrap_or(0);
        // Two-character search index (V81): drain rows the write path did not index, then
        // back-fill older history newest-first. Time-boxed so an hourly pass stays short.
        let _ = self.with_conn("search.gram_index", |conn| {
            shogun_memory::search::maintain_gram_index(conn, GRAM_INDEX_BUDGET)
        });
        let _ = self.with_conn("search_documents.reconcile", |conn| {
            let _ = shogun_memory::search_documents::backfill_batch(conn, 100, now_ms);
            shogun_memory::search_documents::reconcile_invalidated(conn, 100, now_ms)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        });
        // GC after decay, so a row that just crossed the floor this pass goes in the same pass.
        let pruned = self
            .with_conn_mut("recompute.prune_faded", |c| {
                shogun_memory::recompute::prune_faded(c, now_ms, PRUNE_MIN_CONFIDENCE, PRUNE_SILENCE_MS)
            })
            .unwrap_or(0);
        // Telemetry-like tables age out under documented windows (see
        // `shogun_memory::maintenance::prune_telemetry`); bounded per pass.
        let _ = self.with_conn("maintenance.prune_telemetry", |c| {
            shogun_memory::maintenance::prune_telemetry(c, now_ms, TELEMETRY_ROWS_PER_PASS)
        });
        let reclaimed_pages = self
            .reclaim_space(RECLAIM_PAGES_PER_PASS)
            .map(|r| r.freelist_before - r.freelist_after)
            .unwrap_or(0);
        if deduped + collapsed + decayed + corroborated + overdue + stale + aged + pruned > 0 {
            notify_state_changed();
        }
        LocalMaintenance {
            decayed,
            corroborated,
            overdue,
            stale,
            newly_overdue,
            pruned,
            reclaimed_pages,
        }
    }

    /// One bounded space-reclamation pass (see `shogun_memory::maintenance::reclaim_space`).
    /// Runs on the raw connection rather than through `with_conn`: the Dream-effect fence wraps
    /// writes in a transaction, and `incremental_vacuum` must run outside one. `None` when the
    /// lock is poisoned or the pass fails — reclamation is best-effort by design; it never
    /// competes with a user-facing read for more than one bounded slice.
    pub fn reclaim_space(
        &self,
        max_pages: i64,
    ) -> Option<shogun_memory::maintenance::ReclaimReport> {
        let guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.is_autocommit() {
            return None;
        }
        shogun_memory::maintenance::reclaim_space(&guard, max_pages).ok()
    }

    /// After "delete all": give the whole file back. `VACUUM` rebuilds the database into a fresh
    /// file — the freed ciphertext pages disappear rather than lingering on disk — and converts
    /// an older file to `auto_vacuum = INCREMENTAL` so the hourly pass can keep it small from
    /// then on. Best-effort and outside any transaction; a failure leaves a correct (if larger)
    /// database.
    pub fn vacuum_after_wipe(&self) -> bool {
        let guard = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.is_autocommit() {
            return false;
        }
        guard
            .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
            .is_ok()
    }

    pub fn decay_confidence(&self, now_ms: i64, half_life_ms: i64) -> usize {
        self.try_decay_confidence(now_ms, half_life_ms).unwrap_or(0)
    }

    pub fn try_decay_confidence(&self, now_ms: i64, half_life_ms: i64) -> Result<usize, String> {
        self.with_conn_mut_reported("recompute.decay_confidence", |g| {
            shogun_memory::recompute::decay_confidence(g, now_ms, half_life_ms)
                .map_err(|_| "dream confidence write failed".to_string())
        })
    }

    pub fn try_corroborate(&self, now_ms: i64) -> Result<usize, String> {
        self.with_conn_mut_reported("recompute.corroborate", |conn| {
            shogun_memory::recompute::corroborate_at(conn, now_ms)
                .map_err(|_| "dream evidence refresh failed".to_string())
        })
    }

    /// The high-water mark of already-consolidated events (max `input_to_ts` of completed
    /// consolidations) — the scheduler's next window starts here (FR-DC-04). `None` before any cycle.
    pub fn last_consolidated_to(&self) -> Option<i64> {
        self.try_last_consolidated_to().ok().flatten()
    }

    pub fn try_last_consolidated_to(&self) -> Result<Option<i64>, String> {
        self.with_conn_reported("jobs.last_consolidated_to", |conn| {
            shogun_memory::jobs::last_consolidated_to(conn)
                .map_err(|_| "dream high-water read failed".to_string())
        })
    }

    /// Demote Warm embeddings older than `cutoff_ms` to the int8 Cold tier (FR-MEM-04). Returns the
    /// number moved.
    pub fn demote_cold(&self, cutoff_ms: i64) -> usize {
        self.try_demote_cold(cutoff_ms).unwrap_or(0)
    }

    pub fn try_demote_cold(&self, cutoff_ms: i64) -> Result<usize, String> {
        self.with_conn_mut_reported("cold.demote_older_than", |g| {
            shogun_memory::cold::demote_older_than(g, cutoff_ms)
                .map_err(|_| "dream cold-demotion write failed".to_string())
        })
    }

    // Context Fusion → Notch actions
    // The product core (§6.1, "press a button, work is done"): map the current state rows into
    // Fusion input, assemble the ranked action candidates for the focused screen, and hand them to
    // the Notch panel. Matching against the screen, urgency, the floor, ranking and the
    // confidence gate all live in shogun-fusion (`candidates` + `assemble`, FR-CF-01/03,
    // FR-ST-20) so `crates/fusion-eval` scores exactly this pipeline; the daemon supplies rows.

    /// The state rows the action row is assembled from, as Fusion views. Visibility is the
    /// regular `state::list_*` reads' (unsupported rows stay hidden); the three `fusion_supply`
    /// reads only add a commitment's direction / counterparty, a loop's counterparty and a
    /// person's aliases / addresses by id. Any read failure leaves that part empty — the row then
    /// falls back to less, never to an error.
    pub fn fusion_rows(&self) -> shogun_fusion::candidates::StateRows {
        use shogun_fusion::candidates::{
            CommitmentInput, OpenLoopInput, PersonInput, ProjectInput, StateRows,
        };
        use shogun_memory::fusion_supply;

        let links = self
            .with_conn("fusion.commitment_links", fusion_supply::commitment_links)
            .unwrap_or_default();
        let loop_people = self
            .with_conn("fusion.open_loop_counterparties", fusion_supply::open_loop_counterparties)
            .unwrap_or_default();
        let mut terms = self
            .with_conn("fusion.person_terms", fusion_supply::person_terms)
            .unwrap_or_default();
        StateRows {
            people: self
                .people()
                .into_iter()
                .map(|p| {
                    let t = terms.remove(&p.id).unwrap_or_default();
                    PersonInput {
                        id: p.id,
                        name: p.display_name,
                        aliases: t.aliases,
                        emails: t.emails,
                        confidence: p.confidence,
                    }
                })
                .collect(),
            projects: self
                .projects()
                .into_iter()
                .map(|p| ProjectInput { id: p.id, name: p.name, status: p.status, confidence: p.confidence })
                .collect(),
            commitments: self
                .commitment_rows()
                .into_iter()
                .map(|c| {
                    let link = links.get(&c.id).copied().unwrap_or_default();
                    CommitmentInput {
                        id: c.id,
                        description: c.description,
                        theirs: link.theirs,
                        counterparty_id: link.counterparty_id,
                        due_at: c.due_at,
                        status: c.status,
                        confidence: c.confidence,
                        deferred_until: c.deferred_until,
                    }
                })
                .collect(),
            open_loops: self
                .open_loop_rows()
                .into_iter()
                .map(|l| OpenLoopInput {
                    id: l.id,
                    counterparty_id: loop_people.get(&l.id).copied(),
                    kind: l.kind,
                    description: l.description,
                    // #764: a reply is only offered to a real address (fusion checks it); the
                    // description is never a recipient.
                    counterparty_email: l.counterparty_email,
                    status: l.status,
                    confidence: l.confidence,
                    // #926: a local mail guess ranks lower, reads as a possibility, never sends.
                    inferred: l.inferred,
                    deferred_until: l.deferred_until,
                })
                .collect(),
        }
    }

    /// Assemble the context-action candidates for the current screen (§6.1, SLO-02: ≤4 actions).
    /// Reads people / projects / commitments / open loops ([`Self::fusion_rows`]), matches them
    /// against the focused screen and ranks them in shogun-fusion, and returns the
    /// confidence-gated [`ContextCache`]. Low-confidence state never becomes an action
    /// (FR-ST-20); finished and snoozed rows are not candidates; a row needs a real match with
    /// the screen, an urgent due date, or the typed intent to earn a button (FR-CF-03).
    pub fn context_actions(
        &self,
        screen: shogun_fusion::assemble::ScreenContext,
        intent_hint: Option<String>,
    ) -> shogun_fusion::assemble::ContextCache {
        use shogun_fusion::assemble::Intent;

        let states =
            shogun_fusion::candidates::build_candidates(&screen, &self.fusion_rows(), self.now_ms());

        // D-5: scope-matched learned lessons (this app + global), top-k, ride along for the
        // generation prompt. Content only — assemble() never lets them touch levels.
        let lessons = self.lessons_for_screen(&screen.app_bundle_id, LESSON_TOP_K);
        // FR-PAT-05: the Work Patterns priors — reorder and select only; levels stay derived.
        let priors = self.pattern_priors(&screen.app_bundle_id);
        shogun_fusion::assemble::assemble_with_priors(
            screen,
            &states,
            "",
            &Intent { hint: intent_hint },
            &lessons,
            &priors,
        )
    }

    /// The live ranking priors (FR-PAT-05): recent per-kind adoption counts plus the profile's
    /// habit / rhythm / avoid supply for the current hour and app. Empty on any failure — the
    /// un-personalized ranking is always the safe fallback.
    pub fn pattern_priors(&self, context_app: &str) -> shogun_fusion::assemble::PatternPriors {
        let now = self.now_ms();
        let hour = super::briefs::local_hour_now(now);
        self.with_conn("patterns.priors", |conn| {
            let supply = shogun_memory::work_patterns::priors_supply(
                conn,
                now,
                hour,
                (!context_app.is_empty()).then_some(context_app),
            )?;
            let adoption = shogun_memory::lessons::acceptance_by_kind(
                conn,
                now - shogun_memory::work_patterns::WEEK_MS,
            )?;
            Ok::<_, rusqlite::Error>(shogun_fusion::assemble::PatternPriors {
                adoption,
                habit: supply.habit,
                in_band: supply.in_band,
                avoid: supply.avoid,
            })
        })
        .unwrap_or_default()
    }

    /// Recompute the Work Patterns profile from the last 30 days (FR-PAT-03; the Dream Cycle's
    /// PatternDistillation job body). Returns the row count, `None` on failure.
    pub fn distill_work_patterns(&self, now_ms: i64, gmt_offset_secs: i32) -> Option<usize> {
        self.try_distill_work_patterns(now_ms, gmt_offset_secs).ok()
    }

    pub fn try_distill_work_patterns(
        &self,
        now_ms: i64,
        gmt_offset_secs: i32,
    ) -> Result<usize, String> {
        self.with_conn_mut_reported("patterns.distill", |conn| {
            shogun_memory::work_patterns::distill(conn, now_ms, gmt_offset_secs)
                .map_err(|_| "dream pattern-distillation write failed".to_string())
        })
    }

    /// Promote stable, feedback-evidenced activity patterns to `lessons` rows (kind='pattern',
    /// design §4-5) — they ride the existing injection / Learned-UI / decay machinery. `None`
    /// on failure; one entry per upserted lesson otherwise (the wording pass reads
    /// `needs_phrasing` off each).
    pub fn promote_patterns_to_lessons(
        &self,
        now_ms: i64,
    ) -> Option<Vec<shogun_memory::work_patterns::PromotedPattern>> {
        self.try_promote_patterns_to_lessons(now_ms).ok()
    }

    pub fn try_promote_patterns_to_lessons(
        &self,
        now_ms: i64,
    ) -> Result<Vec<shogun_memory::work_patterns::PromotedPattern>, String> {
        self.with_conn_mut_reported("patterns.promote", |conn| {
            shogun_memory::work_patterns::promote_patterns_to_lessons(conn, now_ms)
                .map_err(|_| "dream pattern-promotion write failed".to_string())
        })
    }

    /// Replace a lesson's instruction with a Batch-phrased equivalent. Refusals (empty or
    /// over-long text) come back as `Some(false)` — the template stays, nothing breaks.
    pub fn reword_lesson(&self, id: i64, instruction: &str, now_ms: i64) -> Option<bool> {
        self.try_reword_lesson(id, instruction, now_ms).ok()
    }

    pub fn try_reword_lesson(
        &self,
        id: i64,
        instruction: &str,
        now_ms: i64,
    ) -> Result<bool, String> {
        self.with_conn_reported("lessons.reword", |conn| {
            shogun_memory::lessons::reword_lesson(conn, id, instruction, now_ms)
                .map_err(|_| "dream lesson-reword write failed".to_string())
        })
    }

    /// Take the one pending noticed moment (§5e): the strongest never-noticed pattern past the
    /// notice threshold, stamped as shown in the same call — one notice per pattern, ever. The
    /// nightly Brief consumes this; the Notch banner then reads it back via
    /// [`Self::recent_pattern_notice`] for the in-place Undo.
    pub fn take_pattern_notice(&self) -> Option<shogun_memory::work_patterns::PatternRow> {
        self.try_take_pattern_notice().ok().flatten()
    }

    pub fn try_take_pattern_notice(
        &self,
    ) -> Result<Option<shogun_memory::work_patterns::PatternRow>, String> {
        let now = self.now_ms();
        self.with_conn_reported("patterns.take_notice", |conn| {
            let row = shogun_memory::work_patterns::pending_notice(conn)
                .map_err(|_| "dream pattern-notice read failed".to_string())?;
            if let Some(ref row) = row {
                shogun_memory::work_patterns::mark_noticed(conn, row.id, now)
                    .map_err(|_| "dream pattern-notice write failed".to_string())?;
            }
            Ok(row)
        })
    }

    /// Read the next notice without consuming it. MorningBrief uses this so a failed save cannot
    /// lose the one-shot announcement.
    pub fn try_pending_pattern_notice(
        &self,
    ) -> Result<Option<shogun_memory::work_patterns::PatternRow>, String> {
        self.with_conn_reported("patterns.pending_notice", |conn| {
            shogun_memory::work_patterns::pending_notice(conn)
                .map_err(|_| "dream pattern-notice read failed".to_string())
        })
    }

    /// Consume a notice after its brief has been saved successfully.
    pub fn try_mark_pattern_noticed(&self, id: i64) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("patterns.mark_noticed", |conn| {
            shogun_memory::work_patterns::mark_noticed(conn, id, now)
                .map_err(|_| "dream pattern-notice write failed".to_string())?
                .then_some(())
                .ok_or_else(|| "dream pattern-notice row was missing".to_string())
        })
    }

    /// The pattern noticed within the last `within_ms`, still active — the Notch banner's supply.
    pub fn recent_pattern_notice(
        &self,
        within_ms: i64,
    ) -> Option<shogun_memory::work_patterns::PatternRow> {
        let now = self.now_ms();
        self.with_conn("patterns.recent_notice", |conn| {
            shogun_memory::work_patterns::recent_notice(conn, now - within_ms)
        })
        .ok()
        .flatten()
    }

    // state reads → Fusion supply
    // The daemon reads state rows and maps them into Fusion's input types, so Context Fusion and
    // the Morning Brief run on real DB data. The confidence gate lives in Fusion (FR-ST-20); the
    // daemon only supplies the rows.

    /// Commitments as Fusion/Brief input. `overdue` is derived from the status or a past due time.
    /// Resolved rows (`done`/`cancelled`) are excluded so a commitment the user marked done from
    /// the panel stops feeding drafts, chat memory, counts, and the Morning Brief — not just the
    /// panel view.
    pub fn commitments_due(&self, now_ms: i64) -> Vec<CommitmentDue> {
        self.try_commitments_due(now_ms).unwrap_or_default()
    }

    /// [`Self::commitments_due`] with the failure kept (issue #121). A caller that shows a count
    /// or feeds an answer must not read a dead store as "you owe nobody anything".
    pub fn try_commitments_due(&self, now_ms: i64) -> MemoryResult<Vec<CommitmentDue>> {
        let rows = self.try_commitment_rows()?;
        Ok(rows
            .into_iter()
            .filter(|r| r.status != "done" && r.status != "cancelled" && !r.is_deferred(now_ms))
            .map(|r| CommitmentDue {
                overdue: r.status == "overdue" || r.due_at.is_some_and(|d| d < now_ms),
                description: r.description,
                due_at_ms: r.due_at,
                days_until_due: r.due_at.map(|d| CommitmentDue::days_until(d, now_ms)),
                confidence: r.confidence,
                provenance_event_id: r.first_event_id,
            })
            .collect())
    }

    /// Retained provenance excerpts for an extracted memory.
    pub fn knowledge_rows(&self) -> MemoryResult<Vec<shogun_memory::knowledge::Row>> {
        self.with_conn("knowledge.list", shogun_memory::knowledge::list)
    }

    pub fn knowledge_evidence(&self, key: &str) -> MemoryResult<Vec<state::EvidenceRow>> {
        self.with_conn("knowledge.evidence", |conn| shogun_memory::knowledge::evidence(conn, key))
    }

    pub fn knowledge_neighbors(&self, key: &str) -> MemoryResult<Vec<shogun_memory::knowledge::Row>> {
        self.with_conn("knowledge.neighbors", |conn| shogun_memory::knowledge::neighbors(conn, key))
    }

    /// Retained provenance excerpts for an extracted memory.
    pub fn state_evidence(&self, table: state::StateTable, id: i64) -> MemoryResult<Vec<state::EvidenceRow>> {
        self.with_conn("state.evidence", |conn| state::evidence(conn, table, id))
    }

    /// Bounded neighbors connected by shared provenance, not inferred semantic relationships.
    pub fn related_state(&self, table: state::StateTable, id: i64) -> MemoryResult<Vec<state::RelatedState>> {
        self.with_conn("state.related", |conn| state::related_state(conn, table, id))
    }

    /// People rows (Memory API `state.people.list`).
    pub fn people(&self) -> Vec<state::PersonRow> {
        self.try_people().unwrap_or_default()
    }

    /// Bounded ranked viewer page.
    pub fn try_people_page(&self, limit: i64, offset: i64) -> MemoryResult<(Vec<state::PersonRow>, bool)> {
        self.with_conn("state.people.page", |c| {
            let more = c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM people WHERE {} ORDER BY {} LIMIT 1 OFFSET ?1)",
                shogun_memory::entity_pages::visible_sql(state::StateTable::People, "people"),
                shogun_memory::entity_pages::rank_sql(state::StateTable::People, "people")), [offset.saturating_add(limit)], |r| r.get(0))?;
            Ok::<_, rusqlite::Error>((state::list_people_page(c, limit, offset)?, more))
        })
    }

    /// [`Self::people`] with the failure kept (issue #121).
    pub fn try_people(&self) -> MemoryResult<Vec<state::PersonRow>> {
        self.with_conn("state.people.list", state::list_people)
    }

    /// One person by id (`state.people.get`).
    pub fn person(&self, id: i64) -> Option<state::PersonRow> {
        self.with_conn("state.people.get", |c| state::get_person(c, id))
            .ok()
            .flatten()
    }

    /// Where a person's or project's evidence comes from (`state.people.get`, the Memory page's
    /// "seen in" label; issue #1362), most-cited first and at most six. A failed read is an empty
    /// list: the row still shows, without the label.
    pub fn evidence_sources(&self, table: state::StateTable, id: i64) -> Vec<state::EvidenceSource> {
        const LIMIT: usize = 6;
        self.with_conn("state.evidence.sources", |c| state::evidence_sources(c, table, id, LIMIT))
            .unwrap_or_default()
    }

    /// Project rows (`state.projects.list`).
    pub fn projects(&self) -> Vec<state::ProjectRow> {
        self.try_projects().unwrap_or_default()
    }

    /// Bounded ranked viewer page.
    pub fn try_projects_page(&self, limit: i64, offset: i64) -> MemoryResult<(Vec<state::ProjectRow>, bool)> {
        self.with_conn("state.projects.page", |c| {
            let more = c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM projects WHERE {} ORDER BY {} LIMIT 1 OFFSET ?1)",
                shogun_memory::entity_pages::visible_sql(state::StateTable::Projects, "projects"),
                shogun_memory::entity_pages::rank_sql(state::StateTable::Projects, "projects")), [offset.saturating_add(limit)], |r| r.get(0))?;
            Ok::<_, rusqlite::Error>((state::list_projects_page(c, limit, offset)?, more))
        })
    }

    /// [`Self::projects`] with the failure kept (issue #121).
    pub fn try_projects(&self) -> MemoryResult<Vec<state::ProjectRow>> {
        self.with_conn("state.projects.list", state::list_projects)
    }

    /// One project by id (`state.projects.get`).
    pub fn project(&self, id: i64) -> Option<state::ProjectRow> {
        self.with_conn("state.projects.get", |c| state::get_project(c, id))
            .ok()
            .flatten()
    }

    /// One commitment by id (`state.commitments.get`).
    pub fn commitment(&self, id: i64) -> Option<state::CommitmentRow> {
        self.with_conn("state.commitments.get", |c| state::get_commitment(c, id))
            .ok()
            .flatten()
    }

    /// All commitment rows with ids (panel list — the UI needs the id to resolve a row).
    pub fn commitment_rows(&self) -> Vec<state::CommitmentRow> {
        self.try_commitment_rows().unwrap_or_default()
    }

    /// [`Self::commitment_rows`] with the failure kept (issue #121).
    pub fn try_commitment_rows(&self) -> MemoryResult<Vec<state::CommitmentRow>> {
        self.with_conn("state.commitments.list", state::list_commitments)
    }

    /// All open-loop rows with ids (panel list).
    pub fn open_loop_rows(&self) -> Vec<state::OpenLoopRow> {
        self.try_open_loop_rows().unwrap_or_default()
    }

    /// [`Self::open_loop_rows`] with the failure kept (issue #121).
    pub fn try_open_loop_rows(&self) -> MemoryResult<Vec<state::OpenLoopRow>> {
        self.with_conn("state.open_loops.list", state::list_open_loops)
    }

    /// Mark a commitment done (user resolved it from the panel). `true` if a row changed.
    pub fn resolve_commitment(&self, id: i64) -> bool {
        let now = self.now_ms();
        let changed = self
            .with_conn("state.commitments.resolve", |c| {
                state::set_commitment_status(c, id, state::CommitmentStatus::Done, now)
            })
            .is_ok_and(|n| n > 0);
        if changed {
            notify_state_changed();
        }
        changed
    }

    /// Close an open loop (user resolved it from the panel). `true` if a row changed.
    pub fn resolve_open_loop(&self, id: i64) -> bool {
        let now = self.now_ms();
        let changed = self
            .with_conn("state.open_loops.close", |c| state::close_open_loop(c, id, now))
            .is_ok_and(|n| n > 0);
        if changed {
            notify_state_changed();
        }
        changed
    }

    /// Open contradiction flags (issue #253), newest first — the Context Health pane and
    /// `state.disputes.list` supply. Empty on a store failure (the pane has nothing to decide).
    pub fn state_disputes_open(&self, limit: usize) -> Vec<shogun_memory::disputes::Dispute> {
        self.with_conn("state.disputes.list", |c| {
            shogun_memory::disputes::list_open(c, limit)
        })
        .unwrap_or_default()
    }

    /// Settle one contradiction flag (`state.disputes.resolve`, L1). `Ok(false)` when the
    /// dispute is unknown or already resolved; `Err` carries only the failure class, never row
    /// content. The losing commitment is cancelled, not deleted.
    pub fn state_dispute_resolve(
        &self,
        id: i64,
        resolution: shogun_memory::disputes::Resolution,
    ) -> Result<bool, String> {
        let now = self.now_ms();
        let resolved = self
            .with_conn_mut("state.disputes.resolve", |c| {
                shogun_memory::disputes::resolve(c, id, resolution, now)
            })
            .map_err(|fault| format!("dispute resolve failed: {}", fault.as_str()))?;
        if resolved {
            notify_state_changed();
        }
        Ok(resolved)
    }

    /// Correct one state row and pin it (`state.update`, issue #290, L1). `body` is the shared
    /// request JSON (`{"table","id",...fields}`) every face sends. `Ok(false)` when the row does
    /// not exist; `Err` is a fixed message (never row content): `invalid …` for a bad request.
    pub fn state_update(&self, body: &str) -> Result<bool, String> {
        let (table, id, patch) =
            shogun_memory::state_edit::parse_request(body).map_err(|e| e.to_string())?;
        let now = self.now_ms();
        self.with_conn_mut("state.update", |c| {
            match shogun_memory::state_edit::update_state(c, table, id, &patch, now) {
                Ok(changed) => Ok(Ok(changed)),
                Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
                Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
            }
        })
        .map_err(|fault| format!("state update failed: {}", fault.as_str()))?
        .map_err(str::to_string)
    }

    /// Add a commitment / open loop the user wrote (`state.create`, L1, local). `body` is the
    /// shared request JSON. `Ok(None)` when `person_id` names no person; `Err` is a fixed message.
    pub fn state_create(&self, body: &str) -> Result<Option<i64>, String> {
        let req = shogun_memory::state_edit::parse_create_request(body).map_err(|e| e.to_string())?;
        let now = self.now_ms();
        let created = self
            .with_conn_mut("state.create", |c| {
                match shogun_memory::state_edit::create_state(c, &req, now) {
                    Ok(id) => Ok(Ok(id)),
                    Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
                    Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
                }
            })
            .map_err(|fault| format!("state create failed: {}", fault.as_str()))?
            .map_err(str::to_string)?;
        if created.is_some() {
            notify_state_changed();
        }
        Ok(created)
    }

    /// Delete one state row outright (`state.delete`, issue #290, L1). Its events stay; the same
    /// old events cannot re-create it. Same body / result shape as [`Self::state_update`]
    /// (only `table` and `id` are read).
    pub fn state_delete(&self, body: &str) -> Result<bool, String> {
        let (table, id, patch) =
            shogun_memory::state_edit::parse_request(body).map_err(|e| e.to_string())?;
        if patch != shogun_memory::state_edit::StatePatch::default() {
            return Err("invalid field".to_string());
        }
        let now = self.now_ms();
        self.with_conn_mut("state.delete", |c| {
            match shogun_memory::state_edit::delete_state(c, table, id, now) {
                Ok(changed) => Ok(Ok(changed)),
                Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
                Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
            }
        })
        .map_err(|fault| format!("state delete failed: {}", fault.as_str()))?
        .map_err(str::to_string)
    }

    /// Split a wrong entity merge (`state.split`, FR-ST-10, L1). `body` is the shared request
    /// JSON (`{"table","id","name","event_ids","aliases"}`). `Ok(None)` when the row does not
    /// exist; `Err` is a fixed message, as in [`Self::state_update`].
    pub fn state_split(&self, body: &str) -> Result<Option<shogun_memory::state_split::SplitOutcome>, String> {
        let req = shogun_memory::state_split::parse_request(body).map_err(|e| e.to_string())?;
        let now = self.now_ms();
        self.with_conn_mut("state.split", |c| {
            match shogun_memory::state_split::split(c, &req, now) {
                Ok(outcome) => Ok(Ok(outcome)),
                Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
                Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
            }
        })
        .map_err(|fault| format!("state split failed: {}", fault.as_str()))?
        .map_err(str::to_string)
    }

    /// What one person / project row rests on (`state.split.evidence`): aliases and provenance-event
    /// metadata, never event content. `Ok(None)` when the row does not exist; `Err` is
    /// `invalid table` for a table that cannot be split, or a store failure.
    pub fn state_split_evidence(
        &self,
        table: &str,
        id: i64,
    ) -> Result<Option<shogun_memory::state_split::Evidence>, String> {
        use shogun_memory::state::StateTable;
        let table = StateTable::parse(table)
            .filter(|t| matches!(t, StateTable::People | StateTable::Projects))
            .ok_or_else(|| "invalid table".to_string())?;
        self.with_conn("state.split_evidence", |c| shogun_memory::state_split::evidence(c, table, id))
            .map_err(|fault| format!("state evidence failed: {}", fault.as_str()))
    }

    /// Possible duplicates (`state.merge.candidates`, issue #1362): the related pairs nothing
    /// merged, newest first. `Err` is `invalid table` for a table that cannot be merged, or a
    /// store failure.
    pub fn state_merge_candidates(&self, table: &str) -> Result<Vec<shogun_memory::state_merge::Candidate>, String> {
        use shogun_memory::state::StateTable;
        let table = StateTable::parse(table)
            .filter(|t| matches!(t, StateTable::People | StateTable::Projects))
            .ok_or_else(|| "invalid table".to_string())?;
        self.with_conn("state.merge_candidates", |c| shogun_memory::state_merge::candidates(c, table))
            .map_err(|fault| format!("state merge candidates failed: {}", fault.as_str()))
    }

    /// Merge the records the user named (`state.merge`, L1). `body` is the shared request JSON
    /// (`{"table","ids"}`). `Ok(None)` when a row does not exist; `Err` is a fixed message, as in
    /// [`Self::state_split`].
    pub fn state_merge(&self, body: &str) -> Result<Option<shogun_memory::state_merge::MergeOutcome>, String> {
        let req = shogun_memory::state_merge::parse_merge(body).map_err(|e| e.to_string())?;
        let now = self.now_ms();
        self.with_conn_mut("state.merge", |c| match shogun_memory::state_merge::merge(c, &req, now) {
            Ok(outcome) => Ok(Ok(outcome)),
            Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
            Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
        })
        .map_err(|fault| format!("state merge failed: {}", fault.as_str()))?
        .map_err(str::to_string)
    }

    /// Keep two records apart (`state.keep_separate`, L1). `body` is `{"table","id","other_id"}`.
    /// `Ok(None)` when a row does not exist.
    pub fn state_keep_separate(&self, body: &str) -> Result<Option<()>, String> {
        let req = shogun_memory::state_merge::parse_keep_separate(body).map_err(|e| e.to_string())?;
        let now = self.now_ms();
        self.with_conn_mut("state.keep_separate", |c| match shogun_memory::state_merge::keep_separate(c, &req, now) {
            Ok(outcome) => Ok(Ok(outcome)),
            Err(shogun_memory::state_edit::EditError::Invalid(why)) => Ok(Err(why)),
            Err(shogun_memory::state_edit::EditError::Db(e)) => Err(e),
        })
        .map_err(|fault| format!("state keep separate failed: {}", fault.as_str()))?
        .map_err(str::to_string)
    }

    /// One editable entity page, using the same projection as the world model.
    pub fn world_model_entity(&self, table: &str, id: i64) -> Result<Option<shogun_memory::state_edit::ExportRow>, String> {
        let table = shogun_memory::state::StateTable::parse(table)
            .filter(|table| matches!(table, shogun_memory::state::StateTable::People | shogun_memory::state::StateTable::Projects))
            .ok_or_else(|| "invalid table".to_string())?;
        self.with_conn("state.entity.get", |c| shogun_memory::state_edit::export_entity(c, table, id))
            .map_err(|_| "Entity unavailable.".to_string())
    }

    /// The whole world model for export (`state.export`, issue #290). Low-band rows are kept only
    /// when `include_low` (FR-API-06, the same rule the list tools apply); kept rows below the
    /// fact band carry `possibly`. `None` on a store failure.
    pub fn world_model(
        &self,
        include_low: bool,
    ) -> Option<shogun_memory::state_edit::WorldModel> {
        use shogun_mcp::memory_api::{read_inclusion, ReadInclusion};
        let mut model = self
            .with_conn("state.export", shogun_memory::state_edit::export)
            .ok()?;
        model.filter(|row| match read_inclusion(row.confidence, include_low) {
            ReadInclusion::Included { possibly } => {
                row.possibly = possibly;
                true
            }
            ReadInclusion::Excluded => false,
        });
        Some(model)
    }

    /// [`Self::world_model`] rendered as Markdown — the portable, human-readable export.
    pub fn world_model_markdown(&self, include_low: bool) -> Option<String> {
        let model = self.world_model(include_low)?;
        Some(shogun_memory::state_edit::to_markdown(&model, self.now_ms()))
    }

    /// Delete all extracted state (commitments + open loops + their provenance). Event log,
    /// people, and projects are untouched. `true` on success.
    pub fn clear_state(&self) -> bool {
        let cleared = self.with_conn_mut("state.clear", state::clear_state).is_ok();
        if cleared {
            notify_state_changed();
        }
        cleared
    }

    /// One open loop by id (`state.open_loops.get`).
    pub fn open_loop(&self, id: i64) -> Option<state::OpenLoopRow> {
        self.with_conn("state.open_loops.get", |c| state::get_open_loop(c, id))
            .ok()
            .flatten()
    }
}

/// Tell the host shell the panel's state tables moved (issue #357). The panel lists commitments
/// and open loops and its status line counts them, so both parts re-read. A no-op outside the
/// desktop app (no sink installed, and no panel at all without feature `desktop`); coalesced
/// there, so a burst of writes is one push.
pub(crate) fn notify_state_changed() {
    #[cfg(feature = "desktop")]
    {
        crate::panel_events::notify(crate::panel_events::PanelPart::State);
        crate::panel_events::notify(crate::panel_events::PanelPart::Status);
    }
}
