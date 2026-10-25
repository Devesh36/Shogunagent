//! Near-duplicate capture collapse (FR-CAP-03). Pure decision logic, no macOS, no DB.
//!
//! Accessibility capture re-reads a window's whole text on many focus/scroll/keystroke events, so
//! the same body arrives again and again with tiny differences (a few characters typed, whitespace
//! reflow). FR-CAP-03 says a near-duplicate (~98% similar) must **not** become a new event — it
//! should dedup-touch the existing one. The event log keys dedup on `content_hash`; this module is
//! the piece that decides *which* hash a freshly captured body gets: the prior body's hash when
//! they are near-duplicates, a fresh hash otherwise.
//!
//! Similarity is the Sørensen–Dice coefficient over adjacent-character bigrams of the normalized
//! text — dependency-free, O(n), and robust to edits anywhere in the body (unlike a prefix compare).
//! The stored event always keeps its *original* content; normalization only feeds the comparison.

use std::collections::{HashMap, HashSet};

/// The near-duplicate threshold (FR-CAP-03: "~98% similarity"). At or above this, two bodies are
/// the same event.
pub const NEAR_DUP_THRESHOLD: f64 = 0.98;

/// When at least this percentage of the visible line set changes, the capture represents a new
/// view rather than an incremental edit and should be stored as a full checkpoint.
const CHECKPOINT_CHANGE_PERCENT: usize = 40;

/// The minimal searchable write needed to preserve a complete AX capture relative to the prior
/// complete capture from the same window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRevision {
    /// The visible line set did not change; only dwell/recency needs touching.
    Unchanged,
    /// Only removals occurred. Update the in-memory baseline without indexing repeated text.
    StateOnly,
    /// A small edit added these exact lines; persist only this text.
    Delta(String),
    /// The view changed substantially; persist the complete current capture.
    Checkpoint,
}

fn normalize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_whitespace = false;
    for ch in line.trim().chars() {
        if ch.is_whitespace() {
            in_whitespace = true;
        } else {
            if in_whitespace && !out.is_empty() {
                out.push(' ');
            }
            in_whitespace = false;
            out.push(ch);
        }
    }
    out
}

fn normalized_lines(text: &str) -> HashSet<String> {
    text.lines()
        .map(normalize_line)
        .filter(|line| !line.is_empty())
        .collect()
}

/// Compare two complete AX bodies as unordered visible-line sets.
///
/// Order-only UI reflow is ignored. Whitespace-only changes compare equal, while casing and
/// Unicode remain exact so meaningful edits are not erased. Small edits return only newly visible
/// lines; the caller retains the complete current body in its bounded in-memory baseline.
pub fn decide_text_revision(previous: &str, current: &str) -> TextRevision {
    let previous_lines = normalized_lines(previous);
    let current_lines = normalized_lines(current);
    if previous_lines == current_lines {
        return TextRevision::Unchanged;
    }

    let mut current_seen = HashSet::new();
    let added = current
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let normalized = normalize_line(line);
            !previous_lines.contains(&normalized) && current_seen.insert(normalized)
        })
        .collect::<Vec<_>>();
    if added.is_empty() {
        return TextRevision::StateOnly;
    }

    let removed_count = previous_lines.difference(&current_lines).count();
    let changed_count = added.len().saturating_add(removed_count);
    let baseline_count = previous_lines.len().max(current_lines.len()).max(1);
    if changed_count.saturating_mul(100)
        >= baseline_count.saturating_mul(CHECKPOINT_CHANGE_PERCENT)
    {
        TextRevision::Checkpoint
    } else {
        TextRevision::Delta(added.join("\n"))
    }
}

/// Normalize for comparison only: lowercase, collapse any whitespace run to one space, trim. The
/// original text is what gets stored — this is purely to make the similarity robust to cosmetic
/// reflow.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Multiset of adjacent-character bigrams of `s` (as counts), over `char`s so multibyte text
/// (Japanese) is handled correctly.
fn bigrams(s: &str) -> HashMap<(char, char), u32> {
    let chars: Vec<char> = s.chars().collect();
    let mut m = HashMap::new();
    for w in chars.windows(2) {
        *m.entry((w[0], w[1])).or_insert(0) += 1;
    }
    m
}

/// Sørensen–Dice similarity over character bigrams of the normalized strings, in `[0, 1]`.
/// Two identical normalized strings score 1.0; strings too short to form a bigram fall back to an
/// exact-equality check (1.0 if equal, else 0.0).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (normalize(a), normalize(b));
    if na == nb {
        return 1.0;
    }
    let (ba, bb) = (bigrams(&na), bigrams(&nb));
    let total: u32 = ba.values().sum::<u32>() + bb.values().sum::<u32>();
    if total == 0 {
        // both had <2 chars and were not equal
        return 0.0;
    }
    let mut overlap = 0u32;
    for (bg, ca) in &ba {
        if let Some(cb) = bb.get(bg) {
            overlap += (*ca).min(*cb);
        }
    }
    (2 * overlap) as f64 / total as f64
}

