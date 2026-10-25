/// Human label for evidence citations (FR-MEM-23). Raw `source` tags stay in the DB.
pub fn evidence_source_label(source: &str) -> String {
    match source {
        "screen_ocr" | "screen_text" => "screen text".to_string(),
        "capture" => "window".to_string(),
        "meeting" => "meeting".to_string(),
        "gmail" => "mail".to_string(),
        "gcal" => "calendar".to_string(),
        "slack" => "chat".to_string(),
        "notion" => "doc".to_string(),
        "github" => "code".to_string(),
        "linear" => "issue".to_string(),
        "granola" => "meeting notes".to_string(),
        other => other.to_string(),
    }
}

/// True when the question is likely about on-screen content (visual recall path).
pub fn query_asks_about_screen(query: &str) -> bool {
    let q = query.to_ascii_lowercase();
    [
        "on my screen",
        "on screen",
        "what was on",
        "what did i see",
        "shown on",
        "displayed on",
        "looking at",
        "on my display",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Exact local calendar boundaries supplied by the OS-facing caller.
///
/// Two boundaries are required because a local day can be 23 or 25 hours across DST changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDayBounds {
    pub yesterday_start_ms: i64,
    pub today_start_ms: i64,
}

fn query_has_word(query: &str, word: &str) -> bool {
    query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| part == word)
}

