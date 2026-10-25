//! Stopping an AI search on time (issue #940 B2).
//!
//! [`AgentSearchModel`](super::AgentSearchModel) can only discard a late answer: the request it
//! made keeps running until the provider answers. A surface that holds a client which can be
//! stopped mid-request (the desktop's Agent-lane client: the API lanes drop the in-flight request,
//! a subscription delegate's child is killed) wraps it in [`CancellableSearchModel`], which stops
//! each call at the search's deadline or as soon as the user starts a newer search.
//!
//! [`LatestSearch`] is the "newer search" half: each search takes a flag from it, and taking a new
//! one raises the previous one's, so only the latest search in a surface is ever running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ModelStep, SearchModel};
use crate::llm::LlmError;

/// How often the watcher looks at the clock and the cancel flag. A stopped request costs at most
/// this much past its deadline.
const WATCH_TICK: Duration = Duration::from_millis(25);

/// The provider error a cancelled call reports. Content-free; it maps to `provider_error`, and the
/// surface that cancelled discards the result anyway.
pub const CANCELLED: &str = "cancelled";

/// An Agent-lane client that can be stopped mid-request. When `stop` is raised the call returns
/// promptly with whatever text it had (often none) as `Ok`; the caller knows it raised the flag.
pub trait CancellableModel {
    fn complete_split_cancellable(
        &self,
        system: &str,
        user: &str,
        stop: &Arc<AtomicBool>,
    ) -> Result<String, LlmError>;
}

/// A [`SearchModel`] over two cancellable clients, one per step so usage rows carry each step's
/// prompt tag. Each call stops at the search's deadline (the answer then arrives late and
/// [`refine`](super::refine) reports `provider_timeout`) or when `cancelled` is raised (the call
/// returns an error, so the search stops making calls).
pub struct CancellableSearchModel<C> {
    pub expand: C,
    pub rerank: C,
    pub cancelled: Arc<AtomicBool>,
}

impl<C: CancellableModel> SearchModel for CancellableSearchModel<C> {
    fn complete(
        &self,
        step: ModelStep,
        system: &str,
        user: &str,
        deadline: Instant,
    ) -> Result<String, LlmError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(LlmError::Provider(CANCELLED.to_string()));
        }
        let client = match step {
            ModelStep::Expand => &self.expand,
            ModelStep::Rerank => &self.rerank,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (stop, done, cancelled) = (stop.clone(), done.clone(), self.cancelled.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                        stop.store(true, Ordering::Release);
                        return;
                    }
                    std::thread::sleep(WATCH_TICK);
                }
            })
        };
        let result = client.complete_split_cancellable(system, user, &stop);
        done.store(true, Ordering::Release);
        let _ = watcher.join();
        if self.cancelled.load(Ordering::Acquire) {
            return Err(LlmError::Provider(CANCELLED.to_string()));
        }
        result
    }
}

/// Only the newest search in a surface runs. [`begin`](Self::begin) cancels the one before it.
#[derive(Debug, Default)]
pub struct LatestSearch {
    current: Mutex<Option<Arc<AtomicBool>>>,
}

impl LatestSearch {
    pub const fn new() -> Self {
        Self { current: Mutex::new(None) }
    }

    /// A fresh cancel flag for a new search; the previous search's flag is raised.
    pub fn begin(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut current = self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = current.replace(flag.clone()) {
            previous.store(true, Ordering::Release);
        }
        flag
    }

    /// Cancel whatever is running (the user cleared the box or left the screen).
    pub fn cancel(&self) {
        let mut current = self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = current.take() {
            previous.store(true, Ordering::Release);
        }
    }
}

/// One [`LatestSearch`] per search surface (notch, Full UI window): a newer search or a cancel
/// on one surface never touches another surface's running search.
#[derive(Debug, Default)]
pub struct SurfaceSearches {
    surfaces: Mutex<std::collections::HashMap<String, LatestSearch>>,
}

