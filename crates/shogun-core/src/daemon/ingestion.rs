//! Ingestion-facing daemon types and connector boundary.

use super::*;

/// The outcome of ingesting a batch of synced integration items ([`Db::ingest_integration`]):
/// how many were processed, how many were genuinely new (the `IntegrationSynced` bus count), and
/// how many low-confidence state candidates the new items yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestSummary {
    pub processed: usize,
    pub newly_inserted: usize,
    pub candidates: usize,
}

/// The first-layer connector runtime ([`shogun_integrations::ConnectorRuntime`]) hands each synced
/// batch to this sink; the daemon persists it into the event log via [`Db::ingest_integration`].
/// `newly_inserted` is what an `IntegrationSynced` bus event reports (§6.9). This keeps data gravity
/// in the core (invariant 1) — the connector crate never touches the DB.
impl shogun_integrations::IngestSink for Db {
    fn ingest(&self, items: &[shogun_mcp::sync::IngestItem]) -> usize {
        self.ingest_integration(items).newly_inserted
    }

    /// Every item that did not reach the event log counts as refused: a write error, or the
    /// connection lock not being available at all (issue #781).
    fn ingest_counted(&self, items: &[shogun_mcp::sync::IngestItem]) -> shogun_integrations::IngestCount {
        let summary = self.ingest_integration(items);
        shogun_integrations::IngestCount {
            inserted: summary.newly_inserted,
            rejected: items.len().saturating_sub(summary.processed),
        }
    }

    fn cursor(&self, service: shogun_mcp::scope::Service) -> Option<shogun_mcp::sync::SyncCursor> {
        self.sync_cursor(service.source_str())
    }

    fn store_cursor(&self, service: shogun_mcp::scope::Service, cursor: &shogun_mcp::sync::SyncCursor) {
        self.set_sync_cursor(service.source_str(), cursor);
    }

    /// The read-egress boundary (invariant 3 / FR-TR-03, issue #782): one content-free row per
    /// fetch request, written before it is sent, so a read that fails or times out after leaving
    /// the device is still on the ledger.
    fn record_fetch_attempt(&self, service: shogun_mcp::scope::Service, _kind: shogun_integrations::FetchKind) {
        use crate::llm::traceability::TraceabilitySink;
        self.traceability_sink().record(read_trace(service));
    }
}

/// The traceability row for one connector read: Gmail reads go directly to Google REST;
/// Calendar/Drive reads go direct to the vendor's official MCP. It records THAT a read was sent,
/// never what was asked or fetched — empty chunk, zero bytes. The purposes are the ones the
/// desktop poller has always written, so existing ledgers keep one vocabulary.
fn read_trace(service: shogun_mcp::scope::Service) -> crate::llm::traceability::TraceRecord {
    use crate::llm::traceability::{Route, TraceRecord};
    let (route, purpose, third_party) = if service == shogun_mcp::scope::Service::Gmail {
        (Route::Mcp, "gmail_read", false)
    } else {
        (Route::Mcp, "first_layer_read", false)
    };
    TraceRecord::for_chunk(route, purpose, service.source_str(), "", third_party)
}

impl Db {
    /// Record a captured event (capture → memory, FR-CAP-03 dedup-touch). Swallows storage errors
    /// so the capture daemon never crashes on a write hiccup; returns `(id, touched)` on success.
    ///
    /// Swallowed, but not silent (issue #121): a failed write marks memory degraded, which
    /// is what turns the notch indicator amber instead of letting capture quietly stop recording.
    pub fn capture(&self, ev: &NewEvent<'_>) -> Option<(i64, bool)> {
        self.try_capture(ev).ok()
    }

    /// [`Self::capture`] with the failure kept (issue #121) — for callers that must tell a
    /// rejected write from a successful one.
    pub fn try_capture(&self, ev: &NewEvent<'_>) -> MemoryResult<(i64, bool)> {
        self.try_capture_as(ev, &shogun_memory::actor::current_write())
    }

    /// [`Self::try_capture`] as `ctx` (E2-T4): the event records `ctx.actor` and lands in
    /// `ctx.namespace`. The un-suffixed capture writes as the call's current context (the owner
    /// unless a face set one for this call).
    pub fn try_capture_as(
        &self,
        ev: &NewEvent<'_>,
        ctx: &shogun_memory::actor::WriteContext,
    ) -> MemoryResult<(i64, bool)> {
        self.with_conn("capture", |conn| {
            let written = event_log::insert_or_touch_as(conn, ev, ctx)?;
            self.reconcile_hot_locked(conn, written.0);
            Ok::<_, rusqlite::Error>(written)
        })
    }

