//! Putting a finished meeting on the searchable spine (FR-MT-14, FR-MTUX-02).
//!
//! Transcripts and notes live in their own tables, structured the way the Recap needs them:
//! per-segment, speaker-attributed, time-ordered. That shape is right for reading a meeting back
//! and wrong for finding one. Everything else in SHOGUN is found through `event_log` — the FTS
//! index is a shadow of it, hybrid search walks it, Fusion and extraction read it — so content
//! that never reaches `event_log` is content the user cannot search for.
//!
//! FR-MT-14 puts it as a requirement: the transcript must land where "既存の検索・抽出・Fusion
//! がそのまま効く". This module is what makes that true. It is the same move the connector lane
//! makes (Gmail is normalised into the log while Gmail keeps its own shape) — one spine, many
//! sources, no per-source search path.
//!
//! **What being on the spine does NOT mean** (the A-2 decision, 2026-08-14,
//! `docs/meeting-text-on-the-search-spine.md`): rows with [`SOURCE`] never reach the Batch lane.
//! `event_log::events_in_range_partitioned` excludes them by construction, so search, Fusion,
//! the context pack and *local* extraction all see meetings — and the nightly cloud
//! classification does not. The Deepgram consent covers live transcription only; shipping the
//! finished transcript to the relay every night would be a second, undisclosed egress
//! (invariant 3). `scripts/check-batch-source-filter.py` guards the exclusion in CI.
//!
//! **When**: at the end of a session (Wrapping → Recap), because that is when the transcript is
//! final — and again whenever the note changes afterwards, because a note is often flushed
//! (blur / debounce) moments *after* auto-wrap closed the session.
//!
//! **Changed text replaces its row — in place.** [`index_session`] updates an existing row of the
//! same kind through `event_log::update_content_and_hash` rather than delete-and-reinsert:
//! nightly extraction links `state_provenance` rows to meeting events (a commitment made in a
//! call cites the transcript), and deleting the event would either trip the FK or orphan the
//! evidence behind a state row. An update keeps that provenance honestly valid — the evidence is
//! still "this meeting", now in its edited form — while the stale embedding is dropped so the
//! embed job re-embeds the new text. Only a body that *disappeared* (a note cleared to empty)
//! deletes its row, and then the provenance rows go with it: evidence that no longer exists must
//! not keep vouching for state.

use rusqlite::Connection;

use crate::event_log::{insert, NewEvent};

/// The `source` value meeting-derived rows carry. Distinct from `capture`, so a search result can
/// say "this was said in a meeting" rather than "this was on your screen" (FR-MEM-23) — and so
/// the Batch lane's source filter has a name to exclude.
pub const SOURCE: &str = "meeting";

/// What [`index_session`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Indexed {
    /// Whether every requested search-index mutation landed or an active Dream snapshot pinned one.
    pub outcome: IndexOutcome,
    /// The event row carrying the transcript, if the session had one.
    pub transcript_event_id: Option<i64>,
    /// The event row carrying the user's own note, if they wrote one.
    pub note_event_id: Option<i64>,
    pub changed_event_ids: Vec<i64>,
    pub removed_event_ids: Vec<i64>,
}

/// Durable meeting storage succeeded, but search indexing may be deferred by a Dream snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexOutcome {
    #[default]
    Applied,
    Pinned,
}

impl Indexed {
    pub fn is_empty(&self) -> bool {
        self.transcript_event_id.is_none() && self.note_event_id.is_none()
    }
}

/// Render a session's transcript as one searchable body.
///
/// Speakers are prefixed when known and omitted when not — `Unknown` is stored as NULL precisely
/// because we do not guess (FR-MT-15), and inventing "Someone:" here would launder that NULL into
/// an assertion.
fn transcript_body(conn: &Connection, session_id: i64) -> Result<Option<String>, rusqlite::Error> {
    let segments = crate::transcript_segments::for_session(conn, session_id)?;
    if segments.is_empty() {
        return Ok(None);
    }
    let mut body = String::new();
    for (_ts, speaker, text, _confidence) in segments {
        if !body.is_empty() {
            body.push('\n');
        }
        match speaker.as_deref() {
            Some("me") => body.push_str("Me: "),
            Some("other") => body.push_str("Them: "),
            _ => {}
        }
        body.push_str(&text);
    }
    Ok(Some(body))
}

