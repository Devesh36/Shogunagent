//! Deterministic pre-filter for the text the nightly Batch lane sends (#286). No model, no I/O.
//!
//! Captured accessibility text is mostly UI: navigation labels, window controls, cookie banners,
//! badge counters, and the same page captured again. All of it is billed on the Select KK key
//! (invariant 5) and all of it leaves the device for nothing (invariant 3). This module removes
//! it before `jobs::build_batch_items` (behind the `db` feature) wraps what is left in the
//! classification prompt.
//!
//! # The contract
//!
//! - **Deterministic.** Same input, same output, on every machine. No clock, no randomness, no
//!   model, no locale lookups — only the fixed lists and thresholds below.
//! - **Conservative.** A rule either matches a whole line that carries no meaning on its own
//!   (a lone "Close" or "12"), or removes a line only when an earlier item in the same window
//!   already sent it. Nothing is truncated inside a line and nothing is paraphrased.
//! - **Every content line is sent at least once.** Any line that survives the fixed per-line
//!   rules (1–5 below) reaches the model in at least one item of the window. The window-level
//!   rules (6–8) only drop a line, or an item, that was already sent — tested as a property over
//!   a synthetic day in `jobs.rs`.
//! - **Counts only.** [`BatchPrefilterStats`] holds integers. It never holds a line, a hash of a
//!   line, or anything else derived from captured text (CLAUDE.md telemetry rule).
//!
//! # Rules
//!
//! Per line (each line is first whitespace-normalised; the result is what is sent):
//!
//! 0. **Whitespace.** Runs of spaces/tabs/NBSP/ideographic spaces inside a line collapse to one
//!    space and the ends are trimmed. Zero-width characters that carry no text (U+200B, U+2060,
//!    U+FEFF, soft hyphen) are removed; the emoji joiner U+200D is kept.
//! 1. `blank` — nothing left after rule 0. (This is what "long runs of whitespace" become.)
//! 2. `symbol_only` — only punctuation and UI glyphs: separators, bullets, arrows, box drawing,
//!    check marks, "⋯", CJK brackets and full-width punctuation. A line with any letter, digit or
//!    emoji is never `symbol_only`, so a "👍" reply survives.
//! 3. `badge_count` — a bare 1–3 digit integer, optionally with a trailing `+` and optionally in
//!    one pair of `()`/`[]`: unread badges and pagination ("3", "99+", "(12)"). Anything with a
//!    separator is kept on purpose — "10:42", "2026-09-30", "4 / 12", "1,200" can be the date or
//!    the amount a commitment depends on.
//! 4. `window_control` — the whole line equals (case-insensitively) a window, tab or browser
//!    control: [`WINDOW_CONTROLS`].
//! 5. `consent` — the whole line equals a cookie/consent or sign-in control ([`CONSENT_LABELS`]),
//!    **or** it is a cookie-banner sentence: it starts with one of [`COOKIE_BANNER_PREFIXES`],
//!    mentions cookies, carries a consent marker ("accept", "consent", "agree", "policy", …) and
//!    is at most [`COOKIE_BANNER_MAX_CHARS`] long. A design note that says "We use cookies for
//!    session auth" has no consent marker and is kept.
//!
//! Per window (items are processed in capture order):
//!
//! 6. `repeated_label` — navigation/menu chrome. A line is *label-like* when it is short
//!    (≤ [`LABEL_MAX_CHARS`] chars, ≤ [`LABEL_MAX_WORDS`] words, ≤ [`LABEL_MAX_CJK_CHARS`] CJK
//!    chars) and has no sentence punctuation, `@` or URL. It is dropped only when **all** hold:
//!    - it sits in a run of ≥ [`LABEL_RUN_MIN`] consecutive label-like lines (a menu, not a
//!      lone "Sure" between sentences);
//!    - it has the **same neighbours inside that run** (the label before and after it, or the
//!      run's edge) in ≥ [`REPEAT_MIN_ITEMS`] distinct items of the window. A menu is the same
//!      sequence every time; a short chat reply sits next to a different timestamp or message
//!      each time, so it never qualifies even if the word itself recurs;
//!    - that same line-with-neighbours was already sent by an earlier item, or earlier in this
//!      item. The first copy is always sent.
//! 7. `duplicate` (item) — after rules 0–5 the item's text equals an earlier item's text. This
//!    is #561's exact-duplicate rule, keyed on the cleaned text so two captures of a page that
//!    differ only in chrome or whitespace collapse too.
//! 8. `near_duplicate` (item) — every line of the item (after rules 0–5) already appears in the
//!    immediately preceding kept item: a re-read of the same view that shows nothing new (a
//!    scroll back, a collapsed panel). Capture-time dedup (FR-CAP-03, `capture/dedup.rs`) already
//!    folds ≥98%-similar bodies as they arrive; this catches what arrives out of that window.
//!    Similarity is deliberately *not* used here: a 98%-similar page can differ by exactly the
//!    one sentence that holds a promise, and a subset test cannot lose it.
//!
//! Items that end with no line left are `empty` (nothing but whitespace to begin with) or
//! `chrome_only` (every line matched a rule).
//!
//! # Why rule 6 does not group by app/window
//!
//! The Batch proof type (`shogun_memory::event_log::BatchEventText`) carries only the event id
//! and its text, and widening it would reopen the A-2 source-filter boundary. Rule 6 therefore
//! counts repetitions across the whole night rather than per app. That is safe because it never
//! drops a first copy — grouping by app would only make it drop *more*. Grouping by window does
//! happen one stage later, in `super::session` (feature `db`), which reads the grouping metadata that rides
//! *beside* the proof type and folds the surviving lines into one request per window-hour.

