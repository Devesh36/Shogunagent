//! Dream-cycle job ledger, status, and summaries.

use super::*;

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

/// Durable fencing identity for one exclusive Full-cycle runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DreamRunLease {
    pub(crate) owner: String,
    pub(crate) generation: i64,
}

pub(crate) const DREAM_RUN_LEASE_MS: i64 = 45 * 60 * 1000;

#[derive(Clone)]
struct DreamEffectFence {
    cycle_id: String,
    lease: DreamRunLease,
}

thread_local! {
    static DREAM_EFFECT_FENCE: RefCell<Option<DreamEffectFence>> = const { RefCell::new(None) };
}

pub(crate) struct DreamEffectFenceGuard {
    previous: Option<DreamEffectFence>,
    not_send: PhantomData<Rc<()>>,
}

impl Drop for DreamEffectFenceGuard {
    fn drop(&mut self) {
        DREAM_EFFECT_FENCE.with(|active| {
            active.replace(self.previous.take());
        });
    }
}

pub(crate) fn enter_dream_effect_fence(
    cycle_id: &str,
    lease: &DreamRunLease,
) -> DreamEffectFenceGuard {
    let current = DreamEffectFence {
        cycle_id: cycle_id.to_string(),
        lease: lease.clone(),
    };
    let previous = DREAM_EFFECT_FENCE.with(|active| active.replace(Some(current)));
    DreamEffectFenceGuard {
        previous,
        not_send: PhantomData,
    }
}

pub(super) fn has_dream_effect_fence() -> bool {
    DREAM_EFFECT_FENCE.with(|active| active.borrow().is_some())
}

/// Start a cross-process write transaction and validate the active Dream owner inside it.
///
/// `BEGIN IMMEDIATE` takes SQLite's write reservation before validation. A second connection
/// therefore cannot claim a newer generation between this check and the caller's effect write.
pub(super) fn begin_dream_effect_transaction(
    conn: &rusqlite::Connection,
    now: i64,
) -> Result<bool, String> {
    DREAM_EFFECT_FENCE.with(|active| {
        let active = active.borrow();
        let Some(fence) = active.as_ref() else {
            return Ok(false);
        };
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|_| "dream effect transaction begin failed".to_string())?;
        let owned = shogun_memory::dream_cycles::owns_live_run(
            conn,
            &fence.cycle_id,
            &fence.lease.owner,
            fence.lease.generation,
            now,
        )
        .map_err(|_| {
            let _ = conn.execute_batch("ROLLBACK");
            "dream effect fence read failed".to_string()
        })?;
        if owned {
            Ok(true)
        } else {
            let _ = conn.execute_batch("ROLLBACK");
            Err(crate::dreamcycle::outcome::LEASE_LOST.to_string())
        }
    })
}

impl Db {
    // Dream Cycle job ledger (FR-DC-04)
    // Persist each job's state so a killed cycle resumes by skipping the `done` jobs. The plan
    // vocabulary (JobKind/JobState) is shogun-core's; storage keeps strings, mapped here.

    /// Record a job's state for a cycle (upsert on `(cycle_id, kind)`). Returns false on a write
    /// failure so the caller can react. `input_from_ts..input_to_ts` is the range the job consumed.
    pub fn record_job(
        &self,
        cycle_id: &str,
        kind: JobKind,
        state: JobState,
        input_from_ts: i64,
        input_to_ts: i64,
    ) -> bool {
        self.try_record_job(cycle_id, kind, state, input_from_ts, input_to_ts)
            .is_ok()
    }

    /// [`Self::record_job`] with storage failure preserved for Dream execution.
    pub fn try_record_job(
        &self,
        cycle_id: &str,
        kind: JobKind,
        state: JobState,
        input_from_ts: i64,
        input_to_ts: i64,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("jobs.upsert", |c| {
            shogun_memory::jobs::upsert(
                c,
                cycle_id,
                job_kind_str(kind),
                job_state_str(state),
                input_from_ts,
                input_to_ts,
                now,
            )
            .map_err(|_| "dream job ledger write failed".to_string())
        })
    }

    /// Fenced ledger write for an exclusively-owned Full cycle.
    pub(crate) fn try_record_job_fenced(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
        kind: JobKind,
        state: JobState,
        input_from_ts: i64,
        input_to_ts: i64,
    ) -> Result<(), String> {
        let now = self.now_ms();
        let written = self.with_conn_reported("jobs.upsert_fenced", |conn| {
            shogun_memory::jobs::upsert_fenced(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                job_kind_str(kind),
                job_state_str(state),
                input_from_ts,
                input_to_ts,
                now,
            )
            .map_err(|_| "dream fenced job ledger write failed".to_string())
        })?;
        if written {
            Ok(())
        } else {
            Err(crate::dreamcycle::outcome::LEASE_LOST.to_string())
        }
    }

    /// The persisted job runs for a cycle, as [`JobRun`]s (unrecognised rows are skipped).
    pub fn cycle_runs(&self, cycle_id: &str) -> Vec<JobRun> {
        self.try_cycle_runs(cycle_id).unwrap_or_default()
    }

    /// [`Self::cycle_runs`] with storage and invalid-row failures preserved for resume decisions.
    pub fn try_cycle_runs(&self, cycle_id: &str) -> Result<Vec<JobRun>, String> {
        let rows = self.with_conn_reported("jobs.list_by_cycle", |c| {
            shogun_memory::jobs::list_by_cycle(c, cycle_id)
                .map_err(|_| "dream job ledger read failed".to_string())
        })?;
        rows.into_iter()
            .map(|row| {
                let kind = parse_job_kind(&row.kind)
                    .ok_or_else(|| "dream job ledger contains an unknown kind".to_string())?;
                let state = parse_job_state(&row.state)
                    .ok_or_else(|| "dream job ledger contains an unknown state".to_string())?;
                Ok(JobRun {
                    kind,
                    state,
                    input_from_ts: row.input_from_ts,
                    input_to_ts: row.input_to_ts,
                })
            })
            .collect()
    }

    /// The jobs still to run for `cycle`, given what's persisted (FR-DC-04 resume). A killed cycle
    /// resumes here by skipping the jobs already `done`.
    pub fn resume(&self, cycle_id: &str, cycle: CycleKind) -> Vec<JobKind> {
        self.try_resume(cycle_id, cycle)
            .unwrap_or_else(|_| cycle.sequence().to_vec())
    }

    /// [`Self::resume`] with the ledger read failure preserved. Dream execution must not treat a
    /// failed read as a fresh cycle and repeat already-completed effects.
    pub fn try_resume(&self, cycle_id: &str, cycle: CycleKind) -> Result<Vec<JobKind>, String> {
        Ok(remaining(cycle, &self.try_cycle_runs(cycle_id)?))
    }

    /// Oldest pre-V32 unfinished Full cycle, recovered from the existing job ledger. Its earliest
    /// sequence job supplies the frozen range so an upgrade cannot recompute later jobs at `now`.
    pub fn try_oldest_legacy_unfinished_full_cycle(
        &self,
    ) -> Result<Option<(String, i64, i64)>, String> {
        // Legacy = a job-run ledger with no `dream_cycles` row. A ledger whose cycle row exists
        // (an abandoned night keeps its failed runs) is owned by that row, not re-frozen here:
        // freezing an existing id is a no-op, so re-freezing would block every later night.
        let cycles = self.with_conn_reported("jobs.legacy_unfinished", |conn| {
            let mut cycles = shogun_memory::jobs::recent_cycles(conn, 10_000)
                .map_err(|_| "legacy Dream cycle read failed".to_string())?;
            let mut legacy = Vec::with_capacity(cycles.len());
            for cycle in cycles.drain(..) {
                if shogun_memory::dream_cycles::get(conn, &cycle.cycle_id)
                    .map_err(|_| "legacy Dream cycle read failed".to_string())?
                    .is_none()
                {
                    legacy.push(cycle);
                }
            }
            Ok(legacy)
        })?;
        let mut unfinished = Vec::new();
        for cycle in cycles {
            let mut runs = Vec::with_capacity(cycle.rows.len());
            for row in cycle.rows {
                let kind = parse_job_kind(&row.kind)
                    .ok_or_else(|| "legacy Dream cycle has an unknown job kind".to_string())?;
                let state = parse_job_state(&row.state)
                    .ok_or_else(|| "legacy Dream cycle has an unknown job state".to_string())?;
                runs.push(JobRun {
                    kind,
                    state,
                    input_from_ts: row.input_from_ts,
                    input_to_ts: row.input_to_ts,
                });
            }
            let is_full = runs
                .iter()
                .any(|run| !DEGRADED_SEQUENCE.contains(&run.kind));
            if !is_full || crate::dreamcycle::plan::is_complete(CycleKind::Full, &runs) {
                continue;
            }
            let range = FULL_SEQUENCE
                .iter()
                .find_map(|kind| runs.iter().find(|run| run.kind == *kind))
                .map(|run| (run.input_from_ts, run.input_to_ts));
            if let Some((input_from_ts, input_to_ts)) = range {
                unfinished.push((cycle.started_at, cycle.cycle_id, input_from_ts, input_to_ts));
            }
        }
        Ok(unfinished
            .into_iter()
            .min_by_key(|(started_at, _, _, _)| *started_at)
            .map(|(_, cycle_id, input_from_ts, input_to_ts)| {
                (cycle_id, input_from_ts, input_to_ts)
            }))
    }

