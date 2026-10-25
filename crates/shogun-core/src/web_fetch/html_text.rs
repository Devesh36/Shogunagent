//! HTML to readable text, without an HTML parser (FR-WV-12).
//!
//! The model needs the words on the page, not the DOM: strip what is never prose (scripts,
//! styles, navigation), prefer the article body when the page marks one, turn block boundaries
//! into line breaks, decode entities, and stop at the cap. Written as a hand scanner over the
//! bytes — the pages this reads are the user's own links, and a parser dependency whose failure
//! mode is "chat cannot read pages" is not worth carrying for that.

use super::MAX_PAGE_CHARS;

/// The page as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub title: Option<String>,
    pub text: String,
    /// The text was cut at the cap; the answer has to say so.
    pub truncated: bool,
}

/// Blocks whose contents are never prose.
const DROP_ALWAYS: [&str; 6] = ["script", "style", "noscript", "svg", "template", "iframe"];
/// Blocks that are chrome around the prose, dropped once the region is chosen.
const DROP_CHROME: [&str; 5] = ["nav", "header", "footer", "aside", "form"];
/// Tags that end a line of text.
const BLOCK: [&str; 24] = [
    "p", "div", "br", "li", "h1", "h2", "h3", "h4", "h5", "h6", "tr", "section", "article",
    "blockquote", "pre", "ul", "ol", "table", "hr", "dd", "dt", "figcaption", "main", "body",
];

/// Extract readable text, at most `max_chars` characters.
pub fn extract_text(html: &str, max_chars: usize) -> Extracted {
    let title = inner_of(html, "title").map(|t| collapse_line(&decode_entities(&strip_tags(t))));
    let title = title.filter(|t| !t.is_empty());

    let doc = remove_comments(html);
    let doc = remove_blocks(&doc, &DROP_ALWAYS);
    let region = pick_region(&doc).to_string();
    let region = remove_blocks(&region, &DROP_CHROME);
    let text = to_text(&region);

    let (text, truncated) = cap(&text, max_chars);
    Extracted {
        title,
        text,
        truncated,
    }
}

/// Text of an inline fragment (a caption, a post body): `<br>` becomes a line break, other tags
/// vanish, entities decode.
pub fn inline_text(fragment: &str) -> String {
    let with_breaks = replace_breaks(fragment);
    let stripped = strip_tags(&with_breaks);
    decode_entities(&stripped)
        .lines()
        .map(collapse_line)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn cap(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    // Cut at the last line break before the cap when there is one, so the tail is a whole line.
    let head: String = text.chars().take(max_chars).collect();
    // `rfind` is a byte offset; the halfway guard has to be one too, or multi-byte text lets a
    // break far before the midpoint through.
    let half = head
        .char_indices()
        .nth(max_chars / 2)
        .map(|(b, _)| b)
        .unwrap_or(head.len());
    let cut = head.rfind('\n').filter(|&i| i > half).unwrap_or(head.len());
    (head[..cut].trim_end().to_string(), true)
}

/// The tag name at `html[i..]` when `html[i] == '<'`, lower-cased, without `/`.
fn tag_name_at(lower: &str, i: usize) -> Option<(String, bool)> {
    let rest = &lower[i + 1..];
    let closing = rest.starts_with('/');
    let rest = rest.trim_start_matches('/');
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!name.is_empty()).then_some((name, closing))
}

/// Remove `<tag …>…</tag>` for each tag, case-insensitively; an unclosed block runs to the end.
fn remove_blocks(html: &str, tags: &[&str]) -> String {
    let mut out = html.to_string();
    for tag in tags {
        loop {
            let lower = out.to_ascii_lowercase();
            let Some(start) = find_open_tag(&lower, tag, 0) else {
                break;
            };
            let close_pat = format!("</{tag}");
            let end = match lower[start..].find(&close_pat) {
                Some(rel) => {
                    let close_at = start + rel;
                    lower[close_at..]
                        .find('>')
                        .map(|gt| close_at + gt + 1)
                        .unwrap_or(out.len())
                }
                None => out.len(),
            };
            out.replace_range(start..end, "\n");
        }
    }
    out
}

/// Byte offset of `<tag` followed by whitespace, `>` or `/`, searching from `from`.
fn find_open_tag(lower: &str, tag: &str, from: usize) -> Option<usize> {
    let pat = format!("<{tag}");
    let mut at = from;
    while let Some(rel) = lower[at..].find(&pat) {
        let i = at + rel;
        let after = lower[i + pat.len()..].chars().next();
        let boundary = match after {
            None => true,
            Some(c) => c == '>' || c == '/' || c.is_whitespace(),
        };
        if boundary {
            return Some(i);
        }
        at = i + pat.len();
    }
    None
}

fn remove_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The inner HTML of the first `<tag>…</tag>`.
fn inner_of<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let start = find_open_tag(&lower, tag, 0)?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find(&format!("</{tag}"))? + open_end;
    Some(&html[open_end..close])
}

/// The prose region: `<article>`, else `<main>`, else `<body>`, else the whole document.
fn pick_region(doc: &str) -> &str {
    inner_of(doc, "article")
        .or_else(|| inner_of(doc, "main"))
        .or_else(|| inner_of(doc, "body"))
        .unwrap_or(doc)
}

fn replace_breaks(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(i) = find_open_tag(&lower, "br", at) {
        out.push_str(&html[at..i]);
        out.push('\n');
        at = lower[i..].find('>').map(|gt| i + gt + 1).unwrap_or(html.len());
    }
    out.push_str(&html[at..]);
    out
}

