//! Hourly Batch derivation (#1066 part B, owner decision 2026-10-12). Pure: the clock math, the
//! window ids and the run/skip decision. The effects are the existing frozen-cycle machinery in
//! [`super::schedule`], driven once per local clock hour instead of once per night.
//!
//! - **Window.** The hour that just closed. A cycle consumes `[cursor, floor_hour(now))`, where the
//!   cursor is the high-water mark of the last completed consolidation, so a missed hour (asleep,
//!   offline, relay down) is simply covered by the next cycle, in order, and never twice.
//! - **Id.** `YYYYMMDD-hHH` (local). Cycle ids are the idempotency key: a second tick in the same
//!   hour finds the cycle and does nothing; freezing is `ON CONFLICT DO NOTHING`.
//! - **Lane.** Select KK / relay Batch only (invariant 5). No Batch credential means no hourly
//!   cycle at all: the nightly cycle keeps the local lane as the fallback.
//! - **Conditions.** Power only (the FR-DC-01 battery rule). Not idle: the work is a bounded
//!   local pass plus an asynchronous provider job, and waiting for 15 idle minutes would starve
//!   the hourly cadence on a busy day.

use super::schedule::{local_time, DEFAULT_LOOKBACK_MS};

const HOUR_MS: i64 = 3_600_000;

/// The hourly cycle id for the local hour containing `unix_secs`: `YYYYMMDD-hHH`.
pub fn hour_cycle_id(unix_secs: i64, gmt_offset_secs: i32) -> String {
    let t = local_time(unix_secs, gmt_offset_secs);
    format!("{:08}-h{:02}", t.yyyymmdd, t.hour)
}

/// Start of the local clock hour containing `now_ms` (the end of the hour that just closed).
pub fn window_end_ms(now_ms: i64, gmt_offset_secs: i32) -> i64 {
    let off = i64::from(gmt_offset_secs) * 1000;
    (now_ms + off).div_euclid(HOUR_MS) * HOUR_MS - off
}

/// The event window `[from, to)` an hourly cycle freezes. `last_consolidated_to` is the cursor;
/// `None` (first run) falls back to one day, like the nightly cycle. Never inverted.
pub fn hourly_range(last_consolidated_to: Option<i64>, now_ms: i64, gmt_offset_secs: i32) -> (i64, i64) {
    let to = window_end_ms(now_ms, gmt_offset_secs);
    let from = last_consolidated_to.unwrap_or(to - DEFAULT_LOOKBACK_MS);
    (from.min(to), to)
}

/// Earliest and latest second into the hour a device may submit (minute 5 to minute 45).
pub const SLOT_MIN_SECS: u32 = 5 * 60;
pub const SLOT_SPAN_SECS: u32 = 40 * 60;

/// This device's stable submit offset into each hour, so Macs do not all reach the relay at :00
/// (the hourly twin of the nightly `submit_slot_secs`, FR-SVC-01): xxh64 of the anonymous device
/// id, fixed seed. An empty id (no licence yet, no shared service to protect) gets the minimum.
pub fn hourly_slot_secs(device_id: &str) -> u32 {
    use std::hash::Hasher;
    let id = device_id.trim();
    if id.is_empty() {
        return SLOT_MIN_SECS;
    }
    let mut h = twox_hash::XxHash64::with_seed(0);
    h.write(id.as_bytes());
    SLOT_MIN_SECS + (h.finish() % u64::from(SLOT_SPAN_SECS)) as u32
}

/// Whether `now_ms` is still before this device's slot in the current local hour. Only the start
/// is delayed; the window and cursor are untouched, so catch-up is unaffected.
pub fn before_slot(now_ms: i64, gmt_offset_secs: i32, slot_secs: u32) -> bool {
    now_ms - window_end_ms(now_ms, gmt_offset_secs) < i64::from(slot_secs) * 1000
}

/// Wait before the next attempt after `fails` consecutive failures: 5, 10, 20, 40, then 60 minutes.
/// The one retry policy for hourly and backfill windows, keyed on the cycle row's `failure_count`
/// and `updated_at` (see [`in_backoff`]).
pub fn retry_backoff_ms(fails: u32) -> i64 {
    ((5 * 60_000_i64) << fails.saturating_sub(1).min(4)).min(HOUR_MS)
}

/// Whether a failed window (`failure_count`, last `updated_at`) must still wait before retrying.
pub fn in_backoff(failure_count: i64, updated_at_ms: i64, now_ms: i64) -> bool {
    failure_count > 0
        && now_ms < updated_at_ms + retry_backoff_ms(u32::try_from(failure_count).unwrap_or(u32::MAX))
}

