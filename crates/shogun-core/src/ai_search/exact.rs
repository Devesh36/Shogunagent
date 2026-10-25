//! Exact tokens in a question: quoted phrases, links and identifiers (issue #940).
//!
//! These are the queries keyword search already answers well and an AI rewrite can only make
//! worse. A question made of nothing else skips the model entirely; in a mixed question, results
//! that contain the exact token stay at the top in keyword order, whatever the rerank says.

/// One literal the user typed and expects to find verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactToken {
    /// Text between quotes ("…", “…”, 「…」, 『…』).
    Quoted(String),
    /// An `http(s)://` or `www.` link.
    Link(String),
    /// An identifier-shaped token: ticket keys, invoice numbers, `#941`, versions, emails.
    Identifier(String),
}

impl ExactToken {
    pub fn text(&self) -> &str {
        match self {
            Self::Quoted(t) | Self::Link(t) | Self::Identifier(t) => t,
        }
    }
}

/// Bound on how many exact tokens one question contributes; a pasted log line should not turn
/// every result into an "exact" pin.
const MAX_EXACT_TOKENS: usize = 8;
/// Shortest quoted phrase worth pinning (a quoted single letter is punctuation, not intent).
const MIN_QUOTED_CHARS: usize = 2;

const QUOTE_PAIRS: [(char, char); 4] = [('"', '"'), ('“', '”'), ('「', '」'), ('『', '』')];

/// Words that carry no search intent of their own around an exact token ("find INV-1", "OPS-7
/// のメモ"). A question whose remainder is only these is exact-only.
const FILLER: &[&str] = &[
    "find", "search", "show", "look", "lookup", "for", "the", "about", "where", "what", "when",
    "with", "link", "url", "page", "note", "notes", "mention", "mentions", "mentioning", "ticket",
    "issue", "and", "any", "all", "that", "this", "please", "検索", "探して", "のメモ", "について",
];

fn trim_punct(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '\''
                | '"' | '、' | '。' | '？' | '！'
        )
    })
}

fn is_link(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("www."))
        && token.len() > 8
}

fn is_identifier(token: &str) -> bool {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 3 || chars.len() > 128 {
        return false;
    }
    if !chars
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '#' | '.' | '/' | ':' | '@'))
    {
        return false;
    }
    let digits = chars.iter().any(char::is_ascii_digit);
    let letters = chars.iter().any(char::is_ascii_alphabetic);
    let separators = chars.iter().any(|c| matches!(c, '-' | '_' | '#' | '.' | '/' | ':' | '@'));
    let email = token.contains('@') && token.rsplit('@').next().is_some_and(|d| d.contains('.'));
    // A bare year or count is not an identifier; a word with a digit is (`OPS-1182`, `v1.2`,
    // `#941`, `2026-09-01`), as is snake_case.
    email || (digits && (letters || separators)) || (letters && token.contains('_'))
}

/// Every exact token in `question`, in order of appearance, deduplicated, at most
/// [`MAX_EXACT_TOKENS`].
pub fn exact_tokens(question: &str) -> Vec<ExactToken> {
    fn push(token: ExactToken, out: &mut Vec<ExactToken>) {
        if out.len() < MAX_EXACT_TOKENS
            && !out.iter().any(|t| t.text().eq_ignore_ascii_case(token.text()))
        {
            out.push(token);
        }
    }
    let mut out: Vec<ExactToken> = Vec::new();
    for (open, close) in QUOTE_PAIRS {
        let mut rest = question;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + open.len_utf8()..];
            let Some(end) = after.find(close) else { break };
            let phrase = after[..end].split_whitespace().collect::<Vec<_>>().join(" ");
            if phrase.chars().count() >= MIN_QUOTED_CHARS {
                push(ExactToken::Quoted(phrase), &mut out);
            }
            rest = &after[end + close.len_utf8()..];
        }
    }
    for raw in question.split_whitespace() {
        let token = trim_punct(raw);
        if is_link(raw.trim_end_matches([',', '.', ';', ')', ']', '>', '"', '\''])) {
            push(
                ExactToken::Link(
                    raw.trim_start_matches(['(', '[', '<', '"', '\''])
                        .trim_end_matches([',', '.', ';', ')', ']', '>', '"', '\''])
                        .to_string(),
                ),
                &mut out,
            );
        } else if is_identifier(token) {
            push(ExactToken::Identifier(token.to_string()), &mut out);
        }
    }
    out
}

/// True when the question is nothing but exact tokens (plus filler words): keyword search is the
/// right tool and the model is not consulted.
pub fn is_exact_only_query(question: &str, tokens: &[ExactToken]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let mut rest = question.to_string();
    for (open, close) in QUOTE_PAIRS {
        // Drop every quoted span, quotes included.
        while let Some(start) = rest.find(open) {
            let after = start + open.len_utf8();
            let Some(len) = rest[after..].find(close) else { break };
            rest.replace_range(start..after + len + close.len_utf8(), " ");
        }
    }
    rest.split_whitespace()
        .map(trim_punct)
        .filter(|word| !word.is_empty())
        .filter(|word| !tokens.iter().any(|t| t.text().eq_ignore_ascii_case(word)))
        .filter(|word| !is_link(word))
        .all(|word| {
            let lower = word.to_lowercase();
            FILLER.contains(&lower.as_str()) || word.chars().filter(|c| c.is_alphanumeric()).count() < 3
        })
}

/// Whether `haystack` contains `token` the way a person means it: case-insensitively, with runs
/// of whitespace treated as one space.
pub fn contains_exact(haystack: &str, token: &ExactToken) -> bool {
    let needle = token.text().split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if needle.is_empty() {
        return false;
    }
    let hay = haystack.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    hay.contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_links_and_quotes_are_found() {
        let tokens = exact_tokens(
            "notes on OPS-1182 and #941, see https://github.com/acme/orchid/pull/941. \"single source of truth\" 「打ち上げ延期」 in 2026",
        );
        assert_eq!(
            tokens,
            vec![
                ExactToken::Quoted("single source of truth".into()),
                ExactToken::Quoted("打ち上げ延期".into()),
                ExactToken::Identifier("OPS-1182".into()),
                ExactToken::Identifier("#941".into()),
                ExactToken::Link("https://github.com/acme/orchid/pull/941".into()),
            ]
        );
    }

    #[test]
    fn plain_words_and_bare_numbers_are_not_identifiers() {
        assert!(exact_tokens("decided to delay the launch in 2026").is_empty());
        assert!(exact_tokens("Q3 plan").is_empty());
        assert_eq!(
            exact_tokens("mail from maya@example.com"),
            vec![ExactToken::Identifier("maya@example.com".into())]
        );
    }

    #[test]
    fn exact_only_questions_are_recognised() {
        for q in ["INV-2026-0042", "find INV-2026-0042", "\"push the release\"", "https://example.com/a?b=1", "「打ち上げ延期」"] {
            assert!(is_exact_only_query(q, &exact_tokens(q)), "{q}");
        }
        for q in ["why was OPS-1182 reopened", "what did the doc say about \"single source of truth\"", "delay the launch"] {
            assert!(!is_exact_only_query(q, &exact_tokens(q)), "{q}");
        }
    }

    #[test]
    fn matching_ignores_case_and_spacing() {
        let token = ExactToken::Quoted("single source of truth".into());
        assert!(contains_exact("The event log is the Single  Source\nof Truth.", &token));
        assert!(!contains_exact("source of truth", &token));
    }
}