/// Tags to text: block tags become line breaks, everything else disappears.
fn to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < html.len() {
        // A '<' opens a tag only when a name, '/', '!' or '?' follows; "a < b" in prose is text.
        if html.as_bytes()[i] == b'<' && is_tag_start(html.as_bytes().get(i + 1).copied()) {
            let Some(gt) = html[i..].find('>') else {
                // An unterminated tag at the end of the document: keep the text rather than
                // dropping everything after it.
                out.push_str(&html[i..]);
                break;
            };
            if let Some((name, _)) = tag_name_at(&lower, i) {
                if BLOCK.contains(&name.as_str()) {
                    out.push('\n');
                }
            }
            i += gt + 1;
            continue;
        }
        let c = html[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    // One line per block, no blank lines: a paragraph boundary is a newline, and the chunker
    // splits on newlines. Blank runs carry no information the block tags did not already.
    decode_entities(&out)
        .lines()
        .map(collapse_line)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_tag_start(next: Option<u8>) -> bool {
    matches!(next, Some(b) if b.is_ascii_alphabetic() || b == b'/' || b == b'!' || b == b'?')
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Collapse runs of whitespace inside one line and trim it.
fn collapse_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The entities pages actually use, plus numeric references.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';').filter(|&s| s <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "copy" => Some('©'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    if let Some(hex) = n.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        n.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The default cap for a page (FR-WV-12).
pub fn extract_page(html: &str) -> Extracted {
    extract_text(html, MAX_PAGE_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_less_than_in_prose_is_text_not_a_tag() {
        let t = to_text("<p>when a < b the loop exits.</p><p>Second paragraph</p>");
        assert_eq!(t, "when a < b the loop exits.\nSecond paragraph");
        assert_eq!(to_text("<p>x < y"), "x < y");
    }

    #[test]
    fn the_cap_halfway_guard_counts_characters_not_bytes() {
        // 3-byte characters: a newline at char 40 of 100 is before the midpoint and is not a
        // cut point, so the cap lands at the budget, not a sixth of it.
        let line = "あ".repeat(40);
        let text = format!("{line}\n{}", "い".repeat(200));
        let (kept, truncated) = cap(&text, 100);
        assert!(truncated);
        assert_eq!(kept.chars().count(), 100);
    }

    const PAGE: &str = r#"<!doctype html><html><head><title> The &amp; Title </title><style>p{color:red}</style>
<script>var x = "<p>not text</p>";</script></head><body><nav><a href="/">Home</a> <a>About</a></nav>
<!-- a comment <p>hidden</p> -->
<header><h1>Site</h1></header>
<main><article><h1>Heading</h1><p>First   paragraph &mdash; with <b>bold</b> and &#x3042;.</p>
<ul><li>one</li><li>two</li></ul><p>Line one<br/>line two</p></article></main>
<aside>Related</aside><footer>© 2026</footer></body></html>"#;

    #[test]
    fn prose_comes_out_and_chrome_scripts_and_comments_do_not() {
        let got = extract_page(PAGE);
        assert_eq!(got.title.as_deref(), Some("The & Title"));
        assert_eq!(
            got.text,
            "Heading\nFirst paragraph — with bold and あ.\none\ntwo\nLine one\nline two"
        );
        assert!(!got.truncated);
        for absent in ["Home", "About", "Site", "not text", "hidden", "Related", "2026", "color"] {
            assert!(!got.text.contains(absent), "{absent} leaked");
        }
    }

    #[test]
    fn without_an_article_the_body_is_read_and_unclosed_scripts_run_to_the_end() {
        let got = extract_page("<html><body><p>Hi</p><script>forever");
        assert_eq!(got.text, "Hi");
        let got = extract_page("no tags at all");
        assert_eq!(got.text, "no tags at all");
        assert_eq!(got.title, None);
    }

    #[test]
    fn the_cap_cuts_on_a_line_and_says_so() {
        let html = format!("<p>{}</p><p>{}</p><p>{}</p>", "a".repeat(40), "b".repeat(40), "c".repeat(40));
        let got = extract_text(&html, 100);
        assert!(got.truncated);
        assert_eq!(got.text, format!("{}\n{}", "a".repeat(40), "b".repeat(40)));
        assert!(!extract_text(&html, 1_000).truncated);
    }

    #[test]
    fn inline_fragments_keep_breaks_and_lose_tags() {
        assert_eq!(inline_text("a &amp; b<br>c <a href=x>d</a>"), "a & b\nc d");
        assert_eq!(inline_text("  spaced   out  <BR />  x "), "spaced out\nx");
    }

    #[test]
    fn entities_decode_and_stray_ampersands_survive() {
        assert_eq!(decode_entities("&lt;p&gt; &quot;q&quot; &#39;s&#39; &#8212; &#x1F600;"), "<p> \"q\" 's' — 😀");
        assert_eq!(decode_entities("R&D & more &unknown; &"), "R&D & more &unknown; &");
    }

    #[test]
    fn a_tag_prefix_is_not_the_tag() {
        // `<pre>` must not be mistaken for `<p>`, and `<navigation-menu>` is not `<nav>`.
        let got = extract_page("<body><navigation-menu>keep</navigation-menu><pre>code</pre></body>");
        assert_eq!(got.text, "keep\ncode");
    }
}
