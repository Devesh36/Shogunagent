//! Frontmost-app schedule for the context cache (spec §3.10, NFR-SLO-05 ≤300ms).
//!
//! The macOS adapter polls `NSWorkspace.frontmostApplication` (cheap, no AX) and runs the
//! bounded walk only when this scheduler says so. Two rules for the adapter:
//!
//! 1. The pid poll stays small: a 400ms poll blows the 300ms cache-update budget before the walk
//!    starts.
//! 2. An empty first Chrome walk never `thread::sleep(500ms)`s on the same thread: that drops the
//!    next app switch (VS Code / Discord) for half a second.
//!
//! Walk cost is separately timeboxed ([`WALK_TIMEBOX`]). Poll must stay a small remainder of the
//! [`CACHE_UPDATE_SLO`]. Both budgets live here so the adapter and the SLO test read the same
//! numbers — a literal in either place can drift from the other without anything noticing.

use std::time::{Duration, Instant};

/// NFR-SLO-05: a focus switch must reach the context cache within this.
pub const CACHE_UPDATE_SLO: Duration = Duration::from_millis(300);

/// Cheap NSWorkspace poll. Worst-case wait + [`WALK_TIMEBOX`] must still fit [`CACHE_UPDATE_SLO`].
pub const FRONTMOST_POLL: Duration = Duration::from_millis(50);

/// Ceiling on one bounded AX walk, passed to the adapter's `should_stop`. The remainder of the
/// SLO after the worst-case poll wait — NOT an independent number. A walk that hits it returns
/// what it has with `partial = true`, which is the intended degradation: a late-but-complete
/// cache update misses the switch the user already made.
pub const WALK_TIMEBOX: Duration = Duration::from_millis(250);

/// Tab switches do not change pid — re-walk the same app on this cadence.
pub const CONTENT_REFRESH: Duration = Duration::from_secs(2);

/// Chrome/Safari often expose an empty AX tree on the first query after activation.
pub const EMPTY_TREE_RETRY: Duration = Duration::from_millis(500);

/// How often the adapter reads the focused window's identity (issue #355): a window or tab
/// switch inside one app does not change the pid, so without this it waited for
/// [`CONTENT_REFRESH`]. One AX title read per check — the walk runs only when it changed.
pub const WINDOW_CHECK: Duration = Duration::from_millis(250);

/// Why this tick should walk. The adapter maps onto harness `CacheTrigger`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusWalkReason {
    AppSwitch,
    ContentRefresh,
    EmptyRetry,
    /// Same app, different focused window or tab (issue #355).
    WindowSwitch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusWalk {
    pub pid: i32,
    pub reason: FocusWalkReason,
}

/// Pid-edge + refresh + empty-retry scheduler. No threads, no AX — unit-testable off a Mac.
#[derive(Debug, Default)]
pub struct FocusWatch {
    last_pid: Option<i32>,
    last_walk_at: Option<Instant>,
    retry_pid: Option<i32>,
    retry_at: Option<Instant>,
    /// The focused window last seen for `last_pid`, as an opaque key (a hash — never the title).
    last_window: Option<u64>,
    last_window_check_at: Option<Instant>,
}

