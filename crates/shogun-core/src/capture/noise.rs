//! Accessibility-text noise reduction (issue #302). Pure, per-line, no allocation when a line is
//! already clean.
//!
//! The walk ([`super::walk_policy::walk`]) calls [`clean_line`] on every collected line when
//! [`super::walk_policy::Limits::reduce_noise`] is on. The rules are generic — about characters
//! and shapes, never about an app, a bundle id or a language — and each is chosen so the words a
//! user could search for survive (the retention check in `crates/ax-noise-bench`):
//!
//! - **Invisible characters go.** Zero-width spaces/joiners, the BOM, bidi controls and
//!   private-use code points (icon fonts render toolbar glyphs from these) carry no searchable
//!   text but do cost tokens and break line dedup.
//! - **Whitespace collapses.** Any run of whitespace inside a line becomes one space, so the same
//!   label laid out twice dedups to one line.
//! - **Symbol-only lines go.** A line with no letter or digit in any script (`•`, `·`, `›`, `|`,
//!   `…`, `×`, emoji-only reactions) is layout, not content. Digits are kept: a spreadsheet cell
//!   `42` is content.
//! - **Machine blobs are clipped.** An unbroken ASCII run longer than [`MAX_ASCII_RUN`] (base64,
//!   data URIs, tracking query strings) keeps its first [`KEPT_ASCII_RUN`] characters and an
//!   ellipsis. Non-ASCII runs are never clipped: CJK prose has no spaces at all.

use std::borrow::Cow;

/// Longest unbroken ASCII run kept whole. Real words, identifiers, file paths and ordinary URLs fit.
pub const MAX_ASCII_RUN: usize = 120;
/// How much of a clipped run is kept — enough for a URL's host and path head.
pub const KEPT_ASCII_RUN: usize = 64;

/// Zero-width and formatting code points that render as nothing.
fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x00AD            // soft hyphen
        | 0x200B..=0x200F // zero-width space/joiners, LRM/RLM
        | 0x202A..=0x202E // bidi embedding controls
        | 0x2060..=0x2064 // word joiner, invisible operators
        | 0x2066..=0x2069 // bidi isolates
        | 0xFEFF          // BOM / zero-width no-break space
        | 0xE000..=0xF8FF // private use (icon fonts)
        | 0xF0000..=0x10FFFF)
}

/// One collected line, cleaned. `None` when nothing searchable is left. Borrowed when the line was
/// already clean, which is the common case.
pub fn clean_line(line: &str) -> Option<Cow<'_, str>> {
    let trimmed = line.trim();
    if !trimmed.chars().any(char::is_alphanumeric) {
        return None;
    }
    if is_clean(trimmed) {
        return Some(Cow::Borrowed(trimmed));
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut pending_space = false;
    let mut run_start = 0usize; // byte offset in `out` where the current ASCII run began
    let mut run_chars = 0usize;
    for c in trimmed.chars() {
        if is_invisible(c) {
            continue;
        }
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            run_chars = 0;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
            run_chars = 0;
        }
        if c.is_ascii() {
            if run_chars == 0 {
                run_start = out.len();
            }
            run_chars += 1;
            if run_chars <= MAX_ASCII_RUN {
                out.push(c);
            } else if run_chars == MAX_ASCII_RUN + 1 {
                // Clip: keep the head, mark the cut. The rest of the run is skipped.
                out.truncate(run_start + KEPT_ASCII_RUN);
                out.push('…');
            }
        } else {
            run_chars = 0;
            out.push(c);
        }
    }
    let out = out.trim().to_string();
    out.chars().any(char::is_alphanumeric).then_some(Cow::Owned(out))
}

/// True when [`clean_line`] would return the (trimmed) input unchanged.
fn is_clean(line: &str) -> bool {
    let mut previous_space = false;
    let mut run = 0usize;
    for c in line.chars() {
        if is_invisible(c) {
            return false;
        }
        if c.is_whitespace() {
            if c != ' ' || previous_space {
                return false;
            }
            previous_space = true;
            run = 0;
            continue;
        }
        previous_space = false;
        if c.is_ascii() {
            run += 1;
            if run > MAX_ASCII_RUN {
                return false;
            }
        } else {
            run = 0;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_lines_are_borrowed_unchanged() {
        for line in ["Quarterly review with Alice", "42", "決算資料を金曜までに", "a b c"] {
            assert!(matches!(clean_line(line), Some(Cow::Borrowed(l)) if l == line), "{line}");
        }
        assert!(matches!(clean_line("  padded  "), Some(Cow::Borrowed("padded"))));
    }

    #[test]
    fn symbol_only_lines_are_dropped_but_digits_and_scripts_are_not() {
        for noise in ["•", "·", " › ", "|", "…", "×", "👍", "— —", "\u{200B}", "\u{E5D2}", ""] {
            assert_eq!(clean_line(noise), None, "{noise:?}");
        }
        assert_eq!(clean_line("👍 3").as_deref(), Some("👍 3"));
        assert_eq!(clean_line("Ω").as_deref(), Some("Ω"));
        assert_eq!(clean_line("会議").as_deref(), Some("会議"));
    }

    #[test]
    fn whitespace_collapses_and_invisible_characters_go() {
        assert_eq!(clean_line("Reply\t  all").as_deref(), Some("Reply all"));
        assert_eq!(clean_line("Pro\u{200B}ject\u{00A0}Orchid").as_deref(), Some("Project Orchid"));
        assert_eq!(clean_line("\u{E5D2} Menu").as_deref(), Some("Menu"));
        assert_eq!(clean_line("\u{FEFF}\u{202E}Launch").as_deref(), Some("Launch"));
    }

    #[test]
    fn long_ascii_blobs_are_clipped_but_prose_and_cjk_are_not() {
        let blob = format!("data:image/png;base64,{}", "A".repeat(400));
        let line = format!("see {blob} here");
        let cleaned = clean_line(&line).unwrap();
        assert_eq!(cleaned, format!("see {}… here", &blob[..KEPT_ASCII_RUN]));
        let url = "https://github.com/acme/api/pull/4242/files#diff-3f2a";
        assert_eq!(clean_line(url).as_deref(), Some(url));
        let prose = "word ".repeat(200);
        assert_eq!(clean_line(&prose).as_deref(), Some(prose.trim()));
        let cjk = "決算".repeat(300);
        assert_eq!(clean_line(&cjk).as_deref(), Some(cjk.as_str()));
        // Exactly at the limit stays whole.
        let edge = "x".repeat(MAX_ASCII_RUN);
        assert_eq!(clean_line(&edge).as_deref(), Some(edge.as_str()));
    }
}
