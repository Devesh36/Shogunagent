//! Transcript segments, the length cap and the map-reduce split (FR-WV-04).
//!
//! Two rules, both pure: a segment is never cut in the middle (a chunk boundary inside a
//! sentence makes two summaries of half a thought), and when the whole transcript is over the
//! cap the *first* part is kept and the caller is told — never a silent tail drop.

/// One caption cue: where it starts and what was said.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start_s: f64,
    pub text: String,
}

/// One unit of summarisation: contiguous segments up to the chunk budget.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub start_s: f64,
    pub end_s: f64,
    /// The cues joined with spaces — for budgets and tests.
    pub text: String,
    /// The cues themselves, so the prompt can render every line with its own timestamp
    /// (FR-WV-05: the model copies times, it never invents them).
    pub segments: Vec<Segment>,
}

/// The transcript after the cap. `truncated` is what the answer has to disclose.
#[derive(Debug, Clone, PartialEq)]
pub struct Capped {
    pub segments: Vec<Segment>,
    pub truncated: bool,
    /// Where the kept part ends, so the disclosure can say "the first 1:42:10".
    pub kept_until_s: f64,
}

/// Keep the leading segments whose text fits in `max_chars`.
pub fn cap_transcript(segments: &[Segment], max_chars: usize) -> Capped {
    let mut kept = Vec::new();
    let mut used = 0usize;
    for seg in segments {
        let n = seg.text.chars().count();
        if used + n > max_chars && !kept.is_empty() {
            break;
        }
        used += n;
        kept.push(seg.clone());
    }
    let truncated = kept.len() < segments.len();
    let kept_until_s = kept.last().map(|s| s.start_s).unwrap_or(0.0);
    Capped {
        segments: kept,
        truncated,
        kept_until_s,
    }
}

/// Split segments into chunks of at most `max_chars`, never inside a segment. A single segment
/// longer than the budget becomes its own chunk rather than being cut.
pub fn chunk_segments(segments: &[Segment], max_chars: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<&Segment> = Vec::new();
    let mut used = 0usize;
    let flush = |current: &mut Vec<&Segment>, chunks: &mut Vec<Chunk>| {
        if current.is_empty() {
            return;
        }
        let text = current
            .iter()
            .map(|s| s.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        chunks.push(Chunk {
            start_s: current[0].start_s,
            end_s: current[current.len() - 1].start_s,
            text,
            segments: current.iter().map(|s| (*s).clone()).collect(),
        });
        current.clear();
    };
    for seg in segments {
        let n = seg.text.chars().count();
        if used + n > max_chars && !current.is_empty() {
            flush(&mut current, &mut chunks);
            used = 0;
        }
        current.push(seg);
        used += n;
    }
    flush(&mut current, &mut chunks);
    chunks
}

/// Plain text (a page or a post) split on line boundaries — one line per block, as
/// [`super::html_text`] produces — into chunks of at most `max_chars`; a line longer than the
/// budget is split on sentence ends, and only as a last resort on a character boundary.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for para in text.lines() {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if para.chars().count() > max_chars {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.extend(split_long(para, max_chars));
            continue;
        }
        if !current.is_empty() && current.chars().count() + para.chars().count() + 1 > max_chars {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(para);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_long(para: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(para) {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        if sentence.chars().count() > max_chars {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let chars: Vec<char> = sentence.chars().collect();
            for piece in chars.chunks(max_chars) {
                out.push(piece.iter().collect());
            }
            continue;
        }
        if !current.is_empty() && current.chars().count() + 1 + sentence.chars().count() > max_chars {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(sentence);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Sentence pieces, each keeping its terminator. Handles `.`/`!`/`?` and the Japanese `。`/`！`/`？`.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?' | '。' | '！' | '？') {
            let end = i + c.len_utf8();
            out.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `754` → `12:34`; `3723` → `1:02:03`. What the answer's headings carry (FR-WV-05).
pub fn format_timestamp(secs: f64) -> String {
    let total = secs.max(0.0).floor() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, text: &str) -> Segment {
        Segment {
            start_s: start,
            text: text.into(),
        }
    }

    #[test]
    fn chunks_never_cut_a_segment_and_carry_their_time_range() {
        let segs = vec![seg(0.0, "aaaa"), seg(5.0, "bbbb"), seg(10.0, "cccc"), seg(15.0, "dd")];
        let chunks = chunk_segments(&segs, 8);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "aaaa bbbb");
        assert_eq!((chunks[0].start_s, chunks[0].end_s), (0.0, 5.0));
        assert_eq!(chunks[1].text, "cccc dd");
        assert_eq!(chunks[1].start_s, 10.0);
    }

    #[test]
    fn an_oversized_segment_is_its_own_chunk_not_a_cut() {
        let segs = vec![seg(0.0, "short"), seg(1.0, "this one is far too long"), seg(2.0, "x")];
        let chunks = chunk_segments(&segs, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].text, "this one is far too long");
    }

    #[test]
    fn the_cap_keeps_the_start_and_says_so() {
        let segs: Vec<Segment> = (0..10).map(|i| seg(i as f64 * 60.0, "0123456789")).collect();
        let capped = cap_transcript(&segs, 35);
        assert_eq!(capped.segments.len(), 3);
        assert!(capped.truncated);
        assert_eq!(capped.kept_until_s, 120.0);
        let whole = cap_transcript(&segs, 1_000);
        assert!(!whole.truncated);
        assert_eq!(whole.segments.len(), 10);
    }

    #[test]
    fn text_chunks_split_on_paragraphs_then_sentences() {
        let text = "One. Two.\nThree three three. Four.\nFive.";
        let chunks = chunk_text(text, 20);
        // The long line is split at a sentence end; the short ones stay whole.
        assert_eq!(chunks, vec!["One. Two.", "Three three three.", "Four.", "Five."]);
        // Lines that fit share a chunk.
        assert_eq!(chunk_text("A.\nB.", 20), vec!["A.\nB."]);
    }

    #[test]
    fn japanese_sentences_split_on_their_own_terminators() {
        let pieces = split_sentences("今日は晴れ。明日は雨？そうです！");
        assert_eq!(pieces, vec!["今日は晴れ。", "明日は雨？", "そうです！"]);
    }

    #[test]
    fn timestamps_read_like_a_video_player() {
        assert_eq!(format_timestamp(754.0), "12:34");
        assert_eq!(format_timestamp(3723.4), "1:02:03");
        assert_eq!(format_timestamp(5.0), "0:05");
        assert_eq!(format_timestamp(-3.0), "0:00");
    }
}
