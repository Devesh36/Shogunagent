//! Due dates read out of a commitment's own words (issue #253).
//!
//! The capture-path extractor ([`crate::extract`]) keeps the whole promise sentence as the
//! commitment's description — "I'll send the report by Friday", "明日までに資料を送ります" — so
//! without this module every commitment would be stored with `due_at = NULL`, leaving the
//! contradiction check ([`crate::disputes`]) with nothing to compare.
//!
//! **Deterministic, no model call** — the same rule as the rest of the capture path (invariant 5).
//! Only phrases that name one day unambiguously are recognised; anything vaguer ("soon", "next
//! week", "来週") stays `None` rather than being guessed into a date.
//!
//! The result is the *end* of the named local day (23:59:59.999), so "by Friday" is not overdue
//! until Friday is over.

use std::sync::OnceLock;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A local calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CivilDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(d: CivilDate) -> i64 {
    let y = i64::from(d.year) - i64::from(d.month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(d.month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d.day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01 (`civil_from_days`).
pub fn civil_from_days(days: i64) -> CivilDate {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    CivilDate { year, month, day }
}

/// 0 = Monday … 6 = Sunday.
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        CivilDate { year: year + 1, month: 1, day: 1 }
    } else {
        CivilDate { year, month: month + 1, day: 1 }
    };
    (days_from_civil(next) - days_from_civil(CivilDate { year, month, day: 1 })) as u32
}

/// Seconds east of UTC at an instant. The host installs the OS answer once
/// ([`set_local_offset_provider`]); without it everything is read in UTC, which is what the Linux
/// tests want and what the rest of the crate already falls back to.
type OffsetFn = fn(i64) -> i32;
static OFFSET: OnceLock<OffsetFn> = OnceLock::new();

/// Install the OS's local-offset lookup. First call wins; later calls are ignored.
pub fn set_local_offset_provider(f: OffsetFn) {
    let _ = OFFSET.set(f);
}

pub fn local_offset_secs(at_ms: i64) -> i32 {
    OFFSET.get().map_or(0, |f| f(at_ms))
}

/// The local date an instant falls on, at a fixed offset.
fn local_days(at_ms: i64, offset_secs: i32) -> i64 {
    (at_ms + i64::from(offset_secs) * 1000).div_euclid(DAY_MS)
}

/// The last millisecond of local day `days`. Uses the offset in force at the reference instant; a
/// DST change between the two can move the boundary by an hour, which does not change which day a
/// promise is for.
fn end_of_day_ms(days: i64, offset_secs: i32) -> i64 {
    (days + 1) * DAY_MS - i64::from(offset_secs) * 1000 - 1
}

/// Where a phrase points, before it is anchored to a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spec {
    /// `n` days after the reference day.
    InDays(i64),
    /// The next day that is `wd` (0 = Monday), today included.
    Weekday(i64),
    /// `wd` in the week after the current one.
    NextWeekday(i64),
    /// The last day of the reference month.
    EndOfMonth,
    /// A month/day, in the reference year or — once it has passed — the next.
    MonthDay(u32, u32),
    /// A fully specified date.
    Date(CivilDate),
}

const EN_WEEKDAYS: [(&str, i64); 14] = [
    ("monday", 0),
    ("tuesday", 1),
    ("wednesday", 2),
    ("thursday", 3),
    ("friday", 4),
    ("saturday", 5),
    ("sunday", 6),
    ("mon", 0),
    ("tue", 1),
    ("wed", 2),
    ("thu", 3),
    ("fri", 4),
    ("sat", 5),
    ("sun", 6),
];

const EN_MONTHS: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

/// Words that introduce a deadline. A bare weekday or date elsewhere in the sentence ("the Friday
/// meeting") is not a due date.
const EN_LEAD: [&str; 6] = ["by", "on", "before", "until", "till", "due"];

const JA_WEEKDAYS: [(char, i64); 7] =
    [('月', 0), ('火', 1), ('水', 2), ('木', 3), ('金', 4), ('土', 5), ('日', 6)];

fn words(lower: &str) -> Vec<&str> {
    lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '\''))
        .filter(|w| !w.is_empty())
        .collect()
}

fn en_weekday(w: &str) -> Option<i64> {
    let w = w.trim_end_matches("'s").trim_end_matches('.');
    match w {
        "tues" => return Some(1),
        "thur" | "thurs" => return Some(3),
        _ => {}
    }
    EN_WEEKDAYS
        .iter()
        .find(|(name, _)| w == *name || (name.len() == 3 && w.len() == 4 && w == format!("{name}s")))
        .map(|(_, d)| *d)
}