use std::collections::{HashMap, HashSet};

/// Rule 6: longest line (in chars) that can be navigation chrome.
pub const LABEL_MAX_CHARS: usize = 32;
/// Rule 6: most whitespace-separated words a label may have.
pub const LABEL_MAX_WORDS: usize = 4;
/// Rule 6: CJK text has no spaces, so the word limit cannot bound it. Japanese menu labels
/// ("受信トレイ", "スター付き", "下書き") are well under this; most sentences are over it.
pub const LABEL_MAX_CJK_CHARS: usize = 10;
/// Rule 6: a label is only chrome inside a run of at least this many consecutive labels.
pub const LABEL_RUN_MIN: usize = 3;
/// Rule 6: ...and only when that label recurs in label runs of at least this many items.
pub const REPEAT_MIN_ITEMS: usize = 3;
/// Rule 5: a cookie banner longer than this is treated as prose and kept.
pub const COOKIE_BANNER_MAX_CHARS: usize = 400;

/// Rule 4: whole-line window / tab / browser controls (compared lower-cased).
pub const WINDOW_CONTROLS: &[&str] = &[
    "close",
    "minimize",
    "minimise",
    "zoom",
    "full screen",
    "enter full screen",
    "exit full screen",
    "close window",
    "close tab",
    "new tab",
    "new window",
    "reload",
    "reload this page",
    "back",
    "forward",
    "go back",
    "go forward",
    "show sidebar",
    "hide sidebar",
    "toggle sidebar",
    "more options",
    "more actions",
    "main menu",
    "open menu",
    "close menu",
    "skip to content",
    "skip to main content",
    "skip navigation",
    "back to top",
    "scroll to top",
    "閉じる",
    "最小化",
    "拡大/縮小",
    "新規タブ",
    "戻る",
    "進む",
];

/// Rule 5: whole-line cookie/consent and sign-in controls (compared lower-cased).
pub const CONSENT_LABELS: &[&str] = &[
    "accept",
    "accept all",
    "accept all cookies",
    "accept cookies",
    "allow all",
    "allow all cookies",
    "allow cookies",
    "reject all",
    "reject all cookies",
    "decline all",
    "necessary cookies only",
    "only necessary cookies",
    "use necessary cookies only",
    "manage cookies",
    "cookie settings",
    "cookies settings",
    "cookie preferences",
    "manage preferences",
    "customize cookies",
    "customise cookies",
    "sign in",
    "sign up",
    "log in",
    "login",
    "sign out",
    "log out",
    "logout",
    "create account",
    "create an account",
    "forgot password?",
    "forgot your password?",
    "remember me",
    "keep me signed in",
    "stay signed in",
    "continue with google",
    "continue with apple",
    "continue with microsoft",
    "sign in with google",
    "sign in with apple",
    "sign in with microsoft",
    "don't have an account?",
    "already have an account?",
    "or",
    "ログイン",
    "サインイン",
    "ログアウト",
    "新規登録",
    "すべて許可",
    "すべて拒否",
    "同意する",
    "cookie設定",
];

/// Rule 5: how a cookie-banner sentence starts (compared lower-cased).
pub const COOKIE_BANNER_PREFIXES: &[&str] = &[
    "we use cookies",
    "we and our partners use cookies",
    "this website uses cookies",
    "this site uses cookies",
    "our website uses cookies",
    "by continuing to use",
    "by clicking",
    "by using this site",
    "by using our site",
    "by using this website",
    "当サイトでは",
    "本サイトでは",
    "このサイトでは",
    "当ウェブサイトでは",
];

