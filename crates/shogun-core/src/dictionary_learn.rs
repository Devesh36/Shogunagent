//! Pure steps of dictionary auto-add v2 (docs/dictionary-autoadd-v2-design.md): find the words a
//! user fixed in a dictation, build the bounded model payload, and keep only answers that point
//! at text the user actually typed.

use serde::Deserialize;

/// Longer dictations are documents, not fixes.
pub const MAX_WORDS: usize = 400;
/// More fixed spots than this in one dictation is a rewrite.
pub const MAX_HUNKS: usize = 8;
const MAX_TERM_CHARS: usize = 60;

/// One replaced run of words. `heard` is what was inserted, `fixed` what the user left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub heard: String,
    pub fixed: String,
    /// `fixed` starts a sentence, so a leading capital says nothing about a name.
    pub sentence_start: bool,
}

/// A term to add: `heard` is the misheard spelling to map from, `None` for a keyterm-only term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Learned {
    pub heard: Option<String>,
    pub term: String,
}

fn trim(word: &str) -> &str {
    // Sentence punctuation only, never path/email/code separators.
    word.trim_matches(['.', ',', '!', '?', ';', ':', '"', '(', ')'])
}

/// Words a corrected span may have beyond the dictation (a name fixed as two words, say).
const GROWTH_WORDS: usize = 3;
/// Words a single fix may add over what it replaces.
const HUNK_GROWTH_WORDS: usize = 2;