/// This session's already-indexed rows of `kind`: `(event_id, content_hash)`, oldest first.
fn indexed_rows(
    conn: &Connection,
    session_id: i64,
    kind: &str,
) -> Result<Vec<(i64, String)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, content_hash FROM event_log
         WHERE session_id = ?1 AND source = ?2 AND kind = ?3
         ORDER BY id",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id, SOURCE, kind], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    rows.collect()
}

/// Delete one indexed row through the shared canonical/V21 ordering helper.
fn remove_row(conn: &Connection, event_id: i64) -> Result<(), rusqlite::Error> {
    let _ = crate::search_documents::delete_canonical_event_in(conn, event_id)?;
    Ok(())
}

/// Rewrite an indexed row with edited text. In place, not delete-and-reinsert: nightly extraction
/// links `state_provenance` to meeting events, and dropping the row would either trip that FK or
/// orphan the evidence behind a state row — while the update keeps it honestly valid (the
/// evidence is still this meeting, in its edited form). The stale embedding is removed so the
/// embed job re-embeds the new text; the FTS mirror follows via the AU trigger.
fn rewrite_row(
    conn: &Connection,
    event_id: i64,
    body: &str,
    hash: &str,
) -> Result<IndexOutcome, rusqlite::Error> {
    match crate::event_log::update_content_and_hash(conn, event_id, body, hash) {
        Ok(true) => {}
        Ok(false) => return Err(rusqlite::Error::StatementChangedRows(0)),
        Err(error) if is_dream_pin_error(&error) => return Ok(IndexOutcome::Pinned),
        Err(error) => return Err(error),
    }
    conn.execute("DELETE FROM event_vec WHERE rowid = ?1", [event_id])?;
    conn.execute(
        "DELETE FROM cold_embeddings WHERE event_id = ?1",
        [event_id],
    )?;
    Ok(IndexOutcome::Applied)
}

fn is_dream_pin_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message == crate::dream_cycles::PINNED_EVENT_ERROR
    )
}

/// Make a finished meeting searchable. Returns which rows were written and whether an active Dream
/// snapshot deferred a rewrite. Storage failures remain errors; only the exact pin trigger maps to
/// [`IndexOutcome::Pinned`].
///
/// A session with neither a transcript nor a note indexes to nothing — a meeting where the user
/// took no notes and transcription was off leaves no text, and inventing a placeholder row would
/// put an empty result in every future search. A body that *disappeared* (a note cleared to
/// empty) takes its indexed row with it.
pub fn index_session(conn: &mut Connection, session_id: i64) -> Result<Indexed, rusqlite::Error> {
    let tx = conn.transaction()?;
    let indexed = index_session_tx(&tx, session_id)?;
    tx.commit()?;
    Ok(indexed)
}

/// What [`edit_transcript`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEditOutcome {
    /// Committed. `Some` when the meeting had ended and its searchable rows were rewritten in the
    /// same transaction; `None` while it is still recording (the close indexes the final text).
    Applied(Option<Indexed>),
    /// Rolled back, nothing changed: an unfinished Dream cycle pins the searchable row, and
    /// committing anyway would leave the old (possibly excluded) wording findable.
    Pinned,
}

/// Apply transcript corrections/exclusions and, for a finished meeting, rewrite its searchable
/// rows in one transaction. Either both land or neither does, so search can never keep a line
/// the transcript no longer has (unlike a note save, a Dream pin refuses rather than defers).
pub fn edit_transcript(
    conn: &mut Connection,
    session_id: i64,
    edits: &[crate::transcript_segments::TranscriptEdit],
    now: i64,
) -> Result<TranscriptEditOutcome, rusqlite::Error> {
    match edit_transcript_tx(conn, session_id, edits, now) {
        Err(error) if is_dream_pin_error(&error) => Ok(TranscriptEditOutcome::Pinned),
        other => other,
    }
}

