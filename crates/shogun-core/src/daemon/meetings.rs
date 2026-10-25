//! Meeting-query helpers kept beside the meeting domain.

use super::*;

impl Db {
    pub fn upcoming_calendar_reminders(
        &self,
        now: i64,
    ) -> MemoryResult<Vec<crate::meeting::reminders::Reminder>> {
        self.with_conn("meeting.upcoming_reminders", |conn| {
            let occurrences =
                shogun_memory::calendar::upcoming(conn, now, now.saturating_add(600_000))?;
            Ok::<_, rusqlite::Error>(
                occurrences
                    .into_iter()
                    .filter_map(crate::meeting::reminders::from_occurrence)
                    .collect(),
            )
        })
    }

    /// Bounded meeting history, retaining read failures instead of claiming an empty archive.
    pub fn recent_meetings(
        &self,
        before_id: Option<i64>,
        limit: usize,
    ) -> MemoryResult<Vec<shogun_memory::session::Session>> {
        self.with_conn("meeting.recent", |conn| {
            shogun_memory::session::recent_meetings(conn, before_id, limit)
        })
    }

    /// One meeting interval's row (times, title, app), or `None` when it does not exist or the
    /// read failed — the API read treats both as not found.
    pub fn meeting_session(&self, session_id: i64) -> Option<shogun_memory::session::Session> {
        self.with_conn("meeting.session", |conn| shogun_memory::session::get(conn, session_id))
            .ok()
            .flatten()
            .filter(|s| s.kind == "meeting")
    }

    /// When the meeting/call interval started, or `None` when it no longer exists (deleted) or
    /// the read failed. Unlike `meeting_session` it accepts calls as well as meetings. A background
    /// writer compares it across a long job to tell "same meeting" from "gone, or the id reused".
    pub fn session_started_at(&self, session_id: i64) -> Option<i64> {
        self.with_conn("meeting.session_started_at", |conn| {
            shogun_memory::session::get(conn, session_id)
        })
        .ok()
        .flatten()
        .filter(|s| s.kind == "meeting" || s.kind == "call")
        .map(|s| s.started_at)
    }

    /// Lexical search over meeting recaps and transcripts. Query-relevant, not latest-session.
    pub fn search_meetings(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<shogun_memory::search::MeetingSearchHit> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        self.with_conn("search.search_meetings", |c| {
            shogun_memory::search::search_meetings(c, query, limit)
        })
        .unwrap_or_default()
    }
}

/// The calendar event that is on right now, as meeting detection needs it (FR-MT-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentOccurrence {
    /// Stable local occurrence id, distinct even when recurring events share an external id.
    pub id: i64,
    /// The calendar's event id, for the per-event exclusion setting (FR-MT-02).
    pub external_id: String,
    /// End of the event, epoch ms.
    pub ends_at: i64,
}

impl Db {
    /// Open a meeting interval (FR-MT-05). The macOS adapter has no database access of its own —
    /// every write goes through `Db`, keeping the data tier in the core (invariant 1).
    pub fn open_meeting(
        &self,
        title: Option<&str>,
        app_bundle_id: Option<&str>,
        confidence: f64,
        provenance: &str,
    ) -> Option<i64> {
        self.open_meeting_with(title, app_bundle_id, confidence, provenance, false)
    }