/// Runs of words that differ between `old` and `new` (an LCS diff), as
/// `(old_start, old_end, new_start, new_end)`. Pure insertions have `old_start == old_end`.
/// Words equal but for sentence punctuation count as the same word.
fn runs(old: &[&str], new: &[&str]) -> Vec<(usize, usize, usize, usize)> {
    let old: Vec<&str> = old.iter().map(|word| trim(word)).collect();
    let new: Vec<&str> = new.iter().map(|word| trim(word)).collect();
    let mut lcs = vec![vec![0u16; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let same = |i: usize, j: usize| i < old.len() && j < new.len() && old[i] == new[j];
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < old.len() || j < new.len() {
        if same(i, j) {
            i += 1;
            j += 1;
            continue;
        }
        let (start_old, start_new) = (i, j);
        while (i < old.len() || j < new.len()) && !same(i, j) {
            if j < new.len() && (i == old.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
                j += 1;
            } else {
                i += 1;
            }
        }
        out.push((start_old, i, start_new, j));
    }
    out
}

/// The part of `after` that lines up with the dictation. Text typed or pasted right before or
/// after it (pure insertions at either end) is cut; `None` when what is left still grew past the
/// dictation by more than a few words. Only this part is ever learned from or sent.
pub fn dictated_part(before: &str, after: &str) -> Option<String> {
    let old: Vec<&str> = before.split_whitespace().collect();
    let new: Vec<&str> = after.split_whitespace().collect();
    if old.is_empty() || new.is_empty() || old.len() > MAX_WORDS || new.len() > 2 * MAX_WORDS {
        return None;
    }
    let runs = runs(&old, &new);
    let start = runs.first().filter(|r| r.0 == 0 && r.1 == 0).map_or(0, |r| r.3);
    let end = runs
        .last()
        .filter(|r| r.0 == old.len() && r.1 == old.len())
        .map_or(new.len(), |r| r.2);
    let kept = new.get(start..end)?;
    (!kept.is_empty() && kept.len() <= old.len() + GROWTH_WORDS).then(|| kept.join(" "))
}

/// Word-level diff of the dictated span before and after the user's edits. `None` when the
/// dictation is too long, wiped, rewritten, or has no replaced words.
pub fn hunks(before: &str, after: &str) -> Option<Vec<Hunk>> {
    let old: Vec<&str> = before.split_whitespace().collect();
    let new: Vec<&str> = after.split_whitespace().collect();
    if old.is_empty() || new.is_empty() || old.len() > MAX_WORDS || new.len() > MAX_WORDS || old == new {
        return None;
    }
    let mut changed_old = 0;
    let mut out = Vec::new();
    for (start_old, end_old, start_new, end_new) in runs(&old, &new) {
        changed_old += end_old - start_old;
        // Pure insertions and deletions are not mishearings, and a fix that adds several words
        // carries typing that is not a correction.
        if end_old == start_old
            || end_new == start_new
            || end_new - start_new > end_old - start_old + HUNK_GROWTH_WORDS
        {
            continue;
        }
        let heard = trim(&old[start_old..end_old].join(" ")).to_string();
        let fixed = trim(&new[start_new..end_new].join(" ")).to_string();
        if heard == fixed || heard.chars().count() > MAX_TERM_CHARS || fixed.chars().count() > MAX_TERM_CHARS {
            continue;
        }
        out.push(Hunk {
            sentence_start: start_new == 0 || new[start_new - 1].ends_with(['.', '!', '?']),
            heard,
            fixed,
        });
    }
    // A short dictation fixed whole is still a fix; past that, more than half changed is a rewrite.
    if out.is_empty() || out.len() > MAX_HUNKS || (old.len() >= 6 && changed_old * 2 > old.len()) {
        return None;
    }
    Some(out)
}

pub const SYSTEM_PROMPT: &str = "The user dictated text, speech recognition inserted it (`original`), and the user then edited it (`corrected`). \
Each edit gives the inserted words (`heard`) and the user's replacement (`fixed`). \
Return the edits where the user corrected how a proper noun or specialised term is spelled: names of people, \
companies, products, projects, places, or jargon. For each, `term` is the exact correct spelling copied from `fixed`, \
and `heard` is the exact misrecognised words copied from `heard` (empty when only capitalisation changed). \
Ignore grammar, punctuation, rewording, and ordinary words. Return {\"terms\": []} when none qualify.";

/// The model input: the dictated span before and after the user's edits (at most
/// [`MAX_WORDS`] each, never other field or screen text) and the edits, redacted
/// (owner decision 3, revised 2026-10-19 to match Willow).
pub fn payload(original: &str, corrected: &str, hunks: &[Hunk]) -> String {
    let edits: Vec<serde_json::Value> = hunks
        .iter()
        .map(|h| {
            serde_json::json!({
                "heard": shogun_redact::redact_egress(&h.heard),
                "fixed": shogun_redact::redact_egress(&h.fixed),
            })
        })
        .collect();
    serde_json::json!({
        "original": shogun_redact::redact_egress(original),
        "corrected": shogun_redact::redact_egress(corrected),
        "edits": edits,
    })
    .to_string()
}

#[derive(Deserialize)]
struct Answer {
    terms: Vec<AnswerTerm>,
}

#[derive(Deserialize)]
struct AnswerTerm {
    #[serde(default)]
    heard: String,
    term: String,
}

/// Keep a model term only when it is an exact span of a fix the user made (and its `heard` an
/// exact span of what was inserted). The model proposes; this decides.
pub fn validate(answer: &serde_json::Value, hunks: &[Hunk]) -> Vec<Learned> {
    let Ok(answer) = serde_json::from_value::<Answer>(answer.clone()) else {
        return Vec::new();
    };
    let mut out: Vec<Learned> = Vec::new();
    for item in answer.terms.into_iter().take(MAX_HUNKS) {
        let term = trim(item.term.trim()).to_string();
        let heard = trim(item.heard.trim()).to_string();
        // A capitalisation-only fix maps to no alias: it would normalise to the term itself.
        let heard = (!heard.is_empty() && !same_spelling(&heard, &term)).then_some(heard);
        let in_a_fix = hunks.iter().any(|h| {
            word_span(&h.fixed, &term)
                && match heard.as_deref() {
                    Some(heard) => word_span(&h.heard, heard),
                    None => true,
                }
        });
        if !in_a_fix
            || !acceptable_term(&term)
            || heard.as_ref().is_some_and(|h| !(2..=MAX_TERM_CHARS).contains(&h.chars().count()))
            || out.iter().any(|l| l.term.eq_ignore_ascii_case(&term))
        {
            continue;
        }
        out.push(Learned { heard, term });
    }
    out
}

/// `needle` is a run of whole words of `hay`, sentence punctuation aside.
fn word_span(hay: &str, needle: &str) -> bool {
    let hay: Vec<&str> = hay.split_whitespace().map(trim).collect();
    let needle: Vec<&str> = needle.split_whitespace().map(trim).collect();
    !needle.is_empty() && hay.windows(needle.len()).any(|window| window == needle.as_slice())
}

/// Two spellings the dictionary would store as one alias.
pub fn same_spelling(a: &str, b: &str) -> bool {
    shogun_memory::voice_terms::normalized_alias(a) == shogun_memory::voice_terms::normalized_alias(b)
}

fn acceptable_term(term: &str) -> bool {
    (2..=MAX_TERM_CHARS).contains(&term.chars().count())
        && term.chars().any(char::is_alphabetic)
        && !(term.chars().all(|c| c.is_lowercase()) && is_common_form(term))
}

/// No model: learn after one fix what is plainly a name or term — a SHOGUN person/project name,
/// a name-shaped word, or a word that is not in the ordinary-word list. Speech errors are
/// phonetic ("cooper netties" for "Kubernetes"), so closeness of spelling is not required.
/// Ordinary-word typo fixes ("teh" -> "the"), case-only fixes of non-names and fixes longer than
/// three words never learn.
pub fn local_terms(hunks: &[Hunk], known_names: &[String]) -> Vec<Learned> {
    let mut out: Vec<Learned> = Vec::new();
    for hunk in hunks {
        let known = known_names.iter().any(|n| n.eq_ignore_ascii_case(&hunk.fixed));
        // One real name swapped for another is a different person, not a mishearing.
        let heard_is_name = known_names.iter().any(|n| n.eq_ignore_ascii_case(hunk.heard.trim()));
        if heard_is_name
            || inflection_pair(&hunk.heard, &hunk.fixed)
            || !acceptable_term(&hunk.fixed)
            || hunk.fixed.split_whitespace().count() > 3
            || !(known || name_shaped(hunk) || has_uncommon_word(&hunk.fixed))
            || out.iter().any(|l| l.term.eq_ignore_ascii_case(&hunk.fixed))
        {
            continue;
        }
        let heard = (!same_spelling(&hunk.heard, &hunk.fixed)).then(|| hunk.heard.clone());
        // A case-only fix of a non-name is just a typo fix.
        if heard.is_none() && !known {
            continue;
        }
        out.push(Learned { heard, term: hunk.fixed.clone() });
    }
    out
}

/// Some word of `fixed` is not an ordinary dictionary word. `false` without the word list, so a
/// machine lacking it learns only names (conservative).
fn has_uncommon_word(fixed: &str) -> bool {
    word_lists().is_some_and(|_| {
        fixed
            .split_whitespace()
            .map(|word| trim(word).to_lowercase())
            .any(|word| !word.is_empty() && !word.contains(['\'', '\u{2019}']) && !is_common_form(&word))
    })
}

const INFLECTIONS: [&str; 8] = ["s", "es", "d", "ed", "ing", "er", "ers", "ly"];

fn strip_apostrophes(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, '\'' | '\u{2019}')).flat_map(char::to_lowercase).collect()
}

