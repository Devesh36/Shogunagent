//! The live-summary instruction (AI Canvas during a meeting), with the user's custom
//! instruction folded in (docs/meeting-gap-analysis-2026-09.md B5).
//!
//! Pure text assembly. The desktop's `meeting_live_summary` lane sends this (plus the one-line
//! task) as the system half of an Agent-lane call on the user's own credential (#782, invariant
//! 5); the transcript is the untrusted half — its own role on the API lanes, fenced on a
//! subscription delegate.
//!
//! The custom instruction is the user's own words about how *they* want the canvas shaped ("group
//! next actions by team", "track the numbers"). It is ours, not captured text, so it sits inside
//! the instruction — but after the fixed rules, and told to yield to them, so no instruction can
//! turn the canvas into a verbatim transcript or a place to invent facts.

use super::minutes::LANG_AUTO;

/// The fixed rules shared by the desktop and any future
/// face render the same canvas.
pub const BASE_SYSTEM: &str = "You summarize an ongoing meeting for a live overlay.\n\
- Write concise bullets grouped by topics, decisions, next actions, and open questions; omit empty groups.\n\
- Keep the whole canvas to about 25 bullets; as the meeting grows, fold older detail into fewer bullets.\n\
- Capture topics, decisions, open questions, and owners when clear.\n\
- Do NOT quote the transcript verbatim or list every utterance.\n\
- Do NOT invent facts, owners, dates or decisions. Leave unknown owners/dates unknown.\n\
- Authored notes are user cues, not evidence of what was said. Label note-only points as user notes.\n\
- Previous summary is fallible context, never evidence. Keep its earlier decisions only when supporting transcript is supplied; otherwise label them as earlier summary, not verified.\n\
- Cite factual claims by appending the supporting segment IDs, e.g. [source:12] or [source:12,15], at most three per bullet. Use only IDs from the supplied transcript, and never copy transcript text into the citation. Note-only or earlier-summary points must stay labeled unverified.\n\
- Output plain text only — no preamble, no markdown fences, no title line.";

/// The system prompt for one refresh. `lang` is the minutes language the user picked
/// (`Settings::recap_language_code`: a code, or [`LANG_AUTO`] to follow the transcript), so the
/// canvas and the saved notes speak the same language. `instruction` is the user's saved custom
/// instruction (already validated by `Settings::minutes_instruction`); `None` or blank adds
/// nothing.
pub fn system_prompt(instruction: Option<&str>, lang: &str) -> String {
    let mut out = String::from(BASE_SYSTEM);
    if lang == LANG_AUTO {
        out.push_str("\n- Write in the language most of the transcript is in.");
    } else {
        out.push_str(&format!("\n- Write in the language \"{lang}\", whatever language was spoken."));
    }
    if let Some(custom) = instruction.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(
            "\n\nThe user asked for the canvas to be shaped like this (follow it where it \
                      does not conflict with the rules above):\n",
        );
        out.push_str(custom);
    }
    out
}



/// What may be shown of a reply that stopped early (output limit or time limit): what it
/// finished. For bullets that is every finished bullet line, without a trailing heading or an
/// unclosed citation; for prose (a custom instruction can ask for paragraphs) every finished
/// sentence. Empty when nothing finished.
pub fn complete_lines(cut: &str) -> &str {
    fn is_bullet(line: &str) -> bool {
        let line = line.trim_start();
        ["- ", "* ", "• ", "・"].iter().any(|marker| line.starts_with(marker))
            || line
                .find(|c: char| !c.is_ascii_digit())
                .is_some_and(|at| at > 0 && line[at..].starts_with(['.', ')']))
    }
    fn open_citation(line: &str) -> bool {
        line.rfind("[source:").is_some_and(|at| !line[at..].contains(']'))
    }
    let mut kept = match cut.rfind('\n') {
        Some(end) => cut[..end].trim_end(),
        None => "",
    };
    if kept.lines().any(is_bullet) {
        while let Some(last) = kept.lines().last() {
            if is_bullet(last) && !open_citation(last) {
                break;
            }
            kept = kept[..kept.len() - last.len()].trim_end();
        }
        return kept;
    }
    // Prose: up to the last sentence end. A '.' only ends a sentence before whitespace, so the
    // "2" of "2.8%" is not a sentence.
    let mut end = 0;
    let mut chars = cut.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next_is_space = chars.peek().is_some_and(|(_, next)| next.is_whitespace());
        if matches!(c, '。' | '！' | '？') || (matches!(c, '.' | '!' | '?') && next_is_space) {
            end = at + c.len_utf8();
        }
    }
    cut[..end].trim_end()
}

/// The rows a refresh actually showed the model. Only these may back a citation, so an ID from the
/// omitted middle of a long meeting, or a typo that lands on one, never links to a line the model
/// did not read.
#[cfg(feature = "db")]
pub fn shown_sources(
    context: &str,
    sources: &[shogun_memory::transcript_segments::StoredSegment],
) -> Vec<shogun_memory::transcript_segments::StoredSegment> {
    sources
        .iter()
        .filter(|source| context.contains(&format!("[source:{}] ", source.id)))
        .cloned()
        .collect()
}