/// "sep", "sept", "september", "sept." — any prefix of a month name at least three letters long.
fn en_month(w: &str) -> Option<u32> {
    let w = w.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    EN_MONTHS
        .iter()
        .position(|full| full.starts_with(w) || (w == "sept" && *full == "september"))
        .map(|i| i as u32 + 1)
}

fn day_number(w: &str) -> Option<u32> {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| w.strip_suffix(suffix))
        .unwrap_or(w);
    let n: u32 = digits.parse().ok()?;
    (1..=31).contains(&n).then_some(n)
}

/// `9/30`, `09/30`, `2026-09-30`, `2026/9/30`.
fn numeric_date(w: &str) -> Option<Spec> {
    let parts: Vec<&str> = w.split(['/', '-']).collect();
    let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse::<u32>().ok()).collect();
    let nums = nums?;
    match nums.as_slice() {
        [m, d] if (1..=12).contains(m) && (1..=31).contains(d) && !w.contains('-') => {
            Some(Spec::MonthDay(*m, *d))
        }
        [y, m, d] if (2000..=2100).contains(y) && (1..=12).contains(m) && (1..=31).contains(d) => {
            Some(Spec::Date(CivilDate { year: *y as i32, month: *m, day: *d }))
        }
        _ => None,
    }
}

fn parse_en(lower: &str) -> Option<Spec> {
    // Whole-phrase forms first; order matters ("day after tomorrow" before "tomorrow").
    if lower.contains("day after tomorrow") {
        return Some(Spec::InDays(2));
    }
    if lower.contains("tomorrow") {
        return Some(Spec::InDays(1));
    }
    if lower.contains("tonight")
        || lower.contains("today")
        || lower.contains("end of day")
        || lower.contains("end of the day")
        || words(lower).contains(&"eod")
    {
        return Some(Spec::InDays(0));
    }
    if lower.contains("end of the week") || lower.contains("end of week") || words(lower).contains(&"eow") {
        return Some(Spec::Weekday(4));
    }
    if lower.contains("end of the month") || lower.contains("end of month") || words(lower).contains(&"eom") {
        return Some(Spec::EndOfMonth);
    }

    let w = words(lower);
    for i in 0..w.len() {
        // "in 3 days", "in a week"
        if w[i] == "in" {
            let n = match w.get(i + 1).copied() {
                Some("a") | Some("one") => Some(1),
                Some("two") => Some(2),
                Some("three") => Some(3),
                Some(x) => x.parse::<i64>().ok(),
                None => None,
            };
            match (n, w.get(i + 2).copied()) {
                (Some(n), Some("day" | "days")) if (1..=60).contains(&n) => {
                    return Some(Spec::InDays(n))
                }
                (Some(n), Some("week" | "weeks")) if (1..=8).contains(&n) => {
                    return Some(Spec::InDays(7 * n))
                }
                _ => {}
            }
        }

        if !EN_LEAD.contains(&w[i]) && w[i] != "this" && w[i] != "next" {
            continue;
        }
        let rest = &w[i + 1..];
        let (next, rest) = match rest.first() {
            Some(&"next") => (true, &rest[1..]),
            Some(&"this") => (false, &rest[1..]),
            _ => (w[i] == "next", rest),
        };
        let rest = if rest.first() == Some(&"the") { &rest[1..] } else { rest };
        let Some(first) = rest.first().copied() else { continue };

        if let Some(wd) = en_weekday(first) {
            return Some(if next { Spec::NextWeekday(wd) } else { Spec::Weekday(wd) });
        }
        if next {
            continue; // "next month", "next week": not one day
        }
        if let Some(spec) = numeric_date(first) {
            return Some(spec);
        }
        // "sept 30", "september 30th"
        if let (Some(m), Some(d)) = (en_month(first), rest.get(1).and_then(|x| day_number(x))) {
            return Some(Spec::MonthDay(m, d));
        }
        // "30 sept", "the 30th of september"
        if let Some(d) = day_number(first) {
            let after = if rest.get(1) == Some(&"of") { rest.get(2) } else { rest.get(1) };
            if let Some(m) = after.and_then(|x| en_month(x)) {
                return Some(Spec::MonthDay(m, d));
            }
        }
    }
    None
}

