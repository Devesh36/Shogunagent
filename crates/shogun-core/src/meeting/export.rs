//! Share-ready Markdown for a meeting's Recap.
//!
//! Pure formatting: the caller supplies what the DB said, this renders one Markdown document —
//! title, duration, named speakers, the minutes (summary / template sections / decisions / next
//! actions), the user's notes, and optionally the transcript. Nothing here sends anything:
//! **producing the document is L1; every path that would move it off the device stays L3**
//! (invariant 4) — the desktop hands it to the clipboard, the API hands it to the caller.

use super::minutes::MeetingMinutes;

/// One transcript line for export: `(speaker key or None, text)` in time order.
pub type ExportLine = (Option<String>, String);

/// Everything the document is built from. All optional pieces render as absent, never as empty
/// scaffolding — a notes-only meeting exports as a notes-only document.
pub struct RecapExport<'a> {
    pub title: &'a str,
    pub duration_minutes: Option<i64>,
    /// `(speaker_key, display_name)` — confirmed names only.
    pub speakers: &'a [(String, String)],
    pub minutes: Option<&'a MeetingMinutes>,
    pub notes: Option<&'a str>,
    pub transcript: &'a [ExportLine],
    pub include_transcript: bool,
    /// Who spoke how much (`talk_share::talk_share`); rendered only with two or more speakers.
    pub talk_share: &'a [super::talk_share::TalkShare],
}

/// The display label for a stored speaker key, mirroring the UI (`transforms.ts`): a confirmed
/// name wins; `me`/`other` keep their plain labels; a diarized slot is "Speaker N" (1-based).
/// Shared with the Memory API's transcript read so the three faces label a line the same way.
pub fn speaker_label(key: Option<&str>, names: &[(String, String)]) -> String {
    let Some(key) = key else { return "Speaker".into() };
    if let Some((_, name)) = names.iter().find(|(k, _)| k == key) {
        return name.clone();
    }
    match key {
        "me" => "Me".into(),
        "other" => "Other".into(),
        _ => key
            .strip_prefix("other:")
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| format!("Speaker {}", n + 1))
            .unwrap_or_else(|| "Speaker".into()),
    }
}

/// Render the document. Deterministic; no model, no network, no clock.
pub fn recap_markdown(export: &RecapExport<'_>) -> String {
    let mut out = String::new();
    let title = export.title.trim();
    out.push_str("# ");
    out.push_str(if title.is_empty() { super::recap::UNTITLED } else { title });
    out.push('\n');

    if let Some(minutes) = export.duration_minutes {
        out.push_str(&format!("\n*{minutes} min*\n"));
    }

    if !export.speakers.is_empty() {
        out.push_str("\n**Speakers:** ");
        let names: Vec<&str> = export.speakers.iter().map(|(_, n)| n.as_str()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }

    if let Some(line) = super::talk_share::render_line(export.talk_share, export.speakers) {
        out.push_str("\n**Talk share:** ");
        out.push_str(&line);
        out.push('\n');
    }

    if let Some(m) = export.minutes {
        if !m.summary.trim().is_empty() {
            out.push_str("\n## Summary\n\n");
            out.push_str(m.summary.trim());
            out.push('\n');
        }
        for section in &m.sections {
            if section.content.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("\n## {}\n\n{}\n", section.title.trim(), section.content.trim()));
        }
        if !m.decisions.is_empty() {
            out.push_str("\n## Decisions\n\n");
            for d in &m.decisions {
                out.push_str(&format!("- {}\n", d.trim()));
            }
        }
        if !m.next_actions.is_empty() {
            out.push_str("\n## Next actions\n\n");
            for a in &m.next_actions {
                match &a.owner {
                    Some(owner) => out.push_str(&format!("- [ ] {} — {}\n", a.text.trim(), owner.trim())),
                    None => out.push_str(&format!("- [ ] {}\n", a.text.trim())),
                }
            }
        }
    }

    if let Some(notes) = export.notes.map(str::trim).filter(|n| !n.is_empty()) {
        out.push_str("\n## Notes\n\n");
        out.push_str(notes);
        out.push('\n');
    }

    if export.include_transcript && !export.transcript.is_empty() {
        out.push_str("\n## Transcript\n\n");
        for (speaker, text) in export.transcript {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "**{}:** {}\n\n",
                speaker_label(speaker.as_deref(), export.speakers),
                text
            ));
        }
    }

    out
}

/// The provenance footer a saved Recap file ends with (issue #397): which meeting session it came
/// from, so the file can be traced back to its source in memory. Appended after the export text,
/// which stays byte-for-byte what `meeting.recap.export` returns.
pub fn recap_provenance_footer(session_id: i64) -> String {
    format!("\n---\n\n*Saved from ShogunAI · meeting session {session_id}*\n")
}