    /// Freeze a Full cycle's input range before any provider submission.
    pub fn try_freeze_full_cycle(
        &self,
        cycle_id: &str,
        input_from_ts: i64,
        input_to_ts: i64,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("dream_cycles.freeze", |conn| {
            shogun_memory::dream_cycles::freeze_full_cycle(
                conn,
                cycle_id,
                input_from_ts,
                input_to_ts,
                now,
            )
            .map_err(|_| "dream cycle freeze failed".to_string())
        })
    }

    /// Read and verify the exact input identities frozen with a Full cycle.
    pub fn try_dream_cycle_inputs_partitioned(
        &self,
        cycle_id: &str,
    ) -> Result<shogun_memory::dream_cycles::FrozenInputs, String> {
        self.with_conn_reported("dream_cycles.inputs", |conn| {
            shogun_memory::dream_cycles::frozen_inputs(conn, cycle_id)
                .map_err(|_| "dream frozen input verification failed".to_string())
        })
    }

    /// Acquire the durable exclusive lease required for every Full-cycle lifecycle transition.
    pub(crate) fn try_claim_dream_cycle_run(
        &self,
        cycle_id: &str,
        owner: String,
    ) -> Result<Option<DreamRunLease>, String> {
        let now = self.now_ms();
        let generation = self.with_conn_reported("dream_cycles.claim_run", |conn| {
            shogun_memory::dream_cycles::claim_run(
                conn,
                cycle_id,
                &owner,
                now,
                now.saturating_add(DREAM_RUN_LEASE_MS),
            )
            .map_err(|_| "dream cycle lease write failed".to_string())
        })?;
        Ok(generation.map(|generation| DreamRunLease { owner, generation }))
    }

    /// Validate and renew one exact lease generation before an external or durable effect.
    pub(crate) fn try_renew_dream_cycle_run(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        let renewed = self.with_conn_reported("dream_cycles.renew_run", |conn| {
            shogun_memory::dream_cycles::renew_run(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
                now.saturating_add(DREAM_RUN_LEASE_MS),
            )
            .map_err(|_| "dream cycle lease renewal failed".to_string())
        })?;
        if renewed {
            Ok(())
        } else {
            Err(crate::dreamcycle::outcome::LEASE_LOST.to_string())
        }
    }

