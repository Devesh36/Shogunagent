//! Calendar occurrences as state (FR-MT-06).
//!
//! The calendar sync appends each event to the event log like any other synced record, but an
//! event is about the future: meeting notes need to ask "is something scheduled right now, and who
//! is invited?", which a past-tense log cannot answer. So each synced event is also held here as
//! one row, upserted by its calendar id so it follows reschedules and cancellations.
//!
//! What it feeds today:
//! - linking a meeting session to the event it belongs to (`sessions.calendar_occurrence_id`);
//! - speaker naming: the invitee list is the most precise candidate source (issue #263);
//! - the schedule: Today's "Coming up", the Morning Brief's Today section, the Evening Wrap's
//!   tomorrow and `calendar.upcoming` all read [`agenda`].
//!
//! `location` can be a meeting URL and `attendees` carries email addresses: both stay on this
//! device (FR-MT-06). Nothing here is sent anywhere.

use rusqlite::{params, Connection, OptionalExtension};

/// One invitee, as stored in `calendar_occurrences.attendees`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attendee {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// The account owner. Never a candidate for "one of the others".
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub response: Option<String>,
}

/// An occurrence to upsert from a synced event.
#[derive(Debug, Clone, PartialEq)]
pub struct NewOccurrence<'a> {
    /// `gcal`.
    pub source: &'a str,
    /// The calendar's own event id — the upsert key.
    pub external_id: &'a str,
    pub title: Option<&'a str>,
    pub start_at: i64,
    pub end_at: i64,
    pub all_day: bool,
    pub location: Option<&'a str>,
    pub attendees: &'a [Attendee],
    pub cancelled: bool,
    /// The event_log row this sync wrote or touched (provenance).
    pub event_id: Option<i64>,
}

/// A stored occurrence.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub id: i64,
    /// The calendar's own event id — what "always skip this meeting" settings key on (FR-MT-02).
    pub external_id: String,
    pub title: Option<String>,
    pub start_at: i64,
    pub end_at: i64,
    pub all_day: bool,
    pub location: Option<String>,
    pub attendees: Vec<Attendee>,
    pub cancelled: bool,
}

/// Insert or refresh one occurrence, keyed on `(source, external_id)`. Returns its row id, or
/// `None` when the event has no id to key on (nothing stable to update later).
pub fn upsert(
    conn: &Connection,
    o: &NewOccurrence<'_>,
    now: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    if o.external_id.trim().is_empty() {
        return Ok(None);
    }
    let attendees = serde_json::to_string(o.attendees).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO calendar_occurrences
           (source, external_id, title, start_at, end_at, all_day, location, attendees, status,
            event_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT(source, external_id) DO UPDATE SET
           title = excluded.title, start_at = excluded.start_at, end_at = excluded.end_at,
           all_day = excluded.all_day, location = excluded.location,
           attendees = excluded.attendees, status = excluded.status,
           event_id = COALESCE(excluded.event_id, calendar_occurrences.event_id),
           updated_at = excluded.updated_at",
        params![
            o.source,
            o.external_id,
            o.title,
            o.start_at,
            o.end_at.max(o.start_at),
            o.all_day,
            o.location,
            attendees,
            if o.cancelled { "cancelled" } else { "confirmed" },
            o.event_id,
            now,
        ],
    )?;
    conn.query_row(
        "SELECT id FROM calendar_occurrences WHERE source = ?1 AND external_id = ?2",
        params![o.source, o.external_id],
        |r| r.get(0),
    )
    .optional()
}

fn row_to_occurrence(r: &rusqlite::Row<'_>) -> rusqlite::Result<Occurrence> {
    let attendees: String = r.get(6)?;
    let status: String = r.get(7)?;
    Ok(Occurrence {
        id: r.get(0)?,
        external_id: r.get(8)?,
        title: r.get(1)?,
        start_at: r.get(2)?,
        end_at: r.get(3)?,
        all_day: r.get(4)?,
        location: r.get(5)?,
        attendees: serde_json::from_str(&attendees).unwrap_or_default(),
        cancelled: status == "cancelled",
    })
}

const COLUMNS: &str =
    "id, title, start_at, end_at, all_day, location, attendees, status, external_id";