/// Local time window implied by the question, as `(from_ms, to_ms)` (half-open).
///
/// English and Japanese relative dates: `today` / 今日 / 本日 / 今朝, `yesterday` / 昨日,
/// 一昨日 / `the day before yesterday`, `N days ago` / N日前, `last N days` / 過去N日,
/// `this week` / 今週, `last week` / 先週, 先々週, `this month` / 今月, `last month` / 先月,
/// `this year` / 今年, `last year` / 去年 / 昨年. Weeks start on Monday. "This …" windows end
/// at `now_ms`; past windows end where the next one starts.
///
/// Day boundaries come from `local_days` (exact local midnights, DST-safe). Week, month and year
/// starts are counted back from today's local midnight in whole 24-hour days, so a window that
/// spans a DST change can be off by that hour at its far edge. The local calendar date is
/// recovered from today's midnight assuming a UTC offset within ±12 hours.
pub fn query_time_window(
    query: &str,
    now_ms: i64,
    local_days: LocalDayBounds,
) -> Option<(i64, i64)> {
    let q = super::terms::normalize(query);
    let today = local_days.today_start_ms;
    let yesterday = local_days.yesterday_start_ms;
    let day_ago = |days: i64| today - days * DAY_MS;

    if q.contains("一昨日") || q.contains("おととい") || q.contains("day before yesterday") {
        return Some((yesterday - (today - yesterday), yesterday));
    }
    if q.contains("yesterday") || q.contains("昨日") || q.contains("きのう") {
        return Some((yesterday, today));
    }
    if let Some(days) =
        count_before(&q, &["日前"]).or_else(|| count_before(&q, &[" days ago", " day ago"]))
    {
        if (1..=366).contains(&days) {
            return Some((day_ago(days), day_ago(days - 1)));
        }
    }
    if let Some(days) = count_after(&q, &["過去"], &["日"])
        .or_else(|| count_after(&q, &["last ", "past "], &[" days", " day"]))
    {
        if (1..=366).contains(&days) {
            return Some((now_ms - days * DAY_MS, now_ms));
        }
    }
    if q.contains("today")
        || q.contains("this morning")
        || q.contains("earlier today")
        || q.contains("今日")
        || q.contains("本日")
        || q.contains("今朝")
        || q.contains("きょう")
    {
        return Some((today, now_ms));
    }
    let civil = civil_from_days(local_day_index(today));
    let week_start = today - civil.weekday_from_monday * DAY_MS;
    if q.contains("先々週") || q.contains("week before last") {
        return Some((week_start - 14 * DAY_MS, week_start - 7 * DAY_MS));
    }
    if q.contains("先週") || q.contains("last week") || q.contains("previous week") {
        return Some((week_start - 7 * DAY_MS, week_start));
    }
    if q.contains("今週") || q.contains("this week") {
        return Some((week_start, now_ms));
    }
    let month_start = today - (civil.day - 1) * DAY_MS;
    if q.contains("先月") || q.contains("last month") || q.contains("previous month") {
        let previous_month_days = days_in_month(civil.year, civil.month, -1);
        return Some((month_start - previous_month_days * DAY_MS, month_start));
    }
    if q.contains("今月") || q.contains("this month") {
        return Some((month_start, now_ms));
    }
    let year_start = today - civil.day_of_year * DAY_MS;
    if q.contains("去年") || q.contains("昨年") || q.contains("last year") {
        let previous_year_days = if is_leap(civil.year - 1) { 366 } else { 365 };
        return Some((year_start - previous_year_days * DAY_MS, year_start));
    }
    if q.contains("今年") || q.contains("this year") {
        return Some((year_start, now_ms));
    }
    None
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn is_count_char(character: char) -> bool {
    character.is_ascii_digit() || super::terms::is_kanji_numeral(character)
}

/// The number written right before one of `suffixes` (`3日前`, `3 days ago`, `三日前`).
fn count_before(text: &str, suffixes: &[&str]) -> Option<i64> {
    for suffix in suffixes {
        for (at, _) in text.match_indices(suffix) {
            let mut digits = text[..at]
                .chars()
                .rev()
                .take_while(|character| is_count_char(*character))
                .collect::<Vec<_>>();
            digits.reverse();
            if let Some(value) = parse_number(&digits) {
                return Some(value);
            }
        }
    }
    None
}

/// The number written right after one of `prefixes` and before one of `suffixes` (`過去7日`).
fn count_after(text: &str, prefixes: &[&str], suffixes: &[&str]) -> Option<i64> {
    for prefix in prefixes {
        for (at, _) in text.match_indices(prefix) {
            let rest = &text[at + prefix.len()..];
            let digits = rest
                .chars()
                .take_while(|character| is_count_char(*character))
                .collect::<Vec<_>>();
            let digits_len = digits.iter().map(|character| character.len_utf8()).sum::<usize>();
            if digits.is_empty()
                || !suffixes
                    .iter()
                    .any(|suffix| rest[digits_len..].starts_with(suffix))
            {
                continue;
            }
            if let Some(value) = parse_number(&digits) {
                return Some(value);
            }
        }
    }
    None
}

/// ASCII digits, or kanji numerals up to 99 (三, 十, 二十一).
fn parse_number(characters: &[char]) -> Option<i64> {
    if characters.is_empty() || characters.len() > 4 {
        return None;
    }
    if characters.iter().all(char::is_ascii_digit) {
        return characters.iter().collect::<String>().parse().ok();
    }
    let digit = |character: char| -> Option<i64> {
        Some(match character {
            '一' => 1,
            '二' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            _ => return None,
        })
    };
    match characters {
        ['十'] => Some(10),
        [single] => digit(*single),
        ['十', ones] => Some(10 + digit(*ones)?),
        [tens, '十'] => Some(digit(*tens)? * 10),
        [tens, '十', ones] => Some(digit(*tens)? * 10 + digit(*ones)?),
        _ => None,
    }
}

struct LocalCivil {
    year: i64,
    month: i64,
    day: i64,
    day_of_year: i64,
    weekday_from_monday: i64,
}

/// Days since 1970-01-01 of the local date whose midnight is `today_start_ms` (|offset| < 12 h).
fn local_day_index(today_start_ms: i64) -> i64 {
    (today_start_ms + DAY_MS / 2).div_euclid(DAY_MS)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in the month `offset` months from (`year`, `month`).
fn days_in_month(year: i64, month: i64, offset: i64) -> i64 {
    let index = year * 12 + (month - 1) + offset;
    let (year, month) = (index.div_euclid(12), index.rem_euclid(12) + 1);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> LocalCivil {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    const CUMULATIVE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let month_index = usize::try_from(month - 1).unwrap_or(0).min(11);
    let day_of_year =
        CUMULATIVE[month_index] + day - 1 + i64::from(month > 2 && is_leap(year));
    LocalCivil {
        year,
        month,
        day,
        day_of_year,
        // 1970-01-01 was a Thursday (index 3 counting from Monday).
        weekday_from_monday: (days + 3).rem_euclid(7),
    }
}

/// True when the agent should pull stored screen frames (visual recall path).
pub fn query_wants_visual_recall(query: &str, now_ms: i64, local_days: LocalDayBounds) -> bool {
    if query_asks_about_screen(query) {
        return true;
    }
    query_time_window(query, now_ms, local_days).is_some_and(|_| {
        let q = query.to_ascii_lowercase();
        ["screen", "window", "see", "look", "show", "display", "app"]
            .iter()
            .any(|word| query_has_word(&q, word))
    })
}

/// Default time window for visual-recall frame search.
pub fn visual_recall_window(
    query: &str,
    now_ms: i64,
    local_days: LocalDayBounds,
    retention_ms: i64,
) -> (i64, i64) {
    let retention_start = now_ms.saturating_sub(retention_ms.max(0));
    let (from_ms, to_ms) =
        query_time_window(query, now_ms, local_days).unwrap_or((retention_start, now_ms));
    (from_ms.max(retention_start), to_ms.min(now_ms))
}

#[cfg(test)]
mod time_window_tests {
    use super::*;

    /// Wednesday 2026-10-14 00:00 UTC, with UTC local days.
    const TODAY: i64 = 20_740 * DAY_MS;
    const NOW: i64 = TODAY + 12 * 3_600_000;

    fn window(query: &str) -> Option<(i64, i64)> {
        query_time_window(
            query,
            NOW,
            LocalDayBounds {
                yesterday_start_ms: TODAY - DAY_MS,
                today_start_ms: TODAY,
            },
        )
    }

    fn day(offset: i64) -> i64 {
        TODAY + offset * DAY_MS
    }

    #[test]
    fn the_test_clock_is_a_wednesday() {
        let civil = civil_from_days(local_day_index(TODAY));
        assert_eq!((civil.year, civil.month, civil.day), (2026, 10, 14));
        assert_eq!(civil.weekday_from_monday, 2);
        assert_eq!(civil.day_of_year, 286);
    }

    #[test]
    fn local_midnight_east_and_west_of_utc_maps_to_the_same_date() {
        // Tokyo (+9) and Los Angeles (-7) midnights of 2026-10-14.
        assert_eq!(local_day_index(TODAY - 9 * 3_600_000), 20_740);
        assert_eq!(local_day_index(TODAY + 7 * 3_600_000), 20_740);
    }

    #[test]
    fn days() {
        assert_eq!(window("昨日の会議"), Some((day(-1), day(0))));
        assert_eq!(window("きのうのメモ"), Some((day(-1), day(0))));
        assert_eq!(window("一昨日の打ち合わせ"), Some((day(-2), day(-1))));
        assert_eq!(window("今日の予定"), Some((day(0), NOW)));
        assert_eq!(window("本日の議事録"), Some((day(0), NOW)));
        assert_eq!(window("what did I do today"), Some((day(0), NOW)));
        assert_eq!(window("yesterday's standup"), Some((day(-1), day(0))));
    }

    #[test]
    fn n_days_ago_and_last_n_days() {
        assert_eq!(window("3日前のメモ"), Some((day(-3), day(-2))));
        assert_eq!(window("三日前のメモ"), Some((day(-3), day(-2))));
        assert_eq!(window("３日前"), Some((day(-3), day(-2))), "full-width digits");
        assert_eq!(window("notes from 5 days ago"), Some((day(-5), day(-4))));
        assert_eq!(window("過去7日の障害"), Some((NOW - 7 * DAY_MS, NOW)));
        assert_eq!(window("errors in the last 7 days"), Some((NOW - 7 * DAY_MS, NOW)));
        assert_eq!(window("十日前"), Some((day(-10), day(-9))));
        assert_eq!(window("二十一日前"), Some((day(-21), day(-20))));
    }

    #[test]
    fn weeks_start_on_monday() {
        // Monday 2026-10-12 is day(-2).
        assert_eq!(window("今週の定例"), Some((day(-2), NOW)));
        assert_eq!(window("this week"), Some((day(-2), NOW)));
        assert_eq!(window("先週の定例会議"), Some((day(-9), day(-2))));
        assert_eq!(window("last week"), Some((day(-9), day(-2))));
        assert_eq!(window("先々週"), Some((day(-16), day(-9))));
    }

    #[test]
    fn months_and_years() {
        // October 1 is day(-13); September has 30 days.
        assert_eq!(window("今月の請求"), Some((day(-13), NOW)));
        assert_eq!(window("先月の請求書"), Some((day(-43), day(-13))));
        assert_eq!(window("last month"), Some((day(-43), day(-13))));
        assert_eq!(window("今年"), Some((day(-286), NOW)));
        assert_eq!(window("去年の決算"), Some((day(-286 - 365), day(-286))));
    }

    #[test]
    fn no_cue_no_window() {
        assert_eq!(window("予算の承認"), None);
        assert_eq!(window("25日に締め"), None, "a date of the month is not N days ago");
        assert_eq!(window("weekly sync"), None);
    }
}
