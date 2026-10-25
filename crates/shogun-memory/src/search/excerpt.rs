use super::terms::{normalize, query_concepts, Concept};

/// Lines this short are UI chrome far more often than content (`Drafts 3`, `受信`, `Inbox 24`).
const CHROME_LINE_CHARS: usize = 12;
/// Occurrences of one concept considered when choosing the window.
const MAX_POSITIONS_PER_CONCEPT: usize = 64;
/// Occurrences considered in total (the window search is quadratic in this).
const MAX_HITS: usize = 256;

/// A relevance-centred excerpt of `content`, at most `max_chars` characters.
///
/// A single captured window can be thousands of characters, so handing the model its first N
/// wastes the budget on whatever happened to be at the top of the window — usually chrome, not
/// the part that matched. This places the window where the most distinct query concepts occur
/// (`terms.rs`: CJK words, okurigana-folded compounds, Latin words and short tokens), preferring
/// running text over short chrome lines, and falls back to the first line of running text when
/// nothing matches.
///
/// Char-boundary safe: it slices by `char`, never by byte, so multi-byte text is never cut in
/// half. Matching is case-insensitive and NFKC-insensitive wherever normalisation keeps the
/// character count (positions are mapped back one-to-one), and exact otherwise.
pub fn excerpt(content: &str, query: &str, max_chars: usize) -> String {
    // Keep a little of the run-up so the match reads in context rather than starting mid-thought.
    window(content, query, max_chars, max_chars / 3)
}

/// Longest `snippet` on a search hit, ellipses included.
pub const SNIPPET_CHARS: usize = 240;

/// A short preview with the best query match in the middle, at most [`SNIPPET_CHARS`] characters
/// including the `…` marking each cut end. Same placement rules as [`excerpt`].
pub fn snippet(content: &str, query: &str) -> String {
    let max_chars = SNIPPET_CHARS - 2;
    window(content, query, max_chars, max_chars / 2)
}