/// The account owner's own address: the one Google marks `self` on the most synced events. Form
/// fill offers it for the user's own email field, so a crowded memory lookup cannot lose it.
pub fn self_email(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT json_extract(a.value, '$.email') AS email
         FROM calendar_occurrences o, json_each(o.attendees) a
         WHERE json_extract(a.value, '$.is_self') = 1 AND email LIKE '%_@_%'
         GROUP BY lower(email) ORDER BY COUNT(*) DESC, MAX(o.start_at) DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

/// One occurrence by id.
pub fn get(conn: &Connection, id: i64) -> Result<Option<Occurrence>, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM calendar_occurrences WHERE id = ?1"),
        [id],
        row_to_occurrence,
    )
    .optional()
}

/// Mark an occurrence cancelled, keeping its title, time and event link. A provider deletion
/// tombstone carries only the id, so it must not overwrite the row it cancels.
pub fn cancel(conn: &Connection, source: &str, external_id: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE calendar_occurrences SET status = 'cancelled', updated_at = ?3
         WHERE source = ?1 AND external_id = ?2",
        params![source, external_id, now],
    )?;
    Ok(())
}

/// Bounded current calendar candidates for upcoming notifications. Re-reading each tick makes
/// cancellation and rescheduling visible without keeping a second calendar cache.
pub fn upcoming(conn: &Connection, now: i64, until: i64) -> rusqlite::Result<Vec<Occurrence>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM calendar_occurrences WHERE source = 'gcal'
         AND status = 'confirmed' AND all_day = 0 AND start_at > ?1 AND start_at <= ?2
         AND end_at > start_at ORDER BY start_at, id LIMIT 100"
    ))?;
    let rows = statement.query_map(params![now, until], row_to_occurrence)?;
    rows.collect()
}

/// The span an agenda read covers (Today's "Coming up", the Morning Brief's Today section, the
/// Evening Wrap's tomorrow, `calendar.upcoming`).
///
/// Timed events are instants, so they match on `[from_ms, until_ms)`: anything still running at
/// `from_ms` or starting before `until_ms`. All-day events are *dates*, stored as 00:00 UTC of the
/// first day to 00:00 UTC after the last (the calendar gives no instant), so they match on their
/// own day range `[all_day_from_ms, all_day_until_ms)` — the caller's local dates written as UTC
/// midnights. Comparing a date against local instants would move it a day either side of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgendaWindow {
    pub from_ms: i64,
    pub until_ms: i64,
    pub all_day_from_ms: i64,
    pub all_day_until_ms: i64,
    /// Most rows returned (clamped to 1..=200).
    pub limit: usize,
}

/// Total upcoming events in the window, excluding cancelled and declined invitations.
pub fn agenda_count(conn: &Connection, window: &AgendaWindow) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*) FROM calendar_occurrences WHERE source = 'gcal'
         AND status = 'confirmed'
         AND ((all_day = 0 AND end_at > ?1 AND start_at < ?2)
           OR (all_day = 1 AND end_at > ?3 AND start_at < ?4))
         AND NOT EXISTS (SELECT 1 FROM json_each(attendees) a
             WHERE json_extract(a.value, '$.is_self') = 1
               AND json_extract(a.value, '$.response') = 'declined')",
        params![window.from_ms, window.until_ms, window.all_day_from_ms, window.all_day_until_ms],
        |row| row.get(0),
    )
}

/// Synced Google Calendar events in the window, earliest start first. Cancelled and declined
/// invitations are excluded before applying the read cap.
pub fn agenda(conn: &Connection, window: &AgendaWindow) -> rusqlite::Result<Vec<Occurrence>> {
    let limit = i64::try_from(window.limit.clamp(1, 200)).unwrap_or(200);
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM calendar_occurrences WHERE source = 'gcal'
         AND status = 'confirmed'
         AND ((all_day = 0 AND end_at > ?1 AND start_at < ?2)
           OR (all_day = 1 AND end_at > ?3 AND start_at < ?4))
         AND NOT EXISTS (SELECT 1 FROM json_each(attendees) a
             WHERE json_extract(a.value, '$.is_self') = 1
               AND json_extract(a.value, '$.response') = 'declined')
         ORDER BY start_at, id LIMIT ?5"
    ))?;
    let rows = statement.query_map(
        params![
            window.from_ms,
            window.until_ms,
            window.all_day_from_ms,
            window.all_day_until_ms,
            limit
        ],
        row_to_occurrence,
    )?;
    rows.collect()
}

impl Occurrence {
    /// Invitees other than the account owner (a solo block has none).
    pub fn other_attendees(&self) -> usize {
        self.attendees.iter().filter(|a| !a.is_self).count()
    }