/// Rule 5: a cookie banner asks for consent; a sentence about cookies that does not is kept.
const COOKIE_CONSENT_MARKERS: &[&str] = &[
    "accept", "consent", "agree", "policy", "preferences", "同意", "ポリシー",
];

/// Why a line did not reach the model — one counter per rule. Counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineDrops {
    pub blank: usize,
    pub symbol_only: usize,
    pub badge_count: usize,
    pub window_control: usize,
    pub consent: usize,
    pub repeated_label: usize,
}

impl LineDrops {
    /// Every dropped line.
    pub fn total(&self) -> usize {
        self.blank
            + self.symbol_only
            + self.badge_count
            + self.window_control
            + self.consent
            + self.repeated_label
    }

    /// `(rule, count)` pairs in a fixed order, for storage. The names are the rule names in the
    /// module docs; they are fixed strings and never derived from input.
    pub fn by_rule(&self) -> [(&'static str, usize); 6] {
        [
            ("blank", self.blank),
            ("symbol_only", self.symbol_only),
            ("badge_count", self.badge_count),
            ("window_control", self.window_control),
            ("consent", self.consent),
            ("repeated_label", self.repeated_label),
        ]
    }
}

/// What the pre-filter did to one night's window. Integers only — see the module contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchPrefilterStats {
    /// Items considered (the window's cloud-eligible events).
    pub input: usize,
    /// Items that will be sent.
    pub kept: usize,
    /// Items with nothing but whitespace.
    pub dropped_empty: usize,
    /// Items whose cleaned text equals an earlier item's (rule 7).
    pub dropped_duplicate: usize,
    /// Items that show nothing the previous kept item did not (rule 8).
    pub dropped_near_duplicate: usize,
    /// Items where every line matched a rule.
    pub dropped_chrome_only: usize,
    /// Lines looked at, across all items.
    pub lines_considered: usize,
    /// Lines removed, per rule. Lines of dropped items are not counted here; they are in the
    /// item counters above.
    pub line_drops: LineDrops,
    /// Characters of captured text that would have been sent without the pre-filter (the
    /// per-item excerpts the caller passed in).
    pub chars_before: usize,
    /// Characters of captured text left after the per-line and per-item rules (0–8).
    pub chars_after: usize,

    // Sessionisation (zero-base plan L1, `super::session`). Zero when only the per-line
    // rules ran (`prefilter_texts` alone); `jobs::build_batch_items_with_stats` fills them and
    // then sets `kept` to `sessions`, so `items_sent` in the stored row is the number of
    // requests that left the device.
    /// Requests sent: one per (app, window, hour) session that kept at least one line.
    pub sessions: usize,
    /// Items (rows) folded into sessions after L0(a): `input` = this + [`Self::dropped`] +
    /// `rows_dropped_shadowed` + `rows_dropped_covered`. A row whose every line repeated an earlier line of its session
    /// is still counted here (it was read; nothing of it was sent twice).
    pub rows_in_sessions: usize,
    /// Secondary-source rows (`screen_ocr`) dropped because their window-hour also had primary
    /// (`capture`) text (L0(a)).
    pub rows_dropped_shadowed: usize,
    /// Rows set aside because another Batch lane reads their source tonight (L0(b): `gmail` rows
    /// while mail triage is on — Stage B extracts their commitments and open loops). Not in
    /// `rows_in_sessions` and not in [`Self::dropped`]: they were never the pre-filter's.
    pub rows_dropped_covered: usize,
    /// Lines handed to sessionisation (after rules 0–8).
    pub session_lines_in: usize,
    /// Lines removed inside a session as exact or near duplicates of an earlier line.
    pub session_lines_deduped: usize,
    /// Lines removed to fit a session under its character cap.
    pub session_lines_capped: usize,
    /// Characters of captured text in the session bodies actually sent.
    pub chars_sent: usize,
}

impl BatchPrefilterStats {
    /// Fraction of the window's **items** that will not leave the device. Zero when the window
    /// was empty. (Token reduction, which is what costs money, is [`reduction_rate`].)
    pub fn reduction_ratio(&self) -> f32 {
        if self.input == 0 {
            0.0
        } else {
            1.0 - (self.kept as f32 / self.input as f32)
        }
    }

    /// Items dropped for any reason.
    pub fn dropped(&self) -> usize {
        self.dropped_empty
            + self.dropped_duplicate
            + self.dropped_near_duplicate
            + self.dropped_chrome_only
    }
}

