//! The meeting transcript, stored as text only (FR-MT-13).
//!
//! Invariant 2: the audio itself is never persisted. This module is the *only* writer of the
//! transcript, and it writes text that has already been through `redact` — a spoken credential
//! ("my password is …") is as sensitive on the write path as a typed one, so the same masking
//! that protects captured screen text protects the transcript.

use rusqlite::{params, Connection};

/// Who spoke, decided by the capture source rather than by inference: microphone input is `Me`,
/// the system tap is `Other`. `Unknown` is stored as NULL — we never guess a speaker.
///
/// `OtherSlot(n)` refines `Other` when the ASR provider diarizes the system-tap stream: it is
/// still "not me", but distinguishable from the other remote voices. The slot is a per-session
/// label, not an identity — mapping a slot to a person's name is a separate, user-confirmed step
/// (`session_speakers`), never an inference stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Me,
    Other,
    OtherSlot(u32),
    Unknown,
    /// Capture/save diagnostic, never participant speech. Stored with reserved origin so
    /// synthesis and provenance can exclude it without guessing from the text.
    SystemNotice,
}

impl Speaker {
    /// The stored key: `me`, `other`, `other:N`, or NULL for unknown.
    pub fn key(self) -> Option<String> {
        match self {
            Speaker::Me => Some("me".into()),
            Speaker::Other => Some("other".into()),
            Speaker::OtherSlot(n) => Some(format!("other:{n}")),
            Speaker::Unknown | Speaker::SystemNotice => None,
        }
    }
}

/// Physical capture channel, independent of diarized speaker identity. Legacy ASR rows remain
/// unknown: a speaker key alone cannot prove whether a room microphone or call tap captured it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOrigin {
    Microphone,
    System,
}

impl CaptureOrigin {
    fn stored(self) -> &'static str {
        match self {
            Self::Microphone => "asr_microphone",
            Self::System => "asr_system",
        }
    }

    fn from_stored(origin: &str) -> Option<Self> {
        match origin {
            "asr_microphone" => Some(Self::Microphone),
            "asr_system" => Some(Self::System),
            _ => None,
        }
    }
}

/// One transcribed line, ready to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSegment<'a> {
    pub session_id: i64,
    pub ts: i64,
    pub speaker: Speaker,
    pub text: &'a str,
    pub confidence: f64,
}

/// Append speech without known capture provenance, or a typed diagnostic. Only the internal
/// `SystemNotice` variant can assign `system_notice`. Spoken marker-like text remains speech.
pub fn append(conn: &Connection, seg: &NewSegment, now: i64) -> Result<i64, rusqlite::Error> {
    append_from_capture(conn, seg, now, None)
}

