//! Calendar arithmetic for the AI search prompts (issue #940).
//!
//! The expansion step tells the model today's local date so it can resolve "last month" or
//! 「先週」, and the model answers with local calendar dates. Both directions need a civil-date
//! conversion, and this crate has no date library, so the classic days-from-civil algorithm lives
//! here. Pure integer math: no clock, no time zone database. The caller supplies the UTC offset
//! that was in force at `now`; a range that crosses a DST change is off by at most an hour at one
//! edge, which is far below the resolution of a question like "last month".

const MS_PER_DAY: i64 = 86_400_000;
const MS_PER_MINUTE: i64 = 60_000;

/// A proleptic-Gregorian calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CivilDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(date: CivilDate) -> i64 {
    let y = i64::from(date.year) - i64::from(date.month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(date.month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(date.day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `days` after 1970-01-01 (Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> CivilDate {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    CivilDate { year, month, day }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Parse a strict `YYYY-MM-DD`. Anything else — a time, a slash, a month 13 — is `None`.
pub fn parse_iso_date(text: &str) -> Option<CivilDate> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = text.get(range)?;
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let year = digits(0..4)? as i32;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(CivilDate { year, month, day })
}

/// The local civil date at `ms` for a fixed UTC offset.
pub fn local_date(ms: i64, utc_offset_minutes: i32) -> CivilDate {
    let local = ms + i64::from(utc_offset_minutes) * MS_PER_MINUTE;
    civil_from_days(local.div_euclid(MS_PER_DAY))
}

/// The UTC instant of local midnight at the start of `date`.
pub fn local_midnight_ms(date: CivilDate, utc_offset_minutes: i32) -> i64 {
    days_from_civil(date) * MS_PER_DAY - i64::from(utc_offset_minutes) * MS_PER_MINUTE
}

/// `YYYY-MM-DD`.
pub fn format_date(date: CivilDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

/// English weekday name of `date` (1970-01-01 was a Thursday).
pub fn weekday_name(date: CivilDate) -> &'static str {
    const NAMES: [&str; 7] = [
        "Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday",
    ];
    NAMES[days_from_civil(date).rem_euclid(7) as usize]
}

/// `UTC+09:00` / `UTC-04:30`.
pub fn format_offset(utc_offset_minutes: i32) -> String {
    let sign = if utc_offset_minutes < 0 { '-' } else { '+' };
    let abs = utc_offset_minutes.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 60, abs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip_covers_leap_years_and_the_epoch() {
        for days in [-1, 0, 1, 59, 60, 365, 10_957, 20_731, 20_732, 47_482] {
            assert_eq!(days_from_civil(civil_from_days(days)), days);
        }
        assert_eq!(civil_from_days(0), CivilDate { year: 1970, month: 1, day: 1 });
        assert_eq!(
            days_from_civil(CivilDate { year: 2026, month: 10, day: 5 }),
            20_731
        );
        assert_eq!(weekday_name(CivilDate { year: 2026, month: 10, day: 5 }), "Monday");
    }

    #[test]
    fn iso_dates_parse_strictly() {
        assert_eq!(
            parse_iso_date("2024-02-29"),
            Some(CivilDate { year: 2024, month: 2, day: 29 })
        );
        for bad in ["2025-02-29", "2026-13-01", "2026-1-01", "2026/10/05", "2026-10-05T00", "", "abcd-ef-gh"] {
            assert_eq!(parse_iso_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn local_dates_follow_the_offset() {
        // 2026-10-04T20:00Z is already Monday 5 October in Tokyo.
        let ms = local_midnight_ms(CivilDate { year: 2026, month: 10, day: 4 }, 0) + 20 * 3_600_000;
        assert_eq!(local_date(ms, 540), CivilDate { year: 2026, month: 10, day: 5 });
        assert_eq!(local_date(ms, 0), CivilDate { year: 2026, month: 10, day: 4 });
        let tokyo_midnight = local_midnight_ms(CivilDate { year: 2026, month: 10, day: 5 }, 540);
        assert_eq!(local_date(tokyo_midnight, 540), CivilDate { year: 2026, month: 10, day: 5 });
        assert_eq!(local_date(tokyo_midnight - 1, 540), CivilDate { year: 2026, month: 10, day: 4 });
        assert_eq!(format_offset(540), "UTC+09:00");
        assert_eq!(format_offset(-270), "UTC-04:30");
    }
}