/// OPEN-B2's number: the share of estimated Batch input tokens the pre-filter kept on the
/// device, in `[0, 1]`. `0.0` when nothing would have been sent (no evidence either way) and when
/// the filter did not shrink the input (it never grows it; a larger `after` is clamped rather
/// than reported as a negative saving). `0.35` reads "35% fewer input tokens sent than without
/// the pre-filter".
pub fn reduction_rate(tokens_before: u64, tokens_after: u64) -> f64 {
    if tokens_before == 0 || tokens_after >= tokens_before {
        return 0.0;
    }
    (tokens_before - tokens_after) as f64 / tokens_before as f64
}

/// Estimated input tokens for a request body of `bytes` bytes: ~4 bytes per token, rounded up.
/// The same heuristic as [`Usage::estimated_from_bytes`](crate::llm::usage::Usage::estimated_from_bytes),
/// which the relay route's `llm_usage` rows already use, so the two meters agree. Good for a
/// ratio and a trend (±10% on English, worse on CJK); never for billing.
pub fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

/// One item that survived, with the text to send in place of its excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptText {
    /// Position in the input slice.
    pub index: usize,
    /// The cleaned text: surviving lines joined by `\n`.
    pub text: String,
}

/// Run rules 0–8 over one window of excerpts, in capture order. Returns the surviving items (by
/// input index) and the counts.
pub fn prefilter_texts(excerpts: &[&str]) -> (Vec<KeptText>, BatchPrefilterStats) {
    let mut stats = BatchPrefilterStats {
        input: excerpts.len(),
        ..BatchPrefilterStats::default()
    };

    // Pass 1: per-line rules, and the label-run census rule 6 needs before it can decide.
    struct Item {
        lines: Vec<String>,
        /// Rule 6 key per line; `None` outside a label run.
        in_run: Vec<Option<LabelKey>>,
        blank_input: bool,
    }
    let mut items = Vec::with_capacity(excerpts.len());
    let mut run_items: HashMap<LabelKey, usize> = HashMap::new();
    for excerpt in excerpts {
        stats.chars_before += excerpt.chars().count();
        let mut lines = Vec::new();
        for raw in excerpt.lines() {
            stats.lines_considered += 1;
            let line = normalize_line(raw);
            match fixed_rule(&line) {
                Some(Rule::Blank) => stats.line_drops.blank += 1,
                Some(Rule::SymbolOnly) => stats.line_drops.symbol_only += 1,
                Some(Rule::BadgeCount) => stats.line_drops.badge_count += 1,
                Some(Rule::WindowControl) => stats.line_drops.window_control += 1,
                Some(Rule::Consent) => stats.line_drops.consent += 1,
                None => lines.push(line),
            }
        }
        let in_run = label_keys(&lines);
        let mut counted = HashSet::new();
        for key in in_run.iter().flatten() {
            if counted.insert(key) {
                *run_items.entry(key.clone()).or_insert(0) += 1;
            }
        }
        items.push(Item {
            lines,
            in_run,
            blank_input: excerpt.trim().is_empty(),
        });
    }

    // Pass 2: window rules, in order. `sent_labels` holds label lines already sent.
    let mut kept = Vec::new();
    let mut seen_texts: HashSet<String> = HashSet::new();
    let mut previous_kept: HashSet<String> = HashSet::new();
    let mut sent_labels: HashSet<LabelKey> = HashSet::new();
    for (index, item) in items.into_iter().enumerate() {
        if item.lines.is_empty() {
            if item.blank_input {
                stats.dropped_empty += 1;
            } else {
                stats.dropped_chrome_only += 1;
            }
            continue;
        }
        // Rule 7. The key is the cleaned text; the set holds only texts whose every line has
        // been (or is about to be) sent, which is every item that gets past this point.
        let key = item.lines.join("\n");
        if seen_texts.contains(&key) {
            stats.dropped_duplicate += 1;
            continue;
        }
        seen_texts.insert(key);
        // Rule 8.
        if !previous_kept.is_empty() && item.lines.iter().all(|l| previous_kept.contains(l)) {
            stats.dropped_near_duplicate += 1;
            continue;
        }
        // Rule 6.
        let mut out = Vec::with_capacity(item.lines.len());
        for (line, key) in item.lines.iter().zip(&item.in_run) {
            if let Some(key) = key {
                let recurring = run_items.get(key).copied().unwrap_or(0) >= REPEAT_MIN_ITEMS;
                if recurring && sent_labels.contains(key) {
                    stats.line_drops.repeated_label += 1;
                    continue;
                }
                sent_labels.insert(key.clone());
            }
            out.push(line.as_str());
        }
        previous_kept = item.lines.iter().cloned().collect();
        if out.is_empty() {
            // Every line was a label already sent: the item adds nothing.
            stats.dropped_chrome_only += 1;
            continue;
        }
        let text = out.join("\n");
        stats.chars_after += text.chars().count();
        kept.push(KeptText { index, text });
    }
    stats.kept = kept.len();
    (kept, stats)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Blank,
    SymbolOnly,
    BadgeCount,
    WindowControl,
    Consent,
}

