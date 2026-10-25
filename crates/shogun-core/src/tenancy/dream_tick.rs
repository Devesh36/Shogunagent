//! The hourly Dream Cycle on the multi-principal server (plan E5-T4).
//!
//! The desktop drives the hourly review from its own scheduler; a sidecar serving many principals
//! drives it from here instead: once per tick, for every principal, decide with the same pure
//! rule the desktop uses ([`crate::dreamcycle::hourly::decide`]) and run the cycle only when the
//! principal has **new material** — Batch-eligible events in `[cursor, floor_hour(now))` — or an
//! unfinished hourly cycle to resume. A principal with nothing new costs one indexed probe and no
//! model call (the "no empty Batches" rule, #1066 B).
//!
//! Two lanes:
//! - [`hourly_tick_with_lane`] runs the hourly review through a [`BatchLane`] (the relay, or a
//!   test double): the model path, behind the same frozen-cycle machinery as the desktop.
//! - [`hourly_tick_local`] runs it with the on-device [`LocalRuleClassifier`]: no model, no
//!   egress.
//! - [`hourly_tick_routed`] picks per principal (E5-T5): the Batch lane when the principal has a
//!   relay credential in its token store ([`super::credentials::relay_bearer`]), the local rules
//!   otherwise. This is what `shogun-api --dream-tick` runs.
//!
//! On the Batch lane, the tick then gives the **loop consolidator** its turn
//! ([`tick_principal_batch`], owner decision 2026-10-14, plan E49): one `loops_consolidate` call
//! when a review left signals, or later messages left outcomes of listed items, that no call has
//! read — at most hourly and [`shogun_memory::loops::RUNS_PER_DAY`] times a local day, counted
//! from the traceability rows the lane writes before each send. The same lane carries it (the
//! relay with the principal's credential: the company Batch lane, never BYOK or a subscription
//! delegate). Nothing is sent on the local lane, when the review failed or was skipped (the lane
//! or the store is not usable), or when nothing is unread.
//!
//! Nothing here logs content: outcomes are per-principal counts and fixed codes.

use crate::daemon::Db;
use crate::dreamcycle::gate::{RunConditions, IDLE_THRESHOLD_MS};
use crate::dreamcycle::hourly::{decide, hour_cycle_id, hourly_range, HourlyInputs, HourlySkip};
use crate::dreamcycle::jobs::LocalRuleClassifier;
use crate::dreamcycle::run::GatedRun;
use crate::dreamcycle::schedule::{run_batch_cycle, DreamBatchClients, DreamScheduler};
use crate::llm::anthropic::BatchLane;

use super::{PrincipalId, PrincipalStore};

/// Backlog rows a routed tick embeds per principal before reviewing it (when a local embedding
/// model is attached to the store, E24). The rest waits for the next tick.
pub const EMBED_BACKLOG_PER_TICK: usize = 2_000;

/// Polls a server tick allows one hourly cycle before it leaves the cycle resumable for the next
/// tick (the relay's synchronous review needs none).
pub const SERVER_MAX_POLLS: u32 = 3;

/// What one principal's tick did. Counts and fixed codes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// No new material and nothing to resume: nothing was read or sent.
    NothingNew,
    /// This hour already ran and nothing arrived since.
    AlreadyDone,
    /// The cycle ran; `complete` is false when a job failed and the cycle stays resumable.
    Ran { complete: bool },
    /// The gate declined for another reason (fixed code).
    Skipped(&'static str),
    /// The principal's store could not be opened or read (fixed code).
    Unavailable(&'static str),
    /// The lane failed (fixed code); the cycle is resumable.
    Failed(&'static str),
}

/// Which lane a principal's tick used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickLane {
    /// The Batch lane (the relay with the principal's own credential, or a test double).
    Batch,
    /// The on-device local rules: no model, no egress.
    Local,
}

/// One principal's line of a tick report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalTick {
    pub principal: PrincipalId,
    pub outcome: TickOutcome,
    pub lane: TickLane,
    /// What the loop consolidator applied this tick (counts only); `None` when it was not called
    /// or its call failed.
    pub loops: Option<shogun_memory::loops::Applied>,
}