    /// Fail and release only the exact owner generation; stale cleanup cannot release its successor.
    pub(crate) fn try_release_dream_cycle_run(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.release_run", |conn| {
            shogun_memory::dream_cycles::release_run(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream cycle lease release failed".to_string())
        })
    }

    /// Reserve provider submission under the already-held cycle lease.
    pub(crate) fn try_claim_dream_submission(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
        expected_custom_ids: &[String],
    ) -> Result<bool, String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.claim_submission", |conn| {
            shogun_memory::dream_cycles::claim_submission(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
                expected_custom_ids,
            )
            .map_err(|_| "dream provider submission lease failed".to_string())
        })
    }

    /// Read the verified frozen inputs in original event order for local Consolidation.
    pub fn try_dream_cycle_events(
        &self,
        cycle_id: &str,
    ) -> Result<Vec<shogun_memory::event_log::EventText>, String> {
        self.with_conn_reported("dream_cycles.events", |conn| {
            shogun_memory::dream_cycles::frozen_events(conn, cycle_id)
                .map_err(|_| "dream frozen input verification failed".to_string())
        })
    }

    #[cfg(test)]
    pub(crate) fn test_delete_dream_input_event(&self, event_id: i64) -> Result<(), String> {
        self.with_conn_mut_reported("test.dream_cycles.delete_input", |conn| {
            shogun_memory::search_documents::delete_canonical_event(conn, event_id)
                .map_err(|error| error.to_string())
        })
    }

    #[cfg(test)]
    pub(crate) fn test_mutate_dream_input_event(&self, event_id: i64) -> Result<usize, String> {
        self.with_conn_reported("test.dream_cycles.mutate_input", |conn| {
            conn.execute(
                "UPDATE event_log SET content = 'mutated', content_hash = 'mutated-hash'
                 WHERE id = ?1",
                [event_id],
            )
            .map_err(|error| error.to_string())
        })
    }

    #[cfg(test)]
    pub(crate) fn test_set_dream_cycle_stage(&self, cycle_id: &str, stage: &str) -> Result<usize, String> {
        self.with_conn_reported("test.dream_cycles.stage", |conn| {
            conn.execute(
                "UPDATE dream_cycles SET batch_stage = ?2,
                    provider_batch_id = CASE WHEN ?2 IN ('submitted', 'results_ready')
                                             THEN 'test-batch' END
                 WHERE cycle_id = ?1",
                rusqlite::params![cycle_id, stage],
            )
            .map_err(|error| error.to_string())
        })
    }

    #[cfg(test)]
    pub(crate) fn test_set_dream_batch_items_available(
        &self,
        available: bool,
    ) -> Result<(), String> {
        self.with_conn_reported("test.dream_cycles.batch_items", |conn| {
            let sql = if available {
                "CREATE TABLE dream_batch_items (
                    cycle_id TEXT NOT NULL REFERENCES dream_cycles(cycle_id) ON DELETE CASCADE,
                    custom_id TEXT NOT NULL,
                    PRIMARY KEY (cycle_id, custom_id)
                 ) STRICT"
            } else {
                "DROP TABLE dream_batch_items"
            };
            conn.execute_batch(sql).map_err(|error| error.to_string())
        })
    }

    /// Oldest unfinished Full cycle. A later scheduler date resumes this row before freezing a
    /// new range.
    pub fn try_oldest_unfinished_full_cycle(
        &self,
    ) -> Result<Option<shogun_memory::dream_cycles::DreamCycleRow>, String> {
        self.with_conn_reported("dream_cycles.oldest_unfinished", |conn| {
            shogun_memory::dream_cycles::oldest_unfinished(conn)
                .map_err(|_| "dream cycle resume read failed".to_string())
        })
    }

    /// Oldest unfinished hourly window (a nightly cycle never counts; see
    /// [`shogun_memory::dream_cycles::oldest_unfinished_hourly`]).
    pub fn try_oldest_unfinished_hourly_cycle(
        &self,
    ) -> Result<Option<shogun_memory::dream_cycles::DreamCycleRow>, String> {
        self.with_conn_reported("dream_cycles.oldest_unfinished_hourly", |conn| {
            shogun_memory::dream_cycles::oldest_unfinished_hourly(conn)
                .map_err(|_| "dream cycle resume read failed".to_string())
        })
    }

    /// Read one durable Full-cycle row after a lease transition.
    pub fn try_dream_cycle(
        &self,
        cycle_id: &str,
    ) -> Result<Option<shogun_memory::dream_cycles::DreamCycleRow>, String> {
        self.with_conn_reported("dream_cycles.get", |conn| {
            shogun_memory::dream_cycles::get(conn, cycle_id)
                .map_err(|_| "dream cycle read failed".to_string())
        })
    }

    /// Persist an accepted provider Batch handle and exact submitted ids atomically.
    pub(crate) fn try_mark_dream_batch_submitted(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
        provider_batch_id: &str,
        expected_custom_ids: &[String],
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("dream_cycles.mark_submitted", |conn| {
            shogun_memory::dream_cycles::mark_submitted(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                provider_batch_id,
                expected_custom_ids,
                now,
            )
            .map_err(|_| "dream provider handle write failed".to_string())
        })
    }

    /// Persist use of the existing local classification fallback for this frozen cycle.
    pub(crate) fn try_mark_dream_batch_local(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_local", |conn| {
            shogun_memory::dream_cycles::mark_local(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream local fallback write failed".to_string())
        })
    }

    /// Persist that provider results passed completeness validation.
    pub(crate) fn try_mark_dream_batch_results_ready(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_results_ready", |conn| {
            shogun_memory::dream_cycles::mark_results_ready(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream Batch stage write failed".to_string())
        })
    }

    /// Mark a frozen cycle failed; the bounded final failure abandons it and releases its snapshot.
    pub(crate) fn try_mark_dream_cycle_failed(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_failed", |conn| {
            shogun_memory::dream_cycles::mark_failed(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream cycle failure-state write failed".to_string())
        })
    }

    /// [`Self::try_mark_dream_cycle_failed`] for a window refused for its content: at the limit
    /// its events are not carried into the next freeze (see `dream_cycles::release_run_refused`).
    pub(crate) fn try_mark_dream_cycle_refused(&self, cycle_id: &str, lease: &DreamRunLease) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_refused", |conn| {
            match shogun_memory::dream_cycles::release_run_refused(conn, cycle_id, &lease.owner, lease.generation, now) {
                Ok(true) => Ok(()),
                _ => Err("dream cycle failure-state write failed".to_string()),
            }
        })
    }

    /// At process start: free leases a previous process left behind (a relaunch mid-run), so the
    /// unfinished cycle resumes now instead of after the lease expires. Fenced by generation, so a
    /// straggler from the old process still cannot write. Returns how many were freed.
    pub fn release_foreign_dream_leases(&self) -> usize {
        let mine = format!("{}-%", std::process::id());
        self.with_conn_reported("dream_cycles.release_foreign", |conn| {
            shogun_memory::dream_cycles::release_foreign_leases(conn, &mine)
                .map_err(|_| "dream lease release failed".to_string())
        })
        .unwrap_or(0)
    }

    /// Shrink an oversized unsubmitted cycle to `cap` Batch-eligible inputs (see
    /// [`shogun_memory::dream_cycles::split_oversized`]); `true` when it was split.
    pub(crate) fn try_split_dream_cycle(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
        cap: usize,
    ) -> Result<bool, String> {
        self.with_conn_reported("dream_cycles.split", |conn| {
            shogun_memory::dream_cycles::split_oversized(conn, cycle_id, &lease.owner, lease.generation, cap)
                .map_err(|_| "dream cycle split failed".to_string())
        })
    }

    /// Results lost at the provider: abandon now so the window is re-frozen into a fresh batch.
    pub(crate) fn try_mark_dream_cycle_abandoned(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_abandoned", |conn| {
            shogun_memory::dream_cycles::mark_abandoned(conn, cycle_id, &lease.owner, lease.generation, now)
                .map_err(|_| "dream cycle failure-state write failed".to_string())
        })
    }

    /// [`Self::try_mark_dream_cycle_failed`] for a shared-service outage: never abandons.
    pub(crate) fn try_mark_dream_cycle_failed_retryable(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_reported("dream_cycles.mark_failed", |conn| {
            shogun_memory::dream_cycles::mark_failed_retryable(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream cycle failure-state write failed".to_string())
        })
    }

    /// Mark a frozen cycle complete only after every required job effect reached `done`.
    pub(crate) fn try_mark_dream_cycle_complete(
        &self,
        cycle_id: &str,
        lease: &DreamRunLease,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("dream_cycles.mark_complete", |conn| {
            shogun_memory::dream_cycles::mark_complete(
                conn,
                cycle_id,
                &lease.owner,
                lease.generation,
                now,
            )
            .map_err(|_| "dream cycle completion write failed".to_string())
        })
    }

    /// True after MorningBrief's save and optional notice consumption committed for this cycle.
    pub fn try_dream_brief_effect_exists(&self, cycle_id: &str) -> Result<bool, String> {
        self.with_conn_reported("dream_cycles.brief_effect_exists", |conn| {
            shogun_memory::dream_cycles::brief_effect_exists(conn, cycle_id)
                .map_err(|_| "dream brief effect read failed".to_string())
        })
    }

    /// Atomically save the brief, consume its notice, and bind the effect to the frozen cycle.
    pub fn try_save_dream_brief_effect(
        &self,
        cycle_id: &str,
        date: &str,
        payload_json: &str,
        generated: bool,
        notice_id: Option<i64>,
    ) -> Result<(), String> {
        let now = self.now_ms();
        self.with_conn_mut_reported("dream_cycles.save_brief_effect", |conn| {
            shogun_memory::dream_cycles::save_brief_effect(
                conn,
                cycle_id,
                date,
                payload_json,
                generated,
                notice_id,
                now,
            )
            .map_err(|_| "dream brief effect write failed".to_string())
        })
    }

    /// The most recent `limit` nights, newest first, reconstructed from the job ledger (FR-DC-04).
    /// This is what survives a relaunch: no run reports itself, the ledger is the record.
    ///
    /// Nightly (and manual) cycles only: the night report, the health indicator and the sleep/wake
    /// logic are keyed by night, and 24 hourly cycles a day (#1066 B) must not crowd them out.
    /// Hourly derivation cycles are [`Self::recent_hourly_cycles`].
    pub fn recent_cycles(&self, limit: usize) -> Vec<crate::dreamcycle::run::CycleOutcome> {
        self.recent_cycles_in(limit, shogun_memory::jobs::CycleScope::Nightly)
    }

    /// The most recent `limit` hourly derivation cycles (`YYYYMMDD-hHH`), newest first.
    pub fn recent_hourly_cycles(&self, limit: usize) -> Vec<crate::dreamcycle::run::CycleOutcome> {
        self.recent_cycles_in(limit, shogun_memory::jobs::CycleScope::Hourly)
    }

    fn recent_cycles_in(
        &self,
        limit: usize,
        scope: shogun_memory::jobs::CycleScope,
    ) -> Vec<crate::dreamcycle::run::CycleOutcome> {
        let cycles = self
            .with_conn("jobs.recent_cycles", |c| {
                shogun_memory::jobs::recent_cycles_scoped(c, limit, scope)
            })
            .unwrap_or_default();

        cycles
            .into_iter()
            .map(|c| {
                let runs: Vec<JobRun> = c
                    .rows
                    .iter()
                    .filter_map(|r| {
                        Some(JobRun {
                            kind: parse_job_kind(&r.kind)?,
                            state: parse_job_state(&r.state)?,
                            input_from_ts: r.input_from_ts,
                            input_to_ts: r.input_to_ts,
                        })
                    })
                    .collect();
                let kind = if runs.iter().any(|r| !DEGRADED_SEQUENCE.contains(&r.kind)) {
                    CycleKind::Full
                } else {
                    CycleKind::Degraded
                };
                crate::dreamcycle::run::CycleOutcome {
                    cycle_id: c.cycle_id,
                    kind,
                    succeeded: crate::dreamcycle::plan::is_complete(kind, &runs),
                    jobs_done: runs.iter().filter(|r| r.state == JobState::Done).count(),
                    jobs_failed: runs.iter().filter(|r| r.state == JobState::Failed).count(),
                    // The window is the cycle's, not a job's — every job of a cycle carries it.
                    input_from_ts: runs.iter().map(|r| r.input_from_ts).min().unwrap_or(0),
                    input_to_ts: runs.iter().map(|r| r.input_to_ts).max().unwrap_or(0),
                    started_at: c.started_at,
                    ended_at: c.ended_at,
                }
            })
            .collect()
    }

    /// Whether the loop consolidator should run now: new review signals since its last run, an
    /// hour since that run, and under its daily cap (`shogun_memory::loops::consolidation_due`).
    /// `false` when the store is unavailable.
    pub fn loops_consolidation_due(&self, now_ms: i64) -> bool {
        const DAY_MS: i64 = 86_400_000;
        let off = i64::from(crate::daemon::os_gmt_offset_secs(now_ms)) * 1000;
        let day_start = (now_ms + off).div_euclid(DAY_MS) * DAY_MS - off;
        self.with_conn("loops.due", |c| shogun_memory::loops::consolidation_due(c, now_ms, day_start))
            .unwrap_or(false)
    }

    /// `(kind, text, event_id)` of every review signal, oldest first.
    #[cfg(test)]
    pub fn loop_signals(&self) -> Vec<(String, String, i64)> {
        self.with_conn("loops.signals", |c| {
            c.prepare("SELECT kind, text, event_id FROM loop_signals ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, rusqlite::Error>>()
        })
        .unwrap_or_default()
    }

    /// What the next consolidator call is shown, or `None` when nothing new has a usable source.
    pub fn loops_consolidation_input(&self, now_ms: i64) -> Option<shogun_memory::loops::ConsolidationInput> {
        self.with_conn("loops.input", |c| shogun_memory::loops::consolidation_input(c, now_ms))
            .ok()
            .flatten()
    }

    /// Apply one consolidator answer and mark the signals it read (`shogun_memory::loops::apply`).
    /// Counts only; `None` when the store refused the write.
    pub fn apply_loops_answer(
        &self,
        answer: &serde_json::Value,
        events: &[i64],
        read: &[i64],
        now_ms: i64,
    ) -> Option<shogun_memory::loops::Applied> {
        let applied = self
            .with_conn_mut("loops.apply", |c| shogun_memory::loops::apply(c, answer, events, read, now_ms))
            .ok()?;
        if applied.opened + applied.relisted + applied.resolved + applied.merged > 0 {
            super::maintenance::notify_state_changed();
        }
        Some(applied)
    }

    /// The known people and projects as the text the hourly review is shown, so the model can map a
    /// mention to an existing row instead of creating a twin. The entries the window mentions come
    /// first; empty when nothing is known yet or the store is unavailable.
    pub fn entity_roster(&self, window: &[shogun_memory::event_log::BatchEventText]) -> String {
        self.with_conn("dream.entity_roster", |conn| {
            shogun_memory::entity_dedupe::roster(conn, window.iter().map(|e| e.content.as_str()))
        })
        .unwrap_or_default()
    }

    /// The "which of these are the same?" input for each of people and projects that has enough
    /// rows to be worth asking about: `(label, one `id | name | aliases | about` line per row)`.
    pub fn entity_dedupe_inputs(&self) -> Vec<(&'static str, String)> {
        use shogun_memory::state::StateTable;
        let mut out = Vec::new();
        for (label, table) in [("people", StateTable::People), ("projects", StateTable::Projects)] {
            let input = self.with_conn("dream.entity_dedupe_input", |conn| {
                shogun_memory::entity_dedupe::dedupe_input(conn, table)
            });
            if let Ok(Some(text)) = input {
                out.push((label, text));
            }
        }
        out
    }

    /// Merge the groups a model named for `label` ("people" or "projects"). Returns the rows absorbed.
    pub fn apply_entity_groups(&self, label: &str, json: &str) -> usize {
        use shogun_memory::state::StateTable;
        let table = match label {
            "people" => StateTable::People,
            "projects" => StateTable::Projects,
            _ => return 0,
        };
        let now = self.now_ms();
        let merged = self
            .with_conn_mut("dream.entity_dedupe_apply", |conn| {
                shogun_memory::entity_dedupe::apply_groups(conn, table, json, now)
            })
            .unwrap_or(0);
        if merged > 0 {
            super::maintenance::notify_state_changed();
        }
        merged
    }

    /// Changes whenever the people or projects tables do, so the dedupe call is only made again when
    /// there is something new to look at.
    pub fn entity_fingerprint(&self) -> u64 {
        self.with_conn("dream.entity_fingerprint", shogun_memory::entity_dedupe::fingerprint).unwrap_or(0)
    }

    /// The names the user goes by (their own `self` person row: name and aliases), so a page
    /// rewrite can tell the user's messages from other people's. Empty when unknown.
    pub fn owner_names(&self) -> Vec<String> {
        self.with_conn("dream.owner_names", |conn| {
            let mut stmt = conn.prepare("SELECT display_name, COALESCE(aliases, '[]') FROM people WHERE person_class = 'self'")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut names = Vec::new();
            for row in rows {
                let (name, aliases) = row?;
                names.push(name);
                names.extend(serde_json::from_str::<Vec<String>>(&aliases).unwrap_or_default());
            }
            names.dedup();
            Ok::<_, rusqlite::Error>(names)
        })
        .unwrap_or_default()
    }

    /// Record the names the hourly review said a window shows are the user's: `(window, name)`
    /// pairs ([`shogun_memory::self_identity`], kind `review`). Best-effort.
    pub fn note_self_names(&self, found: &[(String, String)]) {
        use shogun_memory::self_identity::{note, KIND_REVIEW};
        let now = self.now_ms();
        let _ = self.with_conn("dream.self_names", |conn| {
            for (window, name) in found {
                note(conn, name, KIND_REVIEW, &format!("review:{window}"), now)?;
            }
            Ok::<_, rusqlite::Error>(())
        });
    }

    /// A message the user just sent echoed into `visible` (the window text): the author header
    /// at or above it is the user's name in that app (kind `echo`). Best-effort.
    pub fn note_self_from_echo(&self, sent: &str, visible: &str) {
        use shogun_memory::self_identity::{echo_author, note, KIND_ECHO};
        let Some(name) = echo_author(sent, visible) else { return };
        let now = self.now_ms();
        let _ = self.with_conn("rewrite_loop.self_name", |conn| note(conn, &name, KIND_ECHO, &format!("echo:{now}"), now));
    }

    /// The person / project pages due a rewrite now, at most `limit`, newest mention first
    /// ([`shogun_memory::entity_pages::dirty_pages`]). Empty when none or the store is unavailable.
    pub fn entity_page_tasks(&self, limit: usize) -> Vec<shogun_memory::entity_pages::PageTask> {
        use shogun_memory::entity_pages::{dirty_pages, page_task, pages_per_run, BACKLOG_DIRTY};
        let now = self.now_ms();
        self.with_conn("dream.entity_page_tasks", |conn| {
            let mut out = Vec::new();
            let dirty = dirty_pages(conn, now, BACKLOG_DIRTY + 1)?;
            let take = pages_per_run(dirty.len()).min(limit);
            for (table, id) in dirty.into_iter().take(take) {
                out.extend(page_task(conn, table, id)?);
            }
            Ok::<_, rusqlite::Error>(out)
        })
        .unwrap_or_default()
    }

    /// Leave a page with no sendable evidence clean, without a call
    /// ([`shogun_memory::entity_pages::page_skipped`]).
    pub fn skip_entity_page(&self, task: &shogun_memory::entity_pages::PageTask) {
        let _ = self.with_conn("dream.entity_page_skip", |conn| shogun_memory::entity_pages::page_skipped(conn, task));
    }

    /// Store a rewritten page (or count the failure when `answer` is `None` or unusable). Returns
    /// whether a page was written.
    pub fn apply_entity_page(&self, task: &shogun_memory::entity_pages::PageTask, answer: Option<&str>) -> bool {
        let now = self.now_ms();
        let written = self
            .with_conn("dream.entity_page_apply", |conn| match answer {
                Some(json) => shogun_memory::entity_pages::apply_page(conn, task, json, now),
                None => shogun_memory::entity_pages::page_failed(conn, task.table, task.id, now).map(|()| false),
            })
            .unwrap_or(false);
        if written {
            super::maintenance::notify_state_changed();
        }
        written
    }

    /// Backfill (owner decision 2026-10-14): walk back from the hour before the last closed one
    /// (that one is the live hourly window's, submitted at this device's slot), newest first, to
    /// the hours with reviewable events that no completed cycle covers. Returns the newest such
    /// slice `(cycle_id, from_ms, to_ms)` and how many remain (bounded to 14 days).
    pub fn backfill_scan(&self, now_ms: i64, off_secs: i32) -> Option<((String, i64, i64), usize)> {
        const HOUR: i64 = 3_600_000;
        let end = crate::dreamcycle::hourly::window_end_ms(now_ms, off_secs);
        let mut first = None;
        let mut left = 0usize;
        for k in 2..=(14 * 24) {
            let (from, to) = (end - k * HOUR, end - (k - 1) * HOUR);
            let covered = self
                .with_conn("dream.backfill_covered", |conn| {
                    conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM dream_cycles WHERE input_from_ts <= ?1 AND input_to_ts >= ?2
                           AND status = 'done')",
                        rusqlite::params![from, to],
                        |r| r.get::<_, bool>(0),
                    )
                })
                .unwrap_or(true);
            if covered || !self.has_batch_eligible_events(from, to) {
                continue;
            }
            let id = crate::dreamcycle::hourly::hour_cycle_id(from.div_euclid(1000), off_secs);
            // Finished or abandoned: not retried. A failed / running slice is still owed.
            if self.try_dream_cycle(&id).ok().flatten().is_some_and(|c| {
                matches!(c.status, shogun_memory::dream_cycles::CycleStatus::Done | shogun_memory::dream_cycles::CycleStatus::Abandoned)
            }) {
                continue;
            }
            left += 1;
            if first.is_none() {
                first = Some((id, from, to));
            }
        }
        first.map(|f| (f, left))
    }

    /// Record one night's Batch send telemetry for `job` (#286 / OPEN-B2). Counts only — the
    /// row has no column for captured text. Best-effort: a telemetry write failure never fails
    /// the cycle, so this returns whether the row landed instead of an error.
    ///
    /// The first time a night's row lands, the same counts also go out as one anonymous
    /// `dream_batch_sent` event (owner decision 2026-09-25, docs/batch-relay-design.md OPEN-B2):
    /// bucketed item counts, the token reduction to the nearest 5%, and the job name, through the
    /// opt-out analytics handle ([`Self::set_analytics`]). A rewrite of the same night's row sends
    /// nothing more. Emitting never blocks and cannot fail the cycle.
    pub fn record_batch_send_stats(
        &self,
        cycle_id: &str,
        job: &str,
        telemetry: &crate::dreamcycle::jobs::BatchSendTelemetry,
    ) -> bool {
        remember_session_counters(cycle_id, job, telemetry.session_counters());
        let row = send_stats_row(cycle_id, job, self.now_ms(), telemetry);
        let landed = self.with_conn("dream_batch_send_stats.upsert", |conn| {
            let first = shogun_memory::batch_send_stats::get(conn, cycle_id, job)?.is_none();
            shogun_memory::batch_send_stats::upsert(conn, &row)?;
            Ok::<bool, rusqlite::Error>(first)
        });
        match landed {
            Ok(first) => {
                if first {
                    self.emit_batch_sent(job, telemetry);
                }
                true
            }
            Err(_) => false,
        }
    }

    /// Record the night's final session-lane counters once the Batch results were parsed (or the
    /// night sent nothing): the telemetry with [`AnchorStats`](crate::dreamcycle::jobs::AnchorStats)
    /// filled in. Updates what [`Self::dream_status`] exposes next to `last_send` and writes the
    /// night's **one** counts-only log line. It does not rewrite the V54 row (`recorded_at` is the
    /// send time, and the row has no column for these counters — a `V<max+1>` adding them is the
    /// follow-up). Never fails the cycle.
    pub fn record_batch_night_counters(
        &self,
        cycle_id: &str,
        job: &str,
        telemetry: &crate::dreamcycle::jobs::BatchSendTelemetry,
    ) {
        let c = telemetry.session_counters();
        remember_session_counters(cycle_id, job, c);
        // Counts only: no cycle id (a date), no title, no text. `job` is a fixed lane name.
        let job = if crate::analytics::dream_batch::JOBS.contains(&job) { job } else { "other" };
        eprintln!(
            "[dream] batch night {job}: sessions={} rows_in_sessions={} rows_dropped_shadowed={} \
             rows_dropped_covered={} lines_in={} lines_deduped={} lines_capped={} \
             anchors_resolved={} anchors_unresolved={} anchors_line_mismatch={}",
            c.sessions,
            c.rows_in_sessions,
            c.rows_dropped_shadowed,
            c.rows_dropped_covered,
            c.lines_in,
            c.lines_deduped,
            c.lines_capped,
            c.anchors_resolved,
            c.anchors_unresolved,
            c.anchors_line_mismatch,
        );
    }

    /// The runtime source policy of tonight's consolidation sessions: with
    /// mail triage on, `gmail` rows are left to Stage B, which reads every human-correspondence
    /// thread and extracts its commitments and open loops into the same state tables
    /// (`crate::mail_batch`); with triage off nothing else reads them, so they stay in the lane.
    /// The mapping lives in [`SessionPolicy::for_mail_triage`]; this is the one place that reads
    /// the switch for it. The switch reads open when the settings row is unreadable
    /// (`mail_triage_enabled`), so an unreadable row means "Stage B covers mail", which is what
    /// the ingestion path is doing on the same reading.
    ///
    /// [`SessionPolicy::for_mail_triage`]: crate::dreamcycle::jobs::SessionPolicy::for_mail_triage
    pub fn consolidation_session_policy(&self) -> crate::dreamcycle::jobs::SessionPolicy {
        crate::dreamcycle::jobs::SessionPolicy::for_mail_triage(self.mail_triage_enabled())
    }

    /// Persist model-classified candidates, keyed by the event their provenance names: a
    /// candidate that promotes a low-confidence row already linked to that event does so; the
    /// rest are written fresh with `state_provenance(event_id)` through the source-aware,
    /// idempotent writer (`extract::persist_candidates_deduped`, which also refuses a quote the
    /// event's content does not hold). The one write path for the consolidation lane's
    /// `Consolidation` job and for mail triage Stage B's commitments / open loops, so a
    /// promise found in a Gmail thread is stored under exactly the rules a screen capture's is.
    /// Returns how many candidates landed (promoted an existing row or were written fresh); a
    /// storage failure stops at the first error (the caller decides whether the night is lost).
    pub fn try_persist_classified(
        &self,
        window: &str,
        classified: Vec<(i64, Vec<shogun_memory::extract::Candidate>)>,
    ) -> Result<Landed, String> {
        let mut landed = Landed::default();
        let mut signals = Vec::new();
        for (event_id, cands) in classified {
            let mut fresh = Vec::new();
            for candidate in cands {
                // A reviewed commitment / open loop is a signal for the loop consolidator, never a
                // state row of its own (owner decision 2026-10-14). Local-rule candidates (at or
                // below `LOCAL_RULE_MAX_CONFIDENCE`) and entities keep their paths.
                if let Some(signal) = review_signal(event_id, &candidate) {
                    signals.extend(signal);
                    continue;
                }
                if self.try_promote_candidate_for_event(event_id, &candidate)? {
                    landed.promoted += 1;
                    continue;
                }
                fresh.push(candidate);
            }
            if !fresh.is_empty() {
                landed.written += self.try_persist_candidates(event_id, &fresh)?.len();
            }
        }
        if !signals.is_empty() {
            let now = self.now_ms();
            landed.signals = self.with_conn_reported("loops.record_signals", |c| {
                shogun_memory::loops::record_signals(c, window, &signals, now)
                    .map_err(|_| "review signal write failed".to_string())
            })?;
        }
        Ok(landed)
    }

    /// The session-lane counters remembered for `(cycle_id, job)`, if this process recorded them.
    pub fn session_counters_for(
        &self,
        cycle_id: &str,
        job: &str,
    ) -> Option<crate::dreamcycle::jobs::SessionCounters> {
        let slots = LAST_SESSION_COUNTERS.lock().ok()?;
        slots
            .iter()
            .find(|(c, j, _)| c == cycle_id && j == job)
            .map(|(_, _, counters)| *counters)
    }

    /// The anonymous `dream_batch_sent` event for one night (#286). No-op without an analytics
    /// handle or for a job outside `analytics::dream_batch::JOBS`; the handle drops it when the
    /// user opted out.
    fn emit_batch_sent(&self, job: &str, telemetry: &crate::dreamcycle::jobs::BatchSendTelemetry) {
        let Some(handle) = self.analytics.get() else {
            return;
        };
        let p = &telemetry.prefilter;
        let to_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        if let Some(props) = crate::analytics::dream_batch::event_props(
            job,
            to_u64(p.input),
            to_u64(p.kept),
            telemetry.est_tokens_before,
            telemetry.est_tokens_after,
        ) {
            handle.capture(crate::analytics::dream_batch::EVENT, props);
        }
    }

    /// The most recent `limit` nights of Batch send telemetry, newest first (read-only). This is
    /// the series OPEN-B2 is closed from; see docs/batch-relay-design.md §OPEN-B2.
    pub fn batch_send_stats(&self, limit: usize) -> Vec<shogun_memory::batch_send_stats::SendStatsRow> {
        self.with_conn("dream_batch_send_stats.recent", |conn| {
            shogun_memory::batch_send_stats::recent(conn, limit)
        })
        .unwrap_or_default()
    }

    /// The Dream Cycle status for the Full UI (FR-DC-06) and the gate: the last night's outcome, the
    /// failure indicator (FR-DC-05), and whether tonight's full cycle is already done.
    ///
    /// `nights` bounds how far back the indicator looks — it only ever counts the unbroken run of
    /// failures at the front, so a short window is enough and keeps the query cheap.
    pub fn dream_status(
        &self,
        tonight_cycle_id: &str,
        nights: usize,
    ) -> crate::dreamcycle::run::DreamStatus {
        let now_ms = self.now_ms();
        self.dream_status_at(
            tonight_cycle_id,
            nights,
            now_ms,
            crate::daemon::os_gmt_offset_secs(now_ms),
        )
    }

    /// [`Self::dream_status`] at an explicit instant and zone offset (tests, and any caller that
    /// already read the wall clock).
    pub fn dream_status_at(
        &self,
        tonight_cycle_id: &str,
        nights: usize,
        now_ms: i64,
        gmt_offset_secs: i32,
    ) -> crate::dreamcycle::run::DreamStatus {
        let recent = self.recent_cycles(nights.max(1));
        // Only full cycles carry Batch work, so only they can fail the Batch lane. A degraded
        // catch-up night is not evidence either way and must not reset an amber indicator.
        let outcomes: Vec<bool> = recent
            .iter()
            .filter(|c| c.kind == CycleKind::Full)
            .map(|c| c.succeeded)
            .collect();
        let full_run_done_today = recent
            .iter()
            .any(|c| c.cycle_id == tonight_cycle_id && c.kind == CycleKind::Full && c.succeeded);

        // The newest completed Full night the job ledger knows (pre-V32 cycles have no lifecycle
        // row), for the staleness rule.
        let latest_legacy_result = recent
            .iter()
            .filter(|c| c.kind == CycleKind::Full && c.succeeded)
            .filter_map(|c| {
                crate::dreamcycle::outcome::days_from_night(&c.cycle_id)
                    .map(|days| (days, c.cycle_id.clone()))
            })
            .max()
            .map(|(_, id)| id);
        let last = recent.into_iter().next();
        // The hourly derivation lane (#1066 B): when it last reviewed, and whether that worked.
        let hourly = self.recent_hourly_cycles(24 * nights.max(1));
        let hourly_days = crate::dreamcycle::health::day_outcomes(
            // A review still in flight has no failed job yet: it is not a failure.
            hourly.iter().map(|c| (c.cycle_id.as_str(), c.succeeded || c.jobs_failed == 0)),
        );
        let last_hourly = hourly.into_iter().next();
        let (events_processed, state_changes, chunks_sent) = match &last {
            Some(c) => self
                .read_conn("dream.summary_counts", |conn| {
                    (
                        event_log::count_in_range(conn, c.input_from_ts, c.input_to_ts)
                            .unwrap_or(0),
                        state::count_changed_since(conn, c.started_at).unwrap_or(0),
                        shogun_memory::traceability::count_since(conn, c.started_at).unwrap_or(0),
                    )
                })
                .unwrap_or((0, 0, 0)),
            None => (0, 0, 0),
        };

        let (night, action_facts) =
            self.dream_night_report_at(now_ms, gmt_offset_secs, latest_legacy_result);
        let last_send = self.batch_send_stats(1).into_iter().next();
        let last_send_sessions = last_send
            .as_ref()
            .and_then(|row| self.session_counters_for(&row.cycle_id, &row.job));

        crate::dreamcycle::run::DreamStatus {
            last,
            last_hourly,
            last_send,
            last_send_sessions,
            indicator: crate::dreamcycle::health::indicator(
                crate::dreamcycle::health::consecutive_failures(&outcomes).max(
                    crate::dreamcycle::health::consecutive_failures(&hourly_days),
                ),
            ),
            events_processed,
            state_changes,
            chunks_sent,
            full_run_done_today,
            night,
            action_facts,
        }
    }

    /// The morning report for the night a result is expected for (issue #285), from the lifecycle
    /// table, the job ledger and the persisted outcome record, plus the ledger facts its actions
    /// are decided from. `ledger_result` is the newest completed Full cycle the job ledger knows
    /// of (it also covers pre-V32 cycles).
    fn dream_night_report_at(
        &self,
        now_ms: i64,
        gmt_offset_secs: i32,
        ledger_result: Option<String>,
    ) -> (
        crate::dreamcycle::outcome::NightReport,
        crate::dreamcycle::actions::LedgerFacts,
    ) {
        use crate::dreamcycle::actions::{self, PendingFull};
        use crate::dreamcycle::outcome::{self, DueCycle};
        use crate::dreamcycle::schedule::{DEFAULT_WINDOW_END_HOUR, DEFAULT_WINDOW_START_HOUR};
        use shogun_memory::dream_cycles::{BatchStage, CycleStatus};

        let window = outcome::due_night(
            now_ms,
            gmt_offset_secs,
            DEFAULT_WINDOW_START_HOUR,
            DEFAULT_WINDOW_END_HOUR,
        );
        let facts = self
            .read_conn("dream.night_report", |conn| {
                let due = shogun_memory::dream_cycles::get(conn, &window.night)
                    .ok()
                    .flatten()
                    .map(|row| match row.status {
                        CycleStatus::Done => DueCycle::Completed {
                            events: event_log::count_in_range(
                                conn,
                                row.input_from_ts,
                                row.input_to_ts,
                            )
                            .unwrap_or(0),
                        },
                        CycleStatus::Running => outcome::liveness(
                            row.run_owner.is_some(),
                            row.run_lease_until,
                            row.updated_at,
                            now_ms,
                        ),
                        CycleStatus::Failed | CycleStatus::Abandoned => DueCycle::Failed,
                    });
                let unfinished = shogun_memory::dream_cycles::oldest_unfinished(conn)
                    .ok()
                    .flatten();
                let live_run = unfinished.as_ref().is_some_and(|row| {
                    row.status == CycleStatus::Running
                        && outcome::liveness(
                            row.run_owner.is_some(),
                            row.run_lease_until,
                            row.updated_at,
                            now_ms,
                        ) == DueCycle::Running
                });
                // A job accepted by the provider is collected, never resubmitted; anything else
                // unfinished resumes from the ledger.
                let pending_full = match unfinished.map(|row| row.batch_stage) {
                    None => PendingFull::None,
                    Some(BatchStage::Submitted) => PendingFull::AwaitingResults,
                    Some(_) => PendingFull::Resumable,
                };
                let latest_done = shogun_memory::dream_cycles::latest_done(conn)
                    .ok()
                    .flatten();
                let captured_before =
                    event_log::any_before(conn, window.start_ms).unwrap_or(false);
                (due, live_run, pending_full, latest_done, captured_before)
            })
            .unwrap_or((None, false, PendingFull::None, None, false));
        let (due, live_run, pending_full, latest_done, captured_before) = facts;
        // The newer of the two by date; an id that is not a date is never "the latest result".
        let latest_result = [latest_done, ledger_result]
            .into_iter()
            .flatten()
            .filter_map(|id| outcome::days_from_night(&id).map(|days| (days, id)))
            .max()
            .map(|(_, id)| id);
        let record = self.dream_outcome_record();
        let report = outcome::night_report(&outcome::NightInputs {
            window: &window,
            now_ms,
            due,
            live_run,
            had_a_chance: latest_result.is_some() || due.is_some() || captured_before,
            latest_result: latest_result.as_deref(),
            record: record.as_ref(),
        });
        let ledger = actions::LedgerFacts {
            pending_full,
            window: actions::window_phase(
                now_ms,
                &window,
                crate::dreamcycle::sleep_wake::WAKE_CATCH_UP_MS,
            ),
        };
        (report, ledger)
    }

    /// The `dream.status` answer for MCP / REST / CLI (invariant 6): the same status the Settings
    /// panel shows, including the morning reason and the staleness flag (issue #285). Its
    /// `actions` know only the ledger; see [`Self::dream_status_json_with`].
    pub fn dream_status_json(&self) -> serde_json::Value {
        self.dream_status_json_with(&crate::dreamcycle::actions::ExternalFacts::unknown())
    }

    /// [`Self::dream_status_json`] with what the serving process knows beyond the ledger (plan,
    /// Batch credential, cap, battery), so `actions` can show a limit before the press.
    pub fn dream_status_json_with(
        &self,
        external: &crate::dreamcycle::actions::ExternalFacts,
    ) -> serde_json::Value {
        use crate::dreamcycle::schedule::{
            cycle_id, DEFAULT_WINDOW_END_HOUR, DEFAULT_WINDOW_START_HOUR,
        };
        let now_ms = self.now_ms();
        let offset = crate::daemon::os_gmt_offset_secs(now_ms);
        let tonight = cycle_id(
            now_ms.div_euclid(1000),
            offset,
            DEFAULT_WINDOW_START_HOUR,
            DEFAULT_WINDOW_END_HOUR,
        );
        dream_status_to_json(
            &self.dream_status_at(&tonight, DREAM_STATUS_NIGHTS, now_ms, offset),
            external,
        )
    }

    // the morning reason (issue #285)

    /// The persisted last outcome, if a path is configured and the file is readable.
    pub fn dream_outcome_record(&self) -> Option<crate::dreamcycle::outcome::OutcomeRecord> {
        crate::dreamcycle::outcome::load_at(self.dream_outcome_path.as_deref()?)
    }

    /// Persist one outcome. Best effort: a failure to write the explanation must never become a
    /// failure of the cycle itself, and the ledger still says whether the night completed.
    pub(crate) fn note_dream_outcome(
        &self,
        cycle_id: &str,
        kind: crate::dreamcycle::outcome::RecordKind,
        reason: Option<crate::dreamcycle::outcome::DreamReason>,
    ) {
        use crate::dreamcycle::outcome;
        use crate::dreamcycle::schedule::{DEFAULT_WINDOW_END_HOUR, DEFAULT_WINDOW_START_HOUR};
        let Some(path) = self.dream_outcome_path.as_deref() else {
            return;
        };
        let now_ms = self.now_ms();
        let next = outcome::OutcomeRecord::new(cycle_id, kind, reason, now_ms);
        let window = outcome::due_night(
            now_ms,
            crate::daemon::os_gmt_offset_secs(now_ms),
            DEFAULT_WINDOW_START_HOUR,
            DEFAULT_WINDOW_END_HOUR,
        );
        let current = outcome::load_at(path);
        if outcome::should_replace(current.as_ref(), &next, window.start_ms)
            && outcome::save_at(path, &next).is_err()
        {
            crate::elog!("[dream] outcome record write failed");
        }
    }

    /// Record why tonight's cycle failed before or outside the cycle itself — e.g. the shell's
    /// pre-run backup (issue #285).
    pub fn note_dream_failure(
        &self,
        cycle_id: &str,
        reason: crate::dreamcycle::outcome::DreamReason,
    ) {
        self.note_dream_outcome(
            cycle_id,
            crate::dreamcycle::outcome::RecordKind::Failed,
            Some(reason),
        );
    }

    /// Record that the gate declined inside the window because the Mac was busy or on low battery,
    /// so the morning says so instead of "asleep" (issue #285). Other decisions record nothing.
    pub fn note_dream_skip(&self, cycle_id: &str, decision: crate::dreamcycle::gate::RunDecision) {
        if let Some(reason) = crate::dreamcycle::outcome::skip_reason(decision) {
            self.note_dream_outcome(
                cycle_id,
                crate::dreamcycle::outcome::RecordKind::Skipped,
                Some(reason),
            );
        }
    }

    /// The store's monotonic fault count — the Dream Cycle compares it across an attempt to tell a
    /// storage failure from a network one.
    pub(crate) fn dream_fault_mark(&self) -> u64 {
        self.health.snapshot().faults_total
    }

    /// Assemble the Dream Cycle run summary (FR-DC-06) from DB deltas. Everything a run changed —
    /// state rows and traceability chunks — carries a timestamp at or after `run_started_ms`, so a
    /// single "since run start" count captures the run's real effect without a before/after
    /// snapshot. `events_processed` is the size of the input window the cycle consolidated. The
    /// summary is what the Full UI renders. Zeroed counts on a lock failure (never panics).
    pub fn summarize_dream_run(
        &self,
        cycle: CycleKind,
        report: &crate::dreamcycle::run::CycleReport,
        input_from_ts: i64,
        input_to_ts: i64,
        run_started_ms: i64,
        run_ended_ms: i64,
    ) -> crate::dreamcycle::run::DreamRunSummary {
        let (events_processed, state_changes, chunks_sent) = self
            .read_conn("dream.run_summary_counts", |c| {
                (
                    event_log::count_in_range(c, input_from_ts, input_to_ts).unwrap_or(0),
                    state::count_changed_since(c, run_started_ms).unwrap_or(0),
                    shogun_memory::traceability::count_since(c, run_started_ms).unwrap_or(0),
                )
            })
            .unwrap_or((0, 0, 0));
        crate::dreamcycle::run::DreamRunSummary {
            cycle,
            jobs_completed: report.completed.len(),
            events_processed,
            state_changes,
            chunks_sent,
            duration_ms: run_ended_ms.saturating_sub(run_started_ms),
            completed_fully: report.is_complete(),
        }
    }
}

