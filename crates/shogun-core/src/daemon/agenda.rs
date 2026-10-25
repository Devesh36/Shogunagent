//! The user's schedule, read from the synced calendar (`calendar_occurrences`).
//!
//! One source for every surface that shows "what is on": Today's "Coming up" (re-read on every
//! open, never from the saved brief), the Morning Brief's Today section, the Evening Wrap's
//! tomorrow, and the Memory API's `calendar.upcoming` (invariant 6). Calendar events are facts the
//! user's own calendar holds, so nothing here is hedged: there is no "possibly" on a schedule
//! line. Dated commitments are not schedule items; they stay in the brief's commitments section.
//!
//! Local days follow the OS zone (DST folded in) like the brief's own day key; Linux tests run in
//! UTC unless they pass an offset to the pure helpers.

use super::*;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Local days "Coming up" covers, today included.
pub const AGENDA_DAYS: i64 = 7;
/// Default number of events "Coming up" (and `calendar.upcoming`) lists.
pub const AGENDA_MAX: usize = 10;
/// Rows read from the store before day grouping and the cap: all-day rows sort by a UTC midnight,
/// so the SQL order is only roughly the display order.
const AGENDA_READ_LIMIT: usize = 200;

/// One event on the user's schedule, as the surfaces show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgendaEvent {
    /// `calendar_occurrences.id`.
    pub id: i64,
    /// `None` for an event with no title; the surface words it.
    pub title: Option<String>,
    pub start_ms: i64,
    pub end_ms: i64,
    /// A date, not an instant: `start_ms`/`end_ms` are UTC midnights and carry no time of day.
    pub all_day: bool,
    /// The place or the meeting link. Stays on this device.
    pub location: Option<String>,
    /// Invitees other than the user.
    pub attendees: usize,
    /// Local days after today (0 = today). An event already running counts as today.
    pub day: u32,
    /// The local date of `day`, `YYYY-MM-DD`.
    pub date: String,
}

/// The local day number (days since 1970-01-01 in local time) an instant falls on.
fn local_day(ms: i64, offset_secs: &impl Fn(i64) -> i32) -> i64 {
    (ms + i64::from(offset_secs(ms)) * 1000).div_euclid(DAY_MS)
}

/// The instant local day `day` starts. The offset is taken at the guessed instant, so a DST day
/// lands on its real midnight.
fn local_midnight(day: i64, offset_secs: &impl Fn(i64) -> i32) -> i64 {
    let floating = day * DAY_MS;
    let guess = floating - i64::from(offset_secs(floating)) * 1000;
    floating - i64::from(offset_secs(guess)) * 1000
}

fn date_string(day: i64) -> String {
    let d = shogun_memory::due::civil_from_days(day);
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// "Coming up": from now to the end of the `AGENDA_DAYS`-th local day.
fn upcoming_window(now_ms: i64, offset_secs: &impl Fn(i64) -> i32) -> shogun_memory::calendar::AgendaWindow {
    let today = local_day(now_ms, offset_secs);
    shogun_memory::calendar::AgendaWindow {
        from_ms: now_ms,
        until_ms: local_midnight(today + AGENDA_DAYS, offset_secs),
        all_day_from_ms: today * DAY_MS,
        all_day_until_ms: (today + AGENDA_DAYS) * DAY_MS,
        limit: AGENDA_READ_LIMIT,
    }
}

/// One whole local day, `day_offset` days after today (0 = today, 1 = tomorrow).
fn day_window(now_ms: i64, day_offset: i64, offset_secs: &impl Fn(i64) -> i32) -> shogun_memory::calendar::AgendaWindow {
    let day = local_day(now_ms, offset_secs) + day_offset;
    shogun_memory::calendar::AgendaWindow {
        from_ms: local_midnight(day, offset_secs),
        until_ms: local_midnight(day + 1, offset_secs),
        all_day_from_ms: day * DAY_MS,
        all_day_until_ms: (day + 1) * DAY_MS,
        limit: AGENDA_READ_LIMIT,
    }
}

/// Shape stored occurrences into the schedule: declined invitations dropped, each event put on
/// its local day (all-day events on their own date), all-day first within a day, then by start;
/// capped at `max`.
fn agenda_from(
    occurrences: Vec<shogun_memory::calendar::Occurrence>,
    now_ms: i64,
    max: usize,
    offset_secs: &impl Fn(i64) -> i32,
) -> Vec<AgendaEvent> {
    let today = local_day(now_ms, offset_secs);
    let mut events: Vec<AgendaEvent> = occurrences
        .into_iter()
        .filter(|o| !o.declined_by_self())
        .map(|o| {
            let first_day = if o.all_day {
                o.start_at.div_euclid(DAY_MS)
            } else {
                local_day(o.start_at, offset_secs)
            };
            let day = first_day.max(today);
            AgendaEvent {
                id: o.id,
                attendees: o.other_attendees(),
                title: o.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
                start_ms: o.start_at,
                end_ms: o.end_at,
                all_day: o.all_day,
                location: o.location.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()),
                day: u32::try_from(day - today).unwrap_or(0),
                date: date_string(day),
            }
        })
        .collect();
    events.sort_by_key(|e| (e.day, !e.all_day, e.start_ms, e.id));
    events.truncate(max);
    events
}