    /// [`Self::open_meeting`] with the in-person flag (issue #262 FR-MC-01): a manual start for a
    /// room, where every voice is on the microphone and there is no call audio to tap.
    pub fn open_meeting_with(
        &self,
        title: Option<&str>,
        app_bundle_id: Option<&str>,
        confidence: f64,
        provenance: &str,
        in_person: bool,
    ) -> Option<i64> {
        let now = self.now_ms();
        self.with_conn("meeting.open", |conn| {
            // Tie the interval to the calendar event running now, if there is one (FR-MT-06 /
            // FR-MT-05). The event's title is the meeting's name when they agree it is the same
            // meeting; a drop-in call stays unlinked with the window title it was detected by.
            // A failed lookup degrades to "no event", never to a failed meeting.
            let occurrence = shogun_memory::calendar::occurrence_at(conn, now).ok().flatten();
            let linked_title = occurrence.as_ref().and_then(|o| o.title.as_deref());
            shogun_memory::session::open_with(
                conn,
                &shogun_memory::session::NewSession {
                    kind: "meeting",
                    started_at: now,
                    title: linked_title.or(title),
                    app_bundle_id,
                    calendar_occurrence_id: occurrence.as_ref().map(|o| o.id),
                    confidence,
                    provenance,
                },
                in_person,
            )
        })
        .ok()
    }

    /// The calendar event running now (or starting within ten minutes) — detection signal ① of
    /// FR-MT-04, and the id "always skip this meeting" settings key on. `None` on any failure:
    /// a calendar that cannot be read is "no event", never a reason to stop detecting.
    pub fn calendar_occurrence_now(&self) -> Option<CurrentOccurrence> {
        let now = self.now_ms();
        self.with_conn("meeting.occurrence_now", |conn| {
            shogun_memory::calendar::occurrence_at(conn, now)
        })
        .ok()
        .flatten()
        .map(|o| CurrentOccurrence { id: o.id, external_id: o.external_id, ends_at: o.end_at })
    }