/// Nights the `dream.status` indicator looks back over — the desktop's value (it only counts the
/// unbroken run of failures at the front, so a week reaches the red threshold of three).
const DREAM_STATUS_NIGHTS: usize = 7;

/// The wire shape of `dream.status`. Counts, dates and codes only — never captured text.
fn dream_status_to_json(
    status: &crate::dreamcycle::run::DreamStatus,
    external: &crate::dreamcycle::actions::ExternalFacts,
) -> serde_json::Value {
    use crate::dreamcycle::health::Indicator;
    let actions =
        crate::dreamcycle::actions::available(&status.night, &status.action_facts, external);
    serde_json::json!({
        "indicator": match status.indicator {
            Indicator::Normal => "normal",
            Indicator::Amber => "amber",
            Indicator::Red => "red",
        },
        "last": status.last.as_ref().map(|c| serde_json::json!({
            "cycle_id": c.cycle_id,
            "kind": if c.kind == CycleKind::Full { "full" } else { "degraded" },
            "succeeded": c.succeeded,
            "jobs_done": c.jobs_done,
            "jobs_failed": c.jobs_failed,
            "started_at": c.started_at,
            "ended_at": c.ended_at,
            "duration_ms": c.duration_ms(),
        })),
        // #1066 B: the last hourly derivation review. Same shape as `last`; null until the hourly
        // lane has run. Faces that showed "last Dream run" show this one when it is newer.
        "last_hourly": status.last_hourly.as_ref().map(|c| serde_json::json!({
            "cycle_id": c.cycle_id,
            "succeeded": c.succeeded,
            "jobs_done": c.jobs_done,
            "jobs_failed": c.jobs_failed,
            "started_at": c.started_at,
            "ended_at": c.ended_at,
            "duration_ms": c.duration_ms(),
        })),
        "events_processed": status.events_processed,
        "state_changes": status.state_changes,
        "chunks_sent": status.chunks_sent,
        "done_tonight": status.full_run_done_today,
        "night": status.night,
        // #285: what a face may offer for the night, each with `enabled` and, when off, a
        // `disabled_reason` code — known before the press. Codes only; faces map them to copy.
        "actions": actions.list(),
        // #286: the latest night's Batch send telemetry. Counts and fixed rule names only.
        "last_send": status.last_send.as_ref().map(|s| serde_json::json!({
            "cycle_id": s.cycle_id,
            "job": s.job,
            "recorded_at": s.recorded_at,
            "items_considered": s.items_considered,
            "items_sent": s.items_sent,
            "items_dropped_empty": s.items_dropped_empty,
            "items_dropped_duplicate": s.items_dropped_duplicate,
            "items_dropped_near_duplicate": s.items_dropped_near_duplicate,
            "items_dropped_chrome_only": s.items_dropped_chrome_only,
            "lines_considered": s.lines_considered,
            "lines_dropped": s.lines_dropped,
            "chars_before": s.chars_before,
            "chars_after": s.chars_after,
            "est_tokens_before": s.est_tokens_before,
            "est_tokens_after": s.est_tokens_after,
            "reduction_rate": s.reduction_rate(),
            // The session-lane counters of the same night (sessions, rows
            // shadowed, lines deduped / capped, anchors resolved / unresolved / line mismatch).
            // `null` when this process did not record that night (no V54 column yet; a
            // `V<max+1>` is the follow-up). Readers take every field as optional.
            "session_lane": status.last_send_sessions,
        })),
    })
}

