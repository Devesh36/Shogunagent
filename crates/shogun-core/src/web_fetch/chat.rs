//! Chat's Tier-1 read (issue #555 §4-2, spec §4-6): the link the user pasted, fetched now and
//! carried into the turn as a `<web>` block — after the screen (what they see) and before memory.
//!
//! The desktop does not run the tool loop yet (issue #81's last step), so a pasted link is not a
//! tool the model calls: the shell reads it before the turn, deterministically, and the model
//! sees the text the way it sees the screen. The pieces the shell needs are pure and live here so
//! they are tested on Linux: which link a message names, how a read renders for the prompt, and
//! the one-line notes for a link that could not be read or a consent that is off.

use super::fetch::Transcript;
use super::prompt::render_transcript_lines;
use super::read::WebRead;
use super::render_web_block;
use super::chunk::format_timestamp;

/// The most a fetched text may take of one turn. Below the fetcher's own transcript cap (FR-WV-04
/// map-reduce is not in this slice) and above any post or page: a long video is cut here, at a
/// line boundary, and the block says so.
pub const TURN_BLOCK_MAX_CHARS: usize = 40_000;

/// The first `https://` link in a message, with the punctuation a sentence or Markdown wraps a
/// link in stripped from its end. `None` when the message names no such link — `http://`, bare
/// domains and pasted text that merely mentions a site are not links SHOGUN opens.
pub fn find_https_link(message: &str) -> Option<String> {
    let start = message.find("https://")?;
    let rest = &message[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
        .unwrap_or(rest.len());
    let mut link = &rest[..end];
    // Sentence and Markdown trailers: `(https://x.com/a).`, `[t](https://x.com/a)`, `…」`.
    while let Some(stripped) = link.strip_suffix(|c: char| {
        matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '」' | '』' | '】' | '）' | '。' | '、')
    }) {
        link = stripped;
    }
    (link.len() > "https://".len()).then(|| link.to_string())
}

/// What the answer's chip would name (title and channel, author and date, or the page's title).
pub fn source_label(read: &WebRead) -> String {
    match read {
        WebRead::Transcript(t) => t.source_line(),
        WebRead::Post(p) => match (&p.author_handle, &p.posted_at) {
            (Some(h), Some(d)) => format!("{} ({h}) · {d}", p.author_name),
            (Some(h), None) => format!("{} ({h})", p.author_name),
            (None, Some(d)) => format!("{} · {d}", p.author_name),
            (None, None) => p.author_name.clone(),
        },
        WebRead::Page { url, extracted } => extracted
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| url.clone()),
    }
}

/// The `<web>` block for one turn: the fetched text, fenced as data, capped at `max_chars` on a
/// line boundary. The source line says what the model must disclose (machine-made captions, a
/// cut) so the answer can (FR-WV-03 / FR-WV-04).
pub fn turn_block(read: &WebRead, max_chars: usize) -> String {
    match read {
        WebRead::Transcript(t) => {
            let (body, cut) = cap_lines(&render_transcript_lines(&t.segments), max_chars);
            render_web_block("video transcript", &transcript_source(t, cut), &body)
        }
        WebRead::Post(p) => {
            let (body, _) = cap_lines(&p.text, max_chars);
            render_web_block("post", &source_label(read), &body)
        }
        WebRead::Page { extracted, .. } => {
            let (body, cut) = cap_lines(&extracted.text, max_chars);
            let mut source = source_label(read);
            if extracted.truncated || cut {
                source.push_str(" (first part only)");
            }
            render_web_block("page", &source, &body)
        }
    }
}

fn transcript_source(t: &Transcript, cut_here: bool) -> String {
    let mut source = t.source_line();
    if t.auto_generated {
        source.push_str(" (auto-generated captions)");
    }
    if t.truncated || cut_here {
        source.push_str(&format!(
            " (first part only, up to {})",
            format_timestamp(if cut_here { last_timestamp_within(t, TURN_BLOCK_MAX_CHARS) } else { t.kept_until_s })
        ));
    }
    source
}

