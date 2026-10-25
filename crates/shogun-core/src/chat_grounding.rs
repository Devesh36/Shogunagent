//! How a chat turn is grounded in memory (owner request 2026-10-14, "answer accuracy").
//!
//! The pure half of the chat harness, Linux-testable: the clock line the model resolves
//! "yesterday" / "先週" against, the evidence block with citable ids and dates, the retrieval
//! query for a follow-up that names nothing, citation parsing for the source chips, and the
//! localized clarification question. The desktop shell only calls these; the golden eval
//! (`tests/answer_eval.rs`) scores exactly what they produce.
//!
//! Evidence ids are per turn: `E1…En` for what the prompt carries, then `E(n+1)…` for what the
//! local memory tools return in the same turn ([`EvidenceLedger`]). A chip under the answer is
//! shown only for an id the answer actually cited.

use crate::chat_prompt::{HistoryTurn, Role};
use crate::context::Evidence;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The memory part of one chat prompt: the evidence budget (6 × 600) plus the ~1,000 characters of
/// commitment / open-loop fact lines the structured block carries. The structured block and the
/// evidence share it; nothing here raises the total.
pub const MEMORY_PROMPT_BUDGET_CHARS: usize = 6 * 600 + 1_000;
/// The structured state block (commitments, open loops, people, projects, calendar) at most.
pub const STRUCTURED_BLOCK_MAX_CHARS: usize = 1_400;
/// Evidence lines always kept even when the budget is tight: the best matches are the answer.
const EVIDENCE_FLOOR: usize = 2;

/// `YYYY-MM-DD` of the local day `ts_ms` falls on.
pub fn local_date(ts_ms: i64, offset_secs: i32) -> String {
    let day = (ts_ms + i64::from(offset_secs) * 1000).div_euclid(DAY_MS);
    let d = shogun_memory::due::civil_from_days(day);
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// The local weekday name of `ts_ms`.
pub fn local_weekday(ts_ms: i64, offset_secs: i32) -> &'static str {
    const NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let day = (ts_ms + i64::from(offset_secs) * 1000).div_euclid(DAY_MS);
    // 1970-01-01 was a Thursday.
    NAMES[(day + 4).rem_euclid(7) as usize]
}

/// `HH:MM` local.
pub fn local_time(ts_ms: i64, offset_secs: i32) -> String {
    let secs = (ts_ms + i64::from(offset_secs) * 1000).div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

fn utc_offset_label(offset_secs: i32) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
}

/// The per-turn clock line. Volatile on purpose: it rides in the turn's context, never in the
/// cached system prefix (FR-HN-05).
pub fn now_line(now_ms: i64, offset_secs: i32) -> String {
    format!(
        "Now: {} ({}) {} {} — resolve today, yesterday, tomorrow, this week, last week, 今日, 昨日, 明日, 今週, 先週 against this date.\n",
        local_date(now_ms, offset_secs),
        local_weekday(now_ms, offset_secs),
        local_time(now_ms, offset_secs),
        utc_offset_label(offset_secs),
    )
}

/// Whether an evidence source is text read off the screen (its title and excerpt pass the screen
/// sanitizer, as the live screen block does).
pub fn screen_derived(source: &str) -> bool {
    matches!(source, "capture" | "screen_ocr" | "screen_text" | "user_screenshot")
        || source.starts_with("visual_recall_")
}

/// One evidence line: `- [E3 · mail · 2026-10-09 · Orchid launch date moved] excerpt`.
pub fn evidence_line(index: usize, e: &Evidence, offset_secs: impl Fn(i64) -> i32) -> String {
    let screen = screen_derived(&e.source);
    let mut line = format!("- [E{index} · {}", shogun_memory::search::evidence_source_label(&e.source));
    if e.ts > 0 {
        line.push_str(" · ");
        line.push_str(&local_date(e.ts, offset_secs(e.ts)));
    }
    if let Some(t) = e.title.as_deref().filter(|t| !t.is_empty()) {
        line.push_str(" · ");
        if screen {
            line.push_str(&crate::screen_context::screen_prompt_text(t));
        } else {
            line.push_str(&crate::chat_prompt::neutralize_fences(t));
        }
    }
    if let Some(fid) = e.frame_id {
        line.push_str(&format!(" · frame {fid}"));
    }
    line.push_str("] ");
    if screen {
        line.push_str(&crate::screen_context::screen_prompt_text(&e.excerpt));
    } else {
        line.push_str(&crate::chat_prompt::neutralize_fences(&e.excerpt));
    }
    line.push('\n');
    line
}

