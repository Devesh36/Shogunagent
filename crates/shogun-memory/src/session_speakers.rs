//! Real names for a meeting's speakers (FR-MT-15, the stage after `me`/`other`).
//!
//! `transcript_segments.speaker` stores capture-derived keys (`me`, `other`, diarized `other:N`).
//! A key becomes a person's *name* only here, and only because the user said so: assignments are
//! written on an explicit user action (typed, or a candidate the user accepted), never inferred.
//! An unnamed slot stays unnamed — the display layer shows a neutral "Speaker N" for it.

use rusqlite::{params, Connection};

/// One confirmed name for a speaker key within a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerName {
    pub speaker_key: String,
    pub display_name: String,
    /// `user` (typed) | `calendar` | `people` — where the accepted name came from.
    pub source: String,
}

/// A speaker key this module accepts: exactly what the transcript writer produces.
/// Anything else (an API caller inventing keys) is rejected at the write.
pub fn valid_speaker_key(key: &str) -> bool {
    if key == "me" || key == "other" {
        return true;
    }
    key.strip_prefix("other:")
        .is_some_and(|n| !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Assign (or replace) the name for one speaker key. An empty name clears the assignment —
/// "I was wrong about who that was" must be as reachable as naming them (never guessed, and
/// never stuck). Returns whether a row exists after the call.
pub fn assign(
    conn: &Connection,
    session_id: i64,
    speaker_key: &str,
    display_name: &str,
    source: &str,
    now: i64,
) -> Result<bool, rusqlite::Error> {
    if !valid_speaker_key(speaker_key) {
        return Err(rusqlite::Error::InvalidParameterName(
            "invalid speaker key (expected me / other / other:N)".to_string(),
        ));
    }
    let cleaned = crate::sanitize::persist_body(display_name);
    let name = cleaned.text.trim();
    if name.is_empty() {
        conn.execute(
            "DELETE FROM session_speakers WHERE session_id = ?1 AND speaker_key = ?2",
            params![session_id, speaker_key],
        )?;
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO session_speakers (session_id, speaker_key, display_name, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT (session_id, speaker_key) DO UPDATE SET
           display_name = ?3, source = ?4, updated_at = ?5",
        params![session_id, speaker_key, name, source, now],
    )?;
    Ok(true)
}

/// All confirmed names for a session, in assignment order.
pub fn for_session(
    conn: &Connection,
    session_id: i64,
) -> Result<Vec<SpeakerName>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT speaker_key, display_name, source FROM session_speakers
         WHERE session_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(SpeakerName {
            speaker_key: r.get(0)?,
            display_name: r.get(1)?,
            source: r.get(2)?,
        })
    })?;
    rows.collect()
}

/// The speaker keys that actually occur in a session's transcript, in order of first appearance —
/// what the naming UI offers chips for. NULL (unknown) is not a nameable key and is skipped.
pub fn observed_keys(conn: &Connection, session_id: i64) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT speaker FROM transcript_segments
         WHERE session_id = ?1 AND speaker IS NOT NULL AND excluded = 0
         GROUP BY speaker ORDER BY MIN(ts), MIN(id)",
    )?;
    let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Participant-name candidates read from the meeting app's **own screen**. The AX capture lane