/// The conditions the shared gate is given on a server: always idle and on power (a server has
/// no user at the keyboard and no battery), inside the window so the hourly id runs Full.
fn server_conditions() -> RunConditions {
    RunConditions {
        within_window: true,
        window_elapsed: false,
        idle_ms: IDLE_THRESHOLD_MS,
        screen_locked: true,
        power_connected: true,
        battery_pct: 100,
        full_run_done_today: false,
    }
}

/// Decide for one principal at `now_ms`: `Ok(cycle_id)` to run, or the outcome without running.
pub fn plan_principal(db: &Db, now_ms: i64) -> Result<String, TickOutcome> {
    let off = crate::daemon::os_gmt_offset_secs(now_ms);
    let cycle_id = hour_cycle_id(now_ms.div_euclid(1000), off);
    let cursor = db.try_last_consolidated_to().map_err(|_| TickOutcome::Unavailable("cursor_unreadable"))?;
    let (from, to) = hourly_range(cursor, now_ms, off);
    let new_material = from < to && db.has_batch_eligible_events(from, to);
    let unfinished = db
        .try_oldest_unfinished_hourly_cycle()
        .map_err(|_| TickOutcome::Unavailable("ledger_unreadable"))?
        .is_some();
    let done_this_hour = db
        .try_dream_cycle(&cycle_id)
        .map_err(|_| TickOutcome::Unavailable("ledger_unreadable"))?
        .is_some();
    let inputs = HourlyInputs {
        batch_provisioned: true,
        power_connected: true,
        battery_pct: 100,
        done_this_hour,
        attempted_this_hour: false,
        unfinished_cycle: unfinished,
        new_material,
        before_slot: false,
    };
    match decide(&inputs) {
        Ok(()) => Ok(cycle_id),
        Err(HourlySkip::NothingNew) => Err(TickOutcome::NothingNew),
        Err(HourlySkip::AlreadyDone) => Err(TickOutcome::AlreadyDone),
        Err(HourlySkip::NoBatchLane) => Err(TickOutcome::Skipped("no_batch_lane")),
        Err(HourlySkip::InsufficientPower) => Err(TickOutcome::Skipped("insufficient_power")),
        Err(HourlySkip::AlreadyAttempted) => Err(TickOutcome::Skipped("already_attempted")),
        Err(HourlySkip::BeforeSlot) => Err(TickOutcome::Skipped("before_slot")),
    }
}

fn ran(run: GatedRun) -> TickOutcome {
    match run {
        GatedRun::Ran { report, .. } => TickOutcome::Ran { complete: report.is_complete() },
        GatedRun::Skipped(_) => TickOutcome::Skipped("gate"),
    }
}

/// One principal through a Batch lane: the model path, only when [`plan_principal`] says so.
pub async fn tick_principal_with_lane<B: BatchLane>(db: &Db, lane: &B, now_ms: i64) -> TickOutcome {
    let cycle_id = match plan_principal(db, now_ms) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    match run_batch_cycle(
        db,
        DreamBatchClients::classify_only(lane),
        &server_conditions(),
        &cycle_id,
        now_ms,
        SERVER_MAX_POLLS,
        || async {},
    )
    .await
    {
        Ok(run) => ran(run),
        Err(_) => TickOutcome::Failed("batch_lane"),
    }
}

/// Whether the consolidator may follow a review that ended this way: the review ran to the end,
/// or there was nothing new to review (signals or outcomes an earlier hour left unread still get
/// their turn). A failed, skipped or unavailable review sends nothing more.
fn consolidates_after(outcome: &TickOutcome) -> bool {
    matches!(outcome, TickOutcome::Ran { complete: true } | TickOutcome::NothingNew | TickOutcome::AlreadyDone)
}