/// Brief / wrap lines for a whole day's events. Untitled events are left out: a line with no words
/// tells the reader nothing, and the brief has no slot to word "untitled" in the UI language.
fn calendar_lines_from(events: Vec<AgendaEvent>) -> Vec<CalendarLine> {
    events
        .into_iter()
        .filter_map(|e| {
            e.title.map(|title| CalendarLine {
                start_ms: e.start_ms,
                title,
                all_day: e.all_day,
                updated: false,
            })
        })
        .collect()
}

impl Db {
    /// Today's "Coming up" and `calendar.upcoming`: synced calendar events from now through the
    /// next [`AGENDA_DAYS`] local days, at most [`AGENDA_MAX`].
    pub fn calendar_agenda(&self, now_ms: i64) -> MemoryResult<Vec<AgendaEvent>> {
        self.calendar_agenda_page(now_ms, AGENDA_MAX).map(|(events, _)| events)
    }

    /// Bounded agenda with the total number of non-declined events in the window.
    pub fn calendar_agenda_page(&self, now_ms: i64, limit: usize) -> MemoryResult<(Vec<AgendaEvent>, usize)> {
        let window = upcoming_window(now_ms, &os_gmt_offset_secs);
        let rows = self.with_conn("calendar.agenda", |c| shogun_memory::calendar::agenda(c, &window))?;
        let total = self.with_conn("calendar.agenda_count", |c| shogun_memory::calendar::agenda_count(c, &window))?;
        Ok((agenda_from(rows, now_ms, limit.clamp(1, 100), &os_gmt_offset_secs), total))
    }

    /// One saved event, independent of agenda time and page limits.
    pub fn calendar_occurrence(&self, id: i64) -> MemoryResult<Option<shogun_memory::calendar::Occurrence>> {
        self.with_conn("calendar.get", |c| shogun_memory::calendar::get(c, id))
    }

    /// The calendar lines of one whole local day, `day_offset` days after today: the Morning
    /// Brief's Today (0) and the Evening Wrap's tomorrow (1).
    pub fn calendar_day_lines(&self, now_ms: i64, day_offset: i64) -> MemoryResult<Vec<CalendarLine>> {
        let window = day_window(now_ms, day_offset, &os_gmt_offset_secs);
        let rows = self.with_conn("calendar.day_lines", |c| shogun_memory::calendar::agenda(c, &window))?;
        Ok(calendar_lines_from(agenda_from(rows, now_ms, AGENDA_READ_LIMIT, &os_gmt_offset_secs)))
    }

