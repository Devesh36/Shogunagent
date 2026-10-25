//! Update checks are staggered; an automatic relaunch belongs only to the half hour after a
//! morning wake, so someone who unlocks and starts working still gives it an idle moment.

pub const MINUTE_MS: u64 = 60_000;
pub const HOUR_MS: u64 = 60 * MINUTE_MS;
const MORNING_WINDOW_MS: u64 = 30 * MINUTE_MS;

#[derive(Debug)]
pub struct Schedule {
    pub next_check_ms: u64,
    sleep_at_ms: Option<u64>,
    morning_wake_ms: Option<u64>,
    morning_day: Option<i32>,
    seed: u64,
}

impl Schedule {
    pub fn new(now_ms: u64, seed: u64) -> Self {
        Self {
            // FR-SVC-01: spread first checks across 30 minutes, never check on launch.
            next_check_ms: now_ms.saturating_add(1 + seed % (30 * MINUTE_MS)),
            sleep_at_ms: None,
            morning_wake_ms: None,
            morning_day: None,
            seed,
        }
    }

    pub fn checked(&mut self, now_ms: u64, failed: bool) {
        self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let delay = if failed {
            // Offline/signature/install errors retry later, without a tight download loop.
            30 * MINUTE_MS + self.seed % (30 * MINUTE_MS)
        } else {
            6 * HOUR_MS - 30 * MINUTE_MS + self.seed % (60 * MINUTE_MS)
        };
        self.next_check_ms = now_ms.saturating_add(delay);
    }

    pub fn will_sleep(&mut self, now_ms: u64) {
        self.sleep_at_ms = Some(now_ms);
        self.morning_wake_ms = None;
    }

    pub fn did_wake(&mut self, now_ms: u64, local_day: i32, local_hour: u32) {
        let slept = self
            .sleep_at_ms
            .take()
            .and_then(|at| now_ms.checked_sub(at));
        if slept.is_some_and(|duration| duration >= 3 * HOUR_MS)
            && (5..12).contains(&local_hour)
            && self.morning_day != Some(local_day)
        {
            self.morning_day = Some(local_day);
            self.morning_wake_ms = Some(now_ms);
        }
    }

    pub fn can_restart(&self, now_ms: u64, ready_at_ms: u64, safe: bool) -> bool {
        safe && self.morning_wake_ms.is_some_and(|wake| {
            ready_at_ms <= wake
                && now_ms
                    .checked_sub(wake)
                    .is_some_and(|age| age <= MORNING_WINDOW_MS)
        })
    }

    pub fn defer_restart(&mut self) {
        self.morning_wake_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn morning() -> (Schedule, u64) {
        let mut schedule = Schedule::new(0, 42);
        schedule.will_sleep(22 * HOUR_MS);
        let wake = 32 * HOUR_MS;
        schedule.did_wake(wake, 2026300, 8);
        (schedule, wake)
    }

    #[test]
    fn downloads_check_with_launch_and_periodic_jitter() {
        for seed in [0, 1, 42, u64::MAX] {
            let mut schedule = Schedule::new(100, seed);
            assert!(schedule.next_check_ms > 100);
            assert!(schedule.next_check_ms <= 100 + 30 * MINUTE_MS);
            schedule.checked(HOUR_MS, false);
            assert!((HOUR_MS + 330 * MINUTE_MS..HOUR_MS + 390 * MINUTE_MS)
                .contains(&schedule.next_check_ms));
            schedule.checked(HOUR_MS, true);
            assert!((HOUR_MS + 30 * MINUTE_MS..HOUR_MS + HOUR_MS).contains(&schedule.next_check_ms));
        }
    }

    #[test]
    fn a_ready_update_restarts_at_morning_wake_only_when_safe() {
        let (schedule, wake) = morning();
        assert!(schedule.can_restart(wake, wake - HOUR_MS, true));
        assert!(!schedule.can_restart(wake, wake - HOUR_MS, false));
        assert!(schedule.can_restart(wake + MINUTE_MS, wake - HOUR_MS, true));
        assert!(schedule.can_restart(wake + 30 * MINUTE_MS, wake - HOUR_MS, true));
        assert!(!schedule.can_restart(wake + 31 * MINUTE_MS, wake - HOUR_MS, true));
    }

    #[test]
    fn a_download_finishing_after_wake_waits_until_tomorrow() {
        let (schedule, wake) = morning();
        assert!(!schedule.can_restart(wake + MINUTE_MS, wake + 1, true));
    }

    #[test]
    fn launch_short_sleep_and_afternoon_wake_do_not_restart() {
        let mut schedule = Schedule::new(0, 42);
        assert!(!schedule.can_restart(HOUR_MS, 0, true));
        for (duration, hour) in [(HOUR_MS, 8), (10 * HOUR_MS, 14), (10 * HOUR_MS, 3)] {
            schedule.will_sleep(HOUR_MS);
            schedule.did_wake(HOUR_MS + duration, 1, hour);
            assert!(!schedule.can_restart(HOUR_MS + duration, 0, true));
        }
    }

    #[test]
    fn failed_restart_and_second_wake_same_day_do_not_loop() {
        let (mut schedule, wake) = morning();
        schedule.defer_restart();
        assert!(!schedule.can_restart(wake, 0, true));
        schedule.will_sleep(wake);
        schedule.did_wake(wake + 3 * HOUR_MS, 2026300, 11);
        assert!(!schedule.can_restart(wake + 3 * HOUR_MS, 0, true));
        schedule.will_sleep(wake + 14 * HOUR_MS);
        schedule.did_wake(wake + 24 * HOUR_MS, 2026301, 8);
        assert!(schedule.can_restart(wake + 24 * HOUR_MS, 0, true));
    }

    #[test]
    fn clock_going_backwards_never_triggers_a_restart() {
        let (schedule, wake) = morning();
        assert!(!schedule.can_restart(wake - 1, 0, true));
        let mut schedule = Schedule::new(0, 0);
        schedule.will_sleep(10 * HOUR_MS);
        schedule.did_wake(2 * HOUR_MS, 1, 8);
        assert!(!schedule.can_restart(2 * HOUR_MS, 0, true));
    }
}
