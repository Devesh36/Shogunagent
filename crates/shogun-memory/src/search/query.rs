/// Longest sequence of terms sent to FTS. A pathological paste should not turn into a thousand-
/// clause MATCH; the leading terms carry the question anyway.
pub(crate) const MAX_FTS_TERMS: usize = 24;
pub(crate) const MAX_ANSWER_ANCHOR_CHARS: usize = 96;

use super::filtering::{query_time_window, LocalDayBounds};

/// Canonical source family used for query intent and explicit source constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySourceFamily {
    Meeting,
    State,
    Brief,
    Person,
    Project,
    Thread,
    Period,
}

/// Natural-language source cues prefer a family; only a typed caller control requires one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceConstraint {
    #[default]
    Any,
    Prefer(MemorySourceFamily),
    Require(MemorySourceFamily),
}

/// Why a normalized query span is reliable enough for answerability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryAnchorKind {
    Named,
    Quoted,
    Rare,
}

/// Bounded content anchor. Original query text is never retained here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAnchor {
    pub normalized: String,
    pub kind: QueryAnchorKind,
}

/// Canonical source family implied by a natural-language memory question.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryQueryFocus {
    #[default]
    Any,
    Meeting,
    State,
    Brief,
    Period,
    Person,
    Project,
    Thread,
}

impl MemoryQueryFocus {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::Meeting => 1,
            Self::State => 2,
            Self::Brief => 3,
            Self::Period => 4,
            Self::Person => 5,
            Self::Project => 6,
            Self::Thread => 7,
        }
    }
}

/// Deterministic, bounded interpretation of one memory question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQueryPlan {
    pub topical_terms: Vec<String>,
    pub entities: Vec<String>,
    pub anchors: Vec<QueryAnchor>,
    pub retry_query: Option<String>,
    pub focus: MemoryQueryFocus,
    pub source: SourceConstraint,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
}

/// Parse source and local-time hints without sending the question off-device.
pub fn plan_memory_query(query: &str, now_ms: i64, local_days: LocalDayBounds) -> MemoryQueryPlan {
    let normalized_query = normalize_words(query);
    let source = infer_source_constraint(&normalized_query);
    let focus = match source {
        SourceConstraint::Prefer(MemorySourceFamily::Meeting) => MemoryQueryFocus::Meeting,
        SourceConstraint::Prefer(MemorySourceFamily::State) => MemoryQueryFocus::State,
        SourceConstraint::Prefer(MemorySourceFamily::Brief) => MemoryQueryFocus::Brief,
        SourceConstraint::Prefer(MemorySourceFamily::Period) => MemoryQueryFocus::Period,
        _ => MemoryQueryFocus::Any,
    };
    let (from_ms, to_ms) = query_time_window(query, now_ms, local_days)
        .map_or((None, None), |(from, to)| (Some(from), Some(to)));

    let topical_terms = lexical_terms(query)
        .into_iter()
        .map(|term| term.to_lowercase())
        .collect::<Vec<_>>();
    let entities = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| {
            word.chars().next().is_some_and(char::is_uppercase)
                && topical_terms
                    .iter()
                    .any(|term| term == &word.to_lowercase())
        })
        .map(str::to_owned)
        .collect();
    let anchors = extract_explicit_anchors(query);
    let retry_query = (!topical_terms.is_empty()).then(|| topical_terms.join(" "));
    MemoryQueryPlan {
        topical_terms,
        entities,
        anchors,
        retry_query,
        focus,
        source,
        from_ms,
        to_ms,
    }
}