/// One principal through a Batch lane: the hourly review ([`tick_principal_with_lane`]), then the
/// loop consolidator on the same lane when it is due
/// ([`crate::dreamcycle::loops::consolidate_loops`]: unread signals or outcomes, an hour since its
/// last call, under the daily cap). Returns the review's outcome and the consolidator's counts.
pub async fn tick_principal_batch<B: BatchLane>(
    db: &Db,
    lane: &B,
    now_ms: i64,
) -> (TickOutcome, Option<shogun_memory::loops::Applied>) {
    let outcome = tick_principal_with_lane(db, lane, now_ms).await;
    let loops = if consolidates_after(&outcome) {
        crate::dreamcycle::loops::consolidate_loops(db, lane, now_ms).await
    } else {
        None
    };
    (outcome, loops)
}

/// One principal with the on-device local rules: no model, no egress.
pub fn tick_principal_local(db: &Db, now_ms: i64) -> TickOutcome {
    let cycle_id = match plan_principal(db, now_ms) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let classifier = LocalRuleClassifier;
    ran(DreamScheduler::new(db, &classifier).tick(&server_conditions(), &cycle_id, now_ms))
}

/// Every principal the store holds a file for, in id order (open or not).
pub fn principals_on_disk(store: &PrincipalStore) -> Vec<PrincipalId> {
    let Ok(entries) = std::fs::read_dir(store.root()) else {
        return Vec::new();
    };
    let mut principals: Vec<PrincipalId> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().and_then(|name| PrincipalId::parse(name).ok()))
        .filter(|principal| store.exists(principal))
        .collect();
    principals.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    principals
}

/// The hourly tick over `principals` through `lane`.
pub async fn hourly_tick_with_lane<B: BatchLane>(
    store: &PrincipalStore,
    principals: &[PrincipalId],
    lane: &B,
    now_ms: i64,
) -> Vec<PrincipalTick> {
    let mut report = Vec::with_capacity(principals.len());
    for principal in principals {
        let (outcome, loops) = match store.open(principal) {
            Ok(open) => tick_principal_batch(open.db(), lane, now_ms).await,
            Err(_) => (TickOutcome::Unavailable("store_unavailable"), None),
        };
        report.push(PrincipalTick { principal: principal.clone(), outcome, lane: TickLane::Batch, loops });
    }
    report
}

/// The hourly tick over `principals`, each through the Batch lane `lane_for` builds for it, or
/// with the local rules when `lane_for` answers `None` (no relay credential configured for that
/// principal). `lane_for` gets the principal's open [`Db`] so a lane can write its traceability
/// rows into that principal's own store. Building a lane sends nothing: a principal with no new
/// material still makes no call either way.
pub async fn hourly_tick_routed<B, F>(
    store: &PrincipalStore,
    principals: &[PrincipalId],
    lane_for: F,
    now_ms: i64,
) -> Vec<PrincipalTick>
where
    B: BatchLane,
    F: Fn(&PrincipalId, &Db) -> Option<B>,
{
    let mut report = Vec::with_capacity(principals.len());
    for principal in principals {
        let (outcome, lane, loops) = match store.open(principal) {
            Ok(open) => {
                // Embed the principal's backlog first (a no-op without a local model, E24), so
                // the review and the next search see vectors for the material it reviews.
                open.db().embed_pending(EMBED_BACKLOG_PER_TICK);
                match lane_for(principal, open.db()) {
                    Some(lane) => {
                        let (outcome, loops) = tick_principal_batch(open.db(), &lane, now_ms).await;
                        (outcome, TickLane::Batch, loops)
                    }
                    // No Batch lane: the local rules, and never a consolidator call.
                    None => (tick_principal_local(open.db(), now_ms), TickLane::Local, None),
                }
            }
            Err(_) => (TickOutcome::Unavailable("store_unavailable"), TickLane::Local, None),
        };
        report.push(PrincipalTick { principal: principal.clone(), outcome, lane, loops });
    }
    report
}

/// The hourly tick over `principals` with the local rules (`shogun-api --dream-tick`).
pub fn hourly_tick_local(store: &PrincipalStore, principals: &[PrincipalId], now_ms: i64) -> Vec<PrincipalTick> {
    principals
        .iter()
        .map(|principal| {
            let outcome = match store.open(principal) {
                Ok(open) => tick_principal_local(open.db(), now_ms),
                Err(_) => TickOutcome::Unavailable("store_unavailable"),
            };
            PrincipalTick { principal: principal.clone(), outcome, lane: TickLane::Local, loops: None }
        })
        .collect()
}