/// The two spellings differ only by an ordinary inflection or an apostrophe ("email"/"emails",
/// "its"/"it's", "deploy"/"deployed"): a grammar edit, never a mishearing.
fn inflection_pair(heard: &str, fixed: &str) -> bool {
    let (a, b) = (strip_apostrophes(heard.trim()), strip_apostrophes(fixed.trim()));
    if a == b {
        return heard.to_lowercase() != fixed.to_lowercase(); // apostrophe-only; a pure case fix is not
    }
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    long.strip_prefix(short.as_str()).is_some_and(|rest| rest.is_empty() || INFLECTIONS.contains(&rest))
}

/// An ordinary word or one of its plain inflections (the word list has base forms only).
fn is_common_form(word: &str) -> bool {
    let word = strip_apostrophes(word);
    is_common_word(&word)
        || INFLECTIONS.iter().any(|suffix| {
            word.strip_suffix(suffix).is_some_and(|base| base.len() > 2 && (is_common_word(base) || is_common_word(&format!("{base}e"))))
        })
}

fn name_shaped(hunk: &Hunk) -> bool {
    hunk.fixed.split_whitespace().count() <= 3
        && hunk.fixed.split_whitespace().any(|word| {
            let mut chars = word.chars();
            let first_upper = chars.next().is_some_and(char::is_uppercase);
            let inner: Vec<char> = word.chars().skip(1).collect();
            let inner_mark = inner.iter().any(|c| c.is_uppercase() || c.is_ascii_digit())
                || inner[..inner.len().saturating_sub(1)].contains(&'-');
            inner_mark || (first_upper && !hunk.sentence_start)
        })
}

