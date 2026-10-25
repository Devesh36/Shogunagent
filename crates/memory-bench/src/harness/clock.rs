//! RFC 3339 timestamps without a date dependency. The workspace carries no `chrono`/`time`, and
//! one `generated_at` field is not the reason to add one.

use std::time::{SystemTime, UNIX_EPOCH};

/// Civil date from days since 1970-01-01 (proleptic Gregorian; Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a count of seconds since the Unix epoch.
pub fn rfc3339_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// The current instant, RFC 3339 UTC.
pub fn now_rfc3339() -> String {
    rfc3339_utc(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2026-10-25T12:34:56Z
        assert_eq!(rfc3339_utc(1_792_931_696), "2026-10-25T12:34:56Z");
        // Leap day.
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        // Last second of a year.
        assert_eq!(rfc3339_utc(1_767_225_599), "2025-12-31T23:59:59Z");
    }
}
