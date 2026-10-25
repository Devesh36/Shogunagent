//! User-defined scheduled jobs (issue #252, V45): run one of the user's skills on chosen weekdays
//! at a local time.
//!
//! A schedule never executes anything. When a slot comes due, [`tick`] enqueues an ordinary skill
//! run (`skill_runs`, origin `schedule`): an L1 skill runs, anything above L1 waits for the user,
//! and a skill run never sends (invariants 4 and 6). Three guards keep "it works while I'm away"
//! from turning into "it spent my plan while I was away" (invariant 5's spirit):
//!
//! - **Latest slot only.** A Mac asleep through several slots fires once, for the newest.
//! - **Catch-up window.** A slot older than [`CATCH_UP_MS`] when the Mac wakes is skipped, not run:
//!   Friday's 16:00 report is not useful on Monday.
//! - **Daily cap.** At most [`DAILY_RUN_CAP`] scheduled runs per local day across all schedules.

use rusqlite::{params, Connection, OptionalExtension};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MINUTE_MS: i64 = 60 * 1000;

/// How late a missed slot may still fire.
pub const CATCH_UP_MS: i64 = 12 * 60 * 60 * 1000;
/// Scheduled runs allowed per local day, across every schedule.
pub const DAILY_RUN_CAP: i64 = 10;
/// Schedules one Mac may hold.
pub const MAX_SCHEDULES: i64 = 50;

/// Weekday names in bit order: Monday = bit 0 … Sunday = bit 6.
pub const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// One schedule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Job {
    pub id: i64,
    pub skill: String,
    pub input: String,
    pub days: u8,
    pub minute_of_day: u16,
    pub enabled: bool,
    pub origin: String,
    pub last_slot_at: Option<i64>,
    pub last_run_id: Option<i64>,
    pub last_outcome: Option<String>,
    pub created_at: i64,
}

const COLS: &str = "id, skill, input, days, minute_of_day, enabled, origin, last_slot_at, \
                    last_run_id, last_outcome, created_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        skill: r.get(1)?,
        input: r.get(2)?,
        days: r.get::<_, i64>(3)? as u8,
        minute_of_day: r.get::<_, i64>(4)? as u16,
        enabled: r.get::<_, i64>(5)? == 1,
        origin: r.get(6)?,
        last_slot_at: r.get(7)?,
        last_run_id: r.get(8)?,
        last_outcome: r.get(9)?,
        created_at: r.get(10)?,
    })
}

/// `mon,wed,fri` · `weekdays` · `weekends` · `daily` → bitmask. `None` for anything else or empty.
pub fn parse_days(value: &str) -> Option<u8> {
    let mut mask = 0u8;
    for part in value.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
        let part = part.to_ascii_lowercase();
        mask |= match part.as_str() {
            "daily" | "everyday" | "every-day" => 0b111_1111,
            "weekdays" => 0b001_1111,
            "weekends" => 0b110_0000,
            day => {
                let i = WEEKDAYS.iter().position(|w| day.starts_with(w))?;
                1 << i
            }
        };
    }
    (mask != 0).then_some(mask)
}

/// The mask back as `mon,wed,fri`.
pub fn days_label(mask: u8) -> String {
    WEEKDAYS
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, d)| *d)
        .collect::<Vec<_>>()
        .join(",")
}

/// `16:00` / `7:30` → minute of day.
pub fn parse_time(value: &str) -> Option<u16> {
    let (h, m) = value.trim().split_once(':')?;
    let (h, m) = (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?);
    (h < 24 && m < 60 && value.trim().len() <= 5).then_some(h * 60 + m)
}

/// The newest slot at or before `now` for a schedule, at a fixed UTC offset. Looks back one week.
pub fn latest_slot(days: u8, minute_of_day: u16, now_ms: i64, offset_secs: i32) -> Option<i64> {
    let offset_ms = i64::from(offset_secs) * 1000;
    let today = (now_ms + offset_ms).div_euclid(DAY_MS);
    for back in 0..=7 {
        let day = today - back;
        // 1970-01-01 was a Thursday: Monday-based index = (day + 3) mod 7.
        let weekday = (day + 3).rem_euclid(7);
        if days & (1 << weekday) == 0 {
            continue;
        }
        let slot = day * DAY_MS + i64::from(minute_of_day) * MINUTE_MS - offset_ms;
        if slot <= now_ms {
            return Some(slot);
        }
    }
    None
}

pub fn list(conn: &Connection) -> Result<Vec<Job>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM scheduled_jobs ORDER BY id"))?;
    let rows = stmt.query_map([], row)?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Job>, rusqlite::Error> {
    conn.query_row(&format!("SELECT {COLS} FROM scheduled_jobs WHERE id = ?1"), [id], row)
        .optional()
}