/// Rule 0: collapse whitespace runs, trim, and drop zero-width characters that carry no text.
/// Also how [`super::session`] maps a sent line back to the raw line it came from.
pub(crate) fn normalize_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for word in raw
        .split(|c: char| c.is_whitespace())
        .filter(|w| !w.is_empty())
    {
        let word: String = word
            .chars()
            .filter(|c| !matches!(*c, '\u{200B}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'))
            .collect();
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    out
}

/// Rules 1–5 on a normalised line.
fn fixed_rule(line: &str) -> Option<Rule> {
    if line.is_empty() {
        return Some(Rule::Blank);
    }
    if line.chars().all(|c| c == ' ' || is_ui_symbol(c)) {
        return Some(Rule::SymbolOnly);
    }
    if is_badge_count(line) {
        return Some(Rule::BadgeCount);
    }
    let lower = line.to_lowercase();
    if WINDOW_CONTROLS.contains(&lower.as_str()) {
        return Some(Rule::WindowControl);
    }
    if CONSENT_LABELS.contains(&lower.as_str()) || is_cookie_banner(&lower) {
        return Some(Rule::Consent);
    }
    None
}

/// Punctuation and UI glyphs with no text of their own. Letters, digits and emoji are excluded.
fn is_ui_symbol(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c as u32,
            0x00A1..=0x00BF          // Latin-1 punctuation and signs: ¡ « » · ¿ © ° …
            | 0x00D7 | 0x00F7        // × ÷
            | 0x2000..=0x206F        // General Punctuation: – — ‘ ’ “ ” • … ‹ ›
            | 0x2190..=0x21FF        // Arrows
            | 0x2200..=0x22FF        // Mathematical operators (incl. ⋮ ⋯)
            | 0x2318 | 0x2325 | 0x238B | 0x23CE // ⌘ ⌥ ⎋ ⏎
            | 0x2500..=0x25FF        // Box drawing, block elements, geometric shapes
            | 0x2605 | 0x2606 | 0x2630 // ★ ☆ ☰
            | 0x2713..=0x2718        // ✓ ✔ ✕ ✖ ✗ ✘
            | 0x3000..=0x303F        // CJK symbols and punctuation: 、 。 「 」 【 】
            | 0x30FB                 // ・
            | 0xFE10..=0xFE1F        // Vertical forms
            | 0xFE30..=0xFE4F        // CJK compatibility forms
            | 0xFF01..=0xFF0F        // Full-width punctuation
            | 0xFF1A..=0xFF20
            | 0xFF3B..=0xFF40
            | 0xFF5B..=0xFF65
        )
}

/// Rule 3: "3", "99+", "(12)", "[7]".
fn is_badge_count(line: &str) -> bool {
    let inner = line
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .or_else(|| line.strip_prefix('[').and_then(|s| s.strip_suffix(']')))
        .unwrap_or(line);
    let digits = inner.strip_suffix('+').unwrap_or(inner);
    (1..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Rule 5, sentence form. `lower` is the lower-cased normalised line.
fn is_cookie_banner(lower: &str) -> bool {
    lower.chars().count() <= COOKIE_BANNER_MAX_CHARS
        && COOKIE_BANNER_PREFIXES.iter().any(|p| lower.starts_with(p))
        && (lower.contains("cookie") || lower.contains("クッキー"))
        && COOKIE_CONSENT_MARKERS.iter().any(|m| lower.contains(m))
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xF900..=0xFAFF // CJK compatibility ideographs
    )
}

/// Rule 6: short, no sentence punctuation, no address or link.
fn is_label_like(line: &str) -> bool {
    let chars = line.chars().count();
    chars <= LABEL_MAX_CHARS
        && line.split(' ').count() <= LABEL_MAX_WORDS
        && line.chars().filter(|c| is_cjk(*c)).count() <= LABEL_MAX_CJK_CHARS
        && !line
            .chars()
            .any(|c| matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '@'))
        && !line.contains("://")
}

