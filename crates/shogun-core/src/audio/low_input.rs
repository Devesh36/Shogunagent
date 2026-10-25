//! "Your microphone is very quiet" (docs/meeting-gap-analysis-2026-09.md D4).
//!
//! The meeting lane already computes a content-free RMS per frame for the waveform. This
//! detector watches that number over time and says when the input has been effectively silent
//! for a whole window — the case where the user is talking into a muted or wrong device and
//! would otherwise find out at the Recap. Pure state machine: the shell feeds it levels and
//! forwards its transitions to the overlay; nothing here touches audio or the disk.

/// Below this RMS a frame carries no usable speech. Matches the waveform's active floor.
pub const DEFAULT_THRESHOLD: f32 = 0.01;

/// How long the input must stay below the threshold before the warning shows. Long enough to
/// ride out a natural pause (someone else talking on the far side is captured by the system
/// tap, not the mic — so the mic *is* quiet while the user listens), short enough to be useful.
pub const DEFAULT_WINDOW_MS: i64 = 45_000;

#[derive(Debug, Clone)]
pub struct LowInputDetector {
    threshold: f32,
    window_ms: i64,
    /// The last time a frame carried sound, or the time observation started.
    last_sound_at: Option<i64>,
    low: bool,
}

impl Default for LowInputDetector {
    fn default() -> Self {
        Self::new(DEFAULT_THRESHOLD, DEFAULT_WINDOW_MS)
    }
}

impl LowInputDetector {
    pub fn new(threshold: f32, window_ms: i64) -> Self {
        Self {
            threshold,
            window_ms: window_ms.max(1),
            last_sound_at: None,
            low: false,
        }
    }

    /// Feed one frame's RMS. Returns `Some(true)` on the transition into the low state,
    /// `Some(false)` when sound returns, and `None` while nothing changes. The first frame
    /// starts the clock: a quiet start is not reported until a full window has passed.
    pub fn observe(&mut self, rms: f32, now_ms: i64) -> Option<bool> {
        if rms > self.threshold {
            self.last_sound_at = Some(now_ms);
            if self.low {
                self.low = false;
                return Some(false);
            }
            return None;
        }
        let since = match self.last_sound_at {
            Some(at) => at,
            None => {
                self.last_sound_at = Some(now_ms);
                return None;
            }
        };
        if !self.low && now_ms.saturating_sub(since) >= self.window_ms {
            self.low = true;
            return Some(true);
        }
        None
    }

    /// Whether the input is currently judged low.
    pub fn is_low(&self) -> bool {
        self.low
    }

    /// Forget the streak — on pause, device change, or a new session — without reporting a
    /// transition. The next window starts from the next frame.
    pub fn reset(&mut self) {
        self.last_sound_at = None;
        self.low = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_window_reports_low_once_and_sound_clears_it() {
        let mut d = LowInputDetector::new(0.01, 10_000);
        assert_eq!(d.observe(0.0, 0), None, "first frame only starts the clock");
        assert_eq!(d.observe(0.0, 5_000), None);
        assert_eq!(d.observe(0.0, 10_000), Some(true));
        assert!(d.is_low());
        assert_eq!(
            d.observe(0.0, 20_000),
            None,
            "reported once, not every frame"
        );
        assert_eq!(d.observe(0.2, 21_000), Some(false));
        assert!(!d.is_low());
        assert_eq!(d.observe(0.2, 22_000), None);
    }

    #[test]
    fn sound_inside_the_window_keeps_it_from_firing() {
        let mut d = LowInputDetector::new(0.01, 10_000);
        d.observe(0.0, 0);
        d.observe(0.05, 6_000);
        assert_eq!(
            d.observe(0.0, 12_000),
            None,
            "only 6 s since the last sound"
        );
        assert_eq!(d.observe(0.0, 16_000), Some(true));
    }

    #[test]
    fn reset_forgets_the_streak_silently() {
        let mut d = LowInputDetector::new(0.01, 10_000);
        d.observe(0.0, 0);
        d.observe(0.0, 10_000);
        assert!(d.is_low());
        d.reset();
        assert!(!d.is_low());
        assert_eq!(d.observe(0.0, 11_000), None, "clock restarts after reset");
        assert_eq!(d.observe(0.0, 21_000), Some(true));
    }

    #[test]
    fn the_threshold_is_the_waveform_floor() {
        let mut d = LowInputDetector::default();
        assert_eq!(
            d.observe(DEFAULT_THRESHOLD, 0),
            None,
            "exactly the floor is still silence"
        );
        assert_eq!(d.observe(0.0, DEFAULT_WINDOW_MS), Some(true));
    }
}