/// The retrieved-evidence block for this turn, numbered from `E1`, within `budget_chars` (the
/// first [`EVIDENCE_FLOOR`] lines always stay). Returns the block and how many items it carries
/// — the ledger is cut to the same count so a chip can only name a line the model saw.
pub fn render_evidence_block(
    evidence: &[Evidence],
    budget_chars: usize,
    offset_secs: impl Fn(i64) -> i32,
) -> (String, usize) {
    if evidence.is_empty() {
        return (String::new(), 0);
    }
    let mut out = String::from(
        "\nRetrieved from their history, most relevant first. Cite an item you use as [E#]:\n",
    );
    let mut used = 0;
    let mut chars = 0;
    for (i, e) in evidence.iter().enumerate() {
        let line = evidence_line(i + 1, e, &offset_secs);
        let len = line.chars().count();
        if used >= EVIDENCE_FLOOR && chars + len > budget_chars {
            break;
        }
        chars += len;
        used += 1;
        out.push_str(&line);
    }
    (out, used)
}

/// A fact line the structured block carries (the `inline_memory` wording).
fn is_state_fact_line(line: &str) -> bool {
    let line = line.trim_start_matches(shogun_fusion::confidence::POSSIBLY_PREFIX);
    line.starts_with("you committed: ") || line.starts_with("open loop: ")
}

/// The memory half of one chat prompt.
#[derive(Debug, Clone, Default)]
pub struct ChatGrounding {
    /// Remembered facts plus the structured block: the cacheable layer 2 (no clock).
    pub stable_context: String,
    /// "Now: …" for this turn (empty when the caller has no clock to give, e.g. a saved meeting).
    pub now_line: String,
    /// The numbered, dated evidence block (may be empty).
    pub evidence_block: String,
    /// What this turn can cite; the memory tools extend it.
    pub ledger: EvidenceLedger,
    /// How many items retrieval found for the question (the grounding verdict's input).
    pub retrieved: usize,
}

impl ChatGrounding {
    /// Assemble from the retrieved pack: remembered facts minus the commitment / open-loop lines
    /// `structured` now carries, then `structured`, then the evidence within what is left of
    /// [`MEMORY_PROMPT_BUDGET_CHARS`]. The ledger holds exactly the evidence lines rendered.
    pub fn assemble(
        pack: &crate::context::ContextPack,
        structured: &str,
        now_line: String,
        offset_secs: impl Fn(i64) -> i32,
    ) -> Self {
        let facts: Vec<String> = if structured.is_empty() {
            pack.facts.clone()
        } else {
            pack.facts.iter().filter(|f| !is_state_fact_line(f)).cloned().collect()
        };
        let mut stable_context = crate::llm::prompt_cache::render_stable_facts(&facts);
        if !structured.is_empty() {
            if !stable_context.is_empty() {
                stable_context.push('\n');
            }
            stable_context.push_str(structured);
        }
        let budget = MEMORY_PROMPT_BUDGET_CHARS.saturating_sub(stable_context.chars().count());
        let (evidence_block, used) = render_evidence_block(&pack.evidence, budget, offset_secs);
        Self {
            stable_context,
            now_line,
            evidence_block,
            ledger: EvidenceLedger::new(pack.evidence.iter().take(used).cloned().collect()),
            retrieved: pack.evidence.len(),
        }
    }

    /// This turn's volatile memory context: the clock, then the evidence.
    pub fn context(&self) -> String {
        format!("{}{}", self.now_line, self.evidence_block)
    }
}

/// Every item this turn could cite, in id order: the prompt's evidence first, then what the
/// memory tools returned. Ids are 1-based (`E1` is `items[0]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceLedger {
    items: Vec<Evidence>,
}

impl EvidenceLedger {
    pub fn new(prompt_evidence: Vec<Evidence>) -> Self {
        Self { items: prompt_evidence }
    }

    /// The id for `e`: its existing one when the same event (or meeting) is already in the
    /// ledger, a new one otherwise.
    pub fn id_for(&mut self, e: &Evidence) -> usize {
        if let Some(pos) = self.items.iter().position(|item| item.event_id == e.event_id && item.source == e.source) {
            return pos + 1;
        }
        self.items.push(e.clone());
        self.items.len()
    }