/// already stores the meeting window's visible text as session-attached `event_log` rows — a
/// Meet participant tile or a Zoom participant list is the participants written out, no calendar
/// needed. This mines those rows for name-shaped spans that persist across captures.
///
/// Same contract as every candidate source: **offered, never applied** (FR-MT-15 — the only
/// write path is [`assign`], on an explicit user action), so a false positive costs one ignored
/// chip, never a wrong identity in the data.
pub fn participant_candidates(
    conn: &Connection,
    session_id: i64,
    limit: usize,
) -> Result<Vec<String>, rusqlite::Error> {
    // Only the meeting app's windows: focus can drift to a notes app mid-meeting, and prose is
    // where the name heuristic would misfire most. The session title is excluded below — it is
    // painted into every capture and often looks exactly like a name span ("Weekly Sync").
    let title: Option<String> = conn
        .query_row("SELECT title FROM sessions WHERE id = ?1", [session_id], |r| r.get(0))
        .unwrap_or(None);
    let mut stmt = conn.prepare(
        "SELECT e.content FROM event_log e
         JOIN sessions s ON s.id = ?1
         WHERE e.session_id = ?1
           AND (s.app_bundle_id IS NULL OR e.app_bundle_id = s.app_bundle_id)
         ORDER BY e.ts DESC LIMIT 200",
    )?;
    let contents: Vec<String> = stmt
        .query_map([session_id], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    Ok(mine_participant_names(&contents, title.as_deref(), limit))
}

/// Meeting-UI vocabulary that is capitalized on screen and would otherwise read as a name.
/// English + the product names of the meeting apps SHOGUN detects. Never exhaustive — the
/// repetition requirement and the offered-only contract carry the real safety.
const UI_WORDS: &[&str] = &[
    "mute", "unmute", "muted", "share", "sharing", "screen", "video", "audio", "chat", "record",
    "recording", "leave", "join", "joined", "meeting", "participants", "participant", "camera",
    "microphone", "settings", "view", "gallery", "speaker", "host", "cohost", "waiting", "room",
    "rooms", "everyone", "raise", "hand", "reactions", "caption", "captions", "live", "stop",
    "start", "more", "apps", "whiteboard", "notes", "breakout", "security", "invite", "admit",
    "remove", "pin", "spotlight", "volume", "present", "presenting", "google", "meet", "zoom",
    "teams", "webex", "slack", "back", "home", "menu", "close", "open", "end", "call", "cancel",
    "copy", "link", "details", "info", "people", "add", "search", "send", "message", "messages",
    "full", "keyboard", "shortcuts", "help", "report", "options", "layout", "background",
];

fn is_ui_word(word: &str) -> bool {
    let lower = word.to_lowercase();
    UI_WORDS.contains(&lower.as_str())
}

/// A Latin name word: starts uppercase, carries at least one lowercase letter (rejects REC/HD
/// style UI acronyms), letters/apostrophe/hyphen only.
fn is_name_word(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_uppercase() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    if rest.is_empty() || rest.len() > 19 {
        return false;
    }
    rest.iter().any(|c| c.is_lowercase())
        && rest
            .iter()
            .all(|c| c.is_alphabetic() || *c == '\'' || *c == '’' || *c == '-')
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF        // hiragana + katakana
        | 0x4E00..=0x9FFF      // CJK unified
        | 0x3400..=0x4DBF      // CJK ext A
        | 0xFF66..=0xFF9D      // halfwidth katakana
    ) || c == '・' || c == 'ー'
}

/// Name-shaped spans in one captured text: runs of 2–3 Latin name words, and standalone
/// CJK lines of 2–8 chars (a participant list renders one name per line).
fn name_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // CJK: the whole line is the name or it is nothing — CJK prose would need real
        // segmentation, and a participant tile is always a standalone label.
        let char_count = line.chars().count();
        if (2..=8).contains(&char_count)
            && line.chars().all(is_cjk)
            && line.chars().filter(|c| *c != '・' && *c != 'ー').count() >= 2
        {
            spans.push(line.to_string());
            continue;
        }
        // Latin: contiguous runs of name words inside the line.
        let words: Vec<&str> = line.split_whitespace().collect();
        let mut run: Vec<&str> = Vec::new();
        let flush = |run: &mut Vec<&str>, spans: &mut Vec<String>| {
            if (2..=3).contains(&run.len()) && !run.iter().any(|w| is_ui_word(w)) {
                spans.push(run.join(" "));
            }
            run.clear();
        };
        for word in words {
            if is_name_word(word) {
                run.push(word);
            } else {
                flush(&mut run, &mut spans);
            }
        }
        flush(&mut run, &mut spans);
    }
    spans
}