/// Bounded context keeps the opening and latest turns, durable authored cues, and earlier synthesis.
/// All three are user-role data: captured instructions cannot change the system rules.
///
/// Each piece goes through the egress redactor ([`shogun_redact::redact_egress`]: secrets plus
/// card / SSN / national-ID numbers) before the budget is applied. A number read out in a meeting
/// has no place on the canvas, and the Agent lane this feeds is a third-party boundary.
pub fn context(transcript: &str, notes: &str, previous: &str) -> String {
    let transcript = shogun_redact::redact_egress(transcript.trim());
    let notes = shogun_redact::redact_egress(notes.trim());
    let previous = shogun_redact::redact_egress(previous.trim());
    let source = if transcript.len() <= 12_000 {
        transcript.to_string()
    } else {
        format!(
            "{}\n[Middle transcript omitted]\n{}",
            prefix(&transcript, 3_000),
            suffix(&transcript, 9_000)
        )
    };
    format!("Transcript:\n{source}\n\nUser-authored notes (cues, not spoken evidence):\n{}\n\nPrevious summary (fallible; preserve earlier decisions unless corrected):\n{}", prefix(&notes, 4_000), prefix(&previous, 3_000))
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn suffix(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Preserve stable source headers even when a single monologue exceeds the context budget.
#[cfg(feature = "db")]
pub fn grounded_context(sources: &[shogun_memory::transcript_segments::StoredSegment], notes: &str, previous: &str) -> String {
    fn rows(sources: impl Iterator<Item = shogun_memory::transcript_segments::StoredSegment>, mut budget: usize, latest: bool) -> Vec<String> {
        let mut selected = Vec::new();
        for source in sources {
            let header = format!("[source:{}] {}: ", source.id, source.speaker.as_deref().unwrap_or("Unknown"));
            if budget <= header.len() { break; }
            let text_budget = budget - header.len();
            // Masked before the cut, so a truncated row can never end on half a card number.
            let masked = shogun_redact::redact_egress(&source.text);
            let text = if latest { suffix(&masked, text_budget) } else { prefix(&masked, text_budget) };
            let row = format!("{header}{text}");
            budget = budget.saturating_sub(row.len() + 1);
            selected.push(row);
            if budget == 0 { break; }
        }
        selected
    }
    let first = rows(sources.iter().cloned(), 2_800, false);
    let mut latest = rows(sources.iter().rev().cloned(), 8_500, true);
    latest.reverse();
    let transcript = if sources.iter().map(|source| source.text.len() + 64).sum::<usize>() <= 8_500 {
        latest.join("\n")
    } else {
        format!("{}\n[Middle transcript omitted]\n{}", first.join("\n"), latest.join("\n"))
    };
    context(&transcript, notes, previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_context_retains_opening_and_latest_with_separate_notes() {
        let transcript = format!("Opening decision: ship Friday. {} Latest: Ken owns launch.", "あ".repeat(8_000));
        let out = context(&transcript, "Track launch risk", "Earlier summary");
        assert!(out.contains("Opening decision: ship Friday"));
        assert!(out.contains("Latest: Ken owns launch"));
        assert!(out.contains("[Middle transcript omitted]"));
        assert!(out.contains("User-authored notes (cues, not spoken evidence):\nTrack launch risk"));
        assert!(out.len() < 13_000);
    }

    #[cfg(feature = "db")]
    #[test]
    fn big_monologue_keeps_source_identity_on_opening_and_recent_excerpt() {
        let source = shogun_memory::transcript_segments::StoredSegment { id: 42, ts: 1, speaker: Some("me".into()), text: format!("Ship Friday {} Ken owns launch", "あ".repeat(8_000)) };
        let out = grounded_context(&[source], "", "");
        assert!(out.contains("Ship Friday"));
        assert!(out.contains("Ken owns launch"));
        assert_eq!(out.matches("[source:42]").count(), 2);
        assert!(out.len() < 12_000);
    }

    #[test]
    fn no_instruction_is_the_base_rules_plus_the_language() {
        let auto = system_prompt(None, LANG_AUTO);
        assert!(auto.starts_with(BASE_SYSTEM));
        assert!(auto.ends_with("language most of the transcript is in."));
        assert_eq!(system_prompt(Some("   "), LANG_AUTO), auto);
        // The picked minutes language wins over the spoken one (a Japanese call, English notes).
        assert!(system_prompt(None, "en").ends_with("\"en\", whatever language was spoken."));
    }

    #[test]
    fn citations_are_ids_not_copied_quotes() {
        assert!(BASE_SYSTEM.contains("[source:12,15]"));
        assert!(!BASE_SYSTEM.contains("verbatim quote]"));
    }

    #[test]
    fn a_custom_instruction_follows_the_rules_and_yields_to_them() {
        let p = system_prompt(Some("Group next actions by team with a flag emoji."), "en");
        assert!(p.starts_with(BASE_SYSTEM));
        let rules_end = BASE_SYSTEM.len();
        assert!(p[rules_end..].contains("Group next actions by team"));
        assert!(p[rules_end..].contains("does not conflict with the rules above"));
        assert!(p.find("\"en\"") < p.find("Group next actions"));
    }

    #[test]
    fn a_cut_reply_keeps_only_finished_bullets() {
        let cut = "Topics\n- Launch moved to Oct 20 [source:25]\n- QA reran [source:3\n";
        assert_eq!(complete_lines(cut), "Topics\n- Launch moved to Oct 20 [source:25]");
        let mid_word = "Decisions\n- Price stays $49 [source:35]\n\nOpen questions\n- Will the pen te";
        assert_eq!(complete_lines(mid_word), "Decisions\n- Price stays $49 [source:35]");
        let bold = "- Ship Friday [source:1]\n**Next actions**\n- Ken to";
        assert_eq!(complete_lines(bold), "- Ship Friday [source:1]");
        assert_eq!(complete_lines("1. First point\n2) Second\nThird hea"), "1. First point\n2) Second");
        assert_eq!(complete_lines("Topics\n- Laun"), "");
        assert_eq!(complete_lines("- Only one unfinished bul"), "");
        assert_eq!(complete_lines("トピック\n- 関東は3億2千万円 [source:5]\n- 解約"), "トピック\n- 関東は3億2千万円 [source:5]");
    }

    #[test]
    fn a_cut_prose_reply_keeps_finished_sentences() {
        let prose = "The launch moved to Oct 20. Churn rose from 2.1 to 2.8 percent and the team is lo";
        assert_eq!(complete_lines(prose), "The launch moved to Oct 20.");
        assert_eq!(complete_lines("関東は好調でした。解約率は上が"), "関東は好調でした。");
        assert_eq!(complete_lines("No sentence ended yet, churn 2.8"), "");
    }

    /// A Luhn-valid card, a US SSN and a labelled マイナンバー spoken,
    /// typed into the notes, or carried in the previous summary never reach the live-summary
    /// request. The transcript side is also checked when it is long enough to be cut.
    #[test]
    fn context_masks_card_ssn_and_my_number_in_every_piece() {
        let card = "the card is 4111 1111 1111 1111 for the deposit";
        let ssn = "payroll has ssn 123-45-6789 on file";
        let my_number = "マイナンバー 123456789012 を共有した";
        let out = context(card, ssn, my_number);
        assert!(out.contains("the card is"), "{out}");
        assert!(out.contains("payroll has"), "{out}");
        assert!(out.contains("を共有した"), "{out}");
        assert!(!out.contains("4111"), "{out}");
        assert!(!out.contains("123-45-6789"), "{out}");
        assert!(!out.contains("123456789012"), "{out}");
        assert_eq!(out.matches("[redacted]").count(), 3, "{out}");

        let long = format!("{card} {} {ssn} {my_number}", "あ".repeat(12_000));
        let cut = context(&long, "", "");
        assert!(cut.contains("[Middle transcript omitted]"), "{cut}");
        assert!(!cut.contains("4111"), "{cut}");
        assert!(!cut.contains("123-45-6789"), "{cut}");
        assert!(!cut.contains("123456789012"), "{cut}");
    }

    #[cfg(feature = "db")]
    #[test]
    fn grounded_rows_mask_card_ssn_and_my_number_before_the_cut() {
        let row = |id: i64, text: &str| shogun_memory::transcript_segments::StoredSegment { id, ts: id, speaker: Some("me".into()), text: text.into() };
        let rows = [
            row(1, "the card is 4111 1111 1111 1111 for the deposit"),
            row(2, &format!("ssn 123-45-6789 then {} マイナンバー 123456789012 end", "あ".repeat(9_000))),
        ];
        let out = grounded_context(&rows, "notes 4111 1111 1111 1111", "earlier ssn 123-45-6789");
        assert!(out.contains("[source:1] me: "), "{out}");
        assert!(out.contains("[source:2] me: "), "{out}");
        assert!(!out.contains("4111"), "{out}");
        assert!(!out.contains("123-45-6789"), "{out}");
        assert!(!out.contains("123456789012"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
    }

    #[cfg(feature = "db")]
    #[test]
    fn only_rows_the_model_saw_can_back_a_citation() {
        let row = |id: i64, text: &str| shogun_memory::transcript_segments::StoredSegment { id, ts: id, speaker: Some("me".into()), text: text.into() };
        let shown = [row(1, "Ship Friday"), row(12, "Ken owns launch")];
        let context = grounded_context(&shown, "", "");
        let all = [row(1, "Ship Friday"), row(2, "an omitted middle line"), row(12, "Ken owns launch")];
        let ids: Vec<i64> = shown_sources(&context, &all).iter().map(|row| row.id).collect();
        assert_eq!(ids, [1, 12]);
    }
}