/// Relay checks per device per UTC day (owner decision 8).
pub const DAILY_MODEL_CALLS: u32 = 40;

/// Count one relay check against today's allowance; `false` once it is spent.
/// ponytail: in-memory, so a restart resets it; the relay's per-license cap still bounds spend.
pub fn take_model_call(now_ms: i64) -> bool {
    static USED: std::sync::Mutex<(i64, u32)> = std::sync::Mutex::new((0, 0));
    let Ok(mut used) = USED.lock() else { return false };
    let day = now_ms.div_euclid(86_400_000);
    if used.0 != day {
        *used = (day, 0);
    }
    if used.1 >= DAILY_MODEL_CALLS {
        return false;
    }
    used.1 += 1;
    true
}

pub(crate) type WordLists = (std::collections::HashSet<String>, std::collections::HashSet<String>);

/// macOS ships broad local word and proper-name lists; `None` off macOS or when missing.
pub(crate) fn word_lists() -> Option<&'static WordLists> {
    static WORDS: std::sync::OnceLock<Option<WordLists>> = std::sync::OnceLock::new();
    WORDS
        .get_or_init(|| {
            let text = std::fs::read_to_string("/usr/share/dict/words").ok()?;
            let common = text
                .lines()
                .filter(|word| word.bytes().all(|byte| byte.is_ascii_lowercase()))
                .map(str::to_string)
                .collect();
            let proper = std::fs::read_to_string("/usr/share/dict/propernames")
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            Some((common, proper))
        })
        .as_ref()
}