fn infer_source_constraint(normalized_query: &str) -> SourceConstraint {
    let family = if contains_phrase(normalized_query, &["morning brief", "briefing", "brief"]) {
        Some(MemorySourceFamily::Brief)
    } else if contains_phrase(normalized_query, &["person", "people", "contact"]) {
        Some(MemorySourceFamily::Person)
    } else if contains_phrase(normalized_query, &["project"]) {
        Some(MemorySourceFamily::Project)
    } else if contains_phrase(normalized_query, &["thread", "conversation"]) {
        Some(MemorySourceFamily::Thread)
    } else if contains_phrase(
        normalized_query,
        &[
            "meeting",
            "session",
            "standup",
            "recap",
            "transcript",
            "note",
        ],
    ) {
        Some(MemorySourceFamily::Meeting)
    } else if contains_phrase(
        normalized_query,
        &[
            "hour",
            "day",
            "week",
            "month",
            "quarter",
            "period",
            "trend",
            "this week",
            "last week",
            "this month",
            "last month",
        ],
    ) {
        Some(MemorySourceFamily::Period)
    } else if contains_phrase(
        normalized_query,
        &[
            "commitment",
            "committed",
            "promised",
            "open loop",
            "owe",
            "deadline",
        ],
    ) {
        Some(MemorySourceFamily::State)
    } else {
        None
    };
    family.map_or(SourceConstraint::Any, SourceConstraint::Prefer)
}

fn contains_phrase(text: &str, needles: &[&str]) -> bool {
    let padded = format!(" {text} ");
    needles
        .iter()
        .any(|needle| padded.contains(&format!(" {needle} ")))
}