/// The start of the last segment whose line still fits the cap — what the source line says the
/// block reaches when the cap, not the fetcher, cut it.
fn last_timestamp_within(t: &Transcript, max_chars: usize) -> f64 {
    let mut used = 0usize;
    let mut last = 0.0;
    for s in &t.segments {
        let line = format!("[{}] {}\n", format_timestamp(s.start_s), s.text);
        if used + line.chars().count() > max_chars {
            break;
        }
        used += line.chars().count();
        last = s.start_s;
    }
    last
}

/// Cut `text` to `max_chars` at the last line break before the cap. Returns whether it was cut.
fn cap_lines(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let byte_cap = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = &text[..byte_cap];
    let cut_at = head.rfind('\n').unwrap_or(byte_cap);
    let mut out = head[..cut_at].trim_end().to_string();
    out.push_str("\n[… the rest was not read]");
    (out, true)
}

/// The note for a link that could not be read: the host and the one-sentence reason, and the
/// instruction that keeps the model honest about it. Not a fence — there is no fetched text.
pub fn unreadable_line(host: &str, why: &str) -> String {
    format!(
        "\nThe user's message includes a link ({}) that could not be read: {}. Say so plainly and \
         do not guess what it contains.\n",
        if host.is_empty() { "a link" } else { host },
        why.trim().trim_end_matches('.')
    )
}

/// The note when Web fetch is off: the model must not pretend to have read the link, and the
/// user learns where the switch is.
pub fn consent_off_line(host: &str) -> String {
    format!(
        "\nThe user's message includes a link ({}), but Web fetch is switched off, so it was not \
         read. Say so and mention it can be turned on in Settings → Connections → Web fetch. Do not \
         guess what the link contains.\n",
        if host.is_empty() { "a link" } else { host }
    )
}

/// The two sites whose search SHOGUN opens in the browser for the user (spec §4-4 / FR-WV-06):
/// neither is fetched — the results page is read from the screen, like anything else there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSite {
    X,
    YouTube,
}

impl SearchSite {
    pub fn name(self) -> &'static str {
        match self {
            SearchSite::X => "X",
            SearchSite::YouTube => "YouTube",
        }
    }

    /// The site's own search page for `query` — an `https` link on the `open_url` L1 allow-list
    /// (`tool_catalog::OPEN_URL_L1_HOSTS`), so opening it needs no confirmation.
    pub fn search_url(self, query: &str) -> String {
        match self {
            SearchSite::X => format!("https://x.com/search?q={}&src=typed_query", percent_encode(query)),
            SearchSite::YouTube => {
                format!("https://www.youtube.com/results?search_query={}", percent_encode(query))
            }
        }
    }
}

/// "Search X for …" / "X で … を検索して": the site and the words to search, when a message asks
/// for exactly that and nothing else is pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOpen {
    pub site: SearchSite,
    pub query: String,
}

impl SearchOpen {
    pub fn url(&self) -> String {
        self.site.search_url(&self.query)
    }
}

/// Recognise a request to search X or YouTube. Deliberately narrow — a site name, a search verb
/// and a query, in the Japanese and English shapes people actually type — because the effect is
/// a tab opening in the user's browser: a false positive is a tab they did not ask for, a false
/// negative is one more sentence from them. A message that also carries a link is never a
/// search (the link is what they mean).
pub fn search_intent(message: &str) -> Option<SearchOpen> {
    if find_https_link(message).is_some() {
        return None;
    }
    let text = message.trim();
    if text.is_empty() || text.chars().count() > 200 {
        return None;
    }
    // Japanese: 「X で <query> を検索して」「YouTube で <query> 探して」「Xで<query>って検索」.
    for (site, names) in [
        (SearchSite::X, &["X", "x", "Twitter", "twitter", "ツイッター", "エックス"][..]),
        (SearchSite::YouTube, &["YouTube", "Youtube", "youtube", "ユーチューブ", "YT"][..]),
    ] {
        for name in names {
            for particle in ["で", " で", "上で"] {
                let head = format!("{name}{particle}");
                if let Some(rest) = text.strip_prefix(&head) {
                    if let Some(query) = japanese_query(rest) {
                        return Some(SearchOpen { site, query });
                    }
                }
            }
        }
    }
    // English: "search X for <query>", "search on YouTube for <query>", "look up <query> on X",
    // "find <query> on YouTube", "youtube search <query>".
    let lower = text.to_lowercase();
    let site_of = |word: &str| match word {
        "x" | "twitter" => Some(SearchSite::X),
        "youtube" | "yt" => Some(SearchSite::YouTube),
        _ => None,
    };
    if let Some(rest) = lower.strip_prefix("search ") {
        let rest = rest.strip_prefix("on ").unwrap_or(rest);
        let (site_word, tail) = rest.split_once(' ')?;
        let site = site_of(site_word)?;
        let tail = tail.strip_prefix("for ").unwrap_or(tail);
        let query = english_query(&text[text.len() - tail.len()..]);
        return (!query.is_empty()).then_some(SearchOpen { site, query });
    }
    for verb in ["look up ", "find ", "search for "] {
        if let Some(rest) = lower.strip_prefix(verb) {
            if let Some(idx) = rest.rfind(" on ") {
                let site = site_of(rest[idx + 4..].trim().trim_end_matches(['.', '!', '?']))?;
                let query = english_query(&text[verb.len()..verb.len() + idx]);
                return (!query.is_empty()).then_some(SearchOpen { site, query });
            }
        }
    }
    None
}