fn parse_ja(text: &str) -> Option<Spec> {
    if text.contains("明後日") || text.contains("あさって") {
        return Some(Spec::InDays(2));
    }
    if text.contains("明日") || text.contains("あした") {
        return Some(Spec::InDays(1));
    }
    if text.contains("今日") || text.contains("本日") || text.contains("今夜") {
        return Some(Spec::InDays(0));
    }
    if text.contains("今週中") {
        return Some(Spec::Weekday(4));
    }
    if text.contains("今週末") || text.contains("週末まで") {
        return Some(Spec::Weekday(6));
    }
    if text.contains("月末") {
        return Some(Spec::EndOfMonth);
    }

    let chars: Vec<char> = text.chars().collect();
    let number_before = |end: usize| -> Option<(u32, usize)> {
        let mut start = end;
        while start > 0 && chars[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start == end {
            return None;
        }
        let s: String = chars[start..end].iter().collect();
        s.parse().ok().map(|n| (n, start))
    };

    for i in 0..chars.len() {
        // "3日後"
        if chars[i] == '日' && chars.get(i + 1) == Some(&'後') {
            if let Some((n, _)) = number_before(i) {
                if (1..=60).contains(&n) {
                    return Some(Spec::InDays(i64::from(n)));
                }
            }
        }
        // "9月30日"
        if chars[i] == '月' {
            if let Some((m, _)) = number_before(i) {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if chars.get(j) == Some(&'日') {
                    if let Some((d, _)) = number_before(j) {
                        if (1..=12).contains(&m) && (1..=31).contains(&d) {
                            return Some(Spec::MonthDay(m, d));
                        }
                    }
                }
            }
        }
        // "金曜" / "金曜日" / "来週の金曜"
        if chars[i] == '曜' && i > 0 {
            if let Some((_, wd)) = JA_WEEKDAYS.iter().find(|(c, _)| *c == chars[i - 1]) {
                let before: String = chars[..i - 1].iter().collect();
                let next = before.ends_with("来週") || before.ends_with("来週の");
                return Some(if next { Spec::NextWeekday(*wd) } else { Spec::Weekday(*wd) });
            }
        }
    }
    // "9/30まで"
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '/' || c == '-')) {
        if let Some(spec) = numeric_date(token) {
            if text.contains("まで") || text.contains("までに") || text.contains("締め切り") || text.contains("期限") {
                return Some(spec);
            }
        }
    }
    None
}

/// Japanese particles that make an English date before them a deadline ("Fridayまでに",
/// "September 21に", "Monday迄"), the way "by" / "on" / "until" do before it in English.
const JA_DEADLINE_AFTER: [&str; 4] = ["までに", "まで", "迄", "に"];

/// An English date written inside a Japanese sentence (E51): "the Q3 budgetはFridayまでに…",
/// "締切はSeptember 21に変更". Japanese puts the particle after the date, so [`parse_en`]'s
/// leading "by" / "on" never appears. Only a date followed directly by a deadline particle counts
/// (a bare "Friday" in the sentence is no more a deadline than it is in English), and the text must
/// hold Japanese at all. `lower` is the lower-cased text.
fn parse_en_in_ja(lower: &str) -> Option<Spec> {
    if !lower.chars().any(|c| matches!(c as u32, 0x3040..=0x30FF | 0x4E00..=0x9FFF)) {
        return None;
    }
    // ASCII word runs with their byte ranges.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, c) in lower.char_indices() {
        let word = c.is_ascii_alphanumeric() || c == '/' || c == '-';
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, lower.len()));
    }
    let word = |i: usize| runs.get(i).map(|&(a, b)| &lower[a..b]);
    // Only spaces between the words of one date ("september 21", "next friday").
    let adjacent = |i: usize| match (runs.get(i), runs.get(i + 1)) {
        (Some(&(_, end)), Some(&(next, _))) => lower[end..next].chars().all(|c| c == ' '),
        _ => false,
    };
    let deadline_after = |i: usize| {
        runs.get(i).is_some_and(|&(_, end)| JA_DEADLINE_AFTER.iter().any(|p| lower[end..].starts_with(p)))
    };
    for i in 0..runs.len() {
        let Some(first) = word(i) else { continue };
        let next = i > 0 && word(i - 1) == Some("next") && adjacent(i - 1);
        if let Some(wd) = en_weekday(first) {
            if deadline_after(i) {
                return Some(if next { Spec::NextWeekday(wd) } else { Spec::Weekday(wd) });
            }
            continue;
        }
        if let Some(spec) = numeric_date(first) {
            if deadline_after(i) {
                return Some(spec);
            }
            continue;
        }
        if adjacent(i) {
            let second = word(i + 1).unwrap_or_default();
            // "september 21に", "21 septemberまで"
            let month_day = match (en_month(first), day_number(second), day_number(first), en_month(second)) {
                (Some(m), Some(d), _, _) | (_, _, Some(d), Some(m)) => Some(Spec::MonthDay(m, d)),
                _ => None,
            };
            if let Some(spec) = month_day {
                if deadline_after(i + 1) {
                    return Some(spec);
                }
            }
        }
    }
    None
}