fn is_common_word(word: &str) -> bool {
    word_lists().is_some_and(|(common, _)| common.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_find_each_replaced_run_with_context() {
        let found = hunks(
            "ask tore tessu about the shogun eye launch today",
            "ask Torutesu about the ShogunAI launch today",
        )
        .unwrap_or_default();
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].heard.as_str(), found[0].fixed.as_str()), ("tore tessu", "Torutesu"));
        assert_eq!((found[1].heard.as_str(), found[1].fixed.as_str()), ("shogun eye", "ShogunAI"));
        assert!(!found[0].sentence_start);
    }

    #[test]
    fn hunks_skip_rewrites_wipes_inserts_and_long_text() {
        assert!(hunks("one two three four five six", "a b c d e six").is_none(), "rewrite");
        assert!(hunks("hello there", "").is_none(), "wiped");
        assert!(hunks("hello there", "hello there friend").is_none(), "insertion only");
        assert!(hunks("hello there.", "hello there").is_none(), "punctuation only");
        let long = vec!["word"; MAX_WORDS + 1].join(" ");
        assert!(hunks(&long, &long.replacen("word", "Ward", 1)).is_none());
        assert_eq!(hunks("Jon", "John").map(|h| h.len()), Some(1), "whole short dictation fixed");
    }

    #[test]
    fn payload_carries_only_the_dictated_span_and_edits() {
        let found = hunks("email the deck to tore tessu", "email the deck to Torutesu")
            .unwrap_or_default();
        let sent: serde_json::Value = serde_json::from_str(&payload(
            "email the deck to tore tessu",
            "email the deck to Torutesu",
            &found,
        ))
        .unwrap_or_default();
        assert_eq!(sent["edits"][0]["fixed"], "Torutesu");
        assert!(sent["corrected"].as_str().is_some_and(|text| text.ends_with("to Torutesu")));
        assert_eq!(sent.as_object().map(|o| o.len()), Some(3), "only original, corrected, edits");
    }


    #[test]
    fn validate_keeps_only_exact_spans_and_dedupes() {
        let found = hunks("ask tore tessu about it", "ask Torutesu about it").unwrap_or_default();
        let answer = serde_json::json!({"terms": [
            {"heard": "tore tessu", "term": "Torutesu"},
            {"heard": "tore tessu", "term": "torutesu"},
            {"heard": "x", "term": "Invented"},
            {"heard": "made up", "term": "Torutesu"},
        ]});
        assert_eq!(
            validate(&answer, &found),
            vec![Learned { heard: Some("tore tessu".into()), term: "Torutesu".into() }]
        );
        assert!(validate(&serde_json::json!({"nope": 1}), &found).is_empty());
    }

    #[test]
    fn dictated_part_cuts_typing_beside_the_dictation() {
        let before = "ask tore tessu about it";
        assert_eq!(
            dictated_part(before, "secret plan ask Torutesu about it and my bank pin is 1234").as_deref(),
            Some("ask Torutesu about it"),
            "typed before and after is cut"
        );
        assert_eq!(dictated_part(before, "ask Torutesu about it").as_deref(), Some("ask Torutesu about it"));
        assert!(
            dictated_part(before, "ask Torutesu one two three four five six about it").is_none(),
            "grew inside the dictation: no send"
        );
        assert!(dictated_part(before, "").is_none());
        let fixed = dictated_part(before, "ask Torutesu about it, then lunch").unwrap_or_default();
        assert!(!fixed.contains("lunch"));
    }

    #[test]
    fn a_fix_that_adds_many_words_is_not_a_hunk() {
        let found = hunks("send it to jon now", "send it to John Smith and the whole team now");
        assert!(found.is_none(), "one word growing into six is typing, not a fix");
    }

    #[test]
    fn validate_needs_whole_words_and_a_matching_hunk_for_heard() {
        let found = hunks(
            "please meet tore tessu and shogun eye at the office today",
            "please meet Torutesu and ShogunAI at the office today",
        )
        .unwrap_or_default();
        assert_eq!(found.len(), 2);
        let partial = serde_json::json!({"terms": [{"heard": "shogun eye", "term": "ShogunA"}]});
        assert!(validate(&partial, &found).is_empty(), "part of a word");
        let crossed = serde_json::json!({"terms": [{"heard": "tore tessu", "term": "ShogunAI"}]});
        assert!(validate(&crossed, &found).is_empty(), "heard from another fix");
        let right = serde_json::json!({"terms": [{"heard": "shogun eye", "term": "ShogunAI"}]});
        assert_eq!(validate(&right, &found).len(), 1);
    }

    #[test]
    fn case_only_answer_has_no_alias() {
        let found = hunks("meet with shogun today", "meet with Shogun today").unwrap_or_default();
        let answer = serde_json::json!({"terms": [{"heard": "shogun", "term": "Shogun"}]});
        assert_eq!(validate(&answer, &found), vec![Learned { heard: None, term: "Shogun".into() }]);
    }

    #[test]
    fn validate_allows_keyterm_only_terms() {
        let found = hunks("meet with shogun today", "meet with Shogun today").unwrap_or_default();
        let answer = serde_json::json!({"terms": [{"heard": "", "term": "Shogun"}]});
        assert_eq!(validate(&answer, &found), vec![Learned { heard: None, term: "Shogun".into() }]);
    }

    #[test]
    fn model_calls_are_capped_per_day() {
        let day = 9_000 * 86_400_000;
        assert!((0..DAILY_MODEL_CALLS).all(|_| take_model_call(day)));
        assert!(!take_model_call(day + 1));
        assert!(take_model_call(day + 86_400_000), "a new day resets the allowance");
    }

    #[test]
    fn local_fallback_learns_names_after_one_fix() {
        let found = hunks("ping jon and Sara about kafka", "ping John and Sarah about Kafka").unwrap_or_default();
        let learned = local_terms(&found, &["Sarah".to_string()]);
        let terms: Vec<&str> = learned.iter().map(|l| l.term.as_str()).collect();
        // "kafka" -> "Kafka" only changed case: without the model that is not enough to learn.
        assert_eq!(terms, vec!["John", "Sarah"]);
        let first_word = hunks("the cat sat down", "The cat sat down").unwrap_or_default();
        assert!(local_terms(&first_word, &[]).is_empty(), "sentence-start capital is not a name signal");
    }

    #[test]
    fn local_fallback_accepts_phonetic_fixes_far_from_the_spelling() {
        let far = hunks("ask door test you now", "ask Torutesu now").unwrap_or_default();
        assert_eq!(
            local_terms(&far, &[]),
            vec![Learned { heard: Some("door test you".into()), term: "Torutesu".into() }]
        );
        let k8s = hunks("deploy it on cooper netties today", "deploy it on Kubernetes today").unwrap_or_default();
        assert_eq!(local_terms(&k8s, &[]).len(), 1);
        let brand = hunks("open shogun eye now", "open ShogunAI now").unwrap_or_default();
        assert_eq!(local_terms(&brand, &[]).len(), 1);
        let pg = hunks("use postgres ql here", "use PostgreSQL here").unwrap_or_default();
        assert_eq!(local_terms(&pg, &[]).len(), 1);
        assert_eq!(local_terms(&far, &["torutesu".into()]).len(), 1, "known person");
    }

    #[test]
    fn local_fallback_accepts_lowercase_jargon_with_the_word_list() {
        if word_lists().is_none() {
            return;
        }
        let found = hunks("run cube cuddle apply now", "run kubectl apply now").unwrap_or_default();
        assert_eq!(local_terms(&found, &[]).len(), 1);
    }

    #[test]
    fn local_fallback_ignores_inflections_and_name_swaps() {
        for (before, after) in [
            ("send the email now", "send the emails now"),
            ("check its status", "check it's status"),
            ("we deploy today", "we deployed today"),
            ("they are okay", "they're okay"),
            ("two meeting today", "two meetings today"),
        ] {
            let found = hunks(before, after).unwrap_or_default();
            assert!(local_terms(&found, &[]).is_empty(), "{before} -> {after}");
        }
        let swap = hunks("ask Anna about it", "ask Ben about it").unwrap_or_default();
        let names = ["Anna".to_string(), "Ben".to_string()];
        assert!(local_terms(&swap, &names).is_empty(), "known name replaced by another");
    }

    #[test]
    fn local_fallback_rejects_typos_case_only_and_long_fixes() {
        let typo = hunks("fix teh bug in there code", "fix the bug in their code").unwrap_or_default();
        assert!(local_terms(&typo, &[]).is_empty(), "ordinary-word typo fixes");
        let case = hunks("we use kafka daily", "we use Kafka daily").unwrap_or_default();
        assert!(local_terms(&case, &[]).is_empty(), "case-only fix of a non-name");
        let long = hunks("a b c d x y z q", "a b c d One Two Three Four Five").unwrap_or_default();
        assert!(local_terms(&long, &[]).is_empty(), "more than three words");
        let number = hunks("call me at 5 pm", "call me at 6 pm").unwrap_or_default();
        assert!(local_terms(&number, &[]).is_empty(), "numbers only");
    }
}