/// Why a schedule was not added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    TooMany,
    Sqlite(String),
}

/// Add a schedule. Its first slot is the next one after `now` — adding a schedule at 16:05 for
/// 16:00 does not fire immediately.
#[allow(clippy::too_many_arguments)]
pub fn add(
    conn: &Connection,
    skill: &str,
    input: &str,
    days: u8,
    minute_of_day: u16,
    origin: &str,
    now: i64,
    offset_secs: i32,
) -> Result<Job, AddError> {
    let count: i64 = conn
        .query_row("SELECT count(*) FROM scheduled_jobs", [], |r| r.get(0))
        .map_err(|e| AddError::Sqlite(e.to_string()))?;
    if count >= MAX_SCHEDULES {
        return Err(AddError::TooMany);
    }
    let input: String =
        input.trim().chars().take(crate::skill_runs::MAX_INPUT_CHARS).collect();
    let input = crate::sanitize::persist_body(&input).text.into_owned();
    let already = latest_slot(days & 0b111_1111, minute_of_day, now, offset_secs);
    conn.execute(
        "INSERT INTO scheduled_jobs
           (skill, input, days, minute_of_day, enabled, origin, last_slot_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?7)",
        params![skill, input, days & 0b111_1111, minute_of_day, origin, already, now],
    )
    .map_err(|e| AddError::Sqlite(e.to_string()))?;
    get(conn, conn.last_insert_rowid())
        .map_err(|e| AddError::Sqlite(e.to_string()))?
        .ok_or_else(|| AddError::Sqlite("insert vanished".into()))
}

pub fn remove(conn: &Connection, id: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn.execute("DELETE FROM scheduled_jobs WHERE id = ?1", [id])? == 1)
}

pub fn set_enabled(conn: &Connection, id: i64, enabled: bool, now: i64) -> Result<bool, rusqlite::Error> {
    Ok(conn.execute(
        "UPDATE scheduled_jobs SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
        params![i64::from(enabled), now, id],
    )? == 1)
}

/// Scheduled runs enqueued so far on the local day containing `now`.
pub fn runs_today(conn: &Connection, now: i64, offset_secs: i32) -> Result<i64, rusqlite::Error> {
    let offset_ms = i64::from(offset_secs) * 1000;
    let start = (now + offset_ms).div_euclid(DAY_MS) * DAY_MS - offset_ms;
    conn.query_row(
        "SELECT count(*) FROM skill_runs WHERE origin = 'schedule' AND created_at >= ?1",
        [start],
        |r| r.get(0),
    )
}

/// What one tick did with a due schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub job_id: i64,
    pub outcome: &'static str,
    pub run_id: Option<i64>,
}