fn normalize_words(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_explicit_anchors(query: &str) -> Vec<QueryAnchor> {
    let mut anchors = Vec::new();
    let mut quoted = false;
    let mut quoted_text = String::new();
    let mut quoted_overlong = false;
    for character in query.chars() {
        if matches!(character, '"' | '\u{201c}' | '\u{201d}') {
            if quoted {
                if !quoted_overlong {
                    push_anchor(&mut anchors, &quoted_text, QueryAnchorKind::Quoted);
                }
                quoted_text.clear();
                quoted_overlong = false;
            }
            quoted = !quoted;
        } else if quoted {
            if quoted_text.chars().count() < MAX_ANSWER_ANCHOR_CHARS {
                quoted_text.push(character);
            } else {
                quoted_overlong = true;
            }
        }
    }
    for token in query.split(|character: char| !character.is_alphanumeric()) {
        if anchors.len() >= 3 {
            break;
        }
        if !matches!(scan_named_anchor(token), NamedAnchorScan::Named)
            || is_stopword(token)
            || is_anchor_only_stopword(token)
            || is_source_cue(token)
        {
            continue;
        }
        push_anchor(&mut anchors, token, QueryAnchorKind::Named);
    }
    anchors
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedAnchorScan {
    Named,
    NotNamed,
    OverlongAt(usize),
}

fn scan_named_anchor(token: &str) -> NamedAnchorScan {
    let mut characters = token.chars();
    let Some(first) = characters.next() else {
        return NamedAnchorScan::NotNamed;
    };
    let mut count = 1;
    let mut has_lowercase = false;
    let mut has_uppercase = false;
    for character in characters {
        count += 1;
        if count > MAX_ANSWER_ANCHOR_CHARS {
            return NamedAnchorScan::OverlongAt(count);
        }
        has_lowercase |= character.is_lowercase();
        has_uppercase |= character.is_uppercase();
    }

    if (first.is_uppercase() && has_lowercase) || (first.is_lowercase() && has_uppercase) {
        NamedAnchorScan::Named
    } else {
        NamedAnchorScan::NotNamed
    }
}

fn push_anchor(anchors: &mut Vec<QueryAnchor>, raw: &str, kind: QueryAnchorKind) {
    if anchors.len() >= 3
        || raw.chars().take(MAX_ANSWER_ANCHOR_CHARS + 1).count() > MAX_ANSWER_ANCHOR_CHARS
    {
        return;
    }
    let normalized = normalize_words(raw);
    if normalized.is_empty() || anchors.iter().any(|anchor| anchor.normalized == normalized) {
        return;
    }
    anchors.push(QueryAnchor { normalized, kind });
}

fn is_anchor_only_stopword(term: &str) -> bool {
    matches!(term.to_lowercase().as_str(), "que" | "qué")
}

pub(crate) fn is_source_cue(term: &str) -> bool {
    matches!(
        term.to_lowercase().as_str(),
        "meeting"
            | "session"
            | "standup"
            | "recap"
            | "transcript"
            | "note"
            | "brief"
            | "briefing"
            | "person"
            | "people"
            | "contact"
            | "project"
            | "thread"
            | "conversation"
            | "hour"
            | "day"
            | "week"
            | "month"
            | "quarter"
            | "period"
            | "trend"
            | "commitment"
            | "deadline"
            | "status"
    )
}

/// Searchable terms of a user's question, as typed (NFKC; case kept for Latin words).
///
/// Shared by every lexical lane so they speak one vocabulary (see `terms.rs`):
///
/// - Latin words of three or more characters, minus English function words;
/// - two-character Latin tokens that carry a letter (`Q3`, `PR`, `CI`, `v2`) — matched through
///   the V81 gram index, never through the trigram index;
/// - CJK text split at script boundaries: hiragana particles and honorifics (の, は, さん) are
///   dropped, okurigana inside a kanji compound is folded (打ち合わせ ↔ 打合せ), and each kanji /
///   katakana word of two or more characters is one term — not a run of overlapping trigrams,
///   which match any text sharing a particle (田中さんの件 would match 佐藤さんの資料);
/// - relative-date words (昨日, 先週, yesterday) are dropped: they become a time window
///   ([`super::query_time_window`]), never text to match.
pub fn lexical_terms(query: &str) -> Vec<String> {
    super::terms::query_concepts(query)
        .into_iter()
        .map(|concept| concept.display)
        .collect()
}

/// The trigram FTS5 MATCH expression for a question: each term of three or more characters,
/// quoted, joined with OR.
///
/// Quoting matters twice over. Unquoted, a question containing `-`, `*`, `OR` or `NEAR` is parsed
/// as FTS operators and errors or silently means something else. But quoting the *whole* question
/// makes it a single phrase, which only matches text containing those words contiguously — and a
/// question almost never appears verbatim in the answer. So each term is quoted separately and
/// combined with OR: any term can match, and bm25 ranks a document that matches more — and
/// rarer — terms higher.
///
/// A CJK word of three or more characters is one quoted term; the trigram index matches it as a
/// substring. Two-character terms cannot match a trigram index and are left out here; the event
/// lane matches them through the gram index (`lexical.rs`).
///
/// Returns `None` when nothing usable is left, which the caller treats as "no results" rather than
/// running an empty MATCH.
pub fn fts_query(query: &str) -> Option<String> {
    let terms = super::terms::query_concepts(query)
        .into_iter()
        .filter_map(|concept| concept.surface)
        .map(|surface| quote(&surface))
        .collect::<Vec<_>>();
    // A question made only of function words has nothing to retrieve on. Returning None is
    // honest: matching every document via "the" would fill the result budget with noise.
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// English function words, which match nearly every document and so only crowd out real hits.
///
/// bm25 already scores them near zero, but scoring is not the problem — the result *limit* is:
/// a handful of slots filled by documents that merely contain "the" are slots a real match
/// cannot have. Kept deliberately small and closed-class; anything with topical meaning stays.
pub(crate) fn is_stopword(term: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "are", "was", "were", "you", "your", "our", "their", "his", "her",
        "its", "that", "this", "these", "those", "with", "from", "into", "about", "what", "when",
        "where", "which", "who", "whom", "how", "why", "did", "does", "done", "have", "has", "had",
        "can", "could", "would", "should", "will", "shall", "may", "might", "not", "but", "any",
        "all", "some", "there", "here", "then", "than", "them", "they", "she", "him", "get", "got",
        "remember", "recall", "memory", "lookup", "find", "tell", "know", "please", "show",
    ];
    let lower = term.to_ascii_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}


#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::search::LocalDayBounds;

    #[test]
    fn memory_query_plan_extracts_topic_scope_and_yesterday_range() {
        let plan = plan_memory_query(
            "What did we decide in the Phoenix meeting yesterday?",
            300,
            LocalDayBounds {
                yesterday_start_ms: 100,
                today_start_ms: 200,
            },
        );

        assert_eq!(plan.focus, MemoryQueryFocus::Meeting);
        assert_eq!(plan.from_ms, Some(100));
        assert_eq!(plan.to_ms, Some(200));
        // "yesterday" became the time window above; it is not text to match.
        assert_eq!(plan.topical_terms, vec!["decide", "phoenix", "meeting"]);
        assert_eq!(plan.entities, vec!["Phoenix"]);
        assert_eq!(
            plan.retry_query.as_deref(),
            Some("decide phoenix meeting")
        );
    }

    #[test]
    fn memory_query_plan_keeps_empty_query_unscoped() {
        let plan = plan_memory_query(
            "   ",
            300,
            LocalDayBounds {
                yesterday_start_ms: 100,
                today_start_ms: 200,
            },
        );

        assert_eq!(plan.focus, MemoryQueryFocus::Any);
        assert!(plan.topical_terms.is_empty());
        assert!(plan.entities.is_empty());
        assert_eq!(plan.retry_query, None);
        assert_eq!(plan.from_ms, None);
        assert_eq!(plan.to_ms, None);
    }

    #[test]
    fn memory_query_plan_uses_soft_source_intent_and_unicode_named_anchor() {
        let plan = plan_memory_query(
            "Élodie project brief",
            300,
            LocalDayBounds {
                yesterday_start_ms: 100,
                today_start_ms: 200,
            },
        );

        assert_eq!(
            plan.source,
            SourceConstraint::Prefer(MemorySourceFamily::Brief)
        );
        assert_eq!(plan.anchors.len(), 1);
        assert_eq!(plan.anchors[0].normalized, "élodie");
    }

    #[test]
    fn spanish_interrogative_is_not_a_named_anchor() {
        let anchors = extract_explicit_anchors("¿Qué decidió Orchid sobre rollout?");

        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].normalized, "orchid");
    }

    #[test]
    fn spanish_interrogative_remains_a_retrieval_term() {
        assert!(lexical_terms("¿Qué decidió Orchid?").contains(&"Qué".to_owned()));
    }

    #[test]
    fn memory_query_plan_discards_overlong_unicode_and_quoted_anchors() {
        let overlong = "界".repeat(200);
        let query = format!("{overlong} \"{overlong}\"");

        let plan = plan_memory_query(
            &query,
            300,
            LocalDayBounds {
                yesterday_start_ms: 100,
                today_start_ms: 200,
            },
        );

        assert!(plan.anchors.is_empty());
    }

    #[test]
    fn named_anchor_scanner_stops_at_first_over_limit_character() {
        let token = format!("A{}", "é".repeat(4_095));

        assert_eq!(
            scan_named_anchor(&token),
            NamedAnchorScan::OverlongAt(MAX_ANSWER_ANCHOR_CHARS + 1)
        );
    }

    #[test]
    fn named_anchor_character_boundary_accepts_96_and_rejects_97() {
        let accepted = format!("A{}", "é".repeat(95));
        let rejected = format!("A{}", "é".repeat(96));

        assert_eq!(extract_explicit_anchors(&accepted).len(), 1);
        assert!(extract_explicit_anchors(&rejected).is_empty());
    }

    #[test]
    fn memory_query_plan_leaves_implicit_paraphrase_cross_kind() {
        let plan = plan_memory_query(
            "tell me about Phoenix",
            300,
            LocalDayBounds {
                yesterday_start_ms: 100,
                today_start_ms: 200,
            },
        );

        assert_eq!(plan.source, SourceConstraint::Any);
    }
}