/// The mining rule: a span counts only when it appears in **two or more distinct captures** —
/// a persistent UI label (a participant) rather than a phrase that happened to be on screen.
fn mine_participant_names(
    contents: &[String],
    session_title: Option<&str>,
    limit: usize,
) -> Vec<String> {
    use std::collections::BTreeMap;
    let title_lower = session_title.map(str::to_lowercase).unwrap_or_default();
    // span → (distinct-capture count, first-seen order)
    let mut seen: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut order = 0usize;
    for content in contents {
        let mut in_this_capture: Vec<String> = Vec::new();
        for span in name_spans(content) {
            if !title_lower.is_empty() && title_lower.contains(&span.to_lowercase()) {
                continue;
            }
            if in_this_capture.contains(&span) {
                continue;
            }
            in_this_capture.push(span);
        }
        for span in in_this_capture {
            let entry = seen.entry(span).or_insert_with(|| {
                order += 1;
                (0, order)
            });
            entry.0 += 1;
        }
    }
    let mut names: Vec<(String, usize, usize)> = seen
        .into_iter()
        .filter(|(_, (count, _))| *count >= 2)
        .map(|(span, (count, first))| (span, count, first))
        .collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    names.into_iter().map(|(span, _, _)| span).take(limit).collect()
}

