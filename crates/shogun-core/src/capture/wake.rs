//! Focus-driven wake for the capture poller (issue #355, NFR-SLO-05 ≤300ms).
//!
//! The poller's reliable floor is a fixed interval (2s — AXObserver focus pushes are flaky across
//! apps), which on its own means a focus switch can wait up to a whole interval before anything
//! downstream (screen context, reply context, pre-assembled actions) sees the new window. The
//! macOS adapter already receives `NSWorkspaceDidActivateApplicationNotification`; this lets that
//! observer cut the poller's sleep short so the switch is captured *now*.
//!
//! Two rules keep it from costing idle CPU:
//! - A wake is a single pending flag, not a queue: ten notifications during one sleep are one
//!   early tick.
//! - An early tick never starts sooner than [`FOCUS_WAKE_MIN_GAP`] after the previous tick
//!   began, so a burst of ⌘-Tab presses is rate-limited instead of becoming a walk loop.
//!
//! Pure std (Mutex + Condvar), time-injected where it matters, unit-tested off a Mac.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Minimum spacing between a tick and a focus-woken tick. Small against the 300ms cache-update
/// budget; large enough that holding ⌘-Tab cannot spin the AX walk.
pub const FOCUS_WAKE_MIN_GAP: Duration = Duration::from_millis(200);

/// Why [`CaptureWake::wait`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// The regular poll interval elapsed.
    Interval,
    /// A focus change asked for an immediate capture.
    Focus,
}

/// A cloneable wake handle shared by the poller (waits) and the focus observer (notifies).
#[derive(Clone, Debug, Default)]
pub struct CaptureWake {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl CaptureWake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the poller to capture now. Never blocks beyond a lock handoff — safe on the main
    /// thread from a notification block. A poisoned lock is recovered: the flag is a bool.
    pub fn notify(&self) {
        let (lock, cvar) = &*self.inner;
        let mut pending = match lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *pending = true;
        cvar.notify_all();
    }

    /// Whether a focus wake is waiting to be consumed.
    pub fn is_pending(&self) -> bool {
        let (lock, _) = &*self.inner;
        match lock.lock() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    /// Sleep until `last_tick + interval`, or until a focus wake arrives — but never return for a
    /// wake earlier than `last_tick + min_gap`. Consumes the pending flag either way (the tick
    /// that is about to run covers whatever the wake asked for).
    pub fn wait(&self, last_tick: Instant, interval: Duration, min_gap: Duration) -> Woke {
        let (lock, cvar) = &*self.inner;
        let mut pending = match lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let due = last_tick + interval;
        let earliest_wake = last_tick + min_gap.min(interval);
        loop {
            let now = Instant::now();
            if *pending && now >= earliest_wake {
                *pending = false;
                return Woke::Focus;
            }
            if now >= due {
                *pending = false;
                return Woke::Interval;
            }
            let until = if *pending { earliest_wake } else { due };
            let timeout = until.saturating_duration_since(now);
            pending = match cvar.wait_timeout(pending, timeout) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
    }
}

/// Dwell to credit a tick with: the wall time since the previous tick, capped at the regular
/// interval (a long gap is a stalled or sleeping machine, not attention). An early, focus-woken
/// tick therefore credits only the time that actually passed, so dwell totals still add up to
/// wall time instead of gaining a full interval per ⌘-Tab.
pub fn tick_dwell(previous_tick: Option<Instant>, now: Instant, interval: Duration) -> Duration {
    match previous_tick {
        Some(prev) => now.saturating_duration_since(prev).min(interval),
        None => interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(400);
    const GAP: Duration = Duration::from_millis(40);

    #[test]
    fn without_a_wake_the_interval_elapses() {
        let wake = CaptureWake::new();
        let start = Instant::now();
        assert_eq!(wake.wait(start, INTERVAL, GAP), Woke::Interval);
        assert!(start.elapsed() >= INTERVAL);
    }

    #[test]
    fn a_focus_wake_cuts_the_sleep_short() {
        let wake = CaptureWake::new();
        let start = Instant::now();
        let notifier = wake.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            notifier.notify();
        });
        assert_eq!(wake.wait(start, Duration::from_secs(10), GAP), Woke::Focus);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(t.join().is_ok());
        assert!(!wake.is_pending(), "the tick consumes the wake");
    }

    #[test]
    fn an_early_wake_still_respects_the_minimum_gap() {
        let wake = CaptureWake::new();
        wake.notify();
        let start = Instant::now();
        assert_eq!(wake.wait(start, INTERVAL, GAP), Woke::Focus);
        assert!(start.elapsed() >= GAP);
        assert!(start.elapsed() < INTERVAL);
    }

    #[test]
    fn a_burst_of_wakes_is_one_early_tick() {
        let wake = CaptureWake::new();
        for _ in 0..10 {
            wake.notify();
        }
        let start = Instant::now();
        assert_eq!(wake.wait(start, INTERVAL, GAP), Woke::Focus);
        // The flag was consumed: the next wait runs to its interval.
        let next = Instant::now();
        assert_eq!(wake.wait(next, INTERVAL, GAP), Woke::Interval);
    }

    #[test]
    fn a_wake_older_than_the_gap_fires_immediately() {
        let wake = CaptureWake::new();
        let long_ago = Instant::now() - Duration::from_secs(1);
        wake.notify();
        let start = Instant::now();
        // last_tick + gap is already in the past; interval is also past → the wake wins
        // because it is checked first, and either way nothing sleeps.
        assert_eq!(wake.wait(long_ago, INTERVAL, GAP), Woke::Focus);
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn dwell_is_elapsed_time_capped_at_the_interval() {
        let now = Instant::now();
        let interval = Duration::from_secs(2);
        assert_eq!(tick_dwell(None, now, interval), interval);
        assert_eq!(
            tick_dwell(Some(now - Duration::from_millis(300)), now, interval),
            Duration::from_millis(300)
        );
        assert_eq!(
            tick_dwell(Some(now - Duration::from_secs(60)), now, interval),
            interval
        );
    }
}