impl SurfaceSearches {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, surface: &str) -> Arc<AtomicBool> {
        let mut map = self.surfaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(surface.to_string()).or_default().begin()
    }

    /// Cancel that surface's running search and forget the surface.
    pub fn cancel(&self, surface: &str) {
        let mut map = self.surfaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(latest) = map.remove(surface) {
            latest.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Blocks until `stop` is raised (or `max` passes), then answers `answer`.
    struct Slow {
        answer: &'static str,
        max: Duration,
        calls: AtomicUsize,
    }

    impl Slow {
        fn new(answer: &'static str, max: Duration) -> Self {
            Self { answer, max, calls: AtomicUsize::new(0) }
        }
    }

    impl CancellableModel for Slow {
        fn complete_split_cancellable(
            &self,
            _system: &str,
            _user: &str,
            stop: &Arc<AtomicBool>,
        ) -> Result<String, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let until = Instant::now() + self.max;
            while Instant::now() < until {
                if stop.load(Ordering::Acquire) {
                    return Ok(String::new());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(self.answer.to_string())
        }
    }

    fn model(max: Duration) -> CancellableSearchModel<Slow> {
        CancellableSearchModel {
            expand: Slow::new("expand", max),
            rerank: Slow::new("rerank", max),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn a_fast_answer_comes_back_from_the_steps_own_client() {
        let m = model(Duration::ZERO);
        let far = Instant::now() + Duration::from_secs(5);
        assert_eq!(m.complete(ModelStep::Expand, "s", "u", far).ok().as_deref(), Some("expand"));
        assert_eq!(m.complete(ModelStep::Rerank, "s", "u", far).ok().as_deref(), Some("rerank"));
        assert_eq!(m.expand.calls.load(Ordering::SeqCst), 1);
        assert_eq!(m.rerank.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_deadline_stops_the_request_instead_of_waiting_it_out() {
        let m = model(Duration::from_secs(10));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(60);
        let answer = m.complete(ModelStep::Expand, "s", "u", deadline);
        // Stopped, not waited for: well under the client's 10 s, and after the deadline, which is
        // what `refine` reads as `provider_timeout`.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(Instant::now() >= deadline);
        assert_eq!(answer.ok().as_deref(), Some(""));
    }

    #[test]
    fn cancelling_stops_the_request_and_refuses_further_calls() {
        let m = model(Duration::from_secs(10));
        let cancelled = m.cancelled.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            cancelled.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let far = started + Duration::from_secs(30);
        let first = m.complete(ModelStep::Expand, "s", "u", far);
        let _ = canceller.join();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(first, Err(LlmError::Provider(ref code)) if code == CANCELLED));
        // The search makes no further call once cancelled.
        let second = m.complete(ModelStep::Rerank, "s", "u", far);
        assert!(matches!(second, Err(LlmError::Provider(_))));
        assert_eq!(m.rerank.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_new_search_cancels_the_previous_one_only() {
        let latest = LatestSearch::new();
        let first = latest.begin();
        let second = latest.begin();
        assert!(first.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));
        latest.cancel();
        assert!(second.load(Ordering::Acquire));
        // Nothing running: cancelling again is a no-op, and the next search starts clean.
        latest.cancel();
        assert!(!latest.begin().load(Ordering::Acquire));
    }

    #[test]
    fn cancelling_one_surface_leaves_the_other_running() {
        let surfaces = SurfaceSearches::new();
        let notch = surfaces.begin("notch");
        let window = surfaces.begin("window");
        surfaces.cancel("notch");
        assert!(notch.load(Ordering::Acquire));
        assert!(!window.load(Ordering::Acquire));
        // A newer search supersedes only within its own surface.
        let newer = surfaces.begin("window");
        assert!(window.load(Ordering::Acquire));
        assert!(!newer.load(Ordering::Acquire));
        // Cancelling a surface that is not running is a no-op.
        surfaces.cancel("notch");
        assert!(!newer.load(Ordering::Acquire));
    }
}