    /// The account owner answered "no". A declined invitation is not on the user's schedule.
    pub fn declined_by_self(&self) -> bool {
        self.attendees
            .iter()
            .any(|a| a.is_self && a.response.as_deref() == Some("declined"))
    }
}

/// How early before its start a meeting is still "this event": people join a few minutes early.
pub const EARLY_JOIN_MS: i64 = 10 * 60 * 1000;

/// The timed, not-cancelled occurrence a meeting at `at_ms` most plausibly belongs to: one that is
/// running (or starts within [`EARLY_JOIN_MS`]), the one whose start is nearest `at_ms` when
/// several overlap. All-day events are never a meeting's event — they span the whole day and
/// would claim every call in it.
pub fn occurrence_at(conn: &Connection, at_ms: i64) -> Result<Option<Occurrence>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT {COLUMNS} FROM calendar_occurrences
              WHERE status = 'confirmed' AND all_day = 0
                AND start_at - ?2 <= ?1 AND ?1 < end_at
              ORDER BY abs(start_at - ?1), id
              LIMIT 1"
        ),
        params![at_ms, EARLY_JOIN_MS],
        row_to_occurrence,
    )
    .optional()
}

/// The invitees of the occurrence a session is linked to, other than the account owner, with each
/// name normalised through `people` where an email matches (issue #263): the name the user already
/// knows someone by wins over the calendar's display name. Attendees with neither a known person
/// nor a display name (a bare room or list address) are left out — an address is not a name.
pub fn session_attendee_names(
    conn: &Connection,
    session_id: i64,
) -> Result<Vec<String>, rusqlite::Error> {
    let occurrence: Option<i64> = conn
        .query_row(
            "SELECT calendar_occurrence_id FROM sessions WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(occurrence) = occurrence.map(|id| get(conn, id)).transpose()?.flatten() else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = Vec::new();
    for a in occurrence.attendees.iter().filter(|a| !a.is_self) {
        let known: Option<String> = match a.email.as_deref().filter(|e| !e.trim().is_empty()) {
            Some(email) => conn
                .query_row(
                    "SELECT p.display_name FROM people p, json_each(COALESCE(p.emails, '[]')) e
                      WHERE lower(e.value) = lower(?1) AND trim(p.display_name) != ''
                      ORDER BY p.id LIMIT 1",
                    [email.trim()],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        let Some(name) = known.or_else(|| a.name.clone()) else {
            continue;
        };
        let name = name.trim().to_string();
        if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn attendee(email: &str, name: Option<&str>, is_self: bool) -> Attendee {
        Attendee {
            email: Some(email.to_string()),
            name: name.map(str::to_string),
            is_self,
            response: None,
        }
    }

    fn event<'a>(id: &'a str, start: i64, end: i64, attendees: &'a [Attendee]) -> NewOccurrence<'a> {
        NewOccurrence {
            source: "gcal",
            external_id: id,
            title: Some("Design review"),
            start_at: start,
            end_at: end,
            all_day: false,
            location: Some("https://meet.google.com/abc"),
            attendees,
            cancelled: false,
            event_id: None,
        }
    }

    #[test]
    fn agenda_count_excludes_declined_events_before_the_read_limit() {
        let conn = crate::open_in_memory().expect("test");
        for (external_id, response) in [("declined", "declined"), ("accepted", "accepted")] {
            upsert(&conn, &NewOccurrence {
                source: "gcal", external_id, title: Some("Review"), start_at: 100, end_at: 200,
                all_day: false, location: None, cancelled: false, event_id: None,
                attendees: &[Attendee { is_self: true, response: Some(response.into()), ..Default::default() }],
            }, 1).expect("test");
        }
        let window = AgendaWindow { from_ms: 50, until_ms: 300, all_day_from_ms: 0, all_day_until_ms: 1000, limit: 1 };
        assert_eq!(agenda_count(&conn, &window).expect("test"), 1);
        assert_eq!(agenda(&conn, &window).expect("test")[0].external_id, "accepted");
    }

    #[test]
    fn self_email_is_the_address_google_marks_self_most_often() {
        let conn = crate::open_in_memory().unwrap();
        assert_eq!(self_email(&conn).unwrap(), None);
        let me = [attendee("me@example.com", None, true), attendee("kai@example.com", Some("Kai"), false)];
        let old = [attendee("old@example.com", None, true)];
        upsert(&conn, &event("a", 1, 2, &me), 1).unwrap();
        upsert(&conn, &event("b", 3, 4, &me), 1).unwrap();
        upsert(&conn, &event("c", 5, 6, &old), 1).unwrap();
        assert_eq!(self_email(&conn).unwrap().as_deref(), Some("me@example.com"));
    }

    #[test]
    fn a_tombstone_cancels_without_erasing_the_occurrence() {
        let conn = crate::open_in_memory().unwrap();
        let meeting = event("gone", 600_000, 900_000, &[]);
        let id = upsert(&conn, &meeting, 1).unwrap().unwrap();
        cancel(&conn, "gcal", "gone", 2).unwrap();
        let got = get(&conn, id).unwrap().unwrap();
        assert!(got.cancelled);
        assert_eq!((got.title, got.start_at), (meeting.title.map(str::to_string), 600_000));
        assert!(upcoming(&conn, 0, 1_200_000).unwrap().is_empty());
    }

    #[test]
    fn upcoming_reminders_follow_reschedules_and_cancellations() {
        let conn = crate::open_in_memory().unwrap();
        let mut meeting = event("next", 600_000, 900_000, &[]);
        upsert(&conn, &meeting, 1).unwrap();
        assert_eq!(upcoming(&conn, 0, 600_000).unwrap().len(), 1);
        assert!(upcoming(&conn, 600_000, 1_200_000).unwrap().is_empty());
        meeting.start_at = 700_000;
        upsert(&conn, &meeting, 2).unwrap();
        assert!(upcoming(&conn, 0, 600_000).unwrap().is_empty());
        meeting.cancelled = true;
        upsert(&conn, &meeting, 3).unwrap();
        assert!(upcoming(&conn, 0, 1_200_000).unwrap().is_empty());
        meeting.cancelled = false;
        meeting.all_day = true;
        upsert(&conn, &meeting, 4).unwrap();
        assert!(upcoming(&conn, 0, 1_200_000).unwrap().is_empty());
    }

    #[test]
    fn the_agenda_holds_timed_and_all_day_events_in_window_order() {
        const DAY: i64 = 24 * HOUR;
        let conn = crate::open_in_memory().unwrap();
        // "Now" is day 10, 09:00 UTC; the window is the rest of day 10 plus day 11.
        let now = 10 * DAY + 9 * HOUR;
        let window = AgendaWindow {
            from_ms: now,
            until_ms: 12 * DAY,
            all_day_from_ms: 10 * DAY,
            all_day_until_ms: 12 * DAY,
            limit: 50,
        };
        upsert(&conn, &event("past", 10 * DAY + 7 * HOUR, 10 * DAY + 8 * HOUR, &[]), 1).unwrap();
        upsert(&conn, &event("running", 10 * DAY + 8 * HOUR, 10 * DAY + 10 * HOUR, &[]), 1).unwrap();
        upsert(&conn, &event("later", 10 * DAY + 15 * HOUR, 10 * DAY + 16 * HOUR, &[]), 1).unwrap();
        upsert(&conn, &event("tomorrow", 11 * DAY + 9 * HOUR, 11 * DAY + 10 * HOUR, &[]), 1).unwrap();
        upsert(&conn, &event("too-far", 12 * DAY + HOUR, 12 * DAY + 2 * HOUR, &[]), 1).unwrap();
        let all_day = |id: &'static str, first: i64, days: i64| {
            let mut e = event(id, first * DAY, (first + days) * DAY, &[]);
            e.all_day = true;
            e
        };
        upsert(&conn, &all_day("yesterday-all-day", 9, 1), 1).unwrap();
        upsert(&conn, &all_day("today-all-day", 10, 1), 1).unwrap();
        upsert(&conn, &all_day("trip", 8, 4), 1).unwrap();
        let mut cancelled = event("cancelled", 10 * DAY + 11 * HOUR, 10 * DAY + 12 * HOUR, &[]);
        cancelled.cancelled = true;
        upsert(&conn, &cancelled, 1).unwrap();

        let ids: Vec<String> =
            agenda(&conn, &window).unwrap().into_iter().map(|o| o.external_id).collect();
        assert_eq!(
            ids,
            ["trip", "today-all-day", "running", "later", "tomorrow"],
            "ended, cancelled and out-of-window events are left out; a multi-day trip still on \
             today is in"
        );
        let capped = agenda(&conn, &AgendaWindow { limit: 2, ..window }).unwrap();
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn other_attendees_and_a_declined_invitation_are_read_from_the_invitee_list() {
        let conn = crate::open_in_memory().unwrap();
        let mut me = attendee("me@example.com", None, true);
        let others = [attendee("a@example.com", None, false), attendee("b@example.com", None, false)];
        let invitees = [me.clone(), others[0].clone(), others[1].clone()];
        let id = upsert(&conn, &event("ev", 0, HOUR, &invitees), 1).unwrap().unwrap();
        let got = get(&conn, id).unwrap().unwrap();
        assert_eq!(got.other_attendees(), 2);
        assert!(!got.declined_by_self());
        me.response = Some("declined".into());
        let invitees = [me, others[0].clone()];
        upsert(&conn, &event("ev", 0, HOUR, &invitees), 2).unwrap();
        assert!(get(&conn, id).unwrap().unwrap().declined_by_self());
    }

    #[test]
    fn a_resync_updates_the_same_row() {
        let conn = crate::open_in_memory().unwrap();
        let a = upsert(&conn, &event("ev1", 10 * HOUR, 11 * HOUR, &[]), 1).unwrap().unwrap();
        // Rescheduled an hour later, then cancelled.
        let b = upsert(&conn, &event("ev1", 11 * HOUR, 12 * HOUR, &[]), 2).unwrap().unwrap();
        assert_eq!(a, b);
        let mut cancelled = event("ev1", 11 * HOUR, 12 * HOUR, &[]);
        cancelled.cancelled = true;
        upsert(&conn, &cancelled, 3).unwrap();
        let got = get(&conn, a).unwrap().unwrap();
        assert_eq!((got.start_at, got.cancelled), (11 * HOUR, true));
        let rows: i64 =
            conn.query_row("SELECT count(*) FROM calendar_occurrences", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(upsert(&conn, &event("  ", 0, 1, &[]), 4).unwrap(), None, "no id, no row");
    }

    #[test]
    fn the_event_running_now_is_found_early_joiners_included() {
        let conn = crate::open_in_memory().unwrap();
        upsert(&conn, &event("standup", 9 * HOUR, 9 * HOUR + HOUR / 4, &[]), 1).unwrap();
        let review = upsert(&conn, &event("review", 10 * HOUR, 11 * HOUR, &[]), 1).unwrap();
        let mut offsite = event("offsite", 0, 24 * HOUR, &[]);
        offsite.all_day = true;
        upsert(&conn, &offsite, 1).unwrap();

        let at = |ms: i64| occurrence_at(&conn, ms).unwrap().map(|o| o.id);
        assert_eq!(at(10 * HOUR + HOUR / 2), review, "mid-meeting");
        assert_eq!(at(10 * HOUR - 5 * 60_000), review, "five minutes early");
        assert_eq!(at(10 * HOUR - 30 * 60_000), None, "half an hour early is not this event");
        assert_eq!(at(11 * HOUR), None, "over");
        assert_eq!(at(3 * HOUR), None, "the all-day event never claims a call");

        let mut cancelled = event("review", 10 * HOUR, 11 * HOUR, &[]);
        cancelled.cancelled = true;
        upsert(&conn, &cancelled, 2).unwrap();
        assert_eq!(at(10 * HOUR + HOUR / 2), None, "a cancelled event is not a meeting's event");
    }

    #[test]
    fn attendee_names_skip_the_owner_and_bare_addresses_and_prefer_known_people() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO people (display_name, emails, confidence, created_at, updated_at,
                                 last_evidence_at)
             VALUES ('Hanako Tanaka', '[\"TANAKA@example.com\"]', 0.9, 1, 1, 1)",
            [],
        )
        .unwrap();
        let attendees = [
            attendee("me@example.com", Some("Me Myself"), true),
            attendee("tanaka@example.com", Some("tanaka.h"), false),
            attendee("suzuki@example.com", Some("Ichiro Suzuki"), false),
            attendee("room-3f@resource.example.com", None, false),
        ];
        let occ = upsert(&conn, &event("ev", 0, HOUR, &attendees), 1).unwrap();
        let session = |occ: Option<i64>| {
            crate::session::open(
                &conn,
                &crate::session::NewSession {
                    kind: "meeting",
                    started_at: 0,
                    title: None,
                    app_bundle_id: None,
                    calendar_occurrence_id: occ,
                    confidence: 0.8,
                    provenance: "{}",
                },
            )
            .unwrap()
        };
        let sid = session(occ);
        assert_eq!(
            session_attendee_names(&conn, sid).unwrap(),
            vec!["Hanako Tanaka".to_string(), "Ichiro Suzuki".to_string()]
        );

        // A session with no linked event has no calendar candidates.
        assert!(session_attendee_names(&conn, session(None)).unwrap().is_empty());
    }
}