impl FocusWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect the current frontmost pid. `None` pid is "nothing focused" (sleep, login screen)
    /// and clears the edge so the next real pid is an [`FocusWalkReason::AppSwitch`].
    pub fn on_frontmost(&mut self, pid: Option<i32>, now: Instant) -> Option<FocusWalk> {
        let Some(pid) = pid else {
            self.last_pid = None;
            self.retry_pid = None;
            self.retry_at = None;
            return None;
        };

        if self.last_pid != Some(pid) {
            self.last_pid = Some(pid);
            self.last_walk_at = Some(now);
            // The new app's window is recorded by the next window check, not treated as a switch.
            self.last_window = None;
            self.retry_pid = None;
            self.retry_at = None;
            return Some(FocusWalk {
                pid,
                reason: FocusWalkReason::AppSwitch,
            });
        }

        if self.retry_pid == Some(pid) {
            if let Some(at) = self.retry_at {
                if now >= at {
                    self.retry_pid = None;
                    self.retry_at = None;
                    self.last_walk_at = Some(now);
                    return Some(FocusWalk {
                        pid,
                        reason: FocusWalkReason::EmptyRetry,
                    });
                }
            }
        } else {
            self.retry_pid = None;
            self.retry_at = None;
        }

        let refresh_due = match self.last_walk_at {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= CONTENT_REFRESH,
        };
        if refresh_due {
            self.last_walk_at = Some(now);
            return Some(FocusWalk {
                pid,
                reason: FocusWalkReason::ContentRefresh,
            });
        }

        None
    }

    /// Whether the adapter should read the focused window's identity this tick.
    pub fn window_check_due(&mut self, now: Instant) -> bool {
        let due = match self.last_window_check_at {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= WINDOW_CHECK,
        };
        if due {
            self.last_window_check_at = Some(now);
        }
        due
    }

    /// The focused window of `pid` now has identity `window`. Returns a
    /// [`FocusWalkReason::WindowSwitch`] walk when it differs from the one last seen for the same
    /// app; the first sighting after an app switch only records it (the app switch already walked).
    pub fn on_window(&mut self, pid: i32, window: u64, now: Instant) -> Option<FocusWalk> {
        if self.last_pid != Some(pid) {
            return None;
        }
        let previous = self.last_window.replace(window);
        if previous.is_some_and(|w| w != window) {
            self.last_walk_at = Some(now);
            return Some(FocusWalk {
                pid,
                reason: FocusWalkReason::WindowSwitch,
            });
        }
        None
    }

    /// Record the window the walk for `pid` is about to read (call right after an
    /// [`FocusWalkReason::AppSwitch`]). Without it the first window check after the switch can
    /// only record, so a window change in that gap would never trigger a walk.
    pub fn note_window(&mut self, pid: i32, window: u64) {
        if self.last_pid == Some(pid) {
            self.last_window = Some(window);
        }
    }

    /// Call after an [`FocusWalkReason::AppSwitch`] walk that succeeded with an empty tree.
    /// The next [`on_frontmost`] for the same pid fires [`FocusWalkReason::EmptyRetry`] once
    /// [`EMPTY_TREE_RETRY`] has elapsed — without blocking the poll loop.
    pub fn note_empty_ok(&mut self, pid: i32, now: Instant) {
        self.retry_pid = Some(pid);
        self.retry_at = Some(now + EMPTY_TREE_RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_switch_inside_one_app_walks_once() {
        let mut w = FocusWatch::new();
        let t = Instant::now();
        assert_eq!(w.on_frontmost(Some(7), t).map(|s| s.reason), Some(FocusWalkReason::AppSwitch));
        // First sighting after the app switch only records the window.
        assert_eq!(w.on_window(7, 1, t), None);
        assert_eq!(w.on_window(7, 1, t), None);
        let switched = w.on_window(7, 2, t + Duration::from_millis(300));
        assert_eq!(switched.map(|s| s.reason), Some(FocusWalkReason::WindowSwitch));
        assert_eq!(w.on_window(7, 2, t + Duration::from_millis(600)), None);
        // A window report for an app that is no longer frontmost is ignored.
        assert_eq!(w.on_window(8, 3, t), None);
        // A new app starts with no window on record.
        let _ = w.on_frontmost(Some(8), t + Duration::from_secs(1));
        assert_eq!(w.on_window(8, 9, t + Duration::from_secs(1)), None);
    }

    #[test]
    fn a_window_change_right_after_an_app_switch_still_walks() {
        let mut w = FocusWatch::new();
        let t = Instant::now();
        assert_eq!(w.on_frontmost(Some(7), t).map(|s| s.reason), Some(FocusWalkReason::AppSwitch));
        w.note_window(7, 1);
        // The first window check already sees a different window than the walk read.
        let switched = w.on_window(7, 2, t + WINDOW_CHECK);
        assert_eq!(switched.map(|s| s.reason), Some(FocusWalkReason::WindowSwitch));
    }

    #[test]
    fn window_checks_are_rate_limited() {
        let mut w = FocusWatch::new();
        let t = Instant::now();
        assert!(w.window_check_due(t));
        assert!(!w.window_check_due(t + FRONTMOST_POLL));
        assert!(w.window_check_due(t + WINDOW_CHECK));
    }

    fn t0() -> Instant {
        Instant::now()
    }

    /// The budget, over the constants the adapter actually uses. Reading `WALK_TIMEBOX` is the
    /// point: the number the walk is given has to be the number this arithmetic is about, not a
    /// literal in the test that can drift from the adapter's.
    #[test]
    fn poll_plus_walk_timebox_fits_the_cache_update_slo() {
        assert!(
            FRONTMOST_POLL + WALK_TIMEBOX <= CACHE_UPDATE_SLO,
            "poll {FRONTMOST_POLL:?} + walk {WALK_TIMEBOX:?} exceeds {CACHE_UPDATE_SLO:?}"
        );
    }

    #[test]
    fn app_switch_walks_immediately() {
        let mut w = FocusWatch::new();
        let t = t0();
        assert_eq!(
            w.on_frontmost(Some(1), t),
            Some(FocusWalk {
                pid: 1,
                reason: FocusWalkReason::AppSwitch
            })
        );
        assert_eq!(w.on_frontmost(Some(1), t + FRONTMOST_POLL), None);
        assert_eq!(
            w.on_frontmost(Some(2), t + FRONTMOST_POLL * 2),
            Some(FocusWalk {
                pid: 2,
                reason: FocusWalkReason::AppSwitch
            })
        );
    }

    #[test]
    fn empty_chrome_retry_does_not_block_a_switch_to_discord() {
        let mut w = FocusWatch::new();
        let t = t0();
        assert_eq!(
            w.on_frontmost(Some(10), t).map(|s| s.reason),
            Some(FocusWalkReason::AppSwitch)
        );
        w.note_empty_ok(10, t);
        // 200ms later the user is already in Discord — must walk now, not wait out the 500ms.
        let switched = w.on_frontmost(Some(20), t + Duration::from_millis(200));
        assert_eq!(
            switched,
            Some(FocusWalk {
                pid: 20,
                reason: FocusWalkReason::AppSwitch
            })
        );
        // The stale Chrome retry must not fire later.
        assert_eq!(
            w.on_frontmost(Some(20), t + EMPTY_TREE_RETRY + Duration::from_millis(1))
                .map(|s| s.reason),
            None
        );
    }

    #[test]
    fn empty_tree_retries_same_pid_after_the_delay() {
        let mut w = FocusWatch::new();
        let t = t0();
        w.on_frontmost(Some(10), t);
        w.note_empty_ok(10, t);
        assert_eq!(
            w.on_frontmost(Some(10), t + EMPTY_TREE_RETRY - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            w.on_frontmost(Some(10), t + EMPTY_TREE_RETRY),
            Some(FocusWalk {
                pid: 10,
                reason: FocusWalkReason::EmptyRetry
            })
        );
        // One-shot.
        assert_eq!(
            w.on_frontmost(Some(10), t + EMPTY_TREE_RETRY + FRONTMOST_POLL),
            None
        );
    }

    #[test]
    fn same_pid_rewalks_on_the_content_refresh_cadence() {
        let mut w = FocusWatch::new();
        let t = t0();
        w.on_frontmost(Some(7), t);
        assert_eq!(
            w.on_frontmost(Some(7), t + CONTENT_REFRESH - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            w.on_frontmost(Some(7), t + CONTENT_REFRESH),
            Some(FocusWalk {
                pid: 7,
                reason: FocusWalkReason::ContentRefresh
            })
        );
    }

    #[test]
    fn lost_frontmost_then_same_pid_is_an_app_switch() {
        let mut w = FocusWatch::new();
        let t = t0();
        w.on_frontmost(Some(3), t);
        assert_eq!(w.on_frontmost(None, t + FRONTMOST_POLL), None);
        assert_eq!(
            w.on_frontmost(Some(3), t + FRONTMOST_POLL * 2),
            Some(FocusWalk {
                pid: 3,
                reason: FocusWalkReason::AppSwitch
            })
        );
    }
}