fn edit_transcript_tx(
    conn: &mut Connection,
    session_id: i64,
    edits: &[crate::transcript_segments::TranscriptEdit],
    now: i64,
) -> Result<TranscriptEditOutcome, rusqlite::Error> {
    // Dropping the transaction without commit rolls every write back.
    let tx = conn.transaction()?;
    crate::transcript_segments::apply_edits(&tx, session_id, edits, now)?;
    let ended = crate::session::get(&tx, session_id)?.is_some_and(|s| s.ended_at.is_some());
    let indexed = if ended {
        let indexed = index_session_tx(&tx, session_id)?;
        if indexed.outcome == IndexOutcome::Pinned {
            return Ok(TranscriptEditOutcome::Pinned);
        }
        Some(indexed)
    } else {
        None
    };
    tx.commit()?;
    Ok(TranscriptEditOutcome::Applied(indexed))
}

fn index_session_tx(conn: &Connection, session_id: i64) -> Result<Indexed, rusqlite::Error> {
    let Some(session) = crate::session::get(conn, session_id)? else {
        return Ok(Indexed::default());
    };

    let mut out = Indexed::default();
    let title = session.title.as_deref();
    let app = session.app_bundle_id.as_deref();

    // Two rows, not one: the transcript is what was said and the note is what the user chose to
    // write. Merging them would make a search hit unable to say which it found — and the note is
    // the row that must never read as machine-generated.
    for (kind, body) in [
        ("transcript", transcript_body(conn, session_id)?),
        ("note", crate::session_notes::get(conn, session_id)?),
    ] {
        let existing = indexed_rows(conn, session_id, kind)?;
        let Some(body) = body.filter(|b| !b.trim().is_empty()) else {
            for (id, _) in existing {
                remove_row(conn, id)?;
                out.removed_event_ids.push(id);
            }
            continue;
        };
        // Hash what the log will actually store: at persist time hidden runes are stripped
        // (then secrets masked) and the stored hash is recomputed from that text. Hashing the
        // raw body here would disagree with the stored hash forever, so every re-index would
        // "see" an edit and rewrite the row — dropping its embedding — even though nothing
        // changed.
        let hash = {
            let stripped = shogun_redact::strip_hidden(&body);
            if stripped.removed > 0 {
                crate::event_log::content_hash(
                    crate::redact::redact(stripped.text.as_ref()).as_ref(),
                )
            } else {
                crate::event_log::content_hash(&body)
            }
        };

        // Converge on exactly one row per kind: the first existing row is kept (rewritten if the
        // text changed); any extras are defensive cleanup — index_session itself never creates a
        // second one.
        let mut kept = None;
        for (id, existing_hash) in existing {
            if kept.is_none() {
                if existing_hash != hash
                    && rewrite_row(conn, id, &body, &hash)? == IndexOutcome::Pinned
                {
                    out.outcome = IndexOutcome::Pinned;
                }
                kept = Some(id);
            } else {
                remove_row(conn, id)?;
                out.removed_event_ids.push(id);
            }
        }

        let event_id = match kept {
            Some(id) => id,
            None => {
                // Insert unconditionally, never insert_or_touch: its (source, content_hash)
                // dedup is cross-session, so two meetings with identical text would collapse
                // onto ONE row and `attach_event` below would steal it from the earlier
                // session — re-indexing either meeting then steals it back (ping-pong), and
                // one of the two is always missing from search. This session has no row of
                // this kind (checked above), so a fresh row is correct.
                let id = insert(
                    conn,
                    &NewEvent {
                        // The meeting's own time, not the moment of indexing: a search for what
                        // was said last Tuesday must find it under last Tuesday.
                        ts: session.started_at,
                        source: SOURCE,
                        kind,
                        app_bundle_id: app,
                        window_title: title,
                        content: &body,
                        content_hash: &hash,
                        dwell_ms: 0,
                        display_id: None,
                        window_bounds: None,
                    },
                )?;
                crate::session::attach_event(conn, session_id, id)?;
                id
            }
        };
        match kind {
            "transcript" => out.transcript_event_id = Some(event_id),
            _ => out.note_event_id = Some(event_id),
        }
        out.changed_event_ids.push(event_id);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{close, open, NewSession};
    use crate::transcript_segments::{append, NewSegment, Speaker};

    fn session(conn: &Connection, title: &str) -> i64 {
        let id = open(
            conn,
            &NewSession {
                kind: "meeting",
                started_at: 1_000,
                title: Some(title),
                app_bundle_id: Some("us.zoom.xos"),
                calendar_occurrence_id: None,
                confidence: 0.7,
                provenance: "{}",
            },
        )
        .unwrap();
        close(conn, id, 5_000).unwrap();
        id
    }

    fn say(conn: &Connection, session_id: i64, speaker: Speaker, text: &str, ts: i64) {
        append(
            conn,
            &NewSegment {
                session_id,
                ts,
                speaker,
                text,
                confidence: 0.9,
            },
            ts,
        )
        .unwrap();
    }

    #[test]
    fn a_transcript_becomes_findable_by_the_ordinary_search() {
        // The whole point of FR-MT-14: no meeting-specific search path.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Weekly sync");
        say(
            &conn,
            id,
            Speaker::Other,
            "the vendor renewal was settled at 12k",
            1_100,
        );

        let indexed = index_session(&mut conn, id).unwrap();
        assert!(indexed.transcript_event_id.is_some());

        let hits = crate::search::search(&conn, "vendor renewal", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the meeting must be reachable from ordinary search"
        );
        assert_eq!(
            hits[0].source, SOURCE,
            "and be attributable to a meeting (FR-MEM-23)"
        );
    }

    #[test]
    fn a_note_is_indexed_separately_from_the_transcript() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "1:1");
        say(&conn, id, Speaker::Me, "spoken words", 1_100);
        crate::session_notes::save(&conn, id, "typed words", 2_000).unwrap();

        let indexed = index_session(&mut conn, id).unwrap();
        assert!(indexed.transcript_event_id.is_some());
        assert!(indexed.note_event_id.is_some());
        assert_ne!(indexed.transcript_event_id, indexed.note_event_id);
    }

    #[test]
    fn indexed_rows_belong_to_their_session() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Weekly sync");
        say(&conn, id, Speaker::Me, "hello", 1_100);
        index_session(&mut conn, id).unwrap();

        // `session_id` is what the library view and the compression job join on.
        let texts = crate::session::event_texts(&conn, id).unwrap();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].content.contains("hello"));
    }

    #[test]
    fn the_event_carries_the_meeting_time_not_the_indexing_time() {
        // Searching for "what was said last Tuesday" must find it under last Tuesday.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Weekly sync");
        say(&conn, id, Speaker::Me, "hello", 1_100);
        let indexed = index_session(&mut conn, id).unwrap();

        let ts: i64 = conn
            .query_row(
                "SELECT ts FROM event_log WHERE id = ?1",
                [indexed.transcript_event_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ts, 1_000,
            "the session's start, not the sweep that indexed it"
        );
    }

    #[test]
    fn re_indexing_the_same_meeting_does_not_duplicate_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Weekly sync");
        say(&conn, id, Speaker::Me, "hello", 1_100);

        let first = index_session(&mut conn, id).unwrap();
        let second = index_session(&mut conn, id).unwrap();

        assert_eq!(first.transcript_event_id, second.transcript_event_id);
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM event_log WHERE source = ?1",
                [SOURCE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "a regenerated Recap must not double the meeting in search"
        );
    }

    #[test]
    fn later_index_failure_rolls_back_an_earlier_rewrite() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Atomic sync");
        say(&conn, id, Speaker::Me, "old transcript", 1_100);
        crate::session_notes::save(&conn, id, "old note", 1_200).unwrap();
        let indexed = index_session(&mut conn, id).unwrap();
        say(&conn, id, Speaker::Me, "new transcript", 1_300);
        crate::session_notes::save(&conn, id, "new note", 1_400).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_note_rewrite BEFORE UPDATE OF content ON event_log
             WHEN OLD.id = (SELECT max(id) FROM event_log WHERE kind = 'note')
             BEGIN SELECT RAISE(ABORT, 'induced later failure'); END;",
        )
        .unwrap();

        assert!(index_session(&mut conn, id).is_err());
        let transcript: String = conn
            .query_row(
                "SELECT content FROM event_log WHERE id = ?1",
                [indexed.transcript_event_id.unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transcript, "Me: old transcript");
    }

    #[test]
    fn a_silent_meeting_with_no_notes_indexes_to_nothing() {
        // Notes are optional and transcription can be off. An empty placeholder row would put a
        // blank result into every future search.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Quiet meeting");
        assert!(index_session(&mut conn, id).unwrap().is_empty());
        let n: i64 = conn
            .query_row("SELECT count(*) FROM event_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_whitespace_only_note_is_not_indexed() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        crate::session_notes::save(&conn, id, "   \n  ", 2_000).unwrap();
        assert!(index_session(&mut conn, id)
            .unwrap()
            .note_event_id
            .is_none());
    }

    #[test]
    fn an_unknown_speaker_is_not_given_a_name() {
        // FR-MT-15: NULL means we do not know. Rendering "Someone:" would launder that into a
        // claim, and the claim would then be searchable as if it were evidence.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        say(&conn, id, Speaker::Unknown, "an unattributed line", 1_100);
        index_session(&mut conn, id).unwrap();

        let body: String = conn
            .query_row(
                "SELECT content FROM event_log WHERE source = ?1",
                [SOURCE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, "an unattributed line");
    }

    #[test]
    fn known_speakers_are_labelled() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        say(&conn, id, Speaker::Me, "my line", 1_100);
        say(&conn, id, Speaker::Other, "their line", 1_200);
        index_session(&mut conn, id).unwrap();

        let body: String = conn
            .query_row(
                "SELECT content FROM event_log WHERE source = ?1",
                [SOURCE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, "Me: my line\nThem: their line");
    }

    #[test]
    fn a_missing_session_indexes_to_nothing_rather_than_failing() {
        let mut conn = crate::open_in_memory().unwrap();
        assert!(index_session(&mut conn, 9_999).unwrap().is_empty());
    }

    #[test]
    fn an_edited_note_replaces_its_indexed_row() {
        // The late-flush path: the note is saved (and re-saved) after the session closed. The old
        // version must stop being findable — two search hits for one note, one of them stale, is
        // worse than either alone.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        crate::session_notes::save(&conn, id, "first draft of the note", 2_000).unwrap();
        let first = index_session(&mut conn, id).unwrap();

        crate::session_notes::save(&conn, id, "final version of the note", 3_000).unwrap();
        let second = index_session(&mut conn, id).unwrap();
        // The edit rewrites the SAME row (provenance stability), so the id must not change.
        assert_eq!(first.note_event_id, second.note_event_id);

        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM event_log WHERE source = ?1 AND kind = 'note'",
                [SOURCE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the stale note version must be gone");
        assert!(crate::search::search(&conn, "first draft", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            crate::search::search(&conn, "final version", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_note_cleared_to_empty_takes_its_indexed_row_with_it() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        crate::session_notes::save(&conn, id, "temporary thought", 2_000).unwrap();
        index_session(&mut conn, id).unwrap();

        crate::session_notes::save(&conn, id, "", 3_000).unwrap();
        let after = index_session(&mut conn, id).unwrap();
        assert!(after.note_event_id.is_none());
        assert!(crate::search::search(&conn, "temporary thought", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_edit_survives_extraction_having_cited_the_event() {
        // The FK case: the nightly cycle extracted a commitment from the transcript and wrote a
        // state_provenance row citing the meeting event. A later re-index (edited note flushes
        // re-run index_session for every kind) must neither fail on the FK nor orphan the
        // evidence — the rewrite keeps the row, so the citation stays valid.
        use crate::state::{CommitmentDirection, CommitmentStatus, NewCommitment, Provenance};
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Weekly sync");
        say(&conn, id, Speaker::Me, "I will send the budget", 1_100);
        crate::session_notes::save(&conn, id, "note v1", 2_000).unwrap();
        let first = index_session(&mut conn, id).unwrap();
        let transcript_event = first.transcript_event_id.unwrap();

        // extraction cites the transcript event
        crate::state::insert_commitment(
            &mut conn,
            &NewCommitment {
                direction: CommitmentDirection::Mine,
                counterparty_id: None,
                description: "send the budget",
                due_at: None,
                status: CommitmentStatus::Open,
                project_id: None,
                confidence: 0.4,
                now: 2_500,
            },
            &[Provenance::new(transcript_event)],
        )
        .unwrap();

        // the late note edit re-runs the whole index — including the transcript's converge pass
        crate::session_notes::save(&conn, id, "note v2", 3_000).unwrap();
        let second =
            index_session(&mut conn, id).expect("re-index must not trip the provenance FK");
        assert_eq!(second.transcript_event_id, Some(transcript_event));
        assert_eq!(
            crate::search::search(&conn, "note v2", 10).unwrap().len(),
            1
        );

        // the citation is intact
        let cites: i64 = conn
            .query_row(
                "SELECT count(*) FROM state_provenance WHERE event_id = ?1",
                [transcript_event],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cites, 1, "provenance must survive a re-index");
    }

    #[test]
    fn a_cleared_note_with_a_citation_removes_the_citation_too() {
        // Deleting the note deletes its evidence; provenance rows citing it must go rather than
        // dangle (FK) — the state row stays, but it no longer claims this note as its source.
        use crate::state::{NewOpenLoop, OpenLoopKind, Provenance};
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        crate::session_notes::save(&conn, id, "chase the vendor", 2_000).unwrap();
        let first = index_session(&mut conn, id).unwrap();
        let note_event = first.note_event_id.unwrap();
        crate::state::insert_open_loop(
            &mut conn,
            &NewOpenLoop {
                kind: OpenLoopKind::FollowUp,
                description: "chase the vendor",
                counterparty_id: None,
                project_id: None,
                opened_at: 2_100,
                confidence: 0.4,
                now: 2_100,
            },
            &[Provenance::new(note_event)],
        )
        .unwrap();

        crate::session_notes::save(&conn, id, "", 3_000).unwrap();
        index_session(&mut conn, id).expect("clearing a cited note must not trip the FK");
        assert!(crate::search::search(&conn, "chase the vendor", 10)
            .unwrap()
            .is_empty());
        let cites: i64 = conn
            .query_row(
                "SELECT count(*) FROM state_provenance WHERE event_id = ?1",
                [note_event],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cites, 0);
    }

    #[test]
    fn identical_text_in_two_meetings_indexes_two_rows() {
        // insert_or_touch's (source, hash) dedup is cross-session: meeting B would touch
        // meeting A's row and attach_event would steal it — session A silently loses its
        // indexed note, and every re-index steals the row back (ping-pong).
        let mut conn = crate::open_in_memory().unwrap();
        let a = session(&conn, "Standup Monday");
        let b = session(&conn, "Standup Tuesday");
        crate::session_notes::save(&conn, a, "same words", 2_000).unwrap();
        crate::session_notes::save(&conn, b, "same words", 2_500).unwrap();

        let indexed_a = index_session(&mut conn, a).unwrap();
        let indexed_b = index_session(&mut conn, b).unwrap();
        assert_ne!(
            indexed_a.note_event_id, indexed_b.note_event_id,
            "one row per meeting"
        );

        // A's row still belongs to A after B was indexed.
        let owner: i64 = conn
            .query_row(
                "SELECT session_id FROM event_log WHERE id = ?1",
                [indexed_a.note_event_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, a, "meeting B must not steal meeting A's indexed row");
        // And a re-index of A converges on its own row rather than stealing B's.
        let again = index_session(&mut conn, a).unwrap();
        assert_eq!(again.note_event_id, indexed_a.note_event_id);
    }

    #[test]
    fn a_note_with_hidden_runes_reindexes_without_rewriting() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        crate::session_notes::save(&conn, id, "vendor\u{200B} decision", 2_000).unwrap();
        let first = index_session(&mut conn, id).unwrap();

        // Any UPDATE of content is a rewrite — make it loud.
        conn.execute_batch(
            "CREATE TRIGGER no_rewrite BEFORE UPDATE OF content ON event_log
             BEGIN SELECT RAISE(ABORT, 'unchanged note must not be rewritten'); END;",
        )
        .unwrap();
        let second =
            index_session(&mut conn, id).expect("re-index of an unchanged note must be a no-op");
        assert_eq!(first.note_event_id, second.note_event_id);
    }

    #[test]
    fn replacing_a_row_does_not_touch_the_other_kind() {
        // The transcript must survive a note edit: drop_stale is kind-scoped.
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Meeting");
        say(&conn, id, Speaker::Me, "the spoken record", 1_100);
        crate::session_notes::save(&conn, id, "note v1", 2_000).unwrap();
        let first = index_session(&mut conn, id).unwrap();

        crate::session_notes::save(&conn, id, "note v2", 3_000).unwrap();
        let second = index_session(&mut conn, id).unwrap();

        assert_eq!(first.transcript_event_id, second.transcript_event_id);
        assert_eq!(
            crate::search::search(&conn, "spoken record", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pinned_meeting_rewrite_is_reported_without_deleting_embeddings() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Pinned meeting");
        crate::session_notes::save(&conn, id, "note v1", 2_000).unwrap();
        let event_id = index_session(&mut conn, id).unwrap().note_event_id.unwrap();
        crate::vector::upsert(&conn, event_id, &[0.25; 384]).unwrap();
        conn.execute(
            "INSERT INTO cold_embeddings (event_id, partition, scale, dim, codes)
             VALUES (?1, 0, 1.0, 1, x'01')",
            [event_id],
        )
        .unwrap();
        crate::dream_cycles::freeze_full_cycle(&mut conn, "pinned-cycle", 0, 6_000, 6_000).unwrap();
        crate::session_notes::save(&conn, id, "note v2", 7_000).unwrap();

        let indexed = index_session(&mut conn, id).unwrap();
        assert_eq!(indexed.outcome, IndexOutcome::Pinned);

        let content: String = conn
            .query_row(
                "SELECT content FROM event_log WHERE id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, "note v1");
        assert!(crate::vector::get(&conn, event_id).unwrap().is_some());
        let cold: i64 = conn
            .query_row(
                "SELECT count(*) FROM cold_embeddings WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cold, 1);
    }

    fn line(conn: &Connection, session_id: i64, text: &str, ts: i64) -> i64 {
        append(conn, &NewSegment { session_id, ts, speaker: Speaker::Other, text, confidence: 0.9 }, ts).unwrap()
    }

    fn edit(id: i64, ts: i64, expected: &str, text: Option<&str>, excluded: Option<bool>) -> crate::transcript_segments::TranscriptEdit {
        crate::transcript_segments::TranscriptEdit { segment_id: id, expected_ts: ts, expected_text: expected.into(), text: text.map(str::to_string), excluded }
    }

    fn transcript_row(conn: &Connection, session_id: i64) -> Vec<String> {
        conn.prepare("SELECT content FROM event_log WHERE session_id = ?1 AND source = 'meeting' AND kind = 'transcript'")
            .unwrap()
            .query_map([session_id], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn corrections_and_exclusions_after_close_rewrite_the_searchable_transcript() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Sync");
        let kept = line(&conn, id, "budget review stays on Friday", 1_100);
        let private = line(&conn, id, "zebracorn layoffs are confidential", 1_200);
        index_session(&mut conn, id).unwrap();
        assert_eq!(crate::search::search(&conn, "zebracorn", 10).unwrap().len(), 1);

        let outcome = edit_transcript(&mut conn, id, &[
            edit(private, 1_200, "zebracorn layoffs are confidential", None, Some(true)),
            edit(kept, 1_100, "budget review stays on Friday", Some("budget review moves to Monday"), None),
        ], 9_000).unwrap();
        let indexed = match outcome {
            TranscriptEditOutcome::Applied(Some(indexed)) => indexed,
            other => panic!("ended meeting must re-index: {other:?}"),
        };
        assert!(indexed.transcript_event_id.is_some());

        assert!(crate::search::search(&conn, "zebracorn", 10).unwrap().is_empty(), "excluded line must leave search");
        assert!(crate::search::search(&conn, "Friday", 10).unwrap().is_empty(), "corrected wording must leave search");
        assert_eq!(crate::search::search(&conn, "Monday", 10).unwrap().len(), 1);
        assert_eq!(transcript_row(&conn, id), vec!["Them: budget review moves to Monday".to_string()]);

        // Excluding the last line removes the searchable row instead of leaving the old body.
        let corrected = "budget review moves to Monday";
        edit_transcript(&mut conn, id, &[edit(kept, 1_100, corrected, None, Some(true))], 9_100).unwrap();
        assert!(transcript_row(&conn, id).is_empty());
        assert!(crate::search::search(&conn, "Monday", 10).unwrap().is_empty());
    }

    #[test]
    fn edits_during_recording_wait_for_the_close_to_index() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = open(&conn, &NewSession { kind: "meeting", started_at: 1_000, title: Some("Live"), app_bundle_id: None, calendar_occurrence_id: None, confidence: 0.7, provenance: "{}" }).unwrap();
        let private = line(&conn, id, "zebracorn spoken live", 1_100);
        line(&conn, id, "public roadmap", 1_200);
        let outcome = edit_transcript(&mut conn, id, &[edit(private, 1_100, "zebracorn spoken live", None, Some(true))], 2_000).unwrap();
        assert_eq!(outcome, TranscriptEditOutcome::Applied(None));
        assert!(transcript_row(&conn, id).is_empty(), "no half-finished meeting on the spine");

        close(&conn, id, 3_000).unwrap();
        index_session(&mut conn, id).unwrap();
        assert_eq!(transcript_row(&conn, id), vec!["Them: public roadmap".to_string()]);
    }

    #[test]
    fn a_dream_pinned_transcript_refuses_the_edit_and_changes_nothing() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Pinned");
        let private = line(&conn, id, "zebracorn pinned", 1_100);
        line(&conn, id, "other words", 1_200);
        index_session(&mut conn, id).unwrap();
        crate::dream_cycles::freeze_full_cycle(&mut conn, "pinned-edit-cycle", 0, 6_000, 6_000).unwrap();

        let outcome = edit_transcript(&mut conn, id, &[edit(private, 1_100, "zebracorn pinned", None, Some(true))], 7_000).unwrap();
        assert_eq!(outcome, TranscriptEditOutcome::Pinned);
        let rows = crate::transcript_segments::editable_for_session(&conn, id).unwrap();
        assert!(rows.iter().all(|row| !row.excluded), "the exclusion must roll back with the pinned rewrite");
        assert!(transcript_row(&conn, id)[0].contains("zebracorn pinned"));
    }

    #[test]
    fn a_failed_rewrite_rolls_the_correction_back() {
        let mut conn = crate::open_in_memory().unwrap();
        let id = session(&conn, "Atomic");
        let private = line(&conn, id, "zebracorn atomic", 1_100);
        line(&conn, id, "other words", 1_200);
        index_session(&mut conn, id).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_transcript_rewrite BEFORE UPDATE OF content ON event_log
                 WHEN OLD.kind = 'transcript'
                 BEGIN SELECT RAISE(ABORT, 'induced rewrite failure'); END;",
        )
        .unwrap();
        assert!(edit_transcript(&mut conn, id, &[edit(private, 1_100, "zebracorn atomic", None, Some(true))], 7_000).is_err());
        let rows = crate::transcript_segments::editable_for_session(&conn, id).unwrap();
        assert!(rows.iter().all(|row| !row.excluded));
    }
}