    /// Capture an event, then — only if it was newly inserted (not a dedup-touch) — run the
    /// first-stage local-rule extraction over its content and persist any low-confidence
    /// commitment / open-loop candidates, each linked to this event (FR-ST-02). Extraction is
    /// skipped on a dedup-touch so repeated identical captures don't multiply candidates.
    ///
    /// Returns `(event_id, touched, candidate_ids)`. Extraction failures are swallowed (the
    /// candidate list comes back empty) so a heuristic hiccup never blocks capture.
    pub fn capture_and_extract(&self, ev: &NewEvent<'_>) -> Option<(i64, bool, Vec<i64>)> {
        let (id, touched) = self.capture(ev)?;
        if touched {
            return Some((id, touched, Vec::new()));
        }
        let candidates = shogun_memory::extract::extract_untrusted(ev.content);
        let now = self.now_ms();
        let ids = self
            .with_conn_mut("extract.persist_candidates", |c| {
                shogun_memory::extract::persist_candidates_deduped(c, id, &candidates, ev.ts, now)
            })
            .unwrap_or_default();
        if !ids.is_empty() {
            // New or newly linked candidates are rows the panel lists (issue #357).
            super::maintenance::notify_state_changed();
        }
        Some((id, touched, ids))
    }

    /// The canonical content hash used across capture and notes. Delegates to the event log's
    /// own definition — the dedup key belongs to the log it locks, and two definitions would
    /// eventually stop colliding.
    pub(super) fn content_hash(text: &str) -> String {
        shogun_memory::event_log::content_hash(text)
    }

    /// Capture a window body with near-duplicate collapse (FR-CAP-03): if `ev.content` is ≥98%
    /// similar to a recent capture body, reuse that body's hash so the event log dedup-touches
    /// instead of appending a near-identical row; otherwise a fresh hash makes a new event. The
    /// `content_hash` on the passed `ev` is ignored — this method decides it. Returns `(id, touched)`.
    pub fn capture_collapsed(&self, ev: &NewEvent<'_>) -> Option<(i64, bool)> {
        let recents =
            self.recent_bodies_for_collapse(ev.source, ev.app_bundle_id, RECENT_DEDUP_WINDOW);
        let recent_refs: Vec<Recent<'_>> = recents
            .iter()
            .map(|(h, c)| Recent {
                content_hash: h,
                content: c,
            })
            .collect();
        // Collapse against the stored (stripped) form, not the camouflaged bytes.
        let visible = shogun_redact::strip_hidden(ev.content);
        let decision = decide_hash(visible.text.as_ref(), &recent_refs, Self::content_hash);
        let collapsed = NewEvent {
            content_hash: decision.hash(),
            ..ev.clone()
        };
        self.capture(&collapsed)
    }

    /// Recent event bodies `(hash, content)` newest-first to compare against for the near-dup
    /// collapse (FR-CAP-03). How far the candidates are scoped — source, plus app for the sources
    /// that carry one — is the event log's call, so no caller here can widen it by accident.
    pub(super) fn recent_bodies_for_collapse(
        &self,
        source: &str,
        app_bundle_id: Option<&str>,
        limit: usize,
    ) -> Vec<(String, String)> {
        self.with_conn("event_log.recent_bodies_for_collapse", |c| {
            event_log::recent_bodies_for_collapse(c, source, app_bundle_id, limit)
        })
        .unwrap_or_default()
    }

    /// Recent user notes (`source = user`), newest-first, for explicit Memory API context reads.
    pub fn recent_user_notes(&self, limit: usize) -> Vec<String> {
        // Notes carry no app attribution, so the log's scoping entry point returns the
        // source-scoped rows for them. Going through it keeps one caller-facing door.
        self.recent_bodies_for_collapse("user", None, limit)
            .into_iter()
            .map(|(_hash, content)| content)
            .collect()
    }