fn to_i64(n: impl TryInto<i64>) -> i64 {
    n.try_into().unwrap_or(i64::MAX)
}

/// The latest nights' session-lane counters, keyed by `(cycle_id, job)` — process memory, not the
/// store. `dream_batch_send_stats` (V54) has no column for them and migrations are append-only,
/// so a `V<max+1>` adding the columns is the follow-up; until then `dream_status` shows them for
/// the nights this process recorded and nothing after a restart. A few entries, newest last: the
/// status reads the latest row only, and the key check keeps one night's counters off another's.
static LAST_SESSION_COUNTERS: std::sync::Mutex<
    Vec<(String, String, crate::dreamcycle::jobs::SessionCounters)>,
> = std::sync::Mutex::new(Vec::new());

/// How many nights [`LAST_SESSION_COUNTERS`] keeps.
const SESSION_COUNTERS_KEPT: usize = 8;

fn remember_session_counters(cycle_id: &str, job: &str, counters: crate::dreamcycle::jobs::SessionCounters) {
    if let Ok(mut slots) = LAST_SESSION_COUNTERS.lock() {
        slots.retain(|(c, j, _)| !(c == cycle_id && j == job));
        slots.push((cycle_id.to_string(), job.to_string(), counters));
        if slots.len() > SESSION_COUNTERS_KEPT {
            let excess = slots.len() - SESSION_COUNTERS_KEPT;
            slots.drain(..excess);
        }
    }
}

