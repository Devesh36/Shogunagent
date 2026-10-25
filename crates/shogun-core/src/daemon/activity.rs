//! Content-free dashboard usage assembled from retained capture and native-write records.

use serde::Serialize;

use super::{local_date_days_before, local_date_string, local_day_bounds, Db};

/// One capture sampler interval. This is a conservative foreground sample, not attendance time.
pub const CAPTURE_ACTIVITY_MAX_MS: i64 = 2_000;
const CALENDAR_DAYS: i64 = 20 * 7;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DashboardAppUsage {
    pub app_bundle_id: String,
    pub captured_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DashboardUsageRange {
    pub key: &'static str,
    pub apps: Vec<DashboardAppUsage>,
    pub rewrite_count: i64,
    pub inline_edit_count: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DashboardUsageDay {
    pub local_date: String,
    pub captured_ms: i64,
}

/// The `first_recorded_on` field is the oldest retained captured-activity credit. It is not a
/// claim that earlier activity was unavailable, and edit counts can exist without a capture row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DashboardUsage {
    pub first_recorded_on: Option<String>,
    pub calendar_start: String,
    pub calendar_end: String,
    pub ranges: Vec<DashboardUsageRange>,
    pub calendar: Vec<DashboardUsageDay>,
}

impl Db {
    /// Snapshot before a native capture/write begins. Erasure advances this generation while
    /// holding the same connection lock used by the later metadata write.
    pub fn activity_erase_generation(&self) -> u64 {
        self.activity_erase_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Credit one successful AX capture to its canonical retained event. The caller supplies no
    /// content or app identity: both stay on `event_log`. Time is capped to one sample so a
    /// delayed poll, sleep, or stale dwell value cannot become fabricated usage time.
    pub fn record_capture_activity(
        &self,
        event_id: i64,
        captured_ms: i64,
        erase_generation: u64,
    ) -> bool {
        let captured_ms = captured_ms.clamp(0, CAPTURE_ACTIVITY_MAX_MS);
        if captured_ms == 0 {
            return false;
        }
        let end_ms = self.now_ms();
        let start_ms = end_ms.saturating_sub(captured_ms);
        let start_date = local_date_string(start_ms);
        // An interval ending exactly at midnight belongs entirely to the preceding day.
        let final_date = local_date_string(end_ms.saturating_sub(1));
        let credits = if start_date == final_date {
            vec![(start_date, captured_ms)]
        } else {
            let boundary = local_day_bounds(end_ms).today_start_ms;
            if boundary <= start_ms || boundary >= end_ms {
                return false;
            }
            vec![
                (start_date, boundary.saturating_sub(start_ms)),
                (final_date, end_ms.saturating_sub(boundary)),
            ]
        };
        self.with_conn("dashboard.activity_capture", |conn| {
            if self.activity_erase_generation() != erase_generation {
                return Ok(false);
            }
            shogun_memory::activity::record_capture_credits(conn, event_id, end_ms, &credits)
                .map(|()| true)
        })
        .unwrap_or(false)
    }

    /// Record a completed native write. Callers invoke this only after the target has accepted
    /// the text; drafts, cancellation, provider success, and UI status events do not count.
    pub fn record_edit_activity(
        &self,
        kind: shogun_memory::activity::EditKind,
        erase_generation: u64,
    ) -> bool {
        let completed_at_ms = self.now_ms();
        let date = local_date_string(completed_at_ms);
        self.with_conn("dashboard.edit_activity", |conn| {
            if self.activity_erase_generation() != erase_generation {
                return Ok(false);
            }
            shogun_memory::activity::record_edit(conn, &date, kind, completed_at_ms)
                .map(|()| true)
        })
        .unwrap_or(false)
    }

    /// Read the bounded dashboard projection. An error is intentionally returned to the caller,
    /// which serializes `usage: null`; that differs from a valid empty post-instrumentation view.
    pub fn try_dashboard_usage(&self) -> Result<DashboardUsage, String> {
        let now = self.now_ms();
        let today = local_date_string(now);
        let last_30 = local_date_days_before(now, 29);
        let last_7 = local_date_days_before(now, 6);
        let calendar_start = local_date_days_before(now, (CALENDAR_DAYS - 1) as i32);
        self.with_conn_reported("dashboard.usage", |conn| {
            let range = |key: &'static str,
                         from_date: Option<&str>|
             -> Result<DashboardUsageRange, String> {
                let totals = shogun_memory::activity::usage_totals(conn, from_date, &today)
                    .map_err(|_| "dashboard usage read failed".to_string())?;
                Ok(DashboardUsageRange {
                    key,
                    apps: totals
                        .apps
                        .into_iter()
                        .map(|app| DashboardAppUsage {
                            app_bundle_id: app.app_bundle_id,
                            captured_ms: app.captured_ms,
                        })
                        .collect(),
                    rewrite_count: totals.rewrite_count,
                    inline_edit_count: totals.inline_edit_count,
                })
            };
            let ranges = vec![
                range("all_time", None)?,
                range("last30", Some(last_30.as_str()))?,
                range("last7", Some(last_7.as_str()))?,
                range("today", Some(today.as_str()))?,
            ];
            let calendar = shogun_memory::activity::day_usage_between(conn, &calendar_start, &today)
                .map_err(|_| "dashboard usage read failed".to_string())?
                .into_iter()
                .map(|day| DashboardUsageDay {
                    local_date: day.local_date,
                    captured_ms: day.captured_ms,
                })
                .collect();
            let first_recorded_on = shogun_memory::activity::first_recorded_on(conn, &today)
                .map_err(|_| "dashboard usage read failed".to_string())?;
            Ok(DashboardUsage {
                first_recorded_on,
                calendar_start,
                calendar_end: today,
                ranges,
                calendar,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::daemon::Clock;

    fn test_db(now_ms: i64) -> Result<Db, String> {
        let clock: Clock = Arc::new(move || now_ms);
        Db::open_in_memory(clock).map_err(|error| error.to_string())
    }

    #[test]
    fn capture_credit_is_capped_split_and_bounded() -> Result<(), String> {
        // Derive a local midnight instead of assuming UTC: this stays a date-boundary test on
        // macOS and on the portable UTC test path.
        let now_ms = local_day_bounds(1_700_000_000_000).today_start_ms + 1_000;
        let db = test_db(now_ms)?;
        let event_id = db
            .capture(&shogun_memory::event_log::NewEvent {
                ts: now_ms,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Safari"),
                window_title: None,
                content: "fixture",
                content_hash: "dashboard-usage-fixture",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .ok_or("fixture capture failed")?
            .0;
        assert!(db.record_capture_activity(
            event_id,
            CAPTURE_ACTIVITY_MAX_MS + 1,
            db.activity_erase_generation(),
        ));

        let usage = db.try_dashboard_usage()?;
        assert_eq!(usage.calendar.iter().map(|day| day.captured_ms).sum::<i64>(), 2_000);
        assert_eq!(usage.calendar.len(), 2);
        assert!(usage.calendar.iter().all(|day| day.local_date <= local_date_string(now_ms)));
        assert_eq!(usage.ranges[0].apps[0].captured_ms, 2_000);
        assert_eq!(usage.ranges[3].apps[0].captured_ms, 1_000);
        Ok(())
    }

    #[test]
    fn edit_counts_are_independent_of_capture_coverage() -> Result<(), String> {
        let db = test_db(86_400_000)?;
        assert!(db.record_edit_activity(
            shogun_memory::activity::EditKind::ScribeRewrite,
            db.activity_erase_generation(),
        ));
        let usage = db.try_dashboard_usage()?;
        assert_eq!(usage.first_recorded_on, None);
        assert_eq!(usage.ranges[3].rewrite_count, 1);
        Ok(())
    }

    fn fixture_event(db: &Db, ts: i64, hash: &'static str) -> Result<i64, String> {
        db.capture(&shogun_memory::event_log::NewEvent {
            ts,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.apple.Safari"),
            window_title: None,
            content: "fixture",
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        })
        .map(|written| written.0)
        .ok_or_else(|| "fixture capture failed".to_string())
    }

    #[test]
    fn stale_native_activity_cannot_reappear_after_forget_or_wipe() -> Result<(), String> {
        let now = local_day_bounds(1_700_000_000_000).today_start_ms + 10_000;
        let old_ts = now - 24 * 60 * 60 * 1000;
        let forgotten = test_db(now)?;
        let old_event = fixture_event(&forgotten, old_ts, "erase-generation-forget")?;
        let before_forget = forgotten.activity_erase_generation();
        forgotten
            .delete_since(now)
            .ok_or("synthetic forget failed")?;
        assert!(!forgotten.record_capture_activity(old_event, 2_000, before_forget));
        assert!(!forgotten.record_edit_activity(
            shogun_memory::activity::EditKind::ScribeRewrite,
            before_forget,
        ));
        assert!(!forgotten.record_edit_activity(
            shogun_memory::activity::EditKind::InlineEdit,
            before_forget,
        ));
        let usage = forgotten.try_dashboard_usage()?;
        assert!(usage.ranges[3].apps.is_empty());
        assert_eq!(usage.ranges[3].rewrite_count, 0);
        assert_eq!(usage.ranges[3].inline_edit_count, 0);

        let wiped = test_db(now)?;
        let event = fixture_event(&wiped, now, "erase-generation-wipe")?;
        let before_wipe = wiped.activity_erase_generation();
        wiped.delete_all().ok_or("synthetic wipe failed")?;
        assert!(!wiped.record_capture_activity(event, 2_000, before_wipe));
        assert!(!wiped.record_edit_activity(
            shogun_memory::activity::EditKind::ScribeRewrite,
            before_wipe,
        ));
        assert!(!wiped.record_edit_activity(
            shogun_memory::activity::EditKind::InlineEdit,
            before_wipe,
        ));
        let usage = wiped.try_dashboard_usage()?;
        assert!(usage.ranges[3].apps.is_empty());
        assert_eq!(usage.ranges[3].rewrite_count, 0);
        assert_eq!(usage.ranges[3].inline_edit_count, 0);
        Ok(())
    }
}