/// Rule 6: a label with its neighbours inside its run (`None` at the run's edge).
type LabelKey = (Option<String>, String, Option<String>);

/// Rule 6: for each line, its [`LabelKey`] if it sits in a run of ≥ [`LABEL_RUN_MIN`]
/// consecutive label-like lines, else `None`.
fn label_keys(lines: &[String]) -> Vec<Option<LabelKey>> {
    let mut keys = vec![None; lines.len()];
    let mut start = 0;
    while start < lines.len() {
        if !is_label_like(&lines[start]) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < lines.len() && is_label_like(&lines[end]) {
            end += 1;
        }
        if end - start >= LABEL_RUN_MIN {
            for i in start..end {
                let prev = (i > start).then(|| lines[i - 1].clone());
                let next = (i + 1 < end).then(|| lines[i + 1].clone());
                keys[i] = Some((prev, lines[i].clone(), next));
            }
        }
        start = end;
    }
    keys
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Run the filter and return the texts that would be sent, in order.
    fn sent(excerpts: &[&str]) -> Vec<String> {
        prefilter_texts(excerpts).0.into_iter().map(|k| k.text).collect()
    }

    fn one(excerpt: &str) -> String {
        sent(&[excerpt]).join("\n")
    }

    // rule 0 / 1: whitespace

    #[test]
    fn whitespace_runs_collapse_and_blank_lines_go() {
        let (kept, stats) = prefilter_texts(&["  Draft   the\t\tQ3   plan \n\n\n   \n\u{200B}\nby Friday"]);
        assert_eq!(kept[0].text, "Draft the Q3 plan\nby Friday");
        assert_eq!(stats.line_drops.blank, 4);
    }

    #[test]
    fn whitespace_rule_keeps_every_word_and_the_emoji_joiner() {
        // Negative: nothing but spacing changes; a ZWJ emoji sequence stays intact.
        let text = one("I will   send the contract to 👩\u{200D}💻 Dana");
        assert_eq!(text, "I will send the contract to 👩\u{200D}💻 Dana");
    }

    // rule 2: symbol-only

    #[test]
    fn symbol_only_lines_are_dropped() {
        let (kept, stats) = prefilter_texts(&["•\n—\n|\n⋯\n→\n「」\n・・・\n✓\nReview the budget before Monday"]);
        assert_eq!(kept[0].text, "Review the budget before Monday");
        assert_eq!(stats.line_drops.symbol_only, 8);
    }

    #[test]
    fn symbol_rule_keeps_letters_digits_and_emoji() {
        // Negative: a reaction, a price and a one-letter reply are content.
        assert_eq!(one("👍\n$1,200\n→ ok"), "👍\n$1,200\n→ ok");
    }

    // rule 3: badge counts

    #[test]
    fn badge_counters_are_dropped() {
        let (kept, stats) = prefilter_texts(&["Inbox\n3\n99+\n(12)\n[7]\nPlease confirm the venue"]);
        assert_eq!(kept[0].text, "Inbox\nPlease confirm the venue");
        assert_eq!(stats.line_drops.badge_count, 4);
    }

    #[test]
    fn badge_rule_keeps_dates_times_amounts_and_long_numbers() {
        // Negative: every one of these can be what a commitment depends on.
        let input = "10:42\n2026-09-30\n4 / 12\n1,200\n12345\n3.5\n9/30";
        assert_eq!(one(input), input);
    }

    // rule 4: window controls

    #[test]
    fn window_controls_are_dropped_case_insensitively() {
        let (kept, stats) = prefilter_texts(&["Close\nMINIMIZE\nZoom\nNew Tab\nSkip to main content\n閉じる\nShip the release notes"]);
        assert_eq!(kept[0].text, "Ship the release notes");
        assert_eq!(stats.line_drops.window_control, 6);
    }

    #[test]
    fn window_control_rule_only_matches_whole_lines() {
        // Negative: the same words inside a sentence are content.
        let input = "Close the Acme deal by Friday\nI'll go back to Dana tomorrow\nZoom call with Sam at 3";
        assert_eq!(one(input), input);
    }

    // rule 5: consent / sign-in

    #[test]
    fn cookie_and_sign_in_controls_are_dropped() {
        let (kept, stats) = prefilter_texts(&[
            "We use cookies to improve your experience. By clicking Accept, you consent to our use of cookies.\n\
             Accept all\nReject all\nManage preferences\nSign in\nForgot password?\nログイン\n\
             Send Priya the signed NDA",
        ]);
        assert_eq!(kept[0].text, "Send Priya the signed NDA");
        assert_eq!(stats.line_drops.consent, 7);
    }

    #[test]
    fn consent_rule_keeps_prose_about_cookies_and_signing_in() {
        // Negative: no consent marker, a first-person sentence, and "sign in" inside a sentence.
        let input = "We use cookies for session auth; I'll switch us to tokens by Friday\n\
                     I need to sign in to the vendor portal and file the invoice\n\
                     Cookie jar refactor is blocked on review";
        assert_eq!(one(input), input);
    }

    #[test]
    fn a_very_long_cookie_sentence_is_kept_as_prose() {
        let long = format!("We use cookies and you must accept them. {}", "Details follow. ".repeat(40));
        assert_eq!(one(&long), long.trim_end());
    }

    // rule 6: repeated labels

    const MENU: &str = "Inbox\nStarred\nSnoozed\nSent\nDrafts";

    #[test]
    fn a_menu_repeated_across_captures_is_sent_once() {
        let a = format!("{MENU}\nCan you send the Q3 deck by Friday?");
        let b = format!("{MENU}\nThe venue moved to Hall B, please update the invite.");
        let c = format!("{MENU}\nReminder: renew the domain before it lapses.");
        let (kept, stats) = prefilter_texts(&[&a, &b, &c]);
        assert_eq!(kept.len(), 3);
        assert!(kept[0].text.starts_with(MENU), "the first copy is sent");
        assert_eq!(kept[1].text, "The venue moved to Hall B, please update the invite.");
        assert_eq!(kept[2].text, "Reminder: renew the domain before it lapses.");
        assert_eq!(stats.line_drops.repeated_label, 10);
    }

    #[test]
    fn a_new_label_inside_a_repeated_menu_survives() {
        let a = format!("{MENU}\nFirst message body goes here.");
        let b = format!("{MENU}\nSecond message body goes here.");
        let c = "Inbox\nStarred\nSnoozed\nSent\nDrafts\nSpam 4\nThird message body goes here.";
        let sent = sent(&[&a, &b, c]);
        // "Drafts" now sits next to "Spam 4" — a different neighbour, so it is sent again
        // rather than risk dropping something that only looks like the old menu.
        assert_eq!(sent[2], "Drafts\nSpam 4\nThird message body goes here.");
    }

    #[test]
    fn a_recurring_reply_next_to_buttons_is_kept_when_its_neighbours_differ() {
        // Negative: "Sure" sits in a run of labels (timestamp, reply, buttons) in every capture,
        // but next to a different timestamp each time, so each copy reaches the model.
        let a = "Can you review PR 12?\nMe 10:45\nSure\nReply\nReact\nShare";
        let b = "Can you send the deck by Friday?\nMe 11:02\nSure\nReply\nReact\nShare";
        let c = "Could you book the room?\nMe 14:30\nSure\nReply\nReact\nShare";
        let sent = sent(&[a, b, c]);
        assert!(sent.iter().all(|s| s.lines().any(|l| l == "Sure")), "{sent:?}");
        // ...while the button row itself, identical each time, is sent once.
        assert_eq!(sent.join("\n").matches("React").count(), 1, "{sent:?}");
    }

    #[test]
    fn a_short_reply_between_sentences_is_never_a_label() {
        // Negative: "Sure" is not in a run of three labels, so it is sent every time — each "Sure"
        // accepts a different request.
        let a = "Can you review PR 12?\nSure\nThanks, merging after.";
        let b = "Can you send the deck by Friday?\nSure\nGreat, see you then.";
        let c = "Could you book the room?\nSure\nPerfect.";
        let sent = sent(&[a, b, c]);
        assert!(sent.iter().all(|s| s.contains("\nSure\n")), "{sent:?}");
    }

    #[test]
    fn a_label_run_seen_in_fewer_than_three_items_is_kept_everywhere() {
        // Negative: a short to-do list captured twice is not chrome.
        let list = "Buy milk\nCall Bob\nEmail Ann";
        let a = format!("{list}\nGroceries for the week.");
        let b = format!("{list}\nUpdated after lunch.");
        let sent = sent(&[&a, &b]);
        assert!(sent[1].starts_with(list), "{sent:?}");
    }

    #[test]
    fn a_repeated_label_run_is_sent_at_least_once() {
        // Negative: even at the threshold, the first copy goes.
        let list = "Buy milk\nCall Bob\nEmail Ann";
        let a = format!("{list}\nMonday.");
        let b = format!("{list}\nTuesday.");
        let c = format!("{list}\nWednesday.");
        let joined = sent(&[&a, &b, &c]).join("\n");
        for label in ["Buy milk", "Call Bob", "Email Ann"] {
            assert_eq!(joined.matches(label).count(), 1, "{label} sent exactly once");
        }
    }

    #[test]
    fn a_japanese_sentence_is_not_label_like_but_a_japanese_menu_is() {
        assert!(is_label_like("受信トレイ"));
        assert!(is_label_like("スター付き"));
        assert!(!is_label_like("明日の午後までに見積書を送ります"));
        assert!(!is_label_like("Send the deck."));
        assert!(!is_label_like("dana@example.com"));
        assert!(!is_label_like("https://x.test/a"));
        assert!(!is_label_like("one two three four five"));
    }

    // rule 7 / 8: duplicates

    #[test]
    fn captures_that_differ_only_in_chrome_are_duplicates() {
        let a = "Close\nPlease send the SOW by the 30th\n3";
        let b = "Please send the SOW by the 30th\n  \n99+";
        let (kept, stats) = prefilter_texts(&[a, b]);
        assert_eq!(kept.len(), 1);
        assert_eq!(stats.dropped_duplicate, 1);
    }

    #[test]
    fn a_consecutive_subset_view_is_a_near_duplicate() {
        let a = "Thread: launch\nI'll send the deck Friday.\nDana: thanks!";
        let b = "Thread: launch\nI'll send the deck Friday.";
        let (kept, stats) = prefilter_texts(&[a, b]);
        assert_eq!(kept.len(), 1);
        assert_eq!(stats.dropped_near_duplicate, 1);
    }

    #[test]
    fn a_similar_view_with_one_new_line_is_kept() {
        // Negative: a 98%-similar capture that adds the promise must be sent.
        let a = "Thread: launch\nDana: can you send the deck?";
        let b = "Thread: launch\nDana: can you send the deck?\nMe: yes, by Friday.";
        let (kept, stats) = prefilter_texts(&[a, b]);
        assert_eq!(kept.len(), 2);
        assert_eq!(stats.dropped_near_duplicate, 0);
    }

    #[test]
    fn a_subset_of_an_older_non_adjacent_item_is_kept() {
        // Negative: rule 8 compares with the previous kept item only; a line that was sent
        // somewhere earlier may carry new meaning next to different neighbours.
        let a = "Plan review notes\nYes";
        let b = "Budget thread, unrelated content here";
        let c = "Yes";
        let (kept, _) = prefilter_texts(&[a, b, c]);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn empty_and_chrome_only_items_are_counted_apart() {
        let (kept, stats) = prefilter_texts(&["   \n\t", "Close\n•\n3", "Real content line"]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].index, 2);
        assert_eq!(stats.dropped_empty, 1);
        assert_eq!(stats.dropped_chrome_only, 1);
        assert_eq!(stats.input, 3);
        assert_eq!(stats.kept + stats.dropped(), stats.input);
    }

    #[test]
    fn char_counts_cover_before_and_after() {
        let (_, stats) = prefilter_texts(&["Close\nabc", "de"]);
        assert_eq!(stats.chars_before, "Close\nabc".chars().count() + 2);
        assert_eq!(stats.chars_after, 3 + 2);
    }

    #[test]
    fn the_filter_is_deterministic() {
        let a = format!("{MENU}\nOne.");
        let b = format!("{MENU}\nTwo.");
        let c = format!("{MENU}\nThree.");
        let input = [a.as_str(), b.as_str(), c.as_str(), "Close", "x  y"];
        assert_eq!(prefilter_texts(&input), prefilter_texts(&input));
    }

    #[test]
    fn stats_never_carry_captured_text() {
        let secret = "TOP-SECRET-CHUNK-ALPHA";
        let (_, stats) = prefilter_texts(&[secret, "Close", secret]);
        let dumped = format!("{stats:?}");
        assert!(!dumped.contains(secret), "{dumped}");
        assert!(!dumped.contains("Close"), "{dumped}");
    }

    #[test]
    fn reduction_rate_is_the_share_of_tokens_kept_on_device() {
        assert_eq!(reduction_rate(0, 0), 0.0);
        assert_eq!(reduction_rate(1_000, 1_000), 0.0);
        assert_eq!(reduction_rate(1_000, 1_200), 0.0, "never negative");
        assert!((reduction_rate(1_000, 650) - 0.35).abs() < 1e-12);
        assert_eq!(reduction_rate(1_000, 0), 1.0);
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(5), 2);
    }
}