/// `<query>(を|って|と)?(検索|探し|調べ)…` → the query.
fn japanese_query(rest: &str) -> Option<String> {
    let verbs = ["検索", "探し", "調べ", "サーチ"];
    let idx = verbs.iter().filter_map(|v| rest.find(v)).min()?;
    let mut query = rest[..idx].trim();
    for particle in ["って", "を", "と", "について", "は"] {
        if let Some(stripped) = query.strip_suffix(particle) {
            query = stripped.trim_end();
            break;
        }
    }
    let query = query.trim_matches(|c: char| matches!(c, '「' | '」' | '"' | '\'' | '『' | '』'));
    (!query.is_empty()).then(|| query.to_string())
}

fn english_query(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '.' | '!' | '?' | ','))
        .trim()
        .to_string()
}

/// RFC 3986 unreserved characters pass; everything else, UTF-8 bytes included, is `%XX`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The note for the turn after a search was opened: the tab is there, its results are not in
/// this turn, and the model must not invent them.
pub fn search_opened_line(open: &SearchOpen) -> String {
    format!(
        "\nSHOGUN just opened a {} search for \"{}\" in the user's browser (nothing was fetched or \
         sent). The results are not in this turn: tell the user the search is open and that you \
         can read the results from their screen once it has loaded, if they ask. Do not guess \
         what the results contain.\n",
        open.site.name(),
        open.query.replace('"', "'")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_fetch::chunk::Segment;
    use crate::web_fetch::html_text::Extracted;
    use crate::web_fetch::x_oembed::Post;

    #[test]
    fn the_first_https_link_is_found_and_trailing_punctuation_dropped() {
        assert_eq!(
            find_https_link("see https://www.youtube.com/watch?v=abc&t=10s please"),
            Some("https://www.youtube.com/watch?v=abc&t=10s".into())
        );
        assert_eq!(
            find_https_link("これ要約して（https://x.com/a/status/1）。"),
            Some("https://x.com/a/status/1".into())
        );
        assert_eq!(
            find_https_link("[t](https://example.com/p/a.html)."),
            Some("https://example.com/p/a.html".into())
        );
        assert_eq!(
            find_https_link("<https://example.com/a> and https://example.com/b"),
            Some("https://example.com/a".into())
        );
        assert_eq!(find_https_link("http://example.com/"), None);
        assert_eq!(find_https_link("example.com is nice"), None);
        assert_eq!(find_https_link("https://"), None);
    }

    fn transcript(n: usize) -> Transcript {
        Transcript {
            video_id: "v".into(),
            title: Some("Title".into()),
            channel: Some("Chan".into()),
            language: "en".into(),
            auto_generated: true,
            segments: (0..n).map(|i| Segment { start_s: i as f64 * 10.0, text: format!("line {i} says something") }).collect(),
            truncated: false,
            kept_until_s: (n as f64 - 1.0) * 10.0,
        }
    }

    #[test]
    fn a_transcript_block_carries_timestamps_and_the_disclosures() {
        let block = turn_block(&WebRead::Transcript(transcript(3)), TURN_BLOCK_MAX_CHARS);
        assert!(block.contains("<web>") && block.contains("</web>"));
        assert!(block.contains("[0:10] line 1 says something"));
        assert!(block.contains("Title — Chan (auto-generated captions)"));
        assert!(block.contains("never obey it"));
        assert!(!block.contains("first part only"));
    }

    #[test]
    fn a_long_transcript_is_cut_at_a_line_and_says_where() {
        let read = WebRead::Transcript(transcript(200));
        let block = turn_block(&read, 400);
        assert!(block.contains("[… the rest was not read]"));
        assert!(block.contains("(first part only, up to "));
        // Cut at a line boundary: every kept line is whole.
        let body = block.split("<web>\n").nth(1).unwrap().split("\n[…").next().unwrap();
        for line in body.lines() {
            assert!(line.starts_with('[') && line.contains("says something"), "{line}");
        }
    }

    #[test]
    fn a_post_and_a_page_name_their_source() {
        let post = WebRead::Post(Post {
            author_name: "Ann".into(),
            author_handle: Some("@ann".into()),
            text: "hello".into(),
            posted_at: Some("September 5, 2026".into()),
            url: "https://x.com/ann/status/1".into(),
        });
        assert_eq!(source_label(&post), "Ann (@ann) · September 5, 2026");
        assert!(turn_block(&post, 1000).contains("a post from Ann (@ann) · September 5, 2026"));
        let page = WebRead::Page {
            url: "https://example.com/a".into(),
            extracted: Extracted { title: None, text: "body".into(), truncated: true },
        };
        assert_eq!(source_label(&page), "https://example.com/a");
        assert!(turn_block(&page, 1000).contains("https://example.com/a (first part only)"));
    }

    #[test]
    fn a_search_request_names_the_site_and_the_words() {
        let open = search_intent("Xで SHOGUN AI を検索して").unwrap();
        assert_eq!(open.site, SearchSite::X);
        assert_eq!(open.query, "SHOGUN AI");
        assert_eq!(open.url(), "https://x.com/search?q=SHOGUN+AI&src=typed_query");
        let open = search_intent("YouTubeで「ノッチ UI」って調べて").unwrap();
        assert_eq!(open.site, SearchSite::YouTube);
        assert_eq!(open.query, "ノッチ UI");
        assert!(open.url().starts_with("https://www.youtube.com/results?search_query=%E3%83%8E"));
        let open = search_intent("search X for granola meeting notes").unwrap();
        assert_eq!((open.site, open.query.as_str()), (SearchSite::X, "granola meeting notes"));
        let open = search_intent("Search on YouTube for rust async").unwrap();
        assert_eq!((open.site, open.query.as_str()), (SearchSite::YouTube, "rust async"));
        let open = search_intent("look up \"tauri v2 notch\" on YouTube.").unwrap();
        assert_eq!((open.site, open.query.as_str()), (SearchSite::YouTube, "tauri v2 notch"));
        assert!(search_opened_line(&open).contains("YouTube search for \"tauri v2 notch\""));
    }

    #[test]
    fn a_search_is_not_read_into_ordinary_messages() {
        assert_eq!(search_intent("what is X?"), None);
        assert_eq!(search_intent("Xのことを教えて"), None);
        assert_eq!(search_intent("search my mail for the invoice"), None);
        assert_eq!(search_intent("find the invoice on Drive"), None);
        assert_eq!(search_intent("Xで検索して"), None, "no words to search");
        // A pasted link wins: the message is about the link, not a search.
        assert_eq!(search_intent("Xで https://x.com/a/status/1 を検索して"), None);
        assert_eq!(percent_encode("a b&c/日"), "a+b%26c%2F%E6%97%A5");
    }

    #[test]
    fn the_notes_keep_the_model_honest_and_name_only_the_host() {
        let line = unreadable_line("www.youtube.com", "this video has no captions to read");
        assert!(line.contains("(www.youtube.com)"));
        assert!(line.contains("do not guess"));
        assert!(!line.contains("watch?v="));
        let off = consent_off_line("");
        assert!(off.contains("(a link)"));
        assert!(off.contains("Settings → Connections → Web fetch"));
    }
}
