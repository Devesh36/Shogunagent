//! Who spoke how much (docs/meeting-gap-analysis-2026-09.md B2).
//!
//! Measured on the transcript **text**, not on audio time: the audio never outlives the 30 s ring
//! buffer (invariant 2), so a per-speaker duration is not something SHOGUN can know. Characters
//! are the honest proxy that works for Japanese (no word boundaries) and English alike, and the
//! result is called a *share*, never "talk time".
//!
//! Pure: no clock, no DB. The caller hands in the same `(speaker key, text)` lines the Recap
//! builder reads.

/// One speaker's share of the transcript.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TalkShare {
    /// The stored speaker key (`me`, `other`, `other:N`), or `None` for lines with no speaker.
    pub speaker_key: Option<String>,
    /// Characters spoken (whitespace excluded).
    pub chars: usize,
    /// This speaker's fraction of all characters, `0.0..=1.0`.
    pub share: f64,
}

/// Per-speaker shares, largest first (ties keep first-appearance order). Empty when nothing was
/// said. Whitespace-only lines count for nothing, so a speaker that only produced silence markers
/// does not appear.
pub fn talk_share(lines: &[(Option<String>, String)]) -> Vec<TalkShare> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for (speaker, text) in lines {
        let chars = text.chars().filter(|c| !c.is_whitespace()).count();
        if chars == 0 {
            continue;
        }
        match order.iter().position(|k| k == speaker) {
            Some(i) => counts[i] += chars,
            None => {
                order.push(speaker.clone());
                counts.push(chars);
            }
        }
    }
    let total: usize = counts.iter().sum();
    if total == 0 {
        return Vec::new();
    }
    let mut out: Vec<TalkShare> = order
        .into_iter()
        .zip(counts)
        .map(|(speaker_key, chars)| TalkShare {
            speaker_key,
            chars,
            share: chars as f64 / total as f64,
        })
        .collect();
    // Stable sort, most characters first: equal counts keep first-appearance order.
    out.sort_by_key(|share| std::cmp::Reverse(share.chars));
    out
}

/// The one-line rendering for the Recap and its export — `Tanaka 62%, Me 38%` — or `None` when
/// fewer than two speakers said anything (a share of 100% tells the reader nothing).
pub fn render_line(shares: &[TalkShare], names: &[(String, String)]) -> Option<String> {
    if shares.len() < 2 {
        return None;
    }
    let parts: Vec<String> = shares
        .iter()
        .map(|s| {
            format!(
                "{} {}%",
                super::export::speaker_label(s.speaker_key.as_deref(), names),
                (s.share * 100.0).round() as u32
            )
        })
        .collect();
    Some(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(speaker: Option<&str>, text: &str) -> (Option<String>, String) {
        (speaker.map(str::to_string), text.to_string())
    }

    #[test]
    fn shares_are_by_characters_largest_first() {
        let lines = vec![
            line(Some("me"), "shall we renew the contract"),
            line(Some("other:0"), "yes"),
            line(Some("other:0"), "let us do twelve months and revisit pricing"),
            line(None, "…"),
        ];
        let shares = talk_share(&lines);
        assert_eq!(shares.len(), 3);
        assert_eq!(shares[0].speaker_key.as_deref(), Some("other:0"));
        assert_eq!(shares[1].speaker_key.as_deref(), Some("me"));
        assert_eq!(shares[2].speaker_key, None);
        let total: f64 = shares.iter().map(|s| s.share).sum();
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn japanese_text_counts_without_word_boundaries() {
        // No spaces to split on; characters still measure who spoke more.
        let lines = vec![
            line(Some("me"), "更新しますか"),
            line(Some("other:0"), "はい、十二か月で更新して価格は見直しましょう"),
        ];
        let shares = talk_share(&lines);
        assert_eq!(shares[0].speaker_key.as_deref(), Some("other:0"));
        assert!(shares[0].share > shares[1].share);
    }

    #[test]
    fn silence_markers_and_empty_transcripts_produce_no_share() {
        assert!(talk_share(&[]).is_empty());
        assert!(talk_share(&[line(Some("me"), "   ")]).is_empty());
    }

    #[test]
    fn the_line_names_confirmed_speakers_and_skips_a_lone_speaker() {
        let names = vec![("other:0".to_string(), "Tanaka".to_string())];
        let two = talk_share(&[line(Some("other:0"), "aaaaaa"), line(Some("me"), "bbbb")]);
        assert_eq!(render_line(&two, &names).as_deref(), Some("Tanaka 60%, Me 40%"));
        let one = talk_share(&[line(Some("me"), "only me")]);
        assert_eq!(render_line(&one, &names), None);
    }
}