/// A recent event offered as a dedup candidate: its stored hash and content.
#[derive(Debug, Clone, Copy)]
pub struct Recent<'a> {
    pub content_hash: &'a str,
    pub content: &'a str,
}

/// The outcome of a dedup decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashDecision {
    /// `new_content` is a near-duplicate of an existing event — reuse its hash (dedup-touch).
    Duplicate(String),
    /// `new_content` is novel — use this freshly computed hash (a new event).
    Fresh(String),
}

impl HashDecision {
    /// The chosen content_hash, whichever branch.
    pub fn hash(&self) -> &str {
        match self {
            HashDecision::Duplicate(h) | HashDecision::Fresh(h) => h,
        }
    }

    /// Whether the decision was a near-duplicate.
    pub fn is_duplicate(&self) -> bool {
        matches!(self, HashDecision::Duplicate(_))
    }
}

/// Decide the content_hash for `new_content`. If the most-similar recent candidate is at or above
/// [`NEAR_DUP_THRESHOLD`], reuse that candidate's hash (the event log will dedup-touch, FR-CAP-03);
/// otherwise compute a fresh hash with `hash_fn`. Ties pick the highest similarity, then the first
/// candidate in order.
pub fn decide_hash(
    new_content: &str,
    recents: &[Recent<'_>],
    hash_fn: impl Fn(&str) -> String,
) -> HashDecision {
    // NOT max_by: that keeps the LAST of equal elements, and recents arrive newest-first — an
    // exact-tie (two identical recent bodies) would dedup-touch the older row. Keep the first
    // strictly-greater score so ties resolve to the first candidate, as documented.
    let best = recents
        .iter()
        .map(|r| (r, similarity(new_content, r.content)))
        .filter(|(_, s)| *s >= NEAR_DUP_THRESHOLD)
        .fold(None::<(&Recent<'_>, f64)>, |acc, (r, s)| match acc {
            Some((_, best_s)) if s <= best_s => acc,
            _ => Some((r, s)),
        });
    match best {
        Some((r, _)) => HashDecision::Duplicate(r.content_hash.to_string()),
        None => HashDecision::Fresh(hash_fn(new_content)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_hash(s: &str) -> String {
        // a stand-in stable hash for tests (the daemon injects the real xxhash)
        format!("hash:{}", s.len())
    }

    #[test]
    fn an_exact_tie_dedups_to_the_first_candidate() {
        // recents arrive newest-first; two identical bodies under different hashes must touch
        // the FIRST (newest) row, not accumulate dwell on the stale one.
        let recents = [
            Recent { content_hash: "newest", content: "same body text here" },
            Recent { content_hash: "older", content: "same body text here" },
        ];
        let decision = decide_hash("same body text here", &recents, fresh_hash);
        assert_eq!(decision, HashDecision::Duplicate("newest".to_string()));
    }

    #[test]
    fn normalize_collapses_whitespace_and_case() {
        assert_eq!(normalize("  The\tQuick\n  Brown  "), "the quick brown");
    }

    #[test]
    fn identical_text_is_fully_similar() {
        assert_eq!(similarity("hello world", "hello world"), 1.0);
        // differs only by whitespace/case → still identical after normalize
        assert_eq!(similarity("Hello   World", "hello world"), 1.0);
    }

    #[test]
    fn a_few_appended_chars_stay_above_threshold() {
        let base: String = "the quarterly roadmap review meeting notes ".repeat(12);
        let typed = format!("{base}x"); // one keystroke later
        let s = similarity(&base, &typed);
        assert!(s >= NEAR_DUP_THRESHOLD, "near-dup similarity too low: {s}");
    }

    #[test]
    fn different_text_is_well_below_threshold() {
        let s = similarity(
            "the quarterly roadmap review meeting",
            "lunch plans for saturday afternoon downtown",
        );
        assert!(s < 0.5, "unrelated text should be dissimilar: {s}");
    }

    #[test]
    fn decide_reuses_hash_for_near_duplicate() {
        let base: String = "sprint planning board with many tickets and notes ".repeat(10);
        let typed = format!("{base}!");
        let recents = [Recent { content_hash: "abc123", content: &base }];
        let decision = decide_hash(&typed, &recents, fresh_hash);
        assert!(decision.is_duplicate());
        assert_eq!(decision.hash(), "abc123", "near-dup must reuse the prior hash → dedup-touch");
    }

    #[test]
    fn decide_makes_fresh_hash_for_novel_content() {
        let recents = [Recent { content_hash: "abc123", content: "an old unrelated window body" }];
        let decision = decide_hash("a totally different fresh capture", &recents, fresh_hash);
        assert!(!decision.is_duplicate());
        assert_eq!(decision.hash(), fresh_hash("a totally different fresh capture"));
    }

    #[test]
    fn decide_picks_the_most_similar_candidate() {
        let target: String = "weekly one-on-one agenda: growth, blockers, feedback ".repeat(8);
        let near = format!("{target}."); // ~identical
        let far = "completely unrelated scratchpad".to_string();
        let recents = [
            Recent { content_hash: "far", content: &far },
            Recent { content_hash: "near", content: &target },
        ];
        let decision = decide_hash(&near, &recents, fresh_hash);
        assert_eq!(decision.hash(), "near");
    }

    #[test]
    fn no_recents_is_always_fresh() {
        let decision = decide_hash("anything", &[], fresh_hash);
        assert!(!decision.is_duplicate());
    }

    #[test]
    fn japanese_text_bigrams_are_char_aware() {
        // near-identical Japanese bodies (one appended char) collapse; unrelated ones don't.
        let base: String = "四半期のロードマップ会議の議事録です。".repeat(6);
        let typed = format!("{base}あ");
        assert!(similarity(&base, &typed) >= NEAR_DUP_THRESHOLD);
        assert!(similarity(&base, "全く関係のない別の文章").abs() < 0.5);
    }

    #[test]
    fn one_edited_line_is_persisted_as_a_delta_without_repeating_the_window() {
        let previous = (0..100).map(|i| format!("stable line {i}")).collect::<Vec<_>>().join("\n");
        let mut current = (0..100).map(|i| format!("stable line {i}")).collect::<Vec<_>>();
        current[42] = "launch date moved to October 17".to_string();
        let current = current.join("\n");

        assert_eq!(
            decide_text_revision(&previous, &current),
            TextRevision::Delta("launch date moved to October 17".to_string())
        );
    }

    #[test]
    fn reordered_lines_do_not_create_an_event() {
        assert_eq!(
            decide_text_revision("alpha\nbeta\ngamma", "gamma\nalpha\nbeta"),
            TextRevision::Unchanged
        );
    }

    #[test]
    fn a_large_view_change_persists_a_full_checkpoint() {
        let previous = (0..20).map(|i| format!("old channel {i}")).collect::<Vec<_>>().join("\n");
        let current = (0..20).map(|i| format!("new channel {i}")).collect::<Vec<_>>().join("\n");

        assert_eq!(decide_text_revision(&previous, &current), TextRevision::Checkpoint);
    }

    #[test]
    fn unicode_delta_preserves_original_text() {
        assert_eq!(
            decide_text_revision("計画\n担当者", "計画\n担当者\n発売日は十月十七日"),
            TextRevision::Delta("発売日は十月十七日".to_string())
        );
    }

    #[test]
    fn removal_only_updates_state_without_indexing_repeated_text() {
        assert_eq!(
            decide_text_revision("keep\nremoved", "keep"),
            TextRevision::StateOnly
        );
    }

    #[test]
    fn typing_sequence_reduces_persisted_text_by_at_least_eighty_percent() {
        let chrome = (0..340).map(|i| format!("stable ui line {i}")).collect::<Vec<_>>();
        let edits = ["c", "co", "com", "comm", "comme", "comment added for issue 302"];
        let captures = edits
            .iter()
            .map(|edit| chrome.iter().cloned().chain([edit.to_string()]).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>();
        let baseline_bytes = captures.iter().map(String::len).sum::<usize>();
        let mut persisted_bytes = captures[0].len();
        for pair in captures.windows(2) {
            persisted_bytes += match decide_text_revision(&pair[0], &pair[1]) {
                TextRevision::Delta(text) => text.len(),
                TextRevision::Checkpoint => pair[1].len(),
                TextRevision::Unchanged | TextRevision::StateOnly => 0,
            };
        }

        assert!(
            persisted_bytes * 5 <= baseline_bytes,
            "expected >=80% reduction: baseline={baseline_bytes}, persisted={persisted_bytes}"
        );
    }

    #[test]
    #[ignore = "release-only AX delta latency measurement"]
    fn text_revision_latency_stays_below_five_milliseconds_at_expanded_tree_size() {
        let previous = (0..1_500).map(|i| format!("visible accessibility line {i}"))
            .collect::<Vec<_>>().join("\n");
        let current = format!("{previous}\nnewly visible decision text");
        let mut samples = Vec::with_capacity(200);
        for _ in 0..200 {
            let started = std::time::Instant::now();
            let revision = decide_text_revision(&previous, &current);
            samples.push(started.elapsed());
            assert!(matches!(revision, TextRevision::Delta(_)));
        }
        samples.sort_unstable();
        let p50 = samples[samples.len() / 2];
        let p95 = samples[samples.len() * 95 / 100];
        println!("AX delta compare 1,500 lines: p50={p50:?} p95={p95:?}");
        assert!(p95 < std::time::Duration::from_millis(5), "p95={p95:?}");
    }
}