/// The storage row for one job's telemetry. Every field is a count or a fixed rule name.
fn send_stats_row(
    cycle_id: &str,
    job: &str,
    recorded_at: i64,
    t: &crate::dreamcycle::jobs::BatchSendTelemetry,
) -> shogun_memory::batch_send_stats::SendStatsRow {
    let p = &t.prefilter;
    shogun_memory::batch_send_stats::SendStatsRow {
        cycle_id: cycle_id.to_string(),
        job: job.to_string(),
        recorded_at,
        items_considered: to_i64(p.input),
        items_sent: to_i64(p.kept),
        items_dropped_empty: to_i64(p.dropped_empty),
        items_dropped_duplicate: to_i64(p.dropped_duplicate),
        items_dropped_near_duplicate: to_i64(p.dropped_near_duplicate),
        items_dropped_chrome_only: to_i64(p.dropped_chrome_only),
        lines_considered: to_i64(p.lines_considered),
        lines_dropped: to_i64(p.line_drops.total()),
        line_drops_by_rule: p
            .line_drops
            .by_rule()
            .iter()
            .map(|(rule, n)| ((*rule).to_string(), to_i64(*n)))
            .collect(),
        chars_before: to_i64(p.chars_before),
        chars_after: to_i64(p.chars_after),
        est_tokens_before: to_i64(t.est_tokens_before),
        est_tokens_after: to_i64(t.est_tokens_after),
    }
}