fn anchor(spec: Spec, today: i64) -> Option<i64> {
    let date = civil_from_days(today);
    let days = match spec {
        Spec::InDays(n) => today + n,
        Spec::Weekday(wd) => today + (wd - weekday(today)).rem_euclid(7),
        Spec::NextWeekday(wd) => {
            let next_monday = today - weekday(today) + 7;
            next_monday + wd
        }
        Spec::EndOfMonth => days_from_civil(CivilDate {
            day: days_in_month(date.year, date.month),
            ..date
        }),
        Spec::MonthDay(month, day) => {
            let mut year = date.year;
            if day > days_in_month(year, month) {
                return None; // "2/30"
            }
            let mut d = days_from_civil(CivilDate { year, month, day });
            // A date well behind us is next year's ("by 1/15" said in December). A few days
            // behind is more likely a late mention of this year's, and a late deadline is still
            // the deadline — keep it.
            if d < today - 7 {
                year += 1;
                if day > days_in_month(year, month) {
                    return None;
                }
                d = days_from_civil(CivilDate { year, month, day });
            }
            d
        }
        Spec::Date(d) => {
            if d.day > days_in_month(d.year, d.month) {
                return None;
            }
            days_from_civil(d)
        }
    };
    // Nothing sensible is due more than a year out, or before the reference week.
    (today - 7..=today + 366).contains(&days).then_some(days)
}

/// The due instant a promise sentence names, read against `said_at_ms` (when it was captured), at
/// a fixed UTC offset. `None` when the sentence names no single day.
pub fn due_at_with_offset(text: &str, said_at_ms: i64, offset_secs: i32) -> Option<i64> {
    let lower = text.to_lowercase();
    let spec = parse_en(&lower).or_else(|| parse_ja(text)).or_else(|| parse_en_in_ja(&lower))?;
    let today = local_days(said_at_ms, offset_secs);
    anchor(spec, today).map(|days| end_of_day_ms(days, offset_secs))
}

/// Whether `text` names one day the parser reads, whenever it was said (a guard for phrases that
/// are promises only with a deadline, `extract`).
pub fn names_a_day(text: &str) -> bool {
    let lower = text.to_lowercase();
    parse_en(&lower).or_else(|| parse_ja(text)).or_else(|| parse_en_in_ja(&lower)).is_some()
}

/// [`due_at_with_offset`] in the host's local time.
pub fn due_at(text: &str, said_at_ms: i64) -> Option<i64> {
    due_at_with_offset(text, said_at_ms, local_offset_secs(said_at_ms))
}

