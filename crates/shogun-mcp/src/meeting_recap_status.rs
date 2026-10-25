//! Content-free recap job state shared with API readers through the desktop's existing heartbeat.
//! Jobs remain process-local. Missing or stale snapshots report unknown; a fresh restart reports
//! idle rather than resurrecting pending jobs. Saved minutes remain the durable recap content.

use std::collections::HashMap;
use std::path::Path;

pub const FILE_NAME: &str = "meeting_recap_status.json";
pub const MAX_AGE_MS: u64 = 4_000;
const MAX_JOBS: usize = 100;
const STATES: [&str; 8] = [
    "idle",
    "pending",
    "ready",
    "needs_plan",
    "needs_key",
    "unavailable",
    "offline",
    "stalled",
];

/// A missing token is not proof that the user needs a plan. Billing owns eligibility decisions.
pub fn missing_credential_status(batch_provisioned: bool) -> &'static str {
    if batch_provisioned {
        "unavailable"
    } else {
        "needs_plan"
    }
}

/// Process-local jobs. Terminal history is bounded; running jobs retain their commit ownership.
#[derive(Default)]
pub struct Jobs {
    generation: u64,
    rows: HashMap<i64, (u64, &'static str)>,
    started: HashMap<i64, std::time::Instant>,
    online_retried: std::collections::HashSet<i64>,
}

impl Jobs {
    pub fn start(&mut self, session_id: i64) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.rows.insert(session_id, (self.generation, "pending"));
        self.started.insert(session_id, std::time::Instant::now());
        self.prune_terminal_history();
        self.generation
    }

    fn prune_terminal_history(&mut self) {
        while self
            .rows
            .values()
            .filter(|(_, state)| *state != "pending")
            .count()
            > MAX_JOBS
        {
            let oldest = self
                .rows
                .iter()
                .filter(|(_, (_, state))| *state != "pending")
                .min_by_key(|(_, (generation, _))| *generation)
                .map(|(id, _)| *id);
            if let Some(id) = oldest {
                self.rows.remove(&id);
                self.started.remove(&id);
                self.online_retried.remove(&id);
            } else {
                break;
            }
        }
    }

    pub fn is_current(&self, session_id: i64, generation: u64) -> bool {
        self.rows
            .get(&session_id)
            .is_some_and(|(current, _)| *current == generation)
    }

    pub fn elapsed_ms(&self, session_id: i64) -> u64 {
        self.started.get(&session_id).map_or(0, |start| start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
    }

    pub fn status(&self, session_id: i64) -> &'static str {
        self.rows
            .get(&session_id)
            .map_or("idle", |(_, state)| *state)
    }

    pub fn set(&mut self, session_id: i64, generation: u64, state: &'static str) {
        if self.is_current(session_id, generation) && STATES.contains(&state) {
            self.rows.insert(session_id, (generation, state));
            self.prune_terminal_history();
        }
    }

    /// Forget a meeting's job (the meeting was deleted). Its worker stops being current, so it can
    /// neither commit minutes nor change status; generations only grow, so a later job for a
    /// reused id can never be mistaken for it.
    pub fn cancel(&mut self, session_id: i64) {
        self.rows.remove(&session_id);
        self.started.remove(&session_id);
        self.online_retried.remove(&session_id);
    }

    pub fn finish(&mut self, session_id: i64, generation: u64) -> bool {
        if !self.is_current(session_id, generation) {
            return false;
        }
        if self.status(session_id) == "pending" {
            self.set(session_id, generation, "unavailable");
        }
        true
    }

    /// Claim once centrally, so four webviews cannot each resubmit an offline job.
    pub fn online_retries(&mut self) -> Vec<i64> {
        let ids: Vec<_> = self.rows.iter().filter_map(|(id, (_, state))| {
            (*state == "offline" && !self.online_retried.contains(id)).then_some(*id)
        }).collect();
        self.online_retried.extend(ids.iter().copied());
        ids
    }