/// A spreadsheet cell: quoted when it holds a comma, quote or line break, and defused when it
/// would start a formula in a spreadsheet app (`=`, `+`, `-`, `@`).
fn csv_cell(value: &str) -> String {
    let value = value.trim();
    let value = if value.starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

/// The Recap's decisions and next actions as CSV (issue #397): one row each, with the meeting
/// title and the session id as provenance columns. Summary / notes / transcript are prose and
/// stay in the Markdown export.
pub fn recap_csv(title: &str, session_id: i64, minutes: Option<&MeetingMinutes>) -> String {
    let title = title.trim();
    let title = if title.is_empty() { super::recap::UNTITLED } else { title };
    let mut out = String::from("Meeting,Kind,Text,Owner,Session\r\n");
    let mut row = |kind: &str, text: &str, owner: &str| {
        out.push_str(&format!(
            "{},{},{},{},{session_id}\r\n",
            csv_cell(title),
            kind,
            csv_cell(text),
            csv_cell(owner)
        ));
    };
    if let Some(m) = minutes {
        for d in &m.decisions {
            row("Decision", d, "");
        }
        for a in &m.next_actions {
            row("Next action", &a.text, a.owner.as_deref().unwrap_or(""));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::minutes::{MinutesSection, NextAction};

    fn minutes() -> MeetingMinutes {
        MeetingMinutes {
            summary: "Agreed to renew.".into(),
            decisions: vec!["Renew for 12 months".into()],
            next_actions: vec![
                NextAction { text: "Send the contract".into(), owner: Some("Tanaka".into()) },
                NextAction { text: "Book the kickoff".into(), owner: None },
            ],
            evidence: Vec::new(),
            sections: vec![
                MinutesSection { title: "Objections & risks".into(), content: "Price pushback.".into() },
                MinutesSection { title: "Pricing & terms".into(), content: "  ".into() },
            ],
        }
    }

    #[test]
    fn the_document_carries_title_minutes_sections_and_owned_actions() {
        let speakers = vec![("other:0".to_string(), "Tanaka".to_string())];
        let m = minutes();
        let md = recap_markdown(&RecapExport {
            title: "Renewal call",
            duration_minutes: Some(32),
            speakers: &speakers,
            minutes: Some(&m),
            notes: Some("watch the discount"),
            transcript: &[],
            include_transcript: false,
            talk_share: &[],
        });
        assert!(md.starts_with("# Renewal call\n"));
        assert!(md.contains("*32 min*"));
        assert!(md.contains("**Speakers:** Tanaka"));
        assert!(md.contains("## Summary\n\nAgreed to renew."));
        assert!(md.contains("## Objections & risks\n\nPrice pushback."));
        assert!(!md.contains("Pricing & terms"), "blank sections are dropped");
        assert!(md.contains("- [ ] Send the contract — Tanaka"));
        assert!(md.contains("- [ ] Book the kickoff\n"));
        assert!(md.contains("## Notes\n\nwatch the discount"));
        assert!(!md.contains("## Transcript"));
    }

    #[test]
    fn the_transcript_renders_confirmed_names_and_neutral_slots() {
        let speakers = vec![("other:0".to_string(), "Tanaka".to_string())];
        let transcript = vec![
            (Some("me".to_string()), "shall we renew?".to_string()),
            (Some("other:0".to_string()), "yes".to_string()),
            (Some("other:1".to_string()), "agreed".to_string()),
            (None, "…".to_string()),
        ];
        let md = recap_markdown(&RecapExport {
            title: "Renewal call",
            duration_minutes: None,
            speakers: &speakers,
            minutes: None,
            notes: None,
            transcript: &transcript,
            include_transcript: true,
            talk_share: &super::super::talk_share::talk_share(&transcript),
        });
        assert!(md.contains("**Talk share:** "), "two speakers spoke, so the share line renders");
        assert!(md.contains("**Me:** shall we renew?"));
        assert!(md.contains("**Tanaka:** yes"));
        assert!(md.contains("**Speaker 2:** agreed"));
        assert!(md.contains("**Speaker:** …"));
    }

    #[test]
    fn the_csv_has_one_row_per_decision_and_action_with_provenance() {
        let m = minutes();
        let csv = recap_csv("Renewal, Q3", 42, Some(&m));
        let lines: Vec<&str> = csv.split("\r\n").collect();
        assert_eq!(lines[0], "Meeting,Kind,Text,Owner,Session");
        assert_eq!(lines[1], "\"Renewal, Q3\",Decision,Renew for 12 months,,42");
        assert_eq!(lines[2], "\"Renewal, Q3\",Next action,Send the contract,Tanaka,42");
        assert_eq!(lines[3], "\"Renewal, Q3\",Next action,Book the kickoff,,42");
        assert_eq!(lines.len(), 5, "rows end with CRLF");
        // A formula-looking cell is defused, a quote is doubled.
        assert_eq!(csv_cell("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        // No minutes → header only.
        assert_eq!(recap_csv("", 1, None), "Meeting,Kind,Text,Owner,Session\r\n");
    }

    #[test]
    fn the_footer_names_the_session() {
        assert_eq!(recap_provenance_footer(7), "\n---\n\n*Saved from ShogunAI · meeting session 7*\n");
    }

    #[test]
    fn an_empty_meeting_still_exports_a_titled_document() {
        let md = recap_markdown(&RecapExport {
            title: "   ",
            duration_minutes: None,
            speakers: &[],
            minutes: None,
            notes: None,
            transcript: &[],
            include_transcript: true,
            talk_share: &[],
        });
        assert!(md.starts_with(&format!("# {}", crate::meeting::recap::UNTITLED)));
        assert!(!md.contains("## "));
    }
}