/// Whether a nightly cycle's own run holds the Batch lane right now: running under a live lease.
/// Only then does an hourly tick wait. A failed, parked (unknown submission) or abandoned night,
/// or one whose lease lapsed while the Mac slept, never blocks the hourly reviews.
pub fn nightly_holds_lane(is_hourly: bool, running: bool, lease_until: Option<i64>, now_ms: i64) -> bool {
    !is_hourly && running && lease_until.is_some_and(|until| until > now_ms)
}

/// What the driver observed before deciding.
#[derive(Debug, Clone, Copy)]
pub struct HourlyInputs {
    /// The Batch lane is provisioned (licence or running trial).
    pub batch_provisioned: bool,
    pub power_connected: bool,
    pub battery_pct: u8,
    /// This hour's cycle id already completed.
    pub done_this_hour: bool,
    /// An older failed window is still inside its backoff ([`retry_backoff_ms`] of its stored
    /// failure count). A brand-new hour is never held back by it.
    pub attempted_this_hour: bool,
    /// A Full cycle is unfinished (submitted and awaiting results, or resumable).
    pub unfinished_cycle: bool,
    /// Batch-eligible events exist in `[cursor, floor_hour(now))`.
    pub new_material: bool,
    /// Still before this device's slot in the hour ([`before_slot`]).
    pub before_slot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HourlySkip {
    BeforeSlot,
    NoBatchLane,
    InsufficientPower,
    AlreadyAttempted,
    AlreadyDone,
    NothingNew,
}

/// Run now, or why not. An unfinished cycle always resumes (its provider job is already paid for);
/// a new window needs new material, so an idle hour sends nothing — no empty Batches.
pub fn decide(i: &HourlyInputs) -> Result<(), HourlySkip> {
    if !i.batch_provisioned {
        return Err(HourlySkip::NoBatchLane);
    }
    if !(i.power_connected || i.battery_pct >= super::gate::BATTERY_FLOOR_PCT) {
        return Err(HourlySkip::InsufficientPower);
    }
    if i.before_slot {
        return Err(HourlySkip::BeforeSlot);
    }
    if i.attempted_this_hour {
        return Err(HourlySkip::AlreadyAttempted);
    }
    if i.unfinished_cycle {
        return Ok(());
    }
    if i.done_this_hour && !i.new_material {
        return Err(HourlySkip::AlreadyDone);
    }
    if !i.new_material {
        return Err(HourlySkip::NothingNew);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_792_000_000; // an arbitrary instant

    #[test]
    fn only_a_live_nightly_run_holds_the_hourly_lane() {
        let now = 1_000_000;
        assert!(nightly_holds_lane(false, true, Some(now + 1), now), "a live nightly run waits");
        assert!(!nightly_holds_lane(false, true, Some(now), now), "a lease that lapsed in sleep does not");
        assert!(!nightly_holds_lane(false, false, None, now), "a failed or parked night does not");
        assert!(!nightly_holds_lane(true, true, Some(now + 1), now), "an hourly window is the lane's own");
    }

    fn ready() -> HourlyInputs {
        HourlyInputs {
            batch_provisioned: true,
            power_connected: true,
            battery_pct: 100,
            done_this_hour: false,
            attempted_this_hour: false,
            unfinished_cycle: false,
            new_material: true,
            before_slot: false,
        }
    }

    #[test]
    fn retry_backoff_doubles_to_an_hour_and_stays() {
        let mins: Vec<i64> = (1..=8).map(|n| retry_backoff_ms(n) / 60_000).collect();
        assert_eq!(mins, [5, 10, 20, 40, 60, 60, 60, 60]);
        assert!(!in_backoff(0, 0, 1), "never failed: no wait");
        assert!(in_backoff(2, 1_000, 1_000 + 9 * 60_000));
        assert!(!in_backoff(2, 1_000, 1_000 + 10 * 60_000));
    }

    #[test]
    fn ids_follow_the_local_hour_and_are_stable_within_it() {
        let a = hour_cycle_id(T, 0);
        assert_eq!(a.len(), 12);
        assert!(shogun_memory::jobs::is_hourly_cycle_id(&a));
        assert_eq!(a, hour_cycle_id(T + 5, 0), "same hour, same id");
        assert_ne!(a, hour_cycle_id(T + 3_600, 0));
        // 5h30 east (India): the local hour differs from UTC's but is still stable in itself.
        assert_eq!(hour_cycle_id(T, 19_800), hour_cycle_id(T + 60, 19_800));
    }

    #[test]
    fn window_ends_on_the_local_hour_boundary_even_in_half_hour_zones() {
        for off in [0, 19_800, -25_200, 34_200] {
            let end = window_end_ms(T * 1000 + 1_234_567, off);
            assert_eq!((end + i64::from(off) * 1000).rem_euclid(HOUR_MS), 0);
            assert!(end <= T * 1000 + 1_234_567 && T * 1000 + 1_234_567 - end < HOUR_MS);
        }
    }

    #[test]
    fn missed_hours_are_one_ordered_contiguous_window_from_the_cursor() {
        let now = T * 1000;
        let end = window_end_ms(now, 0);
        let cursor = end - 5 * HOUR_MS; // five hours missed
        assert_eq!(hourly_range(Some(cursor), now, 0), (cursor, end));
        // The next cycle starts exactly where this one ended: no gap, no overlap.
        let later = now + HOUR_MS;
        assert_eq!(hourly_range(Some(end), later, 0).0, end);
        // First run: bounded lookback; skewed clock never inverts.
        assert_eq!(hourly_range(None, now, 0), (end - DEFAULT_LOOKBACK_MS, end));
        assert_eq!(hourly_range(Some(end + HOUR_MS), now, 0), (end, end));
    }

    #[test]
    fn slot_is_stable_per_device_and_inside_the_hour_bounds() {
        let mut seen = std::collections::HashSet::new();
        for n in 0..200 {
            let id = format!("device-{n}");
            let slot = hourly_slot_secs(&id);
            assert_eq!(slot, hourly_slot_secs(&id), "stable");
            assert!((SLOT_MIN_SECS..SLOT_MIN_SECS + SLOT_SPAN_SECS).contains(&slot));
            seen.insert(slot);
        }
        assert!(seen.len() > 100, "devices spread out, not clustered");
        assert_eq!(hourly_slot_secs("  "), SLOT_MIN_SECS);
    }

    #[test]
    fn submission_waits_for_the_slot_and_catch_up_is_untouched() {
        let now = T * 1000;
        let end = window_end_ms(now, 0);
        let slot = hourly_slot_secs("device-a");
        assert!(before_slot(end + 1_000, 0, slot));
        assert!(!before_slot(end + i64::from(slot) * 1000, 0, slot));
        assert_eq!(decide(&HourlyInputs { before_slot: true, ..ready() }), Err(HourlySkip::BeforeSlot));
        // The range is the same whenever in the hour the run starts: missed hours stay in order.
        let cursor = end - 4 * HOUR_MS;
        assert_eq!(hourly_range(Some(cursor), end + 1_000, 0), hourly_range(Some(cursor), end + 2_000_000, 0));
    }

    #[test]
    fn no_batch_lane_means_no_hourly_cycle() {
        let i = HourlyInputs { batch_provisioned: false, ..ready() };
        assert_eq!(decide(&i), Err(HourlySkip::NoBatchLane));
    }

    #[test]
    fn nothing_new_sends_nothing() {
        let i = HourlyInputs { new_material: false, ..ready() };
        assert_eq!(decide(&i), Err(HourlySkip::NothingNew));
    }

    #[test]
    fn a_window_runs_once_per_hour() {
        assert_eq!(decide(&ready()), Ok(()));
        assert_eq!(decide(&HourlyInputs { done_this_hour: true, new_material: false, ..ready() }), Err(HourlySkip::AlreadyDone));
        // A window split for the item ceiling leaves material in the same hour: keep draining.
        assert_eq!(decide(&HourlyInputs { done_this_hour: true, ..ready() }), Ok(()));
        assert_eq!(
            decide(&HourlyInputs { attempted_this_hour: true, ..ready() }),
            Err(HourlySkip::AlreadyAttempted)
        );
    }

    #[test]
    fn an_unfinished_cycle_resumes_even_with_nothing_new() {
        let i = HourlyInputs { unfinished_cycle: true, new_material: false, done_this_hour: true, ..ready() };
        assert_eq!(decide(&i), Ok(()));
    }

    #[test]
    fn low_battery_off_power_waits() {
        let i = HourlyInputs { power_connected: false, battery_pct: 29, ..ready() };
        assert_eq!(decide(&i), Err(HourlySkip::InsufficientPower));
        let ok = HourlyInputs { power_connected: false, battery_pct: 30, ..ready() };
        assert_eq!(decide(&ok), Ok(()));
    }
}