/// The local calendar date of an instant, in the host's local time — for showing a due date.
pub fn local_date(at_ms: i64) -> CivilDate {
    civil_from_days(local_days(at_ms, local_offset_secs(at_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 2026-09-23 10:00 UTC.
    fn wed() -> i64 {
        days_from_civil(CivilDate { year: 2026, month: 9, day: 23 }) * DAY_MS + 10 * 3_600_000
    }

    fn due(text: &str) -> Option<CivilDate> {
        due_at_with_offset(text, wed(), 0).map(|ms| civil_from_days(ms.div_euclid(DAY_MS)))
    }

    fn d(month: u32, day: u32) -> Option<CivilDate> {
        Some(CivilDate { year: 2026, month, day })
    }

    #[test]
    fn the_calendar_round_trips() {
        for days in [-1000_i64, 0, 1, 59, 60, 365, 10_957, 20_719, 50_000] {
            assert_eq!(days_from_civil(civil_from_days(days)), days);
        }
        assert_eq!(civil_from_days(0), CivilDate { year: 1970, month: 1, day: 1 });
        assert_eq!(weekday(0), 3, "1970-01-01 was a Thursday");
        assert_eq!(weekday(days_from_civil(CivilDate { year: 2026, month: 9, day: 23 })), 2);
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2027, 2), 28);
    }

    #[test]
    fn english_relative_days() {
        assert_eq!(due("I'll send it today"), d(9, 23));
        assert_eq!(due("I'll have it done by EOD"), d(9, 23));
        assert_eq!(due("I'll send the deck tomorrow"), d(9, 24));
        assert_eq!(due("will get back to you the day after tomorrow"), d(9, 25));
        assert_eq!(due("I'll reply in 3 days"), d(9, 26));
        assert_eq!(due("I'll finish it in a week"), d(9, 30));
    }

    #[test]
    fn english_weekdays() {
        assert_eq!(due("I'll send the report by Friday"), d(9, 25));
        assert_eq!(due("I'll send it on Monday"), d(9, 28));
        assert_eq!(due("I'll send it by Wednesday"), d(9, 23), "today is Wednesday");
        assert_eq!(due("I'll send it next Friday"), d(10, 2));
        assert_eq!(due("I'll send it by next Tue"), d(9, 29));
        assert_eq!(due("I'll wrap it up by end of the week"), d(9, 25));
        assert_eq!(due("I'll pay by end of month"), d(9, 30));
    }

    #[test]
    fn english_dates() {
        assert_eq!(due("I'll send it by 9/30"), d(9, 30));
        assert_eq!(due("I'll send it by 2026-10-02"), d(10, 2));
        assert_eq!(due("I'll send it by Oct 5"), d(10, 5));
        assert_eq!(due("I'll send it by September 30th"), d(9, 30));
        assert_eq!(due("I'll send it by the 5th of October"), d(10, 5));
        assert_eq!(
            due("I'll send it by 1/15"),
            Some(CivilDate { year: 2027, month: 1, day: 15 }),
            "a month-day well behind us is next year's"
        );
    }

    #[test]
    fn vague_or_unanchored_phrases_stay_undated() {
        for text in [
            "I'll send it soon",
            "I'll send it next week",
            "I'll bring it to the Friday meeting",
            "I'll look at the 3/4 split",
            "I'll send it by 2/30",
            "I'll send it in 400 days",
        ] {
            assert_eq!(due(text), None, "{text}");
        }
    }

    #[test]
    fn japanese() {
        assert_eq!(due("明日までに資料を送ります"), d(9, 24));
        assert_eq!(due("今日中に返信します"), d(9, 23));
        assert_eq!(due("明後日に送ります"), d(9, 25));
        assert_eq!(due("金曜までに対応します"), d(9, 25));
        assert_eq!(due("来週の金曜日に送ります"), d(10, 2));
        assert_eq!(due("今週中に確認します"), d(9, 25));
        assert_eq!(due("9月30日までに提出します"), d(9, 30));
        assert_eq!(due("10/5までに送ります"), d(10, 5));
        assert_eq!(due("3日後に連絡します"), d(9, 26));
        assert_eq!(due("月末までに払います"), d(9, 30));
        assert_eq!(due("来週送ります"), None, "来週 alone is not a day");
    }

    #[test]
    fn english_dates_inside_japanese_sentences() {
        assert_eq!(due("the Q3 budgetはFridayまでにOren Valeさんへお送りします"), d(9, 25));
        assert_eq!(due("締切はSeptember 30に変更になりました"), d(9, 30));
        assert_eq!(due("Oct 5までに提出します"), d(10, 5));
        assert_eq!(due("5 Octoberまでに提出します"), d(10, 5));
        assert_eq!(due("next Mondayまでに共有します"), d(9, 28), "as \"by next Monday\" reads");
        assert_eq!(due("10/2迄に送ります"), d(10, 2));
    }

    #[test]
    fn an_english_date_in_japanese_without_a_deadline_particle_stays_undated() {
        for text in [
            "Fridayの定例で話しましょう",   // the Friday meeting, not a deadline
            "September の予算について",     // no day
            "Fridayは休みです",
            "I'll bring it to the Friday meeting", // English, unchanged
        ] {
            assert_eq!(due(text), None, "{text}");
        }
    }

    #[test]
    fn the_due_instant_is_the_end_of_the_local_day() {
        // 23:30 UTC on Wednesday is already Thursday in Tokyo (+9h): "tomorrow" there is Friday.
        let late = days_from_civil(CivilDate { year: 2026, month: 9, day: 23 }) * DAY_MS
            + 23 * 3_600_000
            + 30 * 60_000;
        let tokyo = 9 * 3600;
        let ms = due_at_with_offset("I'll send it tomorrow", late, tokyo).unwrap_or(0);
        let local = (ms + i64::from(tokyo) * 1000).div_euclid(DAY_MS);
        assert_eq!(civil_from_days(local), CivilDate { year: 2026, month: 9, day: 25 });
        // And it is the last millisecond of that local day.
        assert_eq!((ms + i64::from(tokyo) * 1000 + 1).rem_euclid(DAY_MS), 0);
    }
}