    /// Recent durable text activity for explicit Memory API context reads.
    ///
    /// Query-free context cannot rank evidence by relevance, so this returns a small newest-first
    /// tail from the durable text sources that already feed memory. Exact duplicate excerpts are
    /// collapsed because accessibility capture and screen OCR can observe the same window.
    pub(crate) fn recent_context_previews(
        &self,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<(String, shogun_memory::event_log::RecentEventPreview)> {
        const SOURCES: [&str; 7] = [
            "capture",
            "screen_ocr",
            "screen_text",
            "ai_session",
            "meeting",
            "gmail",
            "gcal",
        ];

        if limit == 0 {
            return Vec::new();
        }

        if let Some(snapshot) = self.hot_context_candidates(&SOURCES, limit, excerpt_chars) {
            let generation = snapshot.generation();
            let lossless = snapshot
                .candidates()
                .iter()
                .all(|candidate| !candidate.lossy);
            let durable = self
                .with_conn("memory.context_preview_ids", |conn| {
                    let ids = SOURCES
                        .iter()
                        .map(|source| {
                            shogun_memory::event_log::recent_ids_by_source(conn, source, limit)
                                .map(|ids| ((*source).to_string(), ids))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    // Meeting rows are rewritten in place when a transcript is corrected or a line
                    // excluded, possibly by another process (MCP/REST) whose edit this Hot tier never
                    // saw. Same IDs, new text: read their text from storage, under the same lock.
                    let meeting = shogun_memory::event_log::recent_previews_by_source(
                        conn,
                        shogun_memory::meeting_index::SOURCE,
                        limit,
                        excerpt_chars,
                    )?;
                    Ok::<_, rusqlite::Error>((ids, meeting))
                })
                .ok();
            if let Some((ids, meeting)) = durable {
                if lossless
                    && ids.as_slice() == snapshot.per_source_ids()
                    && self.hot_generation() == Some(generation)
                {
                    let mut previews: Vec<_> = snapshot
                        .into_candidates()
                        .into_iter()
                        .filter(|candidate| candidate.source != shogun_memory::meeting_index::SOURCE)
                        .map(|candidate| (candidate.source, candidate.preview))
                        .collect();
                    previews.extend(meeting.into_iter().map(|row| {
                        (shogun_memory::meeting_index::SOURCE.to_string(), row)
                    }));
                    return Self::merge_context_candidates(previews, limit);
                }
            }
        }

        self.with_conn("memory.context_previews", |conn| {
            let mut previews = Vec::with_capacity(SOURCES.len() * limit);
            for source in SOURCES {
                let rows = shogun_memory::event_log::recent_previews_by_source(
                    conn,
                    source,
                    limit,
                    excerpt_chars,
                )?;
                previews.extend(rows.into_iter().map(|row| (source.to_string(), row)));
            }

            Ok::<_, rusqlite::Error>(Self::merge_context_candidates(previews, limit))
        })
        .unwrap_or_default()
    }

    pub(super) fn merge_context_candidates(
        mut previews: Vec<(String, shogun_memory::event_log::RecentEventPreview)>,
        limit: usize,
    ) -> Vec<(String, shogun_memory::event_log::RecentEventPreview)> {
        use std::collections::HashSet;

        previews.sort_by(|(_, a), (_, b)| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
        let mut seen = HashSet::new();
        previews.retain(|(_, row)| {
            let normalized = row
                .excerpt
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            !normalized.is_empty() && seen.insert(normalized)
        });
        previews.truncate(limit);
        previews
    }

    /// Ingest a window capture end-to-end (the real capture path, FR-CAP-01/03): near-dup
    /// collapse decides the hash, the event is inserted-or-touched, and on a *new* insert the
    /// first-stage local-rule extraction runs over the text. `dwell_ms` accumulates on a touch.
    /// Source is `capture`. Returns `(id, touched, candidate_ids)`; `None` only on a lock failure.
    pub fn ingest_capture(
        &self,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
    ) -> Option<(i64, bool, Vec<i64>)> {
        self.ingest_text_event("capture", bundle_id, window_title, text, dwell_ms, None)
    }

    /// Persist a complete AX checkpoint with exact-content deduplication.
    ///
    /// Unlike [`Self::ingest_capture`], this does not collapse a small edit onto older text. The
    /// AX revision planner has already removed unchanged lines, so every supplied byte is evidence
    /// that must remain searchable.
    pub fn ingest_capture_checkpoint(
        &self,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
    ) -> Option<(i64, bool, Vec<i64>)> {
        self.ingest_exact_capture_event("text", bundle_id, window_title, text, dwell_ms)
    }

    /// Persist only newly visible lines from one small AX revision.
    pub fn ingest_capture_delta(
        &self,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
    ) -> Option<(i64, bool, Vec<i64>)> {
        self.ingest_exact_capture_event("text_delta", bundle_id, window_title, text, dwell_ms)
    }

    pub(super) fn ingest_exact_capture_event(
        &self,
        kind: &'static str,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
    ) -> Option<(i64, bool, Vec<i64>)> {
        let hash = Self::content_hash(text);
        let ev = NewEvent {
            ts: self.now_ms(),
            source: "capture",
            kind,
            app_bundle_id: bundle_id,
            window_title,
            content: text,
            content_hash: &hash,
            dwell_ms,
            display_id: None,
            window_bounds: None,
        };
        self.capture_and_extract(&ev)
    }

    /// Touch a known AX checkpoint/delta when the complete visible line set did not change.
    pub fn touch_capture_event(&self, event_id: i64, dwell_ms: i64) -> bool {
        let now = self.now_ms();
        self.with_conn("capture.touch", |conn| {
            let touched = event_log::touch(conn, event_id, now, dwell_ms)?;
            if touched {
                self.reconcile_hot_locked(conn, event_id);
            }
            Ok::<_, rusqlite::Error>(touched)
        })
        .unwrap_or(false)
    }

    /// Ingest on-device screen OCR text (issue #107). Source is `screen_ocr`; only the extracted
    /// string + provenance reach this method. Optional JPEG frames are stored separately via
    /// [`store_screen_frame`] (finite age retention — explicit invariant-2 exception).
    pub fn ingest_screen_ocr(
        &self,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
        display_id: Option<i64>,
    ) -> Option<(i64, bool, Vec<i64>)> {
        self.ingest_text_event(
            "screen_ocr",
            bundle_id,
            window_title,
            text,
            dwell_ms,
            display_id,
        )
    }

    /// Durable on-screen text OCR read where Accessibility missed it. Unlike `screen_ocr` it is
    /// linked to no frame, so frame expiry never deletes it; Visual recall Off does. Everything
    /// else (redaction, near-dup collapse, FTS) is the ordinary `capture`-style ingest.
    pub fn ingest_screen_text(
        &self,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
        display_id: Option<i64>,
    ) -> Option<(i64, bool, Vec<i64>)> {
        self.ingest_text_event(
            "screen_text",
            bundle_id,
            window_title,
            text,
            dwell_ms,
            display_id,
        )
    }

    pub(super) fn ingest_text_event(
        &self,
        source: &'static str,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        text: &str,
        dwell_ms: i64,
        display_id: Option<i64>,
    ) -> Option<(i64, bool, Vec<i64>)> {
        let ev = NewEvent {
            ts: self.now_ms(),
            source,
            kind: "text",
            app_bundle_id: bundle_id,
            window_title,
            content: text,
            content_hash: "", // ignored — capture_collapsed decides it
            dwell_ms,
            display_id,
            window_bounds: None,
        };
        let (id, touched) = self.capture_collapsed(&ev)?;
        if touched {
            return Some((id, touched, Vec::new()));
        }
        let candidates = shogun_memory::extract::extract_untrusted(text);
        let now = self.now_ms();
        // Deduped: a cue already known as an unresolved row links to that row (one more
        // provenance link) instead of inserting a twin the state list would show twice.
        let ids = self
            .with_conn_mut("extract.persist_candidates", |c| {
                shogun_memory::extract::persist_candidates_deduped(c, id, &candidates, ev.ts, now)
            })
            .unwrap_or_default();
        if !ids.is_empty() {
            // New or newly linked candidates are rows the panel lists (issue #357).
            super::maintenance::notify_state_changed();
        }
        Some((id, touched, ids))
    }

    /// Ingest a batch of synced integration items into the event log (§6.9:
    /// `integration.synced` → event log → search/Fusion). Each record is appended under its own
    /// `source` tag (`gmail` / `gcal` / …) so a synced email is a first-class event next to a
    /// captured window (FR-INT-05); the item's own timestamp is preserved. Dedup is per-source
    /// (FR-CAP-03): re-syncing the same item touches `last_seen_at` rather than duplicating it.
    ///
    /// On a **new** insert (not a dedup-touch) the first-stage local-rule extraction runs
    /// over **prose** bodies (mail, chat, docs), so a commitment stated in an email flows into the
    /// state tables at low confidence (≤ [`shogun_memory::extract::LOCAL_RULE_MAX_CONFIDENCE`])
    /// linked to the ingested event (FR-ST-02). Calendar metadata (title / timestamp / event ID)
    /// is emitted later as [`SourceKind::Structured`], while its description remains evidence;
    /// neither should become a guessed 0.4 speech act. Extraction is skipped on a touch so a re-sync never
    /// multiplies candidates.
    ///
    /// The caller ([`crate::service_gate`]) has already authorized the sync; this method only
    /// persists. On completion, a batch that inserted ≥ 1 **new** item publishes
    /// [`crate::bus::BusEvent::IntegrationSynced`] per source on the bus (§6.9, design §2.2) so
    /// subscribers — e.g. the reply-context invalidator — learn that memory just changed; a
    /// dedup-only or empty batch publishes nothing (nothing changed). Returns a zeroed summary on
    /// a lock failure (never panics).
    pub fn ingest_integration(&self, items: &[shogun_mcp::sync::IngestItem]) -> IngestSummary {
        self.ingest_integration_as(items, None)
    }

    /// [`Self::ingest_integration`] with the writer named (E2-T4). `None` attributes each event
    /// to its connector (`connector:<source>`, in the world); `Some(ctx)` writes every item as
    /// `ctx` — the `memory.ingest` face, whose caller is an agent.
    pub fn ingest_integration_as(
        &self,
        items: &[shogun_mcp::sync::IngestItem],
        ctx: Option<&shogun_memory::actor::WriteContext>,
    ) -> IngestSummary {
        let now = self.now_ms();
        let mut summary = IngestSummary::default();
        // Per-source newly-inserted counts for the bus. A batch normally carries one service, so a
        // tiny vec beats a map.
        let mut synced: Vec<(&'static str, u64)> = Vec::new();
        // Mail triage (issue #319): threads (re)classified this batch and the live needs-reply
        // backlog, published as one `MailTriaged` after the lock is released.
        let mut triaged: u64 = 0;
        let mut needs_reply_now: Option<u64> = None;
        // The master switch (issue #319, V34). Read once per batch rather than per item: it is a
        // user setting, not a per-message property, and re-reading it inside the loop would put a
        // query between every two messages of a 200-item sync.
        //
        // Off means "stop sorting", not "stop syncing": the mail still reaches memory and search
        // below, and existing classifications are left standing so turning it back on returns the
        // user to the inbox they had.
        let triage_on = self.mail_triage_enabled();
        {
            let Some(mut guard) = self.lock_or_note("ingest.items") else {
                return IngestSummary::default();
            };
            // Rejected writes are counted, not logged per item: a batch that fails wholesale
            // would otherwise write one log line per row. One line, and the health signal.
            let mut rejected = 0usize;
            // The event itself is stored even when its occurrence row is not; counted apart so the
            // "rejected of processed" line keeps meaning event-log writes.
            let mut calendar_rejected = 0usize;
            for it in items {
                // A Google deletion tombstone has only its id (no title, no start): it cancels the
                // occurrence and is not memory, so it never becomes an event row.
                if it.calendar.as_ref().is_some_and(|cal| {
                    it.source == "gcal" && cal.cancelled && cal.start_ms == 0 && it.title.is_empty()
                }) {
                    if shogun_memory::calendar::cancel(&guard, it.source, &it.external_id, now).is_err() {
                        calendar_rejected += 1;
                    }
                    continue;
                }
                let hash = Self::content_hash(&it.body);
                let ev = NewEvent {
                    ts: it.ts_ms,
                    source: it.source,
                    kind: it.kind,
                    app_bundle_id: None,
                    window_title: (!it.title.is_empty()).then_some(it.title.as_str()),
                    content: &it.body,
                    content_hash: &hash,
                    dwell_ms: 0,
                    display_id: None,
                    window_bounds: None,
                };
                // A service that knows its conversation id (Gmail thread id) keys the thread on
                // it (FR-MEM-40 `gmail=threadId`), not on the subject.
                let native = (!it.external_id.trim().is_empty()).then_some(it.external_id.as_str());
                let item_ctx = ctx.cloned().unwrap_or_else(|| {
                    shogun_memory::actor::WriteContext::world(shogun_memory::actor::Actor::connector(it.source))
                });
                let Ok((id, touched)) = event_log::insert_or_touch_with_thread_as(&guard, &ev, native, &item_ctx) else {
                    rejected += 1;
                    continue;
                };
                self.reconcile_hot_locked(&guard, id);
                summary.processed += 1;
                // Mail metadata + Stage L triage run on every Gmail item, touched or not: a
                // re-seen snapshot can still carry a label change (INBOX gone) or age a thread
                // into `waiting_on_them`.
                if let Some(meta) = it.mail.as_ref().filter(|_| it.source == "gmail" && triage_on) {
                    if self.triage_mail_locked(&mut guard, id, &it.title, &it.body, it.ts_ms, meta, now) {
                        triaged += 1;
                    }
                }
                // Calendar events are also held as state (FR-MT-06), on every sync, touched or
                // not: a reschedule or a cancellation keeps the description (and so the event-log
                // row) identical, and it is exactly the change the occurrence has to follow.
                if let Some(cal) = it.calendar.as_ref().filter(|_| it.source == "gcal") {
                    let attendees: Vec<shogun_memory::calendar::Attendee> = cal
                        .attendees
                        .iter()
                        .map(|a| shogun_memory::calendar::Attendee {
                            email: a.email.clone(),
                            name: a.name.clone(),
                            is_self: a.is_self,
                            response: a.response.clone(),
                        })
                        .collect();
                    let occurrence = shogun_memory::calendar::NewOccurrence {
                        source: it.source,
                        external_id: &it.external_id,
                        title: (!it.title.is_empty()).then_some(it.title.as_str()),
                        start_at: cal.start_ms,
                        end_at: cal.end_ms,
                        all_day: cal.all_day,
                        location: cal.location.as_deref(),
                        attendees: &attendees,
                        cancelled: cal.cancelled,
                        event_id: Some(id),
                    };
                    if shogun_memory::calendar::upsert(&guard, &occurrence, now).is_err() {
                        calendar_rejected += 1;
                    }
                }
                if touched {
                    continue;
                }
                summary.newly_inserted += 1;
                match synced.iter_mut().find(|(s, _)| *s == it.source) {
                    Some((_, n)) => *n += 1,
                    None => synced.push((it.source, 1)),
                }
                // Calendar descriptions stay searchable evidence, not speech acts (issue #35).
                if extracts_untrusted_body(it.source) {
                    let candidates = shogun_memory::extract::extract_untrusted(&it.body);
                    if !candidates.is_empty() {
                        let ids = shogun_memory::extract::persist_candidates_deduped(
                            &mut guard,
                            id,
                            &candidates,
                            it.ts_ms,
                            now,
                        )
                        .unwrap_or_default();
                        summary.candidates += ids.len();
                    }
                }
            }
            if triaged > 0 {
                needs_reply_now = shogun_memory::mail_triage::needs_reply_open_count(&guard).ok().map(|n| n.max(0) as u64);
            }
            drop(guard);
            if calendar_rejected > 0 {
                self.note_fault("ingest.calendar", MemoryFault::Query, "write_rejected");
            }
            if rejected > 0 {
                self.note_fault("ingest.items", MemoryFault::Query, "write_rejected");
                crate::elog!(
                    "[memory] ingest.items rejected {rejected} of {} item(s)",
                    summary.processed + rejected
                );
            } else {
                self.health.record_success();
            }
        }
        // Publish after the DB lock is released. Non-blocking (AR-07); carries only the source tag
        // and a count — never item content.
        if let Some(bus) = &self.bus {
            for (source, count) in synced {
                bus.publish(crate::bus::BusEvent::IntegrationSynced { source, count });
            }
            if let Some(needs_reply) = needs_reply_now {
                bus.publish(crate::bus::BusEvent::MailTriaged { new: triaged, needs_reply });
            }
        }
        summary
    }

    /// Append a user note to the event log (`memory.append_note`, L1). Source `user`, kind `note`;
    /// content-hashed for dedup like any event. Returns the row id (or `None` on write failure).
    pub fn append_note(&self, text: &str) -> Option<i64> {
        self.append_note_as(text, &shogun_memory::actor::current_write())
    }

    /// [`Self::append_note`] as `ctx` (E2-T4).
    pub fn append_note_as(&self, text: &str, ctx: &shogun_memory::actor::WriteContext) -> Option<i64> {
        let hash = Self::content_hash(text);
        let ev = NewEvent {
            ts: self.now_ms(),
            source: "user",
            kind: "note",
            app_bundle_id: None,
            window_title: None,
            content: text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        };
        self.try_capture_as(&ev, ctx).ok().map(|(id, _)| id)
    }
}