/// `max_chars` of `content`, starting `lead` chars before the chosen match.
fn window(content: &str, query: &str, max_chars: usize, lead: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if max_chars == 0 {
        return String::new();
    }
    if chars.len() <= max_chars {
        return content.trim().to_string();
    }
    let lowered = searchable(&chars);
    let chrome = chrome_mask(&chars);
    let concepts = query_concepts(query);
    let mut hits: Vec<(usize, usize)> = Vec::new(); // (position, concept index)
    for (index, concept) in concepts.iter().enumerate() {
        for position in positions(&lowered, concept) {
            hits.push((position, index));
        }
    }
    hits.sort_unstable();
    hits.truncate(MAX_HITS);

    let mut start = match best_window(&hits, &chrome, max_chars, lead) {
        Some(anchor) => anchor.saturating_sub(lead),
        None => first_running_text(&chars, &chrome).unwrap_or(0),
    };
    let mut end = start + max_chars;
    if end > chars.len() {
        end = chars.len();
        start = end.saturating_sub(max_chars);
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(chars[start..end].iter().collect::<String>().trim());
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Lowercased, NFKC-folded copy with the same character count as `chars` (a character whose
/// folding would change the length is kept as-is, so positions stay aligned).
fn searchable(chars: &[char]) -> Vec<char> {
    chars.iter().map(|character| fold(*character)).collect()
}

fn fold(character: char) -> char {
    if character.is_ascii() {
        return character.to_ascii_lowercase();
    }
    // Full-width ASCII and half-width katakana are the compatibility forms users actually type.
    let candidate = if (0xFF00..=0xFFEF).contains(&(character as u32)) {
        let folded = normalize(&character.to_string()).chars().collect::<Vec<_>>();
        match folded.as_slice() {
            [single] => *single,
            _ => character,
        }
    } else {
        character
    };
    let mut lower = candidate.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => candidate,
    }
}

/// `true` for every character on a line short enough to be chrome.
fn chrome_mask(chars: &[char]) -> Vec<bool> {
    let mut mask = vec![false; chars.len()];
    let mut line_start = 0;
    for index in 0..=chars.len() {
        if index == chars.len() || chars[index] == '\n' {
            let line = &chars[line_start..index];
            let visible = line.iter().filter(|character| !character.is_whitespace()).count();
            if visible <= CHROME_LINE_CHARS {
                for flag in &mut mask[line_start..index] {
                    *flag = true;
                }
            }
            line_start = index + 1;
        }
    }
    mask
}

fn find_all(hay: &[char], needle: &[char], short_word: bool) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > hay.len() {
        return out;
    }
    let latin = |character: Option<&char>| {
        character.is_some_and(|c| c.is_alphanumeric() && !super::terms::is_cjk(*c))
    };
    for start in 0..=hay.len() - needle.len() {
        if hay[start..start + needle.len()] == *needle {
            if short_word
                && (latin(start.checked_sub(1).and_then(|before| hay.get(before)))
                    || latin(hay.get(start + needle.len())))
            {
                continue;
            }
            out.push(start);
            if out.len() >= MAX_POSITIONS_PER_CONCEPT {
                break;
            }
        }
    }
    out
}

/// Where `concept` occurs. Folded okurigana (打合せ ↔ 打ち合わせ) is found by the compound's first
/// kanji followed closely by its second.
fn positions(hay: &[char], concept: &Concept) -> Vec<usize> {
    if let Some(surface) = &concept.surface {
        let found = find_all(hay, &surface.chars().collect::<Vec<_>>(), false);
        if !found.is_empty() {
            return found;
        }
    }
    match concept.grams.as_slice() {
        [gram] => {
            let needle = gram.chars().collect::<Vec<_>>();
            let found = find_all(hay, &needle, concept.short_word);
            if !found.is_empty() || concept.short_word {
                return found;
            }
            match needle.as_slice() {
                [first, second] => find_all(hay, &[*first], false)
                    .into_iter()
                    .filter(|at| {
                        hay.iter()
                            .skip(at + 1)
                            .take(3)
                            .any(|character| character == second)
                    })
                    .collect(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Start of the window covering the most distinct concepts; matches on chrome lines count only
/// when nothing else matches.
fn best_window(
    hits: &[(usize, usize)],
    chrome: &[bool],
    max_chars: usize,
    lead: usize,
) -> Option<usize> {
    let mut best: Option<(usize, usize, usize)> = None; // (distinct, running-text hits, anchor)
    for &(anchor, _) in hits {
        let start = anchor.saturating_sub(lead);
        let end = start + max_chars;
        let inside = hits
            .iter()
            .filter(|(position, _)| *position >= start && *position < end);
        let mut distinct = Vec::new();
        let mut running = 0;
        for (position, concept) in inside {
            if !chrome.get(*position).copied().unwrap_or(false) {
                running += 1;
                if !distinct.contains(concept) {
                    distinct.push(*concept);
                }
            }
        }
        let anchor_on_chrome = chrome.get(anchor).copied().unwrap_or(false);
        let key = (distinct.len(), running, usize::from(!anchor_on_chrome));
        let better = match best {
            None => true,
            Some((best_distinct, best_running, _)) => {
                (key.0, key.1) > (best_distinct, best_running)
            }
        };
        if better && !(anchor_on_chrome && distinct.is_empty() && best.is_some()) {
            best = Some((key.0, key.1, anchor));
        }
    }
    best.map(|(_, _, anchor)| anchor)
}

/// The first character of the first line that is not chrome.
fn first_running_text(chars: &[char], chrome: &[bool]) -> Option<usize> {
    (0..chars.len()).find(|&index| !chrome[index] && !chars[index].is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_centres_the_match_and_marks_both_cuts() {
        let content = format!("{}needle{}", "a ".repeat(400), " b".repeat(400));
        let got = snippet(&content, "Needle");
        assert!(got.chars().count() <= SNIPPET_CHARS);
        assert!(got.starts_with('…') && got.ends_with('…'));
        let at = got.chars().position(|c| c == 'n').expect("match kept");
        let after = got.chars().count() - at;
        assert!(at.abs_diff(after) <= 8, "{at} vs {after}");
    }

    #[test]
    fn snippet_keeps_short_text_whole() {
        assert_eq!(snippet("  short text  ", "zzz"), "short text");
        assert!(snippet(&"x ".repeat(500), "zzz").chars().count() <= SNIPPET_CHARS);
    }

    #[test]
    fn a_japanese_match_is_centred_not_the_chrome_at_the_top() {
        let content = format!(
            "{}\n明日の打ち合わせは田中さんと北斗商事の新倉庫について。{}",
            "受信\n下書き\n送信済み\n".repeat(30),
            "後略".repeat(200)
        );
        let out = excerpt(&content, "田中さんの件", 60);
        assert!(out.contains("田中"), "{out}");
    }

    #[test]
    fn okurigana_variants_find_the_written_form() {
        let content = format!("{}明日の打ち合わせは本社。{}", "あ".repeat(400), "い".repeat(400));
        let out = excerpt(&content, "打合せ", 40);
        assert!(out.contains("打ち合わせ"), "{out}");
    }

    #[test]
    fn running_text_beats_chrome_for_the_same_word() {
        let content = format!(
            "Drafts 3\nInbox\n{}\nDraft contract for Kestrel Foods: payment terms net 45.",
            "filler line that is long enough to be text ".repeat(20)
        );
        let out = excerpt(&content, "draft", 80);
        assert!(out.contains("Draft contract"), "{out}");
    }

    #[test]
    fn no_match_skips_leading_chrome() {
        let content = format!("Inbox\nDrafts\nSent\n{}", "A long paragraph of real text. ".repeat(40));
        let out = excerpt(&content, "zebra", 50);
        assert!(out.starts_with("…A long") || out.starts_with("A long"), "{out}");
    }
}