/// Names the user confirmed in **earlier sessions with the same title** — the recurring meeting
/// has the same people in it, so last week's names are this week's best candidates. Most recent
/// assignment first, distinct, never from this session itself. A session with no title matches
/// nothing (an untitled meeting is not "the same meeting" as another untitled one).
///
/// Offered, never applied: this is the substitute for remembering a voice, which SHOGUN does not
/// do (a voice signature is derived from audio — invariant 2).
pub fn previous_names_for_title(
    conn: &Connection,
    session_id: i64,
    limit: usize,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT sp.display_name FROM session_speakers sp
         JOIN sessions s ON s.id = sp.session_id
         JOIN sessions me ON me.id = ?1
         WHERE sp.session_id != ?1
           AND me.title IS NOT NULL AND TRIM(me.title) != ''
           AND s.title IS NOT NULL AND LOWER(TRIM(s.title)) = LOWER(TRIM(me.title))
         ORDER BY sp.updated_at DESC, sp.id DESC",
    )?;
    let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
    let mut out: Vec<String> = Vec::new();
    for name in rows {
        let name = name?;
        if !out.iter().any(|have| have.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Name candidates for the naming UI: the people the memory already knows, most recently
/// interacted-with first. These are *candidates offered*, never written without the user
/// accepting one (the write path is [`assign`], and it only runs on a user action).
pub fn name_candidates(conn: &Connection, limit: usize) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT display_name FROM people
         WHERE TRIM(display_name) != ''
         ORDER BY COALESCE(last_interaction_at, 0) DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open, NewSession};
    use crate::transcript_segments::{append, NewSegment, Speaker};

    fn session(conn: &Connection) -> i64 {
        open(
            conn,
            &NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some("Weekly sync"),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.65,
                provenance: "{}",
            },
        )
        .unwrap()
    }

    #[test]
    fn a_name_is_assigned_and_read_back() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        assert!(assign(&conn, sid, "other:0", "Tanaka", "user", 9).unwrap());
        let got = for_session(&conn, sid).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].speaker_key, "other:0");
        assert_eq!(got[0].display_name, "Tanaka");
        assert_eq!(got[0].source, "user");
    }

    #[test]
    fn reassigning_replaces_rather_than_appending() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        assign(&conn, sid, "other:0", "Tanaka", "user", 9).unwrap();
        assign(&conn, sid, "other:0", "Suzuki", "user", 10).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name, "Suzuki");
    }

    #[test]
    fn an_empty_name_clears_the_assignment() {
        // "I was wrong about who that was" must be as reachable as naming them.
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        assign(&conn, sid, "me", "Watanabe", "user", 9).unwrap();
        assert!(!assign(&conn, sid, "me", "   ", "user", 10).unwrap());
        assert!(for_session(&conn, sid).unwrap().is_empty());
    }

    #[test]
    fn invented_speaker_keys_are_rejected_at_the_write() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        for bad in ["boss", "other:", "other:x", "other:12345678", "ME"] {
            assert!(assign(&conn, sid, bad, "Tanaka", "user", 9).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn observed_keys_follow_first_appearance_and_skip_unknown() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        for (ts, speaker) in [
            (1_000, Speaker::OtherSlot(1)),
            (2_000, Speaker::Me),
            (3_000, Speaker::Unknown),
            (4_000, Speaker::OtherSlot(0)),
            (5_000, Speaker::OtherSlot(1)),
        ] {
            append(&conn, &NewSegment { session_id: sid, ts, speaker, text: "hi", confidence: 0.9 }, 9).unwrap();
        }
        assert_eq!(observed_keys(&conn, sid).unwrap(), vec!["other:1", "me", "other:0"]);
    }

    #[test]
    fn names_belong_to_their_own_session() {
        let conn = crate::open_in_memory().unwrap();
        let a = session(&conn);
        let b = session(&conn);
        assign(&conn, a, "other:0", "Tanaka", "user", 9).unwrap();
        assert!(for_session(&conn, b).unwrap().is_empty());
    }

    #[test]
    fn hidden_format_characters_never_reach_a_stored_name() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        assign(&conn, sid, "other:0", "Tana\u{200B}ka\u{202E}", "user", 9).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert!(!got[0].display_name.contains('\u{200B}'));
        assert!(!got[0].display_name.contains('\u{202E}'));
    }

    fn meeting_event(
        conn: &Connection,
        session_id: i64,
        ts: i64,
        app: &str,
        content: &str,
    ) {
        let hash = crate::event_log::content_hash(content);
        let id = crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "capture",
                kind: "ax_text",
                app_bundle_id: Some(app),
                window_title: Some("Weekly sync"),
                content,
                content_hash: &hash,
                dwell_ms: 1000,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        crate::session::attach_event(conn, session_id, id).unwrap();
    }

    #[test]
    fn participants_are_mined_from_the_meeting_apps_own_screen() {
        // The AX lane already sees the participant tiles — the calendar is not required to know
        // who is in the room.
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        let tiles = "Weekly sync\nTaro Yamada\nHanako Sato\nMute\nShare screen\nLeave call";
        meeting_event(&conn, sid, 1_000, "us.zoom.xos", tiles);
        meeting_event(&conn, sid, 2_000, "us.zoom.xos", &format!("{tiles}\nRecording"));
        let got = participant_candidates(&conn, sid, 8).unwrap();
        assert!(got.contains(&"Taro Yamada".to_string()), "{got:?}");
        assert!(got.contains(&"Hanako Sato".to_string()), "{got:?}");
        assert!(!got.iter().any(|n| n.contains("Mute") || n.contains("Share")), "{got:?}");
    }

    #[test]
    fn a_phrase_seen_once_is_not_a_participant() {
        // One capture is prose; two captures is a persistent label. "Alice Johnson" typed into a
        // chat message that scrolled away must not become a chip.
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        meeting_event(&conn, sid, 1_000, "us.zoom.xos", "Taro Yamada\nask Alice Johnson later");
        meeting_event(&conn, sid, 2_000, "us.zoom.xos", "Taro Yamada\nnext topic");
        let got = participant_candidates(&conn, sid, 8).unwrap();
        assert_eq!(got, vec!["Taro Yamada"]);
    }

    #[test]
    fn cjk_participant_lines_are_candidates() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        for ts in [1_000, 2_000] {
            meeting_event(&conn, sid, ts, "us.zoom.xos", "山田太郎\n佐藤花子\nミーティングの議論はここに続きます。");
        }
        let got = participant_candidates(&conn, sid, 8).unwrap();
        assert!(got.contains(&"山田太郎".to_string()), "{got:?}");
        assert!(got.contains(&"佐藤花子".to_string()), "{got:?}");
        assert!(!got.iter().any(|n| n.contains("ミーティング")), "prose line leaked: {got:?}");
    }

    #[test]
    fn other_apps_and_the_session_title_never_contribute() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn); // title: "Weekly sync"
        for ts in [1_000, 2_000] {
            // Focus drifted to the notes app mid-meeting: its prose is out of bounds.
            meeting_event(&conn, sid, ts, "com.apple.Notes", "Kenji Tanaka\nKenji Tanaka");
            // The meeting window paints its own title into every capture.
            meeting_event(&conn, sid, ts + 10, "us.zoom.xos", "Weekly Sync\nTaro Yamada");
        }
        let got = participant_candidates(&conn, sid, 8).unwrap();
        assert_eq!(got, vec!["Taro Yamada"], "{got:?}");
    }

    fn titled_session(conn: &Connection, title: Option<&str>, started_at: i64) -> i64 {
        open(
            conn,
            &NewSession {
                kind: "meeting",
                started_at,
                title,
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.65,
                provenance: "{}",
            },
        )
        .unwrap()
    }

    #[test]
    fn names_from_earlier_meetings_with_the_same_title_come_back_most_recent_first() {
        let conn = crate::open_in_memory().unwrap();
        let last_week = titled_session(&conn, Some("Weekly sync"), 1_000);
        let two_weeks_ago = titled_session(&conn, Some("weekly SYNC "), 500);
        let unrelated = titled_session(&conn, Some("Design review"), 800);
        assign(&conn, two_weeks_ago, "other:0", "Suzuki", "user", 5).unwrap();
        assign(&conn, last_week, "other:0", "Tanaka", "user", 10).unwrap();
        assign(&conn, last_week, "other:1", "Suzuki", "user", 11).unwrap();
        assign(&conn, unrelated, "other:0", "Watanabe", "user", 12).unwrap();

        let today = titled_session(&conn, Some("Weekly sync"), 2_000);
        assert_eq!(
            previous_names_for_title(&conn, today, 8).unwrap(),
            vec!["Suzuki", "Tanaka"],
            "same title (case/space-insensitive), newest assignment first, distinct, not Watanabe"
        );
        // The session's own names are not candidates for itself, and a limit is honoured.
        assign(&conn, today, "me", "Me Myself", "user", 20).unwrap();
        assert_eq!(previous_names_for_title(&conn, today, 1).unwrap(), vec!["Suzuki"]);
    }

    #[test]
    fn an_untitled_meeting_borrows_no_names() {
        let conn = crate::open_in_memory().unwrap();
        let earlier = titled_session(&conn, None, 1_000);
        assign(&conn, earlier, "other:0", "Tanaka", "user", 5).unwrap();
        let today = titled_session(&conn, None, 2_000);
        assert!(previous_names_for_title(&conn, today, 8).unwrap().is_empty());
        let blank = titled_session(&conn, Some("   "), 3_000);
        assert!(previous_names_for_title(&conn, blank, 8).unwrap().is_empty());
    }

    #[test]
    fn candidates_come_from_people_most_recent_first() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO people (display_name, last_interaction_at, confidence, created_at, updated_at, last_evidence_at)
             VALUES ('Old Contact', 1000, 0.9, 1, 1, 1), ('Fresh Contact', 2000, 0.9, 1, 1, 1)",
            [],
        )
        .unwrap();
        assert_eq!(
            name_candidates(&conn, 10).unwrap(),
            vec!["Fresh Contact", "Old Contact"]
        );
    }
}