/// Act on every enabled schedule whose latest slot is new. `level_of` resolves a skill name to its
/// effective level (`L1`..`L3`) from the folder as it is now, or `None` if the file is gone.
pub fn tick(
    conn: &Connection,
    now: i64,
    offset_secs: i32,
    level_of: impl Fn(&str) -> Option<String>,
) -> Result<Vec<Fired>, rusqlite::Error> {
    let mut fired = Vec::new();
    for job in list(conn)?.into_iter().filter(|j| j.enabled) {
        let Some(slot) = latest_slot(job.days, job.minute_of_day, now, offset_secs) else {
            continue;
        };
        if job.last_slot_at.is_some_and(|last| slot <= last) {
            continue;
        }
        let (outcome, run_id) = if now - slot > CATCH_UP_MS {
            ("skipped_stale", None)
        } else if runs_today(conn, now, offset_secs)? >= DAILY_RUN_CAP {
            ("skipped_cap", None)
        } else if let Some(level) = level_of(&job.skill) {
            let run = crate::skill_runs::request(conn, &job.skill, &level, "schedule", &job.input, now)?;
            ("queued", Some(run.id))
        } else {
            ("skipped_missing", None)
        };
        conn.execute(
            "UPDATE scheduled_jobs
                SET last_slot_at = ?1, last_run_id = COALESCE(?2, last_run_id),
                    last_outcome = ?3, updated_at = ?4
              WHERE id = ?5",
            params![slot, run_id, outcome, now, job.id],
        )?;
        fired.push(Fired { job_id: job.id, outcome, run_id });
    }
    Ok(fired)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    /// Wednesday 2026-09-23 00:00 UTC.
    const WED: i64 = 20_719 * DAY_MS;
    const JST: i32 = 9 * 3600;

    fn l1(_: &str) -> Option<String> {
        Some("L1".into())
    }

    #[test]
    fn days_and_times_parse() {
        assert_eq!(parse_days("mon,wed,fri"), Some(0b001_0101));
        assert_eq!(parse_days("weekdays"), Some(0b001_1111));
        assert_eq!(parse_days("Daily"), Some(0b111_1111));
        assert_eq!(parse_days("friday"), Some(0b001_0000));
        assert_eq!(parse_days(""), None);
        assert_eq!(parse_days("funday"), None);
        assert_eq!(days_label(0b001_0101), "mon,wed,fri");
        assert_eq!(parse_time("16:00"), Some(960));
        assert_eq!(parse_time("7:05"), Some(425));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("9"), None);
    }

    #[test]
    fn the_latest_slot_respects_weekday_and_local_time() {
        let fri = parse_days("fri").unwrap();
        // Wednesday 12:00 UTC: the latest Friday-16:00-UTC slot is last Friday (5 days back).
        let slot = latest_slot(fri, 960, WED + 12 * HOUR, 0).unwrap();
        assert_eq!(slot, WED - 5 * DAY_MS + 16 * HOUR);
        // Daily 07:00 JST seen at Wednesday 00:30 UTC (= 09:30 JST): today's 07:00 JST = 22:00 UTC Tue.
        let daily = latest_slot(0b111_1111, 420, WED + HOUR / 2, JST).unwrap();
        assert_eq!(daily, WED - 2 * HOUR);
    }

    #[test]
    fn a_new_schedule_does_not_fire_for_a_slot_already_past() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, "weekly", "", 0b111_1111, 60, "ui", WED + 2 * HOUR, 0).unwrap();
        assert!(tick(&conn, WED + 3 * HOUR, 0, l1).unwrap().is_empty());
        // The next day's slot fires, once.
        let next = tick(&conn, WED + DAY_MS + 61 * MINUTE_MS, 0, l1).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].outcome, "queued");
        let run = crate::skill_runs::get(&conn, next[0].run_id.unwrap()).unwrap().unwrap();
        assert_eq!((run.origin.as_str(), run.status.as_str()), ("schedule", "pending"));
        assert!(tick(&conn, WED + DAY_MS + 90 * MINUTE_MS, 0, l1).unwrap().is_empty());
    }

    #[test]
    fn a_sleeping_mac_catches_up_once_and_only_while_it_is_still_useful() {
        let conn = crate::open_in_memory().unwrap();
        add(&conn, "weekly", "", 0b111_1111, 60, "ui", WED, 0).unwrap();
        // Asleep for three days; wakes 5h after the latest slot → one run for the latest slot.
        let woke = tick(&conn, WED + 3 * DAY_MS + 6 * HOUR, 0, l1).unwrap();
        assert_eq!(woke.iter().map(|f| f.outcome).collect::<Vec<_>>(), vec!["queued"]);
        // Asleep again, waking 20h after the latest slot → skipped, not run.
        let late = tick(&conn, WED + 5 * DAY_MS + 21 * HOUR, 0, l1).unwrap();
        assert_eq!(late[0].outcome, "skipped_stale");
    }

    #[test]
    fn the_daily_cap_and_a_missing_skill_skip_instead_of_running() {
        let conn = crate::open_in_memory().unwrap();
        for i in 0..DAILY_RUN_CAP {
            crate::skill_runs::request(&conn, "x", "L1", "schedule", "", WED + i).unwrap();
        }
        add(&conn, "weekly", "", 0b111_1111, 60, "ui", WED - DAY_MS, 0).unwrap();
        let capped = tick(&conn, WED + 2 * HOUR, 0, l1).unwrap();
        assert_eq!(capped[0].outcome, "skipped_cap");
        let conn2 = crate::open_in_memory().unwrap();
        add(&conn2, "gone", "", 0b111_1111, 120, "ui", WED - DAY_MS, 0).unwrap();
        let missing = tick(&conn2, WED + 3 * HOUR, 0, |_| None).unwrap();
        assert_eq!(missing[0].outcome, "skipped_missing");
    }

    #[test]
    fn a_scheduled_run_above_l1_waits_for_the_user_and_disabled_schedules_rest() {
        let conn = crate::open_in_memory().unwrap();
        let job = add(&conn, "chase", "", 0b111_1111, 60, "api", WED - DAY_MS, 0).unwrap();
        let fired = tick(&conn, WED + 2 * HOUR, 0, |_| Some("L3".into())).unwrap();
        let run = crate::skill_runs::get(&conn, fired[0].run_id.unwrap()).unwrap().unwrap();
        assert_eq!(run.status, "awaiting_confirm");
        assert!(set_enabled(&conn, job.id, false, WED).unwrap());
        assert!(tick(&conn, WED + DAY_MS + 2 * HOUR, 0, l1).unwrap().is_empty());
        assert!(remove(&conn, job.id).unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }
}