    pub fn get(&self, id: usize) -> Option<&Evidence> {
        id.checked_sub(1).and_then(|i| self.items.get(i))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[Evidence] {
        &self.items
    }

    /// The items `answer` cites, in the order it first cites them; unknown ids are ignored.
    pub fn cited(&self, answer: &str) -> Vec<&Evidence> {
        parse_citations(answer).into_iter().filter_map(|id| self.get(id)).collect()
    }
}

/// Byte ranges of the citation markers in `text`: `[E3]`, `[E1, E4]`, `[E1][E2]`, `［E2］`.
fn citation_spans(text: &str) -> Vec<(usize, usize, Vec<usize>)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = text[search..].find(['[', '［']) {
        let start = search + rel;
        let open_len = text[start..].chars().next().map_or(1, char::len_utf8);
        let body_start = start + open_len;
        let close = text[body_start..].find([']', '］']).map(|i| body_start + i);
        let Some(close) = close.filter(|c| c - body_start <= 40) else {
            search = body_start;
            continue;
        };
        let body = &text[body_start..close];
        let mut ids = Vec::new();
        let ok = !body.trim().is_empty()
            && body.split([',', '、', ' ']).filter(|p| !p.is_empty()).all(|part| {
                let part = part.trim();
                let digits = part.strip_prefix('E').or_else(|| part.strip_prefix('e'));
                match digits.and_then(|d| d.parse::<usize>().ok()) {
                    Some(n) if n > 0 && n < 1000 => {
                        ids.push(n);
                        true
                    }
                    _ => false,
                }
            });
        let close_len = text[close..].chars().next().map_or(1, char::len_utf8);
        if ok {
            out.push((start, close + close_len, ids));
        }
        search = close + close_len;
    }
    out
}

/// The evidence ids `answer` cites, deduplicated, in first-cited order.
pub fn parse_citations(answer: &str) -> Vec<usize> {
    let mut seen = Vec::new();
    for (_, _, ids) in citation_spans(answer) {
        for id in ids {
            if !seen.contains(&id) {
                seen.push(id);
            }
        }
    }
    seen
}

/// `answer` without its citation markers (and the space before each), for the stored thread:
/// ids are per turn, so a kept `[E2]` would point at a different item on the next turn.
pub fn strip_citations(answer: &str) -> String {
    let spans = citation_spans(answer);
    if spans.is_empty() {
        return answer.to_string();
    }
    let mut out = String::with_capacity(answer.len());
    let mut at = 0;
    for (start, end, _) in spans {
        let kept = answer[at..start].trim_end_matches([' ', '\u{3000}']);
        out.push_str(kept);
        at = end;
    }
    out.push_str(&answer[at..]);
    out
}

fn is_japanese_char(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{ff66}'..='\u{ff9f}')
}

/// Whether the user wrote in Japanese (any kana or kanji).
pub fn is_japanese(text: &str) -> bool {
    text.chars().any(is_japanese_char)
}

const EN_REFERENTS: &[&str] = &[
    "it", "that", "this", "those", "these", "they", "them", "he", "she", "him", "her", "his",
    "there", "else", "why", "who", "whom", "and", "also", "too", "same", "which", "how",
];
const JA_REFERENTS: &[&str] = &[
    "それ", "あれ", "これ", "その", "あの", "この", "彼", "誰", "だれ", "どこ", "なぜ", "なんで",
    "いくら", "他に", "ほか", "担当", "いつ", "どう",
];

/// A question that names nothing on its own: short, or pointing back ("why?", "and Pro?",
/// "それはなぜ？", "誰から？"). Retrieval on its words alone finds nothing or the wrong thing.
pub fn is_follow_up(message: &str, topical_terms: usize) -> bool {
    let trimmed = message.trim();
    if is_japanese(trimmed) {
        let cjk = trimmed.chars().filter(|c| is_japanese_char(*c)).count();
        return cjk <= 10 || JA_REFERENTS.iter().any(|w| trimmed.contains(w));
    }
    let lower = trimmed.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    topical_terms <= 2 || words.iter().any(|w| EN_REFERENTS.contains(w))
}