    pub fn publish(&self, path: &Path, now_ms: u64) -> Result<(), String> {
        let mut rows: Vec<_> = self.rows.iter().collect();
        rows.sort_unstable_by_key(|(_, (generation, _))| std::cmp::Reverse(*generation));
        let snapshot = Snapshot {
            observed_at_ms: now_ms,
            truncated: rows.len() > MAX_JOBS,
            jobs: rows
                .into_iter()
                .take(MAX_JOBS)
                .map(|(id, (_, state))| (*id, (*state).to_string()))
                .collect(),
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
        crate::settings_file::write_atomic(path, &bytes, false)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    observed_at_ms: u64,
    truncated: bool,
    jobs: HashMap<i64, String>,
}

/// Fresh empty snapshots mean no job this process knows about; stale or corrupt means unknown.
pub fn read(path: &Path, session_id: i64, now_ms: u64) -> &'static str {
    let snapshot = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok());
    let Some(snapshot) = snapshot else {
        return "unknown";
    };
    if snapshot.jobs.len() > MAX_JOBS
        || !now_ms
            .checked_sub(snapshot.observed_at_ms)
            .is_some_and(|age| age <= MAX_AGE_MS)
    {
        return "unknown";
    }
    match snapshot.jobs.get(&session_id) {
        None if snapshot.truncated => "unknown",
        None => "idle",
        Some(state) => STATES
            .iter()
            .copied()
            .find(|known| *known == state.as_str())
            .unwrap_or("unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_generations_cannot_change_or_finish_the_current_job() {
        let mut jobs = Jobs::default();
        let old = jobs.start(42);
        let current = jobs.start(42);
        jobs.set(42, old, "needs_plan");
        assert!(!jobs.finish(42, old));
        assert_eq!(jobs.status(42), "pending");
        jobs.set(42, current, "ready");
        assert!(jobs.finish(42, current));
        assert_eq!(jobs.status(42), "ready");
    }

    #[test]
    fn cancelled_job_is_no_longer_current_and_a_new_job_for_the_same_id_is() {
        let mut jobs = Jobs::default();
        let old = jobs.start(42);
        jobs.cancel(42);
        assert!(!jobs.is_current(42, old));
        assert_eq!(jobs.status(42), "idle");
        jobs.set(42, old, "ready");
        assert!(!jobs.finish(42, old));
        assert_eq!(jobs.status(42), "idle");
        let reused = jobs.start(42);
        assert!(jobs.is_current(42, reused));
        assert!(!jobs.is_current(42, old));
    }

    #[test]
    fn snapshots_expire_and_restart_does_not_resurrect_jobs() {
        let dir = std::env::temp_dir().join(format!("shogun-recap-status-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let mut jobs = Jobs::default();
        let generation = jobs.start(42);
        jobs.publish(&path, 10_000).unwrap();
        assert_eq!(read(&path, 42, 10_000), "pending");
        assert_eq!(read(&path, 42, 10_000 + MAX_AGE_MS + 1), "unknown");
        assert_eq!(read(&path, 42, 9_999), "unknown");
        jobs.set(42, generation, "needs_plan");
        jobs.publish(&path, 11_000).unwrap();
        assert_eq!(read(&path, 42, 11_000), "needs_plan");
        std::fs::write(&path, b"invalid").unwrap();
        assert_eq!(read(&path, 42, 11_000), "unknown");
        Jobs::default().publish(&path, 12_000).unwrap();
        assert_eq!(read(&path, 42, 12_000), "idle");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_bound_never_cancels_running_jobs() {
        let mut jobs = Jobs::default();
        let generation = jobs.start(42);
        for id in 100..201 {
            jobs.start(id);
        }
        assert!(jobs.is_current(42, generation));
        jobs.set(42, generation, "ready");
        assert_eq!(jobs.status(42), "ready");
    }

    #[test]
    fn evicted_jobs_cannot_match_a_new_generation() {
        let mut jobs = Jobs::default();
        let old = jobs.start(42);
        jobs.set(42, old, "ready");
        for id in 100..201 {
            let generation = jobs.start(id);
            jobs.set(id, generation, "ready");
        }
        let current = jobs.start(42);
        jobs.set(42, old, "ready");
        assert_eq!(jobs.status(42), "pending");
        assert_ne!(old, current);
        assert_eq!(jobs.rows.len(), MAX_JOBS + 1);
    }
}

#[cfg(test)]
mod online_tests {
    use super::*;
    #[test]
    fn online_retry_is_once_even_across_windows_and_repeated_failures() {
        let mut jobs = Jobs::default();
        let generation = jobs.start(4);
        jobs.set(4, generation, "offline");
        assert_eq!(jobs.online_retries(), vec![4]);
        assert!(jobs.online_retries().is_empty());
        let generation = jobs.start(4);
        jobs.set(4, generation, "offline");
        assert!(jobs.online_retries().is_empty());
    }
}