/// Map a [`JobKind`] to its stored string.
fn job_kind_str(kind: JobKind) -> &'static str {
    dream_ledger::job_kind_str(kind)
}

fn parse_job_kind(s: &str) -> Option<JobKind> {
    dream_ledger::parse_job_kind(s)
}

fn job_state_str(state: JobState) -> &'static str {
    dream_ledger::job_state_str(state)
}

fn parse_job_state(s: &str) -> Option<JobState> {
    dream_ledger::parse_job_state(s)
}

#[cfg(test)]
mod batch_sent_event_tests {
    //! #286 / OPEN-B2: the nightly `dream_batch_sent` event rides the opt-out analytics handle,
    //! once per night, counts only.
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::analytics::{AnalyticsConfig, AnalyticsHandle, Props, Transport};
    use crate::daemon::Db;
    use crate::dreamcycle::jobs::BatchSendTelemetry;
    use crate::dreamcycle::prefilter::BatchPrefilterStats;

    #[derive(Clone, Default)]
    struct Recording {
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Recording {
        fn send_batch(&self, body: String) -> Result<(), ()> {
            self.sent.lock().unwrap().push(body);
            Ok(())
        }
    }

    fn handle(opted_out: bool) -> (AnalyticsHandle, Arc<Mutex<Vec<String>>>) {
        let transport = Recording::default();
        let sent = transport.sent.clone();
        let handle = AnalyticsHandle::spawn_shared(
            AnalyticsConfig { api_key: "phc_test".into(), distinct_id: "anon".into() },
            Props::new(),
            Arc::new(AtomicBool::new(opted_out)),
            transport,
        );
        (handle, sent)
    }