    /// Whether any Google Calendar event was ever synced to this Mac. The connection state lives
    /// in the connector runtime; this is the store's half of "is a calendar feeding the schedule".
    pub fn calendar_synced(&self) -> MemoryResult<bool> {
        self.with_conn("calendar.synced", |c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM calendar_occurrences WHERE source = 'gcal')",
                [],
                |r| r.get(0),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use shogun_memory::calendar::{Attendee, Occurrence};

    const HOUR: i64 = 3_600_000;
    const JST: fn(i64) -> i32 = |_| 9 * 3600;
    const UTC: fn(i64) -> i32 = |_| 0;

    fn occ(id: i64, start: i64, end: i64, all_day: bool) -> Occurrence {
        Occurrence {
            id,
            external_id: format!("ev{id}"),
            title: Some(format!("event {id}")),
            start_at: start,
            end_at: end,
            all_day,
            location: None,
            attendees: Vec::new(),
            cancelled: false,
        }
    }

    #[test]
    fn events_land_on_their_local_day_and_all_day_events_on_their_date() {
        // 2026-10-13 09:00 JST = 00:00 UTC on day 20_739.
        let day = 20_739;
        let now = day * DAY_MS;
        let events = agenda_from(
            vec![
                // 23:30 UTC the same UTC day = 08:30 JST tomorrow.
                occ(1, now + 23 * HOUR + HOUR / 2, now + 24 * HOUR, false),
                // 10:00 JST today.
                occ(2, now + HOUR, now + 2 * HOUR, false),
                // An all-day event on 2026-10-14 (stored as that date's UTC midnight).
                occ(3, (day + 1) * DAY_MS, (day + 2) * DAY_MS, true),
                // A trip that began two days ago and is still on today.
                occ(4, (day - 2) * DAY_MS, (day + 1) * DAY_MS, true),
            ],
            now,
            AGENDA_MAX,
            &JST,
        );
        let got: Vec<(i64, u32, &str)> = events.iter().map(|e| (e.id, e.day, e.date.as_str())).collect();
        assert_eq!(
            got,
            [(4, 0, "2026-10-13"), (2, 0, "2026-10-13"), (3, 1, "2026-10-14"), (1, 1, "2026-10-14")],
            "all-day first within a day, then by start; an ongoing trip counts as today"
        );
    }

    #[test]
    fn declined_invitations_are_not_on_the_schedule_and_the_cap_holds() {
        let now = 100 * DAY_MS;
        let mut declined = occ(1, now + HOUR, now + 2 * HOUR, false);
        declined.attendees = vec![Attendee { is_self: true, response: Some("declined".into()), ..Default::default() }];
        let mut invited = occ(2, now + 3 * HOUR, now + 4 * HOUR, false);
        invited.attendees = vec![
            Attendee { is_self: true, response: Some("accepted".into()), ..Default::default() },
            Attendee { email: Some("a@example.com".into()), ..Default::default() },
        ];
        let events = agenda_from(vec![declined, invited], now, AGENDA_MAX, &UTC);
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].id, events[0].attendees), (2, 1));

        let many: Vec<Occurrence> = (0..30).map(|i| occ(i, now + i * HOUR, now + i * HOUR + 1, false)).collect();
        assert_eq!(agenda_from(many, now, AGENDA_MAX, &UTC).len(), AGENDA_MAX);
    }

    #[test]
    fn windows_follow_local_midnights_and_dates() {
        let day = 20_739;
        // 12:00 JST.
        let now = day * DAY_MS + 3 * HOUR;
        let w = upcoming_window(now, &JST);
        assert_eq!(w.from_ms, now);
        assert_eq!(w.until_ms, (day + AGENDA_DAYS) * DAY_MS - 9 * HOUR, "JST midnight a week on");
        assert_eq!((w.all_day_from_ms, w.all_day_until_ms), (day * DAY_MS, (day + AGENDA_DAYS) * DAY_MS));
        let tomorrow = day_window(now, 1, &JST);
        assert_eq!(tomorrow.from_ms, (day + 1) * DAY_MS - 9 * HOUR);
        assert_eq!(tomorrow.until_ms - tomorrow.from_ms, DAY_MS);
        assert_eq!(tomorrow.all_day_from_ms, (day + 1) * DAY_MS);
    }

    #[test]
    fn untitled_events_stay_off_brief_lines() {
        let now = 100 * DAY_MS;
        let mut untitled = occ(1, now + HOUR, now + 2 * HOUR, false);
        untitled.title = Some("  ".into());
        let lines = calendar_lines_from(agenda_from(vec![untitled, occ(2, now, now + HOUR, true)], now, 10, &UTC));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].all_day && lines[0].title == "event 2");
    }

    fn db_at(now: i64) -> Db {
        Db::open_in_memory(std::sync::Arc::new(move || now)).unwrap()
    }

    fn insert(db: &Db, id: &str, title: &str, start: i64, end: i64, all_day: bool) {
        db.with_conn("test.calendar", |c| {
            shogun_memory::calendar::upsert(
                c,
                &shogun_memory::calendar::NewOccurrence {
                    source: "gcal",
                    external_id: id,
                    title: Some(title),
                    start_at: start,
                    end_at: end,
                    all_day,
                    location: None,
                    attendees: &[],
                    cancelled: false,
                    event_id: None,
                },
                1,
            )
        })
        .unwrap();
    }

    #[test]
    fn agenda_pages_report_the_total_beyond_the_display_and_read_caps() {
        let now = 100 * DAY_MS + 12 * HOUR;
        let db = db_at(now);
        for i in 0..230 {
            insert(&db, &format!("event-{i}"), "Review", now + HOUR + i * 1000, now + 2 * HOUR + i * 1000, false);
        }
        let (first, total) = db.calendar_agenda_page(now, 10).expect("test");
        assert_eq!((first.len(), total), (10, 230));
        let (expanded, total) = db.calendar_agenda_page(now, usize::MAX).expect("test");
        assert_eq!((expanded.len(), total), (100, 230));
    }

    #[test]
    fn occurrence_lookup_survives_page_limits_and_time_changes() {
        let now = 100 * DAY_MS + 12 * HOUR;
        let db = db_at(now);
        for i in 0..105 {
            insert(&db, &format!("lookup-{i}"), "Review", now + HOUR + i * 1000, now + 2 * HOUR + i * 1000, false);
        }
        let (page, _) = db.calendar_agenda_page(now, 100).expect("agenda");
        assert!(page.iter().all(|event| event.id != 105));
        assert_eq!(db.calendar_occurrence(105).expect("occurrence").expect("saved event").external_id, "lookup-104");
        assert!(db.calendar_agenda_page(now + 8 * DAY_MS, 100).expect("later agenda").0.is_empty());
        assert!(db.calendar_occurrence(105).expect("retained occurrence").is_some());
        assert!(db.calendar_occurrence(999).expect("missing occurrence").is_none());
    }

    #[test]
    fn the_db_reads_the_agenda_and_whole_day_lines_from_the_store() {
        // Linux tests run in UTC: day 100, 12:00.
        let now = 100 * DAY_MS + 12 * HOUR;
        let db = db_at(now);
        assert!(!db.calendar_synced().unwrap());
        assert!(db.calendar_agenda(now).unwrap().is_empty());
        insert(&db, "morning", "Standup", 100 * DAY_MS + 9 * HOUR, 100 * DAY_MS + 9 * HOUR + HOUR / 4, false);
        insert(&db, "afternoon", "Design review", 100 * DAY_MS + 15 * HOUR, 100 * DAY_MS + 16 * HOUR, false);
        insert(&db, "holiday", "Holiday", 101 * DAY_MS, 102 * DAY_MS, true);
        insert(&db, "tomorrow", "1:1", 101 * DAY_MS + 10 * HOUR, 101 * DAY_MS + 11 * HOUR, false);
        assert!(db.calendar_synced().unwrap());

        let agenda: Vec<String> =
            db.calendar_agenda(now).unwrap().into_iter().filter_map(|e| e.title).collect();
        assert_eq!(agenda, ["Design review", "Holiday", "1:1"], "the morning standup is over");

        let today: Vec<String> = db.calendar_day_lines(now, 0).unwrap().into_iter().map(|l| l.title).collect();
        assert_eq!(today, ["Standup", "Design review"], "the brief's Today is the whole day");
        let tomorrow: Vec<String> = db.calendar_day_lines(now, 1).unwrap().into_iter().map(|l| l.title).collect();
        assert_eq!(tomorrow, ["Holiday", "1:1"]);
    }
}