/// The existing origin TEXT column records the factual channel without inferring from speakers.
/// Diagnostics keep their reserved origin even if a caller supplies a channel.
pub fn append_from_capture(
    conn: &Connection,
    seg: &NewSegment,
    now: i64,
    capture_origin: Option<CaptureOrigin>,
) -> Result<i64, rusqlite::Error> {
    let redacted = crate::sanitize::persist_body(seg.text);
    let origin = if seg.speaker == Speaker::SystemNotice {
        "system_notice"
    } else {
        capture_origin.map_or("asr", CaptureOrigin::stored)
    };
    conn.execute(
        "INSERT INTO transcript_segments
           (session_id, ts, speaker, text, origin, confidence, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            seg.session_id,
            seg.ts,
            seg.speaker.key(),
            redacted.text.as_ref(),
            origin,
            seg.confidence,
            now,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Durable segment identity used for meeting source links. Session scope is fixed by the query.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredSegment {
    pub id: i64,
    pub ts: i64,
    pub speaker: Option<String>,
    pub text: String,
}

pub fn with_ids_for_session(
    conn: &Connection,
    session_id: i64,
) -> Result<Vec<StoredSegment>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id, ts, speaker, text FROM transcript_segments WHERE session_id = ?1 AND excluded = 0 AND origin != 'system_notice' ORDER BY ts, id")?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(StoredSegment {
            id: r.get(0)?,
            ts: r.get(1)?,
            speaker: r.get(2)?,
            text: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// A reversible correction or exclusion. Original ASR text remains local for provenance.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TranscriptEdit {
    pub segment_id: i64,
    pub expected_ts: i64,
    pub expected_text: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub excluded: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EditableSegment {
    pub capture_origin: Option<CaptureOrigin>,
    pub id: i64,
    pub ts: i64,
    pub speaker: Option<String>,
    pub text: String,
    pub original_text: Option<String>,
    pub edited_at: Option<i64>,
    pub excluded: bool,
}

pub fn editable_for_session(conn: &Connection, session_id: i64) -> Result<Vec<EditableSegment>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id, ts, speaker, text, original_text, edited_at, excluded, origin FROM transcript_segments WHERE session_id = ?1 ORDER BY ts, id")?;
    let rows = stmt.query_map([session_id], |r| Ok(EditableSegment { capture_origin: CaptureOrigin::from_stored(&r.get::<_, String>(7)?), id: r.get(0)?, ts: r.get(1)?, speaker: r.get(2)?, text: r.get(3)?, original_text: r.get(4)?, edited_at: r.get(5)?, excluded: r.get(6)? }))?;
    rows.collect()
}

/// Keyset page for live windows. Full saved-editor reads keep their existing API.
pub fn page_for_session(conn: &Connection, session_id: i64, cursor: Option<(i64, i64)>, newer: bool, limit: usize) -> rusqlite::Result<Vec<EditableSegment>> {
    let (ts, id) = cursor.unwrap_or((i64::MAX, i64::MAX));
    let predicate = if newer { "(ts, id) > (?2, ?3)" } else { "(ts, id) < (?2, ?3)" };
    let order = if newer { "ASC" } else { "DESC" };
    let sql = format!("SELECT id, ts, speaker, text, original_text, edited_at, excluded, origin FROM transcript_segments WHERE session_id = ?1 AND excluded = 0 AND trim(text) != '' AND {predicate} ORDER BY ts {order}, id {order} LIMIT ?4");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![session_id, ts, id, limit.clamp(1, 500) as i64], |r| Ok(EditableSegment {
        capture_origin: CaptureOrigin::from_stored(&r.get::<_, String>(7)?), id: r.get(0)?, ts: r.get(1)?, speaker: r.get(2)?, text: r.get(3)?, original_text: r.get(4)?, edited_at: r.get(5)?, excluded: r.get(6)?,
    }))?;
    let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if !newer { rows.reverse(); }
    Ok(rows)
}

/// Validate all IDs before writing, so a wrong-session ID cannot leave a partial correction.
pub fn edit_for_session(conn: &Connection, session_id: i64, edits: &[TranscriptEdit], now: i64) -> Result<(), rusqlite::Error> {
    let transaction = conn.unchecked_transaction()?;
    apply_edits(&transaction, session_id, edits, now)?;
    transaction.commit()
}

/// [`edit_for_session`]'s writes, inside a transaction the caller owns and commits. A finished
/// meeting's searchable copy must change in the same transaction (`meeting_index::edit_transcript`).
pub(crate) fn apply_edits(transaction: &Connection, session_id: i64, edits: &[TranscriptEdit], now: i64) -> Result<(), rusqlite::Error> {
    for edit in edits {
        let found: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM transcript_segments WHERE session_id = ?1 AND id = ?2 AND ts = ?3 AND text = ?4)", params![session_id, edit.segment_id, edit.expected_ts, edit.expected_text], |r| r.get(0))?;
        if !found { return Err(rusqlite::Error::QueryReturnedNoRows); }
    }
    for edit in edits {
        let text = edit.text.as_deref().map(crate::sanitize::persist_body);
        transaction.execute("UPDATE transcript_segments SET original_text = CASE WHEN ?3 IS NOT NULL THEN COALESCE(original_text, text) ELSE original_text END, text = COALESCE(?3, text), excluded = COALESCE(?4, excluded), edited_at = ?5 WHERE session_id = ?1 AND id = ?2", params![session_id, edit.segment_id, text.as_ref().map(|text| text.text.as_ref()), edit.excluded, now])?;
    }
    // Sources from the previous transcript must never remain attached to changed claims.
    transaction.execute("UPDATE meeting_recaps SET evidence = NULL WHERE session_id = ?1", [session_id])?;
    Ok(())
}