    /// When the calendar event a meeting is linked to ends — the FR-MT-11 "event over + 10 min"
    /// end condition. `None` for a drop-in meeting or a cancelled event.
    pub fn meeting_occurrence_end(&self, session_id: i64) -> Option<i64> {
        self.with_conn("meeting.occurrence_end", |conn| {
            let linked: Option<i64> = conn
                .query_row(
                    "SELECT calendar_occurrence_id FROM sessions WHERE id = ?1",
                    [session_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            Ok::<_, rusqlite::Error>(match linked {
                Some(id) => shogun_memory::calendar::get(conn, id)?,
                None => None,
            })
        })
        .ok()
        .flatten()
        .filter(|o| !o.cancelled)
        .map(|o| o.end_at)
    }

    /// Attach an already-recorded event to a session (FR-MT-05). Best-effort: the event is durable
    /// whether or not it attaches, so a lock/write failure is swallowed. Returns whether it stuck.
    pub fn attach_event_to_meeting(&self, session_id: i64, event_id: i64) -> bool {
        self.with_conn("meeting.attach_event", |conn| {
            shogun_memory::session::attach_event(conn, session_id, event_id)
        })
        .is_ok()
    }

    /// Close a meeting interval. Idempotent — the first close wins (FR-MT-11).
    ///
    /// Closing also puts the finished meeting on the searchable spine (FR-MT-14,
    /// `meeting_index::index_session`): the transcript is final exactly now, and a close that
    /// skipped indexing would leave a meeting the user can read back but never find. Indexing is
    /// best-effort — the close itself must never fail because search maintenance did.
    pub fn close_meeting(&self, id: i64) -> bool {
        self.close_meeting_at(id, self.now_ms())
    }

    /// [`Self::close_meeting`], stamping `ended_at` with when the meeting actually ended (the
    /// Stop press or the auto-end decision) rather than when closing ran. The desktop closes only
    /// after the provider drains its final words, up to ~30 s later (issue #1005); stamping that
    /// moment overstated the duration and the recap window. Never later than now.
    pub fn close_meeting_at(&self, id: i64, ended_at: i64) -> bool {
        let ended_at = ended_at.min(self.now_ms());
        self.with_conn_mut("meeting.close", |conn| {
            shogun_memory::session::close(conn, id, ended_at)?;
            // Search maintenance is best-effort on purpose: the close itself has already stuck,
            // and failing it here would re-open a meeting the user ended.
            if let Ok(indexed) = shogun_memory::meeting_index::index_session(conn, id) {
                self.reconcile_meeting_index_locked(conn, &indexed);
            }
            Ok::<(), rusqlite::Error>(())
        })
        .is_ok()
    }

    /// Save the note typed during a meeting (FR-MT-10).
    ///
    /// A note is often flushed (blur / debounce) moments after auto-wrap already closed the
    /// session, and users edit notes from the Recap afterwards — so if the session has ended,
    /// the index row is refreshed here (`index_session` replaces a changed note's row). While
    /// the session is still open, indexing waits for the close: half-typed notes do not belong
    /// in search.
    pub fn save_meeting_note(&self, session_id: i64, body: &str) -> bool {
        let now = self.now_ms();
        self.with_conn_mut("meeting.save_note", |conn| {
            shogun_memory::session_notes::save(conn, session_id, body, now)?;
            let ended = shogun_memory::session::get(conn, session_id)
                .ok()
                .flatten()
                .is_some_and(|s| s.ended_at.is_some());
            if ended {
                let indexed = shogun_memory::meeting_index::index_session(conn, session_id)?;
                // A Dream pin defers only the searchable mirror. The note above is already durable,
                // so `Pinned` is a successful save; reconciliation keeps the prior mirror honest.
                self.reconcile_meeting_index_locked(conn, &indexed);
            }
            Ok::<(), rusqlite::Error>(())
        })
        .is_ok()
    }

    /// Store the model-generated Recap for a meeting interval (FR-MT-19). Upsert on
    /// `session_id` (one Recap per interval): a re-run replaces the degraded/previous minutes. The
    /// summary is redacted inside the memory writer. Best-effort — a write failure leaves the
    /// existing (degraded) Recap in place rather than interrupting anything.
    pub fn save_meeting_recap(
        &self,
        session_id: i64,
        summary: &str,
        decisions_json: &str,
        next_actions_json: &str,
        sections_json: Option<&str>,
        model: &str,
    ) -> bool {
        let now = self.now_ms();
        self.with_conn("meeting.save_recap", |conn| {
            shogun_memory::meeting_recaps::save(
                conn,
                session_id,
                summary,
                decisions_json,
                next_actions_json,
                sections_json,
                model,
                now,
            )
        })
        .is_ok()
    }

    /// Store generated minutes only if the session still has the template used to build them.
    /// The check and write share one connection lock, so a template change cannot land between
    /// them and let an obsolete generator overwrite the newly requested shape.
    #[allow(clippy::too_many_arguments)]
    pub fn save_meeting_recap_if_template(
        &self,
        session_id: i64,
        expected_template: Option<&str>,
        summary: &str,
        decisions_json: &str,
        next_actions_json: &str,
        sections_json: Option<&str>,
        model: &str,
        evidence_json: &str,
        expected_sources: &[shogun_memory::transcript_segments::StoredSegment],
    ) -> bool {
        let now = self.now_ms();
        self.with_conn("meeting.save_recap_if_template", |conn| {
            let current = shogun_memory::meeting_recaps::session_template(conn, session_id)?;
            let sources = shogun_memory::transcript_segments::with_ids_for_session(conn, session_id)?;
            if current.as_deref() != expected_template || sources != expected_sources {
                return Ok::<bool, rusqlite::Error>(false);
            }
            shogun_memory::meeting_recaps::save_with_evidence(
                conn,
                session_id,
                summary,
                decisions_json,
                next_actions_json,
                sections_json,
                model,
                evidence_json,
                now,
            )?;
            Ok::<bool, rusqlite::Error>(true)
        })
        .unwrap_or(false)
    }

    /// The template chosen for a meeting interval, if any. Unknown/absent ids resolve to the
    /// default at the caller (`meeting::templates::resolve`).
    pub fn session_template(&self, session_id: i64) -> Option<String> {
        self.with_conn("meeting.session_template", |conn| {
            shogun_memory::meeting_recaps::session_template(conn, session_id)
        })
        .ok()
        .flatten()
    }

    /// `[Track]` one Recap next action (FR-MT-17): the user confirms a candidate, and only then
    /// does it enter state — as a commitment at confidence 1.0 resting on the tap itself.
    /// `direction` ("mine" / "theirs") and `counterparty_id` override the owner the Recap named.
    /// Refuses text that is not one of the session's own suggestions.
    pub fn track_recap_action(
        &self,
        session_id: i64,
        text: &str,
        direction: Option<&str>,
        counterparty_id: Option<i64>,
    ) -> Result<shogun_memory::recap_track::Tracked, String> {
        use shogun_memory::recap_track::{track, TrackError, TrackRequest};
        use shogun_memory::state::CommitmentDirection;
        let direction = match direction.map(str::trim).filter(|d| !d.is_empty()) {
            None => None,
            Some("mine") => Some(CommitmentDirection::Mine),
            Some("theirs") => Some(CommitmentDirection::Theirs),
            Some(other) => return Err(format!("unknown direction \"{other}\" (mine|theirs)")),
        };
        let req = TrackRequest { session_id, text, direction, counterparty_id, now: self.now_ms() };
        self.with_conn_mut("meeting.recap.track", |conn| match track(conn, &req) {
            Ok(t) => Ok(Ok(t)),
            Err(TrackError::Memory(e)) => Err(e),
            Err(refused) => Ok(Err(refused.to_string())),
        })
        .map_err(|fault| format!("track failed: {}", fault.as_str()))?
    }

    /// For each of a session's next actions, in Recap order, the commitment it was tracked as.
    pub fn recap_tracked(&self, session_id: i64) -> Vec<Option<i64>> {
        self.with_conn("meeting.recap.tracked", |conn| {
            shogun_memory::recap_track::tracked_for_session(conn, session_id)
        })
        .unwrap_or_default()
    }

    /// Record which template a session's minutes should use (L1, local, reversible). The id must
    /// exist in the registry — an invented name is refused with a reason, not stored.
    pub fn set_session_template(
        &self,
        session_id: i64,
        template: Option<&str>,
    ) -> Result<bool, String> {
        if let Some(id) = template {
            if crate::meeting::templates::by_id(id).is_none() {
                return Err(format!("unknown meeting template \"{id}\""));
            }
        }
        let now = self.now_ms();
        self.with_conn("meeting.set_template", |conn| {
            shogun_memory::meeting_recaps::set_session_template(conn, session_id, template, now)
        })
        .map_err(|e| e.as_str().to_string())
    }

    /// All saved segments, including excluded ones, for the correction surface.
    pub fn meeting_transcript_edit_rows(&self, session_id: i64) -> MemoryResult<Vec<shogun_memory::transcript_segments::EditableSegment>> {
        self.with_conn("meeting.transcript_edit_rows", |conn| shogun_memory::transcript_segments::editable_for_session(conn, session_id))
    }

    /// Explicit user corrections are reversible; no generated minutes or notes are overwritten.
    /// A finished meeting's searchable copy (durable, FTS and Hot) changes with the edit or the
    /// edit is refused, so an excluded line cannot stay findable behind a successful save.
    pub fn edit_meeting_transcript(&self, session_id: i64, edits: &[shogun_memory::transcript_segments::TranscriptEdit]) -> Result<(), String> {
        use shogun_memory::meeting_index::TranscriptEditOutcome;
        if session_id <= 0 || self.meeting_session(session_id).is_none() { return Err("meeting not found".into()); }
        if edits.is_empty() || edits.len() > 100 || edits.iter().any(|edit| edit.segment_id <= 0 || (edit.text.is_none() && edit.excluded.is_none()) || edit.text.as_ref().is_some_and(|text| text.trim().is_empty() || text.chars().count() > 20_000)) { return Err("invalid transcript edit".into()); }
        let now = self.now_ms();
        self.with_conn_mut_reported("meeting.transcript_edit", |conn| {
            match shogun_memory::meeting_index::edit_transcript(conn, session_id, edits, now) {
                Ok(TranscriptEditOutcome::Applied(indexed)) => {
                    if let Some(indexed) = &indexed { self.reconcile_meeting_index_locked(conn, indexed); }
                    Ok(())
                }
                Ok(TranscriptEditOutcome::Pinned) => Err("memory is being organized; try this transcript edit again in a few minutes".to_string()),
                Err(_) => Err("could not edit this transcript; reload and try again".to_string()),
            }
        })?;
        if let Some(bus) = &self.bus { bus.publish(crate::bus::BusEvent::MeetingTranscriptChanged { session_id }); }
        Ok(())
    }

    /// Durable transcript sources from one session; database failures remain distinguishable.
    pub fn meeting_transcript_sources(
        &self,
        session_id: i64,
    ) -> MemoryResult<Vec<shogun_memory::transcript_segments::StoredSegment>> {
        self.with_conn("meeting.transcript_sources", |conn| {
            shogun_memory::transcript_segments::with_ids_for_session(conn, session_id)
        })
    }

    /// Bounded persisted transcript page for live windows.
    pub fn meeting_transcript_page(&self, session_id: i64, cursor: Option<(i64, i64)>, newer: bool, limit: usize) -> MemoryResult<Vec<shogun_memory::transcript_segments::EditableSegment>> {
        self.with_conn("meeting.transcript_page", |conn| shogun_memory::transcript_segments::page_for_session(conn, session_id, cursor, newer, limit))
    }

    /// The transcript of a meeting interval as `(speaker, text)` in time order. Drops
    /// the ts/confidence columns the Recap builder does not need. Empty when the interval has no
    /// transcript (audio degraded to notes-only) or on a DB error.
    pub fn transcript_for_recap(&self, session_id: i64) -> Vec<(Option<String>, String)> {
        self.with_conn("meeting.transcript_for_recap", |conn| {
            shogun_memory::transcript_segments::for_session(conn, session_id)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(_ts, speaker, text, _confidence)| (speaker, text))
        .collect()
    }

    /// Full transcript lines for the post-meeting viewer (FR-MT-10): `(ts, speaker, text)` in time
    /// order. Empty when the interval has no transcript or on a DB error.
    pub fn meeting_transcript(&self, session_id: i64) -> Vec<(i64, Option<String>, String)> {
        self.try_meeting_transcript(session_id).unwrap_or_default()
    }

    pub fn try_meeting_transcript(
        &self,
        session_id: i64,
    ) -> MemoryResult<Vec<(i64, Option<String>, String)>> {
        let rows = self.with_conn("meeting.transcript", |conn| {
            shogun_memory::transcript_segments::for_session(conn, session_id)
        })?;
        Ok(rows
            .into_iter()
            .map(|(ts, speaker, text, _confidence)| (ts, speaker, text))
            .collect())
    }

    /// The note typed during a meeting interval (FR-MT-10), if any. `None` covers both "no note"
    /// and a DB error — the Recap builder treats both the same (nothing to add from notes), and
    /// the failure itself is recorded in the health signal rather than lost (issue #121).
    pub fn meeting_note(&self, session_id: i64) -> Option<String> {
        self.try_meeting_note(session_id).ok().flatten()
    }

    /// The same read with a DB error kept apart from "no note". The notch editor loads the stored
    /// note before it lets a keystroke save (#767); reading a failure as "empty" would let the
    /// next save replace the whole note.
    pub fn try_meeting_note(&self, session_id: i64) -> MemoryResult<Option<String>> {
        self.with_conn("meeting.note", |conn| {
            shogun_memory::session_notes::get(conn, session_id)
        })
    }

    /// Append one transcribed line to a meeting interval (FR-MT-13). The text is redacted inside the
    /// memory writer. Best-effort: a write failure drops the line rather than interrupting capture.
    pub fn append_transcript(
        &self,
        session_id: i64,
        ts: i64,
        speaker: shogun_memory::transcript_segments::Speaker,
        text: &str,
        confidence: f64,
    ) -> bool {
        self.append_transcript_from_capture(session_id, ts, speaker, text, confidence, None)
    }

    /// Append text with a factual physical capture channel, separate from speaker identity.
    pub fn append_transcript_from_capture(
        &self,
        session_id: i64,
        ts: i64,
        speaker: shogun_memory::transcript_segments::Speaker,
        text: &str,
        confidence: f64,
        capture_origin: Option<shogun_memory::transcript_segments::CaptureOrigin>,
    ) -> bool {
        let now = self.now_ms();
        self.with_conn("meeting.append_transcript", |conn| {
            shogun_memory::transcript_segments::append_from_capture(
                conn,
                &shogun_memory::transcript_segments::NewSegment {
                    session_id,
                    ts,
                    speaker,
                    text,
                    confidence,
                },
                now,
                capture_origin,
            )
        })
        .is_ok()
    }

    /// Close intervals left open by a previous run (crash, force-quit, power cut).
    ///
    /// Only rows that started before `started_before_ms` (the caller's boot time) are touched, so
    /// a call that lands after this run has opened a live meeting cannot zero-length it. Returns
    /// how many were closed. Their last transcript timestamp (or start when empty) is the last
    /// known moment; time with the app off must never be invented as meeting duration.
    pub fn close_abandoned_meetings(&self, started_before_ms: i64) -> usize {
        let now = self.now_ms();
        self.with_conn_mut("meeting.close_abandoned", |conn| {
            // Ids first, then close, then index: a crash-abandoned meeting still holds whatever
            // transcript and note it captured, and closing is the moment that text goes on the search
            // spine (FR-MT-14) — the bulk UPDATE alone would leave these the only meetings the user
            // can read back but never find. Indexing is best-effort, same as `close_meeting`.
            let ids = shogun_memory::meeting_recovery::close_abandoned(conn, started_before_ms, now)?;
            let closed = ids.len();
            for id in ids {
                if let Ok(indexed) = shogun_memory::meeting_index::index_session(conn, id) {
                    self.reconcile_meeting_index_locked(conn, &indexed);
                }
            }
            Ok::<usize, rusqlite::Error>(closed)
        })
        .unwrap_or(0)
    }

    /// Claim crash recap work before submission, across launches.
    pub fn claim_recovered_recaps(&self) -> Vec<i64> {
        self.with_conn_mut("meeting.recovery.claim", shogun_memory::meeting_recovery::claim).unwrap_or_default()
    }

    /// Quiet notices are fetched only after the notch is listening.
    pub fn recovered_meeting_notices(&self) -> Vec<Option<String>> {
        self.with_conn_mut("meeting.recovery.notices", shogun_memory::meeting_recovery::notices).unwrap_or_default()
    }

    /// The degraded Recap for an interval (FR-MT-19): what can be said locally, with no model and
    /// no network. `None` only when the interval does not exist.
    pub fn meeting_recap(&self, session_id: i64) -> Option<crate::meeting::recap::Recap> {
        self.try_meeting_recap(session_id).ok().flatten()
    }

    pub fn try_meeting_recap(
        &self,
        session_id: i64,
    ) -> MemoryResult<Option<crate::meeting::recap::Recap>> {
        self.with_conn("meeting.recap", |conn| {
            let Some(session) = shogun_memory::session::get(conn, session_id)? else {
                return Ok::<_, rusqlite::Error>(None);
            };
            let notes = shogun_memory::session_notes::get(conn, session_id)?;
            // How much this Recap had to work with. Counted rather than estimated — it is the honest
            // answer to "is anything actually being captured in meetings?" (context health).
            let captured: i64 = conn
                .query_row(
                    "SELECT count(*) FROM event_log WHERE session_id = ?1 AND kind != 'track'",
                    [session_id],
                    |r| r.get(0),
                )?;
            Ok(Some(crate::meeting::recap::degraded(
                &session,
                notes,
                captured as usize,
            )))
        })
    }

    /// Delete one saved meeting for good (D6) — the Meetings list's own ⋯ Delete and the
    /// `meeting.delete` tool. `Ok(false)` when there is no such meeting.
    pub fn delete_meeting(&self, session_id: i64) -> Result<bool, String> {
        self.with_conn_mut_reported("meeting.delete", |conn| {
            shogun_memory::session::delete_meeting(conn, session_id)
                .map_err(|_| "meeting delete failed".to_string())
        })
    }

    /// Rewrite part of a meeting's minutes by hand (D1). Validates and trims the edit, then
    /// stores it in the same columns every reader uses and marks the row edited so a later
    /// regeneration leaves it alone. `Ok(false)` when the session does not exist.
    pub fn edit_meeting_recap(
        &self,
        session_id: i64,
        edit: crate::meeting::minutes::MinutesEdit,
    ) -> Result<bool, String> {
        if edit.is_empty() {
            return Err("invalid minutes edit: nothing to change".to_string());
        }
        let edit = edit
            .normalized()
            .map_err(|reason| format!("invalid minutes edit: {reason}"))?;
        if self.meeting_session(session_id).is_none() {
            return Ok(false);
        }
        let decisions_json = edit
            .decisions
            .as_ref()
            .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "[]".to_string()));
        let next_actions_json = edit
            .next_actions
            .as_ref()
            .map(|a| serde_json::to_string(a).unwrap_or_else(|_| "[]".to_string()));
        let now = self.now_ms();
        self.with_conn_reported("meeting.edit_recap", |conn| {
            shogun_memory::meeting_recaps::edit(
                conn,
                session_id,
                &shogun_memory::meeting_recaps::RecapEdit {
                    summary: edit.summary.as_deref(),
                    decisions_json: decisions_json.as_deref(),
                    next_actions_json: next_actions_json.as_deref(),
                },
                now,
            )
            .map(|()| true)
            .map_err(|_| "meeting recap edit failed".to_string())
        })
    }

    /// Hand edited minutes back to the model (D1): the text stays, the next regeneration may
    /// replace it. `Ok(false)` when there was no edit to reset.
    pub fn reset_meeting_recap_edit(&self, session_id: i64) -> Result<bool, String> {
        self.with_conn_reported("meeting.reset_recap_edit", |conn| {
            shogun_memory::meeting_recaps::clear_edit(conn, session_id)
                .map_err(|_| "meeting recap reset failed".to_string())
        })
    }

    /// The stored (model-generated) minutes for an interval (FR-MT-19), if they exist yet.
    ///
    /// Unlike [`Self::meeting_recap`] (the always-available degraded Recap), this returns `None`
    /// until the Batch lane has finished and written a row: the minutes land asynchronously after
    /// the degraded Recap is already on screen, so the panel refetches on the `meeting_recap`
    /// event. The two structured columns come back as raw JSON strings — the wiring layer
    /// deserializes them (never panicking on a bad column).
    pub fn meeting_recap_full(
        &self,
        session_id: i64,
    ) -> Option<shogun_memory::meeting_recaps::StoredRecap> {
        self.try_meeting_recap_full(session_id).ok().flatten()
    }

    pub fn try_meeting_recap_full(
        &self,
        session_id: i64,
    ) -> Result<Option<shogun_memory::meeting_recaps::StoredRecap>, String> {
        self.with_conn_reported("meeting.recap_full", |conn| {
            shogun_memory::meeting_recaps::get(conn, session_id)
                .map_err(|_| "dream meeting-recap read failed".to_string())
        })
    }
}