    fn telemetry() -> BatchSendTelemetry {
        BatchSendTelemetry {
            prefilter: BatchPrefilterStats {
                input: 120,
                kept: 80,
                sessions: 80,
                rows_in_sessions: 110,
                rows_dropped_shadowed: 3,
                session_lines_in: 900,
                session_lines_deduped: 400,
                session_lines_capped: 20,
                ..BatchPrefilterStats::default()
            },
            est_tokens_before: 10_000,
            est_tokens_after: 6_300,
            anchors: Default::default(),
        }
    }

    #[test]
    fn session_counters_ride_next_to_last_send_until_the_v54_column_exists() {
        use crate::dreamcycle::jobs::{AnchorStats, SessionCounters};
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let cycle = "night-counters-test";
        // Send time: the row lands and the session counters are known, anchors not yet.
        assert!(db.record_batch_send_stats(cycle, "consolidation", &telemetry()));
        let status = db.dream_status(cycle, 7);
        assert_eq!(status.last_send.as_ref().map(|r| r.cycle_id.as_str()), Some(cycle));
        let c = status.last_send_sessions.expect("recorded by this process");
        assert_eq!((c.sessions, c.rows_dropped_shadowed, c.lines_deduped, c.lines_capped), (80, 3, 400, 20));
        assert_eq!((c.anchors_resolved, c.anchors_unresolved), (0, 0));
        // Results parsed: the final counters replace them; the row is not rewritten.
        let mut t = telemetry();
        t.anchors = AnchorStats { resolved: 41, unresolved: 2, line_mismatch: 1 };
        db.record_batch_night_counters(cycle, "consolidation", &t);
        let status = db.dream_status(cycle, 7);
        let c = status.last_send_sessions.unwrap();
        assert_eq!((c.anchors_resolved, c.anchors_unresolved, c.anchors_line_mismatch), (41, 2, 1));
        assert_eq!(db.batch_send_stats(10).len(), 1);
        // The JSON a face / `shogun dream` reads carries them under `last_send.session_lane`, and
        // an older reader's struct (every field defaulted) still parses what a newer one writes.
        let json = db.dream_status_json();
        let lane = &json["last_send"]["session_lane"];
        assert_eq!(lane["sessions"], 80);
        assert_eq!(lane["anchors_resolved"], 41);
        let parsed: SessionCounters = serde_json::from_value(lane.clone()).unwrap();
        assert_eq!(parsed, c);
        let older: SessionCounters = serde_json::from_str(r#"{"sessions":5}"#).unwrap();
        assert_eq!(older, SessionCounters { sessions: 5, ..SessionCounters::default() });
        // Another night's row does not borrow this night's counters, and a night this process
        // never recorded shows none.
        let other = Db::open_in_memory(Arc::new(|| 6_000)).unwrap();
        assert!(other.record_batch_send_stats("night-other", "consolidation", &telemetry()));
        assert_eq!(other.dream_status("night-other", 7).last_send_sessions, Some(telemetry().session_counters()));
        assert_eq!(db.dream_status(cycle, 7).last_send_sessions, Some(c), "this night keeps its own");
        assert!(db.session_counters_for("night-never-recorded", "consolidation").is_none());
    }

    /// Every event the worker sent, once it has flushed (on disconnect, or its 3 s timer). A
    /// "nothing" answer waits past that timer, so a clone still holding the channel open cannot
    /// hide an event that would have gone out.
    fn events(sent: &Arc<Mutex<Vec<String>>>, expect_any: bool) -> Vec<serde_json::Value> {
        let deadline = if expect_any { 60 } else { 35 };
        for _ in 0..deadline {
            if !sent.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        sent.lock()
            .unwrap()
            .iter()
            .flat_map(|body| {
                let v: serde_json::Value = serde_json::from_str(body).unwrap();
                v["batch"].as_array().cloned().unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn the_db_stays_shareable_across_threads() {
        // The desktop manages `Db` as Tauri state (Send + Sync); the handle slot must keep it so.
        fn shareable<T: Send + Sync + 'static>() {}
        shareable::<Db>();
        shareable::<AnalyticsHandle>();
    }

    #[test]
    fn one_counts_only_event_per_night() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let (h, sent) = handle(false);
        assert!(db.set_analytics(h.clone()));
        let (other, _) = handle(false);
        assert!(!db.clone().set_analytics(other), "the first handle wins, across clones");

        assert!(db.record_batch_send_stats("night-1", "consolidation", &telemetry()));
        // A rewrite of the same night's row (a retried submission) sends nothing more.
        assert!(db.record_batch_send_stats("night-1", "consolidation", &telemetry()));
        drop(db);
        drop(h);

        let events = events(&sent, true);
        assert_eq!(events.len(), 1, "{events:?}");
        let event = &events[0];
        assert_eq!(event["event"], "dream_batch_sent");
        let props = event["properties"].as_object().unwrap();
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 4, "{keys:?}");
        assert_eq!(props["job"], "consolidation");
        assert_eq!(props["items_considered"], "100_249");
        assert_eq!(props["items_sent"], "50_99");
        // 37% fewer estimated tokens → 35.
        assert_eq!(props["token_reduction_pct"], 35);
    }

    #[test]
    fn opt_out_suppresses_the_event_and_the_row_still_lands() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        let (h, sent) = handle(true);
        db.set_analytics(h.clone());
        assert!(db.record_batch_send_stats("night-1", "consolidation", &telemetry()));
        assert_eq!(db.batch_send_stats(10).len(), 1, "local telemetry is unaffected");
        drop(db);
        drop(h);
        assert!(events(&sent, false).is_empty());
    }

    #[test]
    fn without_a_handle_or_for_an_unknown_job_nothing_is_emitted() {
        let db = Db::open_in_memory(Arc::new(|| 5_000)).unwrap();
        // No handle yet: the row lands, nothing to send it through.
        assert!(db.record_batch_send_stats("night-0", "consolidation", &telemetry()));
        let (h, sent) = handle(false);
        db.set_analytics(h.clone());
        // A job name outside the fixed list never becomes a property.
        assert!(db.record_batch_send_stats("night-1", "Slack — #secret-channel", &telemetry()));
        drop(db);
        drop(h);
        assert!(events(&sent, false).is_empty());
    }
}

/// What one classified batch did (counts only — never text): rows a match promoted, rows written
/// fresh, and review signals recorded for the loop consolidator. Makes "the review changed
/// nothing" diagnosable in the counts-only log line of the consolidation job and of mail triage
/// Stage B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Landed {
    pub promoted: usize,
    pub written: usize,
    pub signals: usize,
}

/// A reviewed commitment / open loop as a typed signal: `Some(Some(_))` with its quote,
/// `Some(None)` when it has none (dropped — it may not promote a local row either), `None` for a
/// local-rule candidate or an entity.
fn review_signal(
    event_id: i64,
    candidate: &shogun_memory::extract::Candidate,
) -> Option<Option<shogun_memory::loops::NewSignal>> {
    use shogun_memory::extract::Candidate;
    use shogun_memory::loops::signal_kind;
    let (kind, text, confidence, evidence) = match candidate {
        Candidate::Commitment { direction, description, confidence, evidence } => {
            (signal_kind(direction.as_str()), description, *confidence, evidence)
        }
        Candidate::OpenLoop { kind, description, confidence, evidence } => {
            (signal_kind(kind.as_str()), description, *confidence, evidence)
        }
        Candidate::Entity(_) => return None,
    };
    (confidence > shogun_memory::extract::LOCAL_RULE_MAX_CONFIDENCE).then(|| {
        evidence.as_ref().map(|quote| shogun_memory::loops::NewSignal {
            kind,
            text: text.clone(),
            event_id,
            quote: quote.clone(),
        })
    })
}

impl Landed {
    /// Candidates that landed in a state row, either way.
    pub fn total(self) -> usize {
        self.promoted + self.written
    }
}