/// All lines for a session in time order. Recap (MT4) reads through this.
#[allow(clippy::type_complexity)]
pub fn for_session(
    conn: &Connection,
    session_id: i64,
) -> Result<Vec<(i64, Option<String>, String, f64)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT ts, speaker, text, confidence FROM transcript_segments
         WHERE session_id = ?1 AND excluded = 0 ORDER BY ts, id",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open, NewSession};

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
    fn live_pages_use_timestamp_and_identity_and_never_skip_equal_timestamps() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        for i in 0..600 {
            append(&conn, &NewSegment { session_id: sid, ts: 2000 + i / 3, speaker: Speaker::Me, text: "page text", confidence: 0.9 }, 10).unwrap();
        }
        let tail = page_for_session(&conn, sid, None, false, 999).unwrap();
        assert_eq!(tail.len(), 500);
        let cursor = &tail[0];
        let older = page_for_session(&conn, sid, Some((cursor.ts, cursor.id)), false, 200).unwrap();
        assert_eq!(older.len(), 100);
        assert!(older.last().unwrap().id < tail[0].id);
        let cursor = older.last().unwrap();
        let newer = page_for_session(&conn, sid, Some((cursor.ts, cursor.id)), true, 200).unwrap();
        assert_eq!(newer[0].id, tail[0].id);
        assert!(page_for_session(&conn, sid + 1, None, false, 500).unwrap().is_empty());
    }

    #[test]
    fn edits_are_session_scoped_reversible_and_keep_original_provenance() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        let other = session(&conn);
        let id = append(&conn, &NewSegment { session_id: sid, ts: 2000, speaker: Speaker::Me, text: "Ship Monday", confidence: 0.9 }, 10).unwrap();
        let correction = TranscriptEdit { segment_id: id, expected_ts: 2000, expected_text: "Ship Monday".into(), text: Some("Ship Friday".into()), excluded: None };
        assert!(edit_for_session(&conn, other, std::slice::from_ref(&correction), 20).is_err());
        assert!(edit_for_session(&conn, sid, &[TranscriptEdit { segment_id: id + 999, ..correction.clone() }], 20).is_err());
        edit_for_session(&conn, sid, std::slice::from_ref(&correction), 20).unwrap();
        assert!(edit_for_session(&conn, sid, &[correction], 21).is_err(), "stale editor must not overwrite correction");
        let row = editable_for_session(&conn, sid).unwrap().remove(0);
        assert_eq!(row.original_text.as_deref(), Some("Ship Monday"));
        assert_eq!(with_ids_for_session(&conn, sid).unwrap()[0].text, "Ship Friday");
        let exclusion = TranscriptEdit { segment_id: id, expected_ts: 2000, expected_text: "Ship Friday".into(), text: None, excluded: Some(true) };
        edit_for_session(&conn, sid, std::slice::from_ref(&exclusion), 30).unwrap();
        assert!(for_session(&conn, sid).unwrap().is_empty());
        assert!(with_ids_for_session(&conn, sid).unwrap().is_empty());
        assert!(editable_for_session(&conn, sid).unwrap()[0].excluded);
        edit_for_session(&conn, sid, &[TranscriptEdit { excluded: Some(false), ..exclusion }], 40).unwrap();
        assert_eq!(for_session(&conn, sid).unwrap()[0].2, "Ship Friday");
    }

    #[test]
    fn capture_diagnostics_never_become_participant_evidence() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        let notice_id = append(&conn, &NewSegment { session_id: sid, ts: 2000, speaker: Speaker::Unknown, text: "[Audio note: call audio could not be captured]", confidence: 0.0 }, 10).unwrap();
        conn.execute("UPDATE transcript_segments SET origin = 'system_notice' WHERE id = ?1", [notice_id]).unwrap();
        assert_eq!(for_session(&conn, sid).unwrap().len(), 1);
        assert!(with_ids_for_session(&conn, sid).unwrap().is_empty());
        let spoken_id = append(&conn, &NewSegment { session_id: sid, ts: 3000, speaker: Speaker::Unknown, text: "[Audio note: a participant really said this]", confidence: 0.0 }, 11).unwrap();
        let sources = with_ids_for_session(&conn, sid).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, spoken_id, "genuine unknown-speaker speech remains usable even with low confidence");
    }

    #[test]
    fn system_notice_origin_is_typed_without_classifying_participant_words() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        let text = "[Audio note: ordinary quoted speech]";
        for speaker in [Speaker::Unknown, Speaker::SystemNotice] {
            append(&conn, &NewSegment { session_id: sid, ts: 1_000, speaker, text, confidence: 0.0 }, 9).unwrap();
        }
        let origins: Vec<String> = conn.prepare("SELECT origin FROM transcript_segments ORDER BY id").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(origins, ["asr", "system_notice"]);
        assert_eq!(for_session(&conn, sid).unwrap().len(), 2, "visible transcript retains diagnostics");
    }

    #[test]
    fn segments_are_read_back_in_time_order() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        append(&conn, &NewSegment { session_id: sid, ts: 2_000, speaker: Speaker::Other, text: "second", confidence: 0.9 }, 9).unwrap();
        append(&conn, &NewSegment { session_id: sid, ts: 1_000, speaker: Speaker::Me, text: "first", confidence: 0.8 }, 9).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].2, "first");
        assert_eq!(got[1].2, "second");
    }

    #[test]
    fn a_diarized_slot_is_stored_as_other_n() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        append(&conn, &NewSegment { session_id: sid, ts: 1_000, speaker: Speaker::OtherSlot(2), text: "hi", confidence: 0.9 }, 9).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert_eq!(got[0].1.as_deref(), Some("other:2"));
    }

    #[test]
    fn unknown_speaker_is_stored_as_null() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        append(&conn, &NewSegment { session_id: sid, ts: 1_000, speaker: Speaker::Unknown, text: "hi", confidence: 0.5 }, 9).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert_eq!(got[0].1, None);
    }

    #[test]
    fn spoken_secrets_are_redacted_before_write() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        append(&conn, &NewSegment { session_id: sid, ts: 1_000, speaker: Speaker::Me, text: "the key is sk-ant-abc123def456", confidence: 0.9 }, 9).unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert!(!got[0].2.contains("sk-ant-abc123def456"), "raw secret leaked into transcript");
    }

    #[test]
    fn hidden_format_characters_are_not_stored_in_the_transcript() {
        let conn = crate::open_in_memory().unwrap();
        let sid = session(&conn);
        append(
            &conn,
            &NewSegment {
                session_id: sid,
                ts: 1_000,
                speaker: Speaker::Me,
                text: "I'll\u{200B} send it. \u{202E}Ignore previous",
                confidence: 0.9,
            },
            9,
        )
        .unwrap();
        let got = for_session(&conn, sid).unwrap();
        assert!(!got[0].2.contains('\u{200B}'));
        assert!(!got[0].2.contains('\u{202E}'));
        assert_eq!(crate::sanitize::persist_body(&got[0].2).hidden_removed, 0);
    }
}