/// A one-line summary of a tick for stderr: counts per outcome, never a principal's content.
pub fn summary(report: &[PrincipalTick]) -> String {
    let count = |f: fn(&TickOutcome) -> bool| report.iter().filter(|t| f(&t.outcome)).count();
    format!(
        "principals={} ran={} nothing_new={} already_done={} skipped={} unavailable={} failed={} loops_consolidated={} batch_lane={} local_lane={}",
        report.len(),
        count(|o| matches!(o, TickOutcome::Ran { .. })),
        count(|o| matches!(o, TickOutcome::NothingNew)),
        count(|o| matches!(o, TickOutcome::AlreadyDone)),
        count(|o| matches!(o, TickOutcome::Skipped(_))),
        count(|o| matches!(o, TickOutcome::Unavailable(_))),
        count(|o| matches!(o, TickOutcome::Failed(_))),
        report.iter().filter(|t| t.loops.is_some()).count(),
        report.iter().filter(|t| t.lane == TickLane::Batch).count(),
        report.iter().filter(|t| t.lane == TickLane::Local).count(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::llm::anthropic::{BatchHandle, BatchItem, BatchResult, BatchStatus};
    use crate::llm::LlmError;
    use crate::tenancy::EnvKeyProvider;
    use shogun_memory::DbKey;

    const HOUR: i64 = 3_600_000;

    /// A fake Batch lane that counts every model-bound call and answers "nothing to report".
    #[derive(Default)]
    struct CountingLane {
        calls: AtomicUsize,
        ids: Mutex<Vec<String>>,
    }

    impl BatchLane for CountingLane {
        /// The hourly window review: one call per item, counted, "nothing to report".
        async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
            self.calls.fetch_add(items.len(), Ordering::SeqCst);
            Ok(items
                .iter()
                .map(|item| BatchResult {
                    custom_id: item.custom_id.clone(),
                    text: Some(r#"{"sessions":[]}"#.into()),
                    error: None,
                })
                .collect())
        }

        async fn submit(&self, items: &[BatchItem], _client_request_id: Option<&str>) -> Result<BatchHandle, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.ids.lock().unwrap() = items.iter().map(|item| item.custom_id.clone()).collect();
            Ok(BatchHandle { id: "tick-batch".into(), status: BatchStatus::Ended })
        }

        async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
            Ok(BatchHandle { id: batch_id.into(), status: BatchStatus::Ended })
        }

        async fn results(&self, _batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
            Ok(self
                .ids
                .lock()
                .unwrap()
                .iter()
                .map(|custom_id| BatchResult {
                    custom_id: custom_id.clone(),
                    text: Some(r#"{"commitments":[],"open_loops":[]}"#.into()),
                    error: None,
                })
                .collect())
        }
    }

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("shogun_dream_tick_{label}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(root: &TempRoot, now: i64) -> PrincipalStore {
        let keys = EnvKeyProvider::with_default(DbKey::new([0x3c; 32]));
        PrincipalStore::new(root.0.clone(), Arc::new(keys), Arc::new(move || now), 8)
    }

    fn ingest(db: &Db, key: &str, text: &str, ts: i64) {
        let request = shogun_mcp::ingest_api::ingest_request(&serde_json::json!({
            "events": [{ "content": text, "ts_ms": ts }],
            "source": "tick-test", "actor": "agent:tick", "thread_key": "t", "idempotency_key": key,
        }))
        .unwrap();
        db.agent_ingest(&request).unwrap();
    }

    /// E5-T4: a principal with zero new events makes zero model calls; one with new material is
    /// reviewed once; a second tick in the same hour sends nothing more.
    #[tokio::test]
    async fn only_principals_with_new_material_reach_the_lane() {
        let now = 200 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let root = TempRoot::new("lane");
        let store = store(&root, now);
        let idle = PrincipalId::parse("idle").unwrap();
        let busy = PrincipalId::parse("busy").unwrap();
        store.open(&idle).unwrap();
        let open = store.open(&busy).unwrap();
        ingest(open.db(), "b1", "I'll send the rollout plan to Oren by Friday.", now - 2 * HOUR);
        drop(open);

        let principals = principals_on_disk(&store);
        assert_eq!(principals, vec![busy.clone(), idle.clone()]);

        // Idle alone: nothing read, nothing sent.
        let lane = CountingLane::default();
        let report = hourly_tick_with_lane(&store, std::slice::from_ref(&idle), &lane, now).await;
        assert_eq!(report[0].outcome, TickOutcome::NothingNew);
        assert_eq!(lane.calls.load(Ordering::SeqCst), 0, "zero new events, zero model calls");

        // Both: only the busy principal reaches the lane.
        let lane = CountingLane::default();
        let report = hourly_tick_with_lane(&store, &principals, &lane, now).await;
        let outcome = |p: &PrincipalId| report.iter().find(|t| &t.principal == p).map(|t| t.outcome.clone());
        assert_eq!(outcome(&idle), Some(TickOutcome::NothingNew));
        assert_eq!(outcome(&busy), Some(TickOutcome::Ran { complete: true }));
        assert_eq!(lane.calls.load(Ordering::SeqCst), 1);

        // The same hour again: the busy principal's window is consumed, nothing is sent.
        let again = CountingLane::default();
        let report = hourly_tick_with_lane(&store, &principals, &again, now + 60_000).await;
        assert!(report.iter().all(|t| matches!(t.outcome, TickOutcome::NothingNew | TickOutcome::AlreadyDone)), "{report:?}");
        assert_eq!(again.calls.load(Ordering::SeqCst), 0);
        assert!(summary(&report).starts_with("principals=2 ran=0"));
    }

    /// Shares one counting lane between the principals a routed tick hands it to.
    struct SharedLane(Arc<CountingLane>);

    impl BatchLane for SharedLane {
        async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
            self.0.review(items).await
        }
        async fn submit(&self, items: &[BatchItem], id: Option<&str>) -> Result<BatchHandle, LlmError> {
            self.0.submit(items, id).await
        }
        async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
            self.0.poll(batch_id).await
        }
        async fn results(&self, batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
            self.0.results(batch_id).await
        }
    }

    /// E5-T5: `--dream-tick` sends a principal through the Batch lane only when its token store
    /// holds a relay credential; the others are reviewed by the local rules with no model call.
    #[tokio::test]
    async fn the_routed_tick_uses_the_batch_lane_only_with_a_stored_relay_credential() {
        use crate::tenancy::credentials::{local_store, relay_bearer};
        use shogun_integrations::{CredentialSlot, TokenStore};

        let now = 220 * 24 * HOUR + 11 * HOUR + 20 * 60_000;
        let root = TempRoot::new("routed");
        let store = store(&root, now);
        let with_relay = PrincipalId::parse("with-relay").unwrap();
        let without = PrincipalId::parse("without").unwrap();
        for (principal, key) in [(&with_relay, "r1"), (&without, "r2")] {
            let open = store.open(principal).unwrap();
            ingest(open.db(), key, "I'll send the rollout plan to Oren by Friday.", now - 2 * HOUR);
        }

        let keys: Arc<dyn crate::tenancy::KeyProvider> =
            Arc::new(EnvKeyProvider::with_default(DbKey::new([0x3c; 32])));
        let tokens = local_store(root.0.join(".tokens"), keys);
        tokens
            .put(with_relay.as_str(), CredentialSlot::BatchRelay, &crate::llm::Secret::new("v1.relay-bearer"))
            .unwrap();

        let principals = principals_on_disk(&store);
        assert_eq!(principals, vec![with_relay.clone(), without.clone()], "the token directory is not a principal");

        let counting = Arc::new(CountingLane::default());
        let report = hourly_tick_routed(
            &store,
            &principals,
            |principal, _db| relay_bearer(&tokens, principal).map(|_| SharedLane(counting.clone())),
            now,
        )
        .await;
        let line = |p: &PrincipalId| report.iter().find(|t| &t.principal == p).cloned().unwrap();
        assert_eq!(line(&with_relay).lane, TickLane::Batch);
        assert_eq!(line(&with_relay).outcome, TickOutcome::Ran { complete: true });
        assert_eq!(line(&without).lane, TickLane::Local);
        assert_eq!(line(&without).outcome, TickOutcome::Ran { complete: true });
        assert_eq!(counting.calls.load(Ordering::SeqCst), 1, "only the principal with a credential reached the lane");
        assert!(summary(&report).ends_with("batch_lane=1 local_lane=1"), "{}", summary(&report));

        // Removing the credential sends the next new material through the local rules.
        tokens.delete(with_relay.as_str(), CredentialSlot::BatchRelay).unwrap();
        let open = store.open(&with_relay).unwrap();
        ingest(open.db(), "r3", "I'll book the venue for the offsite by Tuesday.", now + HOUR);
        drop(open);
        let counting = Arc::new(CountingLane::default());
        let report = hourly_tick_routed(
            &store,
            std::slice::from_ref(&with_relay),
            |principal, _db| relay_bearer(&tokens, principal).map(|_| SharedLane(counting.clone())),
            now + 2 * HOUR + 60_000,
        )
        .await;
        assert_eq!(report[0].lane, TickLane::Local);
        assert_eq!(counting.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_local_tick_reviews_new_material_without_a_model() {
        let now = 300 * 24 * HOUR + 9 * HOUR + 10 * 60_000;
        let root = TempRoot::new("local");
        let store = store(&root, now);
        let busy = PrincipalId::parse("busy").unwrap();
        let open = store.open(&busy).unwrap();
        ingest(open.db(), "l1", "I'll draft the cost estimate for Atlas Ledger by Monday.", now - 2 * HOUR);
        drop(open);
        let principals = principals_on_disk(&store);
        let report = hourly_tick_local(&store, &principals, now);
        assert_eq!(report[0].outcome, TickOutcome::Ran { complete: true }, "{report:?}");
        let open = store.open(&busy).unwrap();
        assert_eq!(open.db().try_last_consolidated_to().unwrap(), Some(crate::dreamcycle::hourly::window_end_ms(now, 0)));
        let report = hourly_tick_local(&store, &principals, now + 60_000);
        assert!(matches!(report[0].outcome, TickOutcome::NothingNew | TickOutcome::AlreadyDone), "{report:?}");
        assert!(report.iter().all(|t| t.loops.is_none()), "the local lane never consolidates");
    }

    /// An offline Batch lane that behaves like the relay where the consolidator's gate can see it:
    /// it writes one traceability row per item into the principal's own store before it answers
    /// (digest and size only). The window review reports every "I'll send" line as a commitment
    /// anchored on its line; the consolidator re-lists every new signal with its exact quote at 0.9.
    /// `fail` makes every call fail before anything is recorded.
    struct TracedLane {
        sink: crate::db_sink::DbTraceabilitySink,
        purposes: Mutex<Vec<String>>,
        fail: bool,
    }

    impl TracedLane {
        fn new(db: &Db, fail: bool) -> Self {
            Self { sink: db.traceability_sink(), purposes: Mutex::new(Vec::new()), fail }
        }

        fn purposes(&self) -> Vec<String> {
            self.purposes.lock().unwrap().clone()
        }

        fn answer(item: &BatchItem) -> String {
            let data = item.chunk.as_str();
            if item.purpose == shogun_memory::loops::PURPOSE {
                let open: Vec<serde_json::Value> = data
                    .lines()
                    .skip_while(|l| !l.starts_with("New signals"))
                    .skip(1)
                    .take_while(|l| !l.starts_with("Outcomes noticed"))
                    .filter_map(|l| {
                        let (head, quote) = l.rsplit_once(" | ")?;
                        let quote = quote.strip_prefix('"')?.strip_suffix('"')?;
                        let parts: Vec<&str> = head.splitn(3, " | ").collect();
                        let event: i64 = parts.get(1)?.strip_prefix('E')?.parse().ok()?;
                        Some(serde_json::json!({"id": null, "kind": "commitment", "horizon": "short",
                            "source_event_id": event, "source_quote": quote, "confidence": 0.9, "text": parts.get(2)?}))
                    })
                    .collect();
                return serde_json::json!({"open": open, "resolved": [], "duplicates": []}).to_string();
            }
            let mut sessions = Vec::new();
            for line in data.lines() {
                if let Some(id) = line.strip_prefix("Session: ") {
                    sessions.push((id.trim().to_string(), Vec::new()));
                    continue;
                }
                let Some((n, text)) = line.strip_prefix('[').and_then(|r| r.split_once("] ")) else { continue };
                let (Ok(n), Some((_, items))) = (n.parse::<usize>(), sessions.last_mut()) else { continue };
                if text.contains("I'll send") {
                    let description = text.trim_end_matches('.');
                    items.push(serde_json::json!({"direction": "mine", "description": description, "line": n,
                        "anchor": text.chars().take(40).collect::<String>(), "anchor_len": description.chars().count()}));
                }
            }
            let sessions: Vec<serde_json::Value> = sessions
                .into_iter()
                .filter(|(_, items)| !items.is_empty())
                .map(|(id, items)| serde_json::json!({"id": id, "commitments": items, "open_loops": [], "entities": []}))
                .collect();
            serde_json::json!({ "sessions": sessions }).to_string()
        }
    }

    impl BatchLane for TracedLane {
        async fn review(&self, items: &[BatchItem]) -> Result<Vec<BatchResult>, LlmError> {
            use crate::llm::traceability::{Route, TraceRecord, TraceabilitySink};
            if self.fail {
                return Err(LlmError::Provider("relay unreachable".into()));
            }
            let mut out = Vec::new();
            for item in items {
                self.sink.try_record(TraceRecord::for_chunk(Route::BatchRelay, item.purpose.clone(), "relay.test", &item.chunk, false))?;
                self.purposes.lock().unwrap().push(item.purpose.clone());
                out.push(BatchResult { custom_id: item.custom_id.clone(), text: Some(Self::answer(item)), error: None });
            }
            Ok(out)
        }

        fn synchronous_review(&self) -> bool {
            true
        }

        async fn submit(&self, _items: &[BatchItem], _id: Option<&str>) -> Result<BatchHandle, LlmError> {
            Err(LlmError::Provider("not used".into()))
        }

        async fn poll(&self, batch_id: &str) -> Result<BatchHandle, LlmError> {
            Ok(BatchHandle { id: batch_id.into(), status: BatchStatus::Ended })
        }

        async fn results(&self, _batch_id: &str) -> Result<Vec<BatchResult>, LlmError> {
            Ok(Vec::new())
        }
    }

    fn consolidator_rows(db: &Db) -> Vec<shogun_memory::traceability::TraceRow> {
        let filter = shogun_memory::traceability::Filter { purpose: Some(shogun_memory::loops::PURPOSE.into()), ..Default::default() };
        db.traceability_sink().rows(&filter)
    }

    /// E49: on the Batch lane, a review that produced signals is followed by one consolidator call
    /// on the same lane, traced before it is sent; its items land at the answer's confidence. Within
    /// the hour nothing more is sent even when a new review leaves new signals; those wait for the
    /// next tick, which sends them though it has nothing new to review. A failed lane and the local
    /// lane send nothing.
    #[tokio::test]
    async fn the_batch_tick_runs_the_consolidator_after_a_review_with_signals() {
        use std::sync::atomic::AtomicI64;
        let start = 240 * 24 * HOUR + 10 * HOUR + 30 * 60_000;
        let clock = Arc::new(AtomicI64::new(start));
        let root = TempRoot::new("loops");
        let keys = EnvKeyProvider::with_default(DbKey::new([0x3c; 32]));
        let at = clock.clone();
        let store = PrincipalStore::new(root.0.clone(), Arc::new(keys), Arc::new(move || at.load(Ordering::SeqCst)), 8);
        let set = |ms: i64| clock.store(ms, Ordering::SeqCst);
        let busy = PrincipalId::parse("busy").unwrap();
        let open = store.open(&busy).unwrap();
        let db = open.db();
        ingest(db, "k1", "I'll send the rollout plan to Oren by Friday.", start - 2 * HOUR);

        // 10:30: the review reports a signal; the consolidator follows on the same lane.
        let lane = TracedLane::new(db, false);
        let (outcome, loops) = tick_principal_batch(db, &lane, start).await;
        assert_eq!(outcome, TickOutcome::Ran { complete: true });
        let applied = loops.expect("the consolidator ran");
        assert_eq!(applied.opened + applied.relisted, 1, "{applied:?}");
        assert_eq!(lane.purposes(), vec![crate::dreamcycle::jobs::WINDOW_PURPOSE.to_string(), shogun_memory::loops::PURPOSE.to_string()]);
        let rows = consolidator_rows(db);
        assert_eq!(rows.len(), 1, "one traced call");
        assert!(rows[0].chunk_bytes > 0 && !rows[0].chunk_xxh64.is_empty(), "a digest and a size, never the text");
        let listed: Vec<(f64, String)> = db.commitment_rows().into_iter().map(|c| (c.confidence, c.status)).collect();
        assert_eq!(listed, vec![(0.9, "open".to_string())], "stored at the answer's confidence");

        // 11:10: a new message reviewed, its signal unread, but the last call was 40 minutes ago.
        set(start + 40 * 60_000);
        ingest(db, "k2", "I'll send the pricing deck to Priya by Monday.", start + 10 * 60_000);
        let lane = TracedLane::new(db, false);
        let (outcome, loops) = tick_principal_batch(db, &lane, start + 40 * 60_000).await;
        assert_eq!(outcome, TickOutcome::Ran { complete: true });
        assert_eq!(loops, None, "at most hourly");
        assert_eq!(lane.purposes(), vec![crate::dreamcycle::jobs::WINDOW_PURPOSE.to_string()]);

        // 11:40: nothing new to review, but the signal left unread gets its turn.
        set(start + 70 * 60_000);
        let lane = TracedLane::new(db, false);
        let (outcome, loops) = tick_principal_batch(db, &lane, start + 70 * 60_000).await;
        assert!(matches!(outcome, TickOutcome::NothingNew | TickOutcome::AlreadyDone), "{outcome:?}");
        assert!(loops.is_some());
        assert_eq!(lane.purposes(), vec![shogun_memory::loops::PURPOSE.to_string()]);
        assert_eq!(consolidator_rows(db).len(), 2);

        // 13:10: nothing unread: no call.
        set(start + 160 * 60_000);
        let lane = TracedLane::new(db, false);
        let (_, loops) = tick_principal_batch(db, &lane, start + 160 * 60_000).await;
        assert_eq!(loops, None);
        assert!(lane.purposes().is_empty(), "nothing new, nothing sent");

        // Through the routed tick: the report carries the consolidator's counts.
        set(start + 4 * HOUR);
        ingest(db, "k3", "I'll send the vendor contract to Kenji by Friday.", start + 3 * HOUR);
        let report = hourly_tick_routed(&store, std::slice::from_ref(&busy), |_, db| Some(TracedLane::new(db, false)), start + 4 * HOUR).await;
        assert_eq!(report[0].lane, TickLane::Batch);
        assert!(report[0].loops.is_some(), "{report:?}");
        assert!(summary(&report).contains("loops_consolidated=1"), "{}", summary(&report));
        assert_eq!(consolidator_rows(db).len(), 3);

        // A failing lane: the review fails and nothing more is attempted.
        set(start + 6 * HOUR);
        ingest(db, "k4", "I'll send the latency report to Dana by Friday.", start + 5 * HOUR);
        let lane = TracedLane::new(db, true);
        let (outcome, loops) = tick_principal_batch(db, &lane, start + 6 * HOUR).await;
        assert_eq!(outcome, TickOutcome::Failed("batch_lane"));
        assert_eq!(loops, None);
        assert_eq!(consolidator_rows(db).len(), 3, "no call when the lane is unavailable");
        drop(open);
    }
}
