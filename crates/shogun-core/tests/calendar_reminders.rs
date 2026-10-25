#![allow(clippy::unwrap_used, clippy::expect_used)]

use shogun_core::meeting::reminders::{safe_join_url, Reminder, Scheduler};

fn event() -> Reminder {
    Reminder {
        occurrence_id: 7,
        title: Some("Planning".into()),
        start_at: 600_000,
        end_at: 900_000,
        join_url: "https://meet.google.com/abc-defg-hij".into(),
    }
}
#[test]
fn due_reminder_survives_remount_but_dismissal_does_not_repeat() {
    let mut scheduler = Scheduler::default();
    assert_eq!(scheduler.tick(539_999, 1, true, vec![event()]), None);
    assert_eq!(
        scheduler.tick(540_000, 1, true, vec![event()]),
        Some(event())
    );
    assert_eq!(
        scheduler.tick(540_001, 1, true, vec![event()]),
        Some(event())
    );
    scheduler.dismiss(7, 600_000);
    assert_eq!(scheduler.tick(540_002, 1, true, vec![event()]), None);
}
#[test]
fn cancellation_disconnect_disabled_and_expiry_clear_snapshot() {
    for (minutes, connected, events, now) in [
        (1, true, vec![], 550_000),
        (1, false, vec![event()], 550_000),
        (0, true, vec![event()], 550_000),
        (1, true, vec![event()], 600_000),
    ] {
        let mut scheduler = Scheduler::default();
        scheduler.tick(540_000, 1, true, vec![event()]);
        assert_eq!(scheduler.tick(now, minutes, connected, events), None);
    }
}
#[test]
fn wake_in_window_and_rescheduling_work_without_replaying_dismissed_start() {
    let mut scheduler = Scheduler::default();
    assert_eq!(
        scheduler.tick(590_000, 1, true, vec![event()]),
        Some(event())
    );
    scheduler.dismiss(7, 600_000);
    let mut later = event();
    later.start_at = 660_000;
    assert_eq!(
        scheduler.tick(600_000, 1, true, vec![later.clone()]),
        Some(later)
    );
}
#[test]
fn settings_only_accept_supported_lead_times_and_urls_cannot_launch_apps() {
    let mut scheduler = Scheduler::default();
    assert_eq!(scheduler.tick(540_000, 2, true, vec![event()]), None);
    for bad in [
        "file:///tmp/x",
        "javascript:alert(1)",
        "zoommtg://join",
        "https://user:pass@meet.google.com/x",
        "https://localhost/x",
        "https://127.0.0.1/x",
        "https://evil.example/x",
        "https://meet.google.com.evil.example/x",
    ] {
        assert!(!safe_join_url(bad), "{bad}");
    }
    assert!(safe_join_url(&event().join_url));
}

#[test]
fn old_settings_default_to_one_minute_with_the_shipped_capture_default() {
    let settings: shogun_core::meeting::settings::Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(settings.calendar_reminder_minutes, 1);
    // Meeting notes ship on (owner decision 2026-10-20).
    assert!(settings.enabled);
}

#[cfg(feature = "db")]
#[test]
fn cancelled_all_day_declined_or_missing_join_never_notify() {
    use shogun_core::meeting::reminders::from_occurrence;
    use shogun_memory::calendar::{Attendee, Occurrence};
    let occurrence = Occurrence {
        id: 1,
        external_id: "event".into(),
        title: None,
        start_at: 60_000,
        end_at: 120_000,
        all_day: false,
        location: Some(event().join_url),
        attendees: vec![],
        cancelled: false,
    };
    assert!(from_occurrence(occurrence.clone()).is_some());
    let mut cancelled = occurrence.clone();
    cancelled.cancelled = true;
    assert!(from_occurrence(cancelled).is_none());
    let mut all_day = occurrence.clone();
    all_day.all_day = true;
    assert!(from_occurrence(all_day).is_none());
    let mut declined = occurrence.clone();
    declined.attendees.push(Attendee {
        is_self: true,
        response: Some("declined".into()),
        ..Default::default()
    });
    assert!(from_occurrence(declined).is_none());
    let mut no_link = occurrence;
    no_link.location = None;
    assert!(from_occurrence(no_link).is_none());
}

#[test]
fn reminder_join_checks_the_same_url_the_browser_opens() {
    assert!(!safe_join_url(
        r"https://evil.example\@meet.google.com/abc-defg-hij"
    ));
    assert!(!safe_join_url(r"https://evil.example\@zoom.us/j/123456789"));
    let mut scheduler = Scheduler::default();
    let mut input = event();
    input.join_url = "https://MEET.GOOGLE.COM:443/abc-defg-hij".into();
    let snapshot = scheduler.tick(540_000, 1, true, vec![input]).unwrap();
    assert_eq!(snapshot.join_url, event().join_url);
}