/// Words of an earlier answer worth retrieving on: names and identifiers (capitalized or with a
/// digit), and Japanese runs of katakana or kanji. Never sentence glue.
fn answer_entities(text: &str) -> Vec<String> {
    const SKIP: &[&str] = &["It", "The", "This", "That", "They", "She", "He", "I", "We", "You", "Yes", "No", "Per", "There"];
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '\'' )) {
        let word = word.trim_matches(['-', '\'']);
        if word.chars().count() < 2 {
            continue;
        }
        let first_upper = word.chars().next().is_some_and(char::is_uppercase);
        let has_digit = word.chars().any(|c| c.is_ascii_digit());
        if is_japanese(word) {
            // Runs of katakana / kanji are names and nouns; hiragana is grammar.
            let mut run = String::new();
            for c in word.chars().chain(std::iter::once(' ')) {
                let keep = matches!(c, '\u{30a0}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}');
                if keep {
                    run.push(c);
                } else {
                    if run.chars().count() >= 2 && !out.contains(&run) {
                        out.push(run.clone());
                    }
                    run.clear();
                }
            }
        } else if (first_upper && !SKIP.contains(&word)) || (has_digit && word.chars().any(char::is_alphabetic)) {
            let w = word.to_string();
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// At most this much history is folded into one retrieval query.
const HISTORY_QUERY_MAX_CHARS: usize = 240;

/// The query a chat turn retrieves with. The message itself always leads; a follow-up
/// ([`is_follow_up`]) also carries the previous user turn's words and the names in the last
/// answer, so "why was it moved?" retrieves what "it" was. Words that name a time
/// (`is_time_word`, the same parser the search window uses) are left out of the borrowed half:
/// "yesterday" in the last question must not narrow this question's window.
pub fn retrieval_query(
    message: &str,
    history: &[HistoryTurn],
    is_time_word: impl Fn(&str) -> bool,
) -> String {
    let message = strip_citations(message);
    let topical = shogun_memory::search::lexical_terms(&message)
        .into_iter()
        .filter(|t| !is_time_word(t))
        .count();
    if history.is_empty() || !is_follow_up(&message, topical) {
        return message;
    }
    let mut extra: Vec<String> = Vec::new();
    let push = |term: String, extra: &mut Vec<String>| {
        if !is_time_word(&term) && !extra.contains(&term) && !message.contains(&term) {
            extra.push(term);
        }
    };
    if let Some(user) = history.iter().rev().find(|t| t.role == Role::User) {
        let text = strip_citations(&user.text);
        if is_japanese(&text) {
            for term in answer_entities(&text) {
                push(term, &mut extra);
            }
            // Kana-only nouns in a Japanese question still matter ("サクラ" is katakana and kept
            // above; a hiragana noun is rare enough to leave to the memory tools).
        }
        for term in shogun_memory::search::lexical_terms(&text) {
            if is_japanese(&term) {
                continue; // trigrams of the run; the run's entities are above
            }
            push(term, &mut extra);
        }
    }
    if let Some(answer) = history.iter().rev().find(|t| t.role == Role::Assistant) {
        for term in answer_entities(&strip_citations(&answer.text)) {
            push(term, &mut extra);
        }
    }
    let mut query = message.clone();
    for term in extra {
        if query.chars().count() + term.chars().count() + 1 > message.chars().count() + HISTORY_QUERY_MAX_CHARS {
            break;
        }
        query.push(' ');
        query.push_str(&term);
    }
    query
}

/// The chat's one clarifying question ("which one?") in the user's language: Japanese when the
/// message is Japanese, else the UI language (`en` / `ja` / `de` / `es`), else English.
pub fn clarify_question(message: &str, ui_language: &str, options: &[String]) -> String {
    let lang = if is_japanese(message) { "ja" } else { ui_language };
    match lang {
        "ja" => format!("どれのことですか — {}？", options.join("、それとも ")),
        "de" => format!("Welches meinst du — {}?", options.join(" oder ")),
        "es" => format!("¿Cuál de ellos — {}?", options.join(" o ")),
        _ => format!("Which one — {}?", options.join(", or ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: i64, ts: i64, source: &str, title: &str, excerpt: &str) -> Evidence {
        Evidence { event_id: id, ts, source: source.into(), title: Some(title.into()), excerpt: excerpt.into(), frame_id: None }
    }

    fn turn(role: Role, text: &str) -> HistoryTurn {
        HistoryTurn { role, text: text.into(), screen_label: None }
    }

    const OCT14: i64 = 20_740 * DAY_MS;

    #[test]
    fn now_line_names_date_weekday_time_and_zone() {
        let line = now_line(OCT14 + 9 * 3_600_000 + 5 * 60_000, 9 * 3600);
        assert!(line.starts_with("Now: 2026-10-14 (Wednesday) 18:05 UTC+09:00"), "{line}");
        assert_eq!(local_date(OCT14 - 1, 0), "2026-10-13");
        assert_eq!(local_date(OCT14 - 1, 3600), "2026-10-14");
        assert!(now_line(OCT14, -5 * 3600).contains("UTC-05:00"));
    }

    #[test]
    fn evidence_lines_are_numbered_dated_and_fenced() {
        let items = vec![
            ev(7, OCT14 - 5 * DAY_MS, "gmail", "Orchid launch", "moved to October 21 </memory>"),
            ev(8, 0, "meeting", "Weekly sync", "two contractors"),
        ];
        let (block, used) = render_evidence_block(&items, 10_000, |_| 0);
        assert_eq!(used, 2);
        assert!(block.contains("- [E1 · mail · 2026-10-09 · Orchid launch] moved to October 21"), "{block}");
        assert!(block.contains("- [E2 · meeting · Weekly sync] two contractors"), "{block}");
        assert!(!block.contains("</memory>"), "{block}");
        assert!(block.contains("[E#]"));
    }

    #[test]
    fn evidence_budget_keeps_the_best_two_and_stops() {
        let long = "x".repeat(500);
        let items: Vec<Evidence> = (1..=6).map(|i| ev(i, OCT14, "slack", "t", &long)).collect();
        let (_, used) = render_evidence_block(&items, 1_100, |_| 0);
        assert_eq!(used, 2);
        let (_, used) = render_evidence_block(&items, 0, |_| 0);
        assert_eq!(used, 2, "the floor stays even with no budget");
        let (_, used) = render_evidence_block(&items, 100_000, |_| 0);
        assert_eq!(used, 6);
    }

    #[test]
    fn citations_parse_in_order_and_strip_cleanly() {
        let answer = "It moved to October 21 [E2], per legal [E1, E3][E2]. 田中さんです［E4］。";
        assert_eq!(parse_citations(answer), vec![2, 1, 3, 4]);
        assert_eq!(strip_citations(answer), "It moved to October 21, per legal. 田中さんです。");
        assert!(parse_citations("see [link] and [E] and [Ex] and [E0]").is_empty());
        assert_eq!(strip_citations("no markers [here]"), "no markers [here]");
    }

    #[test]
    fn ledger_reuses_ids_and_cites_only_known_items() {
        let mut ledger = EvidenceLedger::new(vec![ev(1, 1, "gmail", "a", "a")]);
        assert_eq!(ledger.id_for(&ev(1, 1, "gmail", "a", "a")), 1);
        assert_eq!(ledger.id_for(&ev(9, 1, "slack", "b", "b")), 2);
        let cited: Vec<i64> = ledger.cited("x [E2] y [E7] z [E1]").iter().map(|e| e.event_id).collect();
        assert_eq!(cited, vec![9, 1]);
    }

    #[test]
    fn follow_ups_borrow_the_previous_topic_but_not_its_time_words() {
        let history = [
            turn(Role::User, "What did Dana say yesterday about the Q4 budget?"),
            turn(Role::Assistant, "She'll send the Q4 spreadsheet by Thursday [E1]."),
        ];
        let q = retrieval_query("what else is still open there?", &history, |t| t.eq_ignore_ascii_case("yesterday"));
        assert!(q.starts_with("what else is still open there?"));
        assert!(q.contains("Dana") && q.contains("budget") && q.contains("Q4"), "{q}");
        assert!(!q.contains("yesterday"), "{q}");
        // A self-contained question is left alone.
        let q = retrieval_query("When is the Orchid launch now?", &history, |_| false);
        assert_eq!(q, "When is the Orchid launch now?");
        // Japanese: the earlier question's katakana / kanji nouns come along.
        let history = [turn(Role::User, "サクラの請求書はいつ届く？"), turn(Role::Assistant, "10月20日までに届く予定です。")];
        let q = retrieval_query("誰から？", &history, |_| false);
        assert!(q.contains("サクラ") && q.contains("請求書"), "{q}");
        // No history, nothing to borrow.
        assert_eq!(retrieval_query("why?", &[], |_| false), "why?");
    }

    #[test]
    fn clarification_follows_the_users_language() {
        let options = vec!["Orchid".to_string(), "Kestrel".to_string()];
        assert_eq!(clarify_question("how's that going?", "en", &options), "Which one — Orchid, or Kestrel?");
        assert_eq!(clarify_question("あの件どうなった？", "en", &options), "どれのことですか — Orchid、それとも Kestrel？");
        assert!(clarify_question("how's that going?", "de", &options).starts_with("Welches"));
        assert!(clarify_question("how's that going?", "es", &options).starts_with("¿Cuál"));
        assert!(clarify_question("how's that going?", "fr", &options).starts_with("Which one"));
    }
}
