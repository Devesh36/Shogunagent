//! Visual Recall frame storage, retention, and search.

use super::*;

/// The window `capture.status` counts Accessibility capture events over (`capture_events_10m`).
const CAPTURE_EVENTS_WINDOW_MS: i64 = 10 * 60 * 1000;

// Stable codes for a refused or failed frame re-scan (issue #782). The desktop command returns
// exactly these words across IPC, and the webview maps them to catalog copy; never prose.
/// The frame id names no saved screen (never existed, or the user deleted it).
pub const RESCAN_ERR_FRAME_MISSING: &str = "frame_missing";
/// The frame was saved but its retention period has passed, so it was removed.
pub const RESCAN_ERR_FRAME_EXPIRED: &str = "frame_expired";
/// Visual recall is Off, so its passively saved screens were removed.
pub const RESCAN_ERR_VISUAL_RECALL_OFF: &str = "visual_recall_off";
/// On-device text recognition produced nothing (or is unavailable on this platform).
pub const RESCAN_ERR_OCR_FAILED: &str = "ocr_failed";
/// The settings, retention purge or the event write failed.
pub const RESCAN_ERR_MEMORY_UNAVAILABLE: &str = "memory_unavailable";

/// Why [`Db::rescan_screen_frame`] produced no new text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRescanError {
    /// The settings file could not be read, or the retention / Off purge failed.
    RetentionCheckFailed,
    /// No saved screen with that id.
    NotFound,
    /// The frame existed but was past the selected retention and has been removed.
    Expired,
    /// Visual recall is Off; its passive frames were removed before the lookup.
    VisualRecallOff,
    /// Text recognition returned nothing.
    OcrFailed,
    /// The linked event's text could not be replaced.
    EventUpdateFailed,
}

impl FrameRescanError {
    /// The stable code a UI face words through its catalog.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => RESCAN_ERR_FRAME_MISSING,
            Self::Expired => RESCAN_ERR_FRAME_EXPIRED,
            Self::VisualRecallOff => RESCAN_ERR_VISUAL_RECALL_OFF,
            Self::OcrFailed => RESCAN_ERR_OCR_FAILED,
            Self::RetentionCheckFailed | Self::EventUpdateFailed => RESCAN_ERR_MEMORY_UNAVAILABLE,
        }
    }
}

/// The new text a frame re-scan stored on the frame's linked event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRescan {
    pub frame_id: i64,
    pub event_id: i64,
    pub ocr_text: String,
}

impl Db {
    /// Apply the Visual recall retention and Off rules from `settings_path` before a frame read
    /// or write: expire frames past the selected retention, and drop passive frames when Off.
    /// Every face (Memory API, desktop) runs this first, so none of them can reach a frame the
    /// user's settings have already ruled out.
    pub fn enforce_visual_recall_retention(
        &self,
        settings_path: &std::path::Path,
    ) -> Result<crate::capture::visual_recall::Settings, String> {
        crate::capture::visual_recall::with_locked_settings(settings_path, |settings| {
            let retention_ms = settings
                .retention
                .retain_ms()
                .unwrap_or(3 * crate::capture::visual_recall::DAY_MS);
            self.purge_screen_frames(retention_ms)?;
            if !settings.enabled {
                self.purge_auto_screen_frames()?;
            }
            Ok(settings.clone())
        })?
    }

    /// Read-only "View OCR text": OCR one saved screen for its line boxes. Retention and the Off
    /// rule run first; nothing is stored and the frame's stored text is untouched.
    pub fn screen_frame_text_lines(
        &self,
        settings_path: &std::path::Path,
        frame_id: i64,
    ) -> Result<Vec<crate::capture::visual_recall::OcrLine>, FrameRescanError> {
        self.screen_frame_text_lines_with(
            settings_path,
            frame_id,
            crate::capture::visual_recall::ocr_jpeg_lines,
        )
    }

    pub(crate) fn screen_frame_text_lines_with(
        &self,
        settings_path: &std::path::Path,
        frame_id: i64,
        ocr: impl FnOnce(&[u8], [f64; 4]) -> Option<Vec<crate::capture::visual_recall::OcrLine>>,
    ) -> Result<Vec<crate::capture::visual_recall::OcrLine>, FrameRescanError> {
        let existed = self.get_screen_frame_summary(frame_id).is_some();
        let settings = self
            .enforce_visual_recall_retention(settings_path)
            .map_err(|_| FrameRescanError::RetentionCheckFailed)?;
        let Some(rec) = self.get_screen_frame(frame_id) else {
            return Err(match (existed, settings.enabled) {
                (false, _) => FrameRescanError::NotFound,
                (true, false) => FrameRescanError::VisualRecallOff,
                (true, true) => FrameRescanError::Expired,
            });
        };
        let summary = &rec.summary;
        let region = summary
            .ocr_region
            .normalized(summary.width, summary.height)
            .ok_or(FrameRescanError::OcrFailed)?;
        ocr(&rec.jpeg, region).ok_or(FrameRescanError::OcrFailed)
    }

    /// Re-run on-device OCR on one saved screen and replace its linked event's OCR text — the
    /// `visual_recall.rescan_frame` L1 local write (issue #410), shared by the Memory API and
    /// the desktop timeline's "Re-read text" action (issue #782, invariant 6). The retention and
    /// Off rules run first, so an expired or Off-purged frame is refused, never re-read.
    pub fn rescan_screen_frame(
        &self,
        settings_path: &std::path::Path,
        frame_id: i64,
    ) -> Result<FrameRescan, FrameRescanError> {
        self.rescan_screen_frame_with(
            settings_path,
            frame_id,
            crate::capture::visual_recall::ocr_jpeg_bytes,
        )
    }

    /// [`Self::rescan_screen_frame`] with the OCR step injected (tests run off macOS).
    ///
    /// Only the focused window's rectangle is read (#969): a full-display preview also shows other
    /// windows, and their text must never land on this frame's app.
    pub(crate) fn rescan_screen_frame_with(
        &self,
        settings_path: &std::path::Path,
        frame_id: i64,
        ocr: impl FnOnce(&[u8], [f64; 4]) -> Option<String>,
    ) -> Result<FrameRescan, FrameRescanError> {
        // Whether the frame was there before the rules ran, so a refusal can say why it is gone.
        let existed = self.get_screen_frame_summary(frame_id).is_some();
        let settings = self
            .enforce_visual_recall_retention(settings_path)
            .map_err(|_| FrameRescanError::RetentionCheckFailed)?;
        let Some(rec) = self.get_screen_frame(frame_id) else {
            return Err(match (existed, settings.enabled) {
                (false, _) => FrameRescanError::NotFound,
                (true, false) => FrameRescanError::VisualRecallOff,
                (true, true) => FrameRescanError::Expired,
            });
        };
        let summary = &rec.summary;
        let region = summary
            .ocr_region
            .normalized(summary.width, summary.height)
            .ok_or(FrameRescanError::OcrFailed)?;
        let text = ocr(&rec.jpeg, region).ok_or(FrameRescanError::OcrFailed)?;
        let stored = match summary.event_id {
            Some(event_id) => self.update_event_ocr_text(event_id, &text).map(|ok| ok.then_some(event_id)),
            // A preview taken while Accessibility text was enough has no OCR event yet.
            None => self.link_rescanned_ocr_event(summary, &text),
        };
        match stored {
            Ok(Some(event_id)) => Ok(FrameRescan {
                frame_id,
                event_id,
                ocr_text: text,
            }),
            Ok(None) | Err(_) => Err(FrameRescanError::EventUpdateFailed),
        }
    }

    /// Store a re-read as the frame's first `screen_ocr` event, dated when the screen was saved,
    /// and link it in the same transaction. `None` when the frame vanished or was linked meanwhile.
    fn link_rescanned_ocr_event(
        &self,
        summary: &shogun_memory::screen_frames::FrameSummary,
        text: &str,
    ) -> Result<Option<i64>, String> {
        let hash = Self::content_hash(text);
        let event = NewEvent {
            ts: summary.created_at_ms,
            source: "screen_ocr",
            kind: "text",
            app_bundle_id: summary.app_bundle_id.as_deref(),
            window_title: summary.window_title.as_deref(),
            content: text,
            content_hash: &hash,
            dwell_ms: 0,
            display_id: summary.display_id,
            window_bounds: None,
        };
        self.with_conn_mut_reported("screen_frames.link_rescan", |conn| {
            let tx = conn.transaction().map_err(|e| format!("begin rescan link: {e}"))?;
            let (event_id, _) = event_log::insert_or_touch(&tx, &event)
                .map_err(|e| format!("store re-read OCR text: {e}"))?;
            if !shogun_memory::screen_frames::link_ocr_event(&tx, summary.id, event_id)
                .map_err(|e| format!("link re-read OCR text: {e}"))?
            {
                return Ok(None);
            }
            tx.commit().map_err(|e| format!("commit rescan link: {e}"))?;
            self.reconcile_hot_locked(conn, event_id);
            Ok(Some(event_id))
        })
    }


    /// Whether the capture poller is paused because website rules require an unavailable URL.
    pub fn capture_paused_url(&self) -> Option<bool> {
        self.capture_status()
            .and_then(|status| status.fresh_pause(shogun_mcp::desktop_heartbeat::now_ms()))
    }

    pub fn capture_status(&self) -> Option<crate::capture::status::Status> {
        self.capture_paused_url
            .lock()
            .ok()
            .and_then(|status| *status)
    }

    /// Record the poll observation time so a stalled poller cannot publish fresh status.
    pub fn set_capture_paused_url(&self, paused: bool) {
        if let Ok(mut status) = self.capture_paused_url.lock() {
            *status = Some(crate::capture::status::Status {
                observed_at_ms: shogun_mcp::desktop_heartbeat::now_ms(),
                paused_url: paused,
            });
        }
    }

    /// Recent on-device screen OCR previews for settings / Full UI (text only, no pixels).
    pub fn screen_ocr_previews(
        &self,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<shogun_memory::event_log::RecentEventPreview> {
        self.with_conn("screen_ocr.previews", |c| {
            shogun_memory::event_log::recent_previews_by_source(
                c,
                "screen_ocr",
                limit,
                excerpt_chars,
            )
        })
        .unwrap_or_default()
    }

    /// How many `screen_ocr` events landed in the last 24 hours.
    pub fn screen_ocr_count_24h(&self) -> i64 {
        let now = self.now_ms();
        let since = now - 24 * 60 * 60 * 1000;
        self.with_conn("screen_ocr.count_24h", |c| {
            shogun_memory::event_log::count_source_in_range(c, "screen_ocr", since, now)
        })
        .unwrap_or(0)
    }

    /// How many Accessibility capture events (`source = "capture"`) landed in the last
    /// [`CAPTURE_EVENTS_WINDOW_MS`], counting both the window's start and the current instant.
    /// A count only — no text, titles or apps. Diagnostic: `capture.status` returns it as
    /// `capture_events_10m` so a pause can be checked without reading content.
    /// Connector sync and screen OCR are other sources and never counted here.
    pub fn capture_event_count_10m(&self) -> i64 {
        let now = self.now_ms();
        let since = now.saturating_sub(CAPTURE_EVENTS_WINDOW_MS);
        self.with_conn("capture.count_10m", |c| {
            // `count_source_in_range` is half-open; `+ 1` keeps an event stamped at `now`.
            shogun_memory::event_log::count_source_in_range(
                c,
                "capture",
                since,
                now.saturating_add(1),
            )
        })
        .unwrap_or(0)
    }

    /// Persist a compressed Visual Recall JPEG, linked to the same poll's `screen_ocr` event (when
    /// OCR ran) and Accessibility capture (when one was stored).
    ///
    /// Explicit exception to invariant 2 (user decision 2026-08-02): frames are local-only,
    /// encrypted at rest with the memory DB, and purged after the selected finite duration.
    #[allow(clippy::too_many_arguments)] // frame metadata is one row; a params struct adds nothing
    pub fn store_screen_frame(
        &self,
        event_id: Option<i64>,
        accessibility_event_id: Option<i64>,
        bundle_id: Option<&str>,
        window_title: Option<&str>,
        display_id: Option<i64>,
        width: u32,
        height: u32,
        ocr_region: shogun_memory::screen_frames::OcrRegion,
        jpeg: &[u8],
    ) -> Option<i64> {
        if bundle_id.is_some_and(|bundle| crate::capture::own_app::is_own_app(bundle, "")) {
            return None;
        }
        let now = self.now_ms();
        self.with_conn("screen_frames.insert", |c| {
            let current_bytes = shogun_memory::screen_frames::stats(c)?.total_bytes;
            if !shogun_memory::retention::capture_allowed(current_bytes) {
                return Ok(None);
            }
            shogun_memory::screen_frames::insert(
                c,
                &shogun_memory::screen_frames::NewFrame {
                    created_at_ms: now,
                    event_id,
                    accessibility_event_id,
                    app_bundle_id: bundle_id,
                    window_title,
                    display_id,
                    width,
                    height,
                    ocr_region,
                    jpeg,
                },
            )
            .map(Some)
        })
        .ok()
        .flatten()
    }

    /// Record the `screen_text` event a just-stored frame produced, so deleting the frame can
    /// delete it by id.
    pub fn link_screen_text(&self, frame_id: i64, event_id: i64) {
        let _ = self.with_conn("screen_frames.link_screen_text", |c| {
            shogun_memory::screen_frames::link_screen_text(c, frame_id, event_id)
        });
    }

    /// Whether automatic capture is paused at the encrypted frame-store ceiling.
    pub fn screen_frame_capture_paused(&self) -> bool {
        self.screen_frame_stats().total_bytes >= shogun_memory::retention::FRAME_CAPTURE_MAX_BYTES
    }

    /// Expire encrypted frames past the selected finite duration. No byte-budget deletion occurs;
    /// new capture pauses at the storage ceiling instead.
    pub fn purge_screen_frames(&self, retention_ms: i64) -> Result<usize, String> {
        use shogun_memory::retention::Policy;
        let now = self.now_ms();
        self.with_conn_mut_reported("screen_frames.purge_expired", |conn| {
            let items = shogun_memory::screen_frames::retention_items(conn)
                .map_err(|e| format!("read frame retention items: {e}"))?;
            let sweep = Policy::frames(retention_ms).sweep(&items, now);
            shogun_memory::screen_frames::delete_ids(conn, &sweep.all())
                .map_err(|e| format!("purge screen frames: {e}"))
        })
    }

    /// Drop auto-capture frames (passive OCR), the OCR text read from them and the durable
    /// `screen_text` copies. Returns frames removed.
    pub fn purge_auto_screen_frames(&self) -> Result<usize, String> {
        self.with_conn_mut_reported("screen_frames.purge_auto", |conn| {
            let (removed_frames, screen_text) = shogun_memory::screen_frames::purge_auto_only(conn)
                .map_err(|e| format!("purge automatic screen frames: {e}"))?;
            let removed = removed_frames.len();
            let mut event_ids: Vec<i64> = removed_frames.into_iter().flatten().collect();
            event_ids.extend(screen_text);
            event_ids.sort_unstable();
            event_ids.dedup();
            // `purge_auto_only` deletes every frame of each returned `screen_ocr` event, so each
            // event is gone by commit: drop it from Hot without re-reading it.
            for event_id in event_ids {
                self.remove_hot_locked(event_id);
            }
            Ok(removed)
        })
    }

    /// Make every `screen_frames` delete fail (or stop failing), the way a locked or damaged
    /// store refuses a purge while reads still work.
    #[cfg(test)]
    pub(crate) fn test_block_screen_frame_deletes(&self, blocked: bool) -> Result<(), String> {
        self.with_conn_reported("test.screen_frames.block_deletes", |conn| {
            let sql = if blocked {
                "CREATE TRIGGER test_block_frame_delete BEFORE DELETE ON screen_frames
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END"
            } else {
                "DROP TRIGGER test_block_frame_delete"
            };
            conn.execute_batch(sql).map_err(|error| error.to_string())
        })
    }

    /// Delete one stored frame and its linked OCR event when no other frame references it.
    pub fn delete_screen_frame(&self, frame_id: i64) -> Result<bool, String> {
        self.with_conn_mut_reported("screen_frames.delete_by_id", |conn| {
            let event_id = conn
                .query_row(
                    "SELECT event_id FROM screen_frames WHERE id = ?1",
                    [frame_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()
                .map_err(|e| format!("read linked screen event: {e}"))?
                .flatten();
            let removed = shogun_memory::screen_frames::delete_by_id_with_copies(conn, frame_id)
                .map_err(|e| format!("delete screen frame: {e}"))?;
            let Some(copies) = removed else {
                return Ok(false);
            };
            for id in event_id.into_iter().chain(copies) {
                self.reconcile_deleted_screen_event(conn, id);
            }
            Ok(true)
        })
    }

    /// Delete only hidden legacy self-captures after an explicit user action.
    pub fn delete_hidden_self_screen_frames(&self) -> Result<usize, String> {
        self.with_conn_mut_reported("screen_frames.delete_hidden_self", |conn| {
            let removed = shogun_memory::screen_frames::delete_hidden_self_frames(conn)
                .map_err(|error| format!("delete hidden self frames: {error}"))?;
            for &event_id in removed.iter().flatten() {
                self.reconcile_deleted_screen_event(conn, event_id);
            }
            Ok(removed.len())
        })
    }

    fn reconcile_deleted_screen_event(&self, conn: &rusqlite::Connection, event_id: i64) {
        match event_log::hot_event_by_id(conn, event_id) {
            Ok(Some(_)) => self.reconcile_hot_locked(conn, event_id),
            Ok(None) => self.remove_hot_locked(event_id),
            Err(error) => {
                self.note_fault("hot.frame_delete", MemoryFault::Query, error.fault_class());
                self.mark_hot_failed();
            }
        }
    }

    /// Aggregate hidden legacy self-capture storage; included in total store accounting.
    pub fn hidden_self_screen_frame_stats(&self) -> shogun_memory::screen_frames::FrameStats {
        self.with_conn("screen_frames.hidden_self_stats", shogun_memory::screen_frames::hidden_self_stats)
            .unwrap_or_default()
    }

    /// Persist refreshed OCR text for a screen frame's linked event (visual recall re-scan).
    pub fn update_event_ocr_text(&self, event_id: i64, text: &str) -> Result<bool, String> {
        let hash = Self::content_hash(text);
        self.with_conn_reported("event_log.update_ocr_text", |conn| {
            let updated =
                shogun_memory::event_log::update_content_and_hash(conn, event_id, text, &hash)
                    .map_err(|e| format!("update event OCR text: {e}"))?;
            if updated {
                self.reconcile_hot_locked(conn, event_id);
            }
            Ok(updated)
        })
    }

    /// List frames in the retention window for UI timeline (newest first).
    pub fn list_screen_frames(
        &self,
        retention_ms: i64,
        limit: usize,
    ) -> Vec<shogun_memory::screen_frames::FrameSummary> {
        let now = self.now_ms();
        let from = now.saturating_sub(retention_ms.max(0));
        self.screen_frames_in_range(from, now, limit)
    }

    /// Page retained visual-recall frame metadata using a stable keyset cursor.
    pub fn screen_frame_timeline_page(
        &self,
        query: shogun_memory::screen_frames::TimelineQuery<'_>,
    ) -> Vec<shogun_memory::screen_frames::FrameSummary> {
        self.with_conn("screen_frames.timeline_page", |conn| {
            shogun_memory::screen_frames::timeline_page(conn, &query)
        })
        .unwrap_or_default()
    }

    /// Count retained visual-recall metadata rows matching timeline filters.
    pub fn screen_frame_timeline_count(
        &self,
        query: &str,
        from_ms: i64,
        to_ms: i64,
        app_bundle_id: Option<&str>,
    ) -> i64 {
        self.with_conn("screen_frames.timeline_count", |conn| {
            shogun_memory::screen_frames::timeline_count(conn, query, from_ms, to_ms, app_bundle_id)
        })
        .unwrap_or(0)
    }

    /// Distinct captured application ids in a retained timeline range.
    pub fn screen_frame_timeline_apps(&self, from_ms: i64, to_ms: i64) -> Vec<String> {
        self.with_conn("screen_frames.timeline_apps", |conn| {
            shogun_memory::screen_frames::timeline_apps(conn, from_ms, to_ms)
        })
        .unwrap_or_default()
    }

    /// Frame-cache stats for settings (count / oldest / bytes — no pixels).
    pub fn screen_frame_stats(&self) -> shogun_memory::screen_frames::FrameStats {
        self.with_conn("screen_frames.stats", shogun_memory::screen_frames::stats)
            .unwrap_or_default()
    }

    /// Stored encrypted JPEG bytes in a time window, used only for aggregate storage estimates.
    pub fn screen_frame_bytes_in_range(&self, from_ms: i64, to_ms: i64) -> i64 {
        self.with_conn("screen_frames.bytes_in_range", |conn| {
            shogun_memory::screen_frames::bytes_in_range(conn, from_ms, to_ms)
        })
        .unwrap_or(0)
    }

    /// Search stored screen frames for a visual-recall question (metadata + OCR excerpt).
    pub fn search_screen_frames(
        &self,
        query: &str,
        retention_ms: i64,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<ScreenFrameRef> {
        self.recall_screen_frames_with_retention(query, retention_ms, limit, excerpt_chars)
    }

    /// Search stored frames in an explicit time window (Memory API / MCP).
    pub fn search_screen_frames_window(
        &self,
        query: &str,
        from_ms: i64,
        to_ms: i64,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<ScreenFrameRef> {
        self.with_conn("screen_frames.search_for_recall", |c| {
            shogun_memory::screen_frames::search_for_recall(
                c,
                query,
                from_ms,
                to_ms,
                limit,
                excerpt_chars,
            )
        })
        .unwrap_or_default()
        .into_iter()
        .map(|h| ScreenFrameRef {
            frame_id: h.frame_id,
            event_id: h.event_id,
            evidence_event_id: h.evidence_event_id,
            ts: h.ts,
            app_bundle_id: h.app_bundle_id,
            window_title: h.window_title,
            width: h.width,
            height: h.height,
            ocr_excerpt: h.ocr_excerpt,
            accessibility_excerpt: h.accessibility_excerpt,
            text_excerpt: h.text_excerpt,
            text_source: h.text_source,
            needs_rescan: h.needs_rescan,
            source: h.source,
        })
        .collect()
    }

    /// Count stored frames whose `created_at_ms` lies in `[from_ms, to_ms]`.
    pub fn screen_frames_count_in_range(&self, from_ms: i64, to_ms: i64) -> i64 {
        self.with_conn("screen_frames.count_in_range", |c| {
            c.query_row(
                "SELECT count(*) FROM screen_frames WHERE created_at_ms >= ?1 AND created_at_ms <= ?2",
                rusqlite::params![from_ms, to_ms],
                |r| r.get(0),
            )
        })
        .unwrap_or(0)
    }

    /// Latest stored frame metadata (no JPEG bytes).
    pub fn latest_screen_frame_summary(
        &self,
    ) -> Option<shogun_memory::screen_frames::FrameSummary> {
        let now = self.now_ms();
        self.screen_frames_in_range(0, now, 1).into_iter().next()
    }

    pub(super) fn recall_screen_frames(
        &self,
        query: &str,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<ScreenFrameRef> {
        let max_ms = i64::from(crate::capture::visual_recall::MAX_CUSTOM_RETENTION_DAYS)
            .saturating_mul(crate::capture::visual_recall::DAY_MS);
        // The sweep that deletes expired frames runs on a timer, and the Off purge can fail; this
        // read must not depend on either having run (issue #782). Off returns nothing, and the
        // window never reaches past the user's retention.
        let retention_ms = match self.visual_recall_settings_path.as_deref() {
            Some(path) => {
                let settings = crate::capture::visual_recall::load_settings(path);
                if !settings.enabled {
                    return Vec::new();
                }
                settings.retention.retain_ms().map_or(max_ms, |ms| ms.min(max_ms))
            }
            None => max_ms,
        };
        self.recall_screen_frames_with_retention(query, retention_ms, limit, excerpt_chars)
    }

    pub(super) fn recall_screen_frames_with_retention(
        &self,
        query: &str,
        retention_ms: i64,
        limit: usize,
        excerpt_chars: usize,
    ) -> Vec<ScreenFrameRef> {
        let now = self.now_ms();
        let local_days = local_day_bounds(now);
        let (from_ms, to_ms) =
            shogun_memory::search::visual_recall_window(query, now, local_days, retention_ms);
        self.with_conn("screen_frames.search_for_recall", |c| {
            shogun_memory::screen_frames::search_for_recall(
                c,
                query,
                from_ms,
                to_ms,
                limit,
                excerpt_chars,
            )
        })
        .unwrap_or_default()
        .into_iter()
        .map(|h| ScreenFrameRef {
            frame_id: h.frame_id,
            event_id: h.event_id,
            evidence_event_id: h.evidence_event_id,
            ts: h.ts,
            app_bundle_id: h.app_bundle_id,
            window_title: h.window_title,
            width: h.width,
            height: h.height,
            ocr_excerpt: h.ocr_excerpt,
            accessibility_excerpt: h.accessibility_excerpt,
            text_excerpt: h.text_excerpt,
            text_source: h.text_source,
            needs_rescan: h.needs_rescan,
            source: h.source,
        })
        .collect()
    }

    pub(super) fn frame_ids_for_events(
        &self,
        event_ids: &[i64],
    ) -> std::collections::HashMap<i64, i64> {
        self.with_conn("screen_frames.frame_ids_for_events", |c| {
            shogun_memory::screen_frames::frame_ids_for_events(c, event_ids)
        })
        .unwrap_or_default()
    }

    /// Fetch frame metadata and OCR without loading the JPEG BLOB.
    pub fn get_screen_frame_summary(
        &self,
        frame_id: i64,
    ) -> Option<shogun_memory::screen_frames::FrameSummary> {
        self.with_conn("screen_frames.get_summary_by_id", |c| {
            shogun_memory::screen_frames::get_summary_by_id(c, frame_id)
        })
        .ok()
        .flatten()
    }

    /// Fetch one stored frame by id (JPEG bytes + metadata). Local-only; never leaves device.
    pub fn get_screen_frame(
        &self,
        frame_id: i64,
    ) -> Option<shogun_memory::screen_frames::FrameRecord> {
        self.with_conn("screen_frames.get_by_id", |c| {
            shogun_memory::screen_frames::get_by_id(c, frame_id)
        })
        .ok()
        .flatten()
    }

    /// Frames captured in a wall-clock window (newest first).
    pub fn screen_frames_in_range(
        &self,
        from_ms: i64,
        to_ms: i64,
        limit: usize,
    ) -> Vec<shogun_memory::screen_frames::FrameSummary> {
        self.with_conn("screen_frames.list_in_range", |c| {
            shogun_memory::screen_frames::list_in_range(c, from_ms, to_ms, limit)
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod url_status_tests {
    use super::*;

    #[test]
    fn local_and_file_readers_share_freshness() -> Result<(), Box<dyn std::error::Error>> {
        use crate::capture::status::{self, Status, MAX_AGE_MS};
        let db = Db::open_in_memory(Arc::new(|| 5_000))?;
        assert_eq!(db.capture_paused_url(), None);
        let now = shogun_mcp::desktop_heartbeat::now_ms();
        let path =
            std::env::temp_dir().join(format!("shogun-url-parity-{}.json", std::process::id()));
        for (observed_at_ms, expected) in [
            (now, Some(true)),
            (now.saturating_sub(MAX_AGE_MS + 1_000), None),
            (now + 60_000, None),
        ] {
            *db.capture_paused_url
                .lock()
                .map_err(|_| "status poisoned")? = Some(Status {
                observed_at_ms,
                paused_url: true,
            });
            status::write(&path, observed_at_ms, true)?;
            assert_eq!(db.capture_paused_url(), expected);
            assert_eq!(
                status::read(&path, shogun_mcp::desktop_heartbeat::now_ms()),
                expected
            );
            assert!(db.capture_status().is_some_and(|last| last.paused_url));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod capture_count_tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn event<'a>(ts: i64, source: &'a str, hash: &'a str) -> NewEvent<'a> {
        NewEvent {
            ts,
            source,
            kind: "text",
            app_bundle_id: None,
            window_title: None,
            content: "evidence",
            content_hash: hash,
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        }
    }

    #[test]
    fn counts_only_capture_events_inside_the_ten_minute_window(
    ) -> Result<(), Box<dyn std::error::Error>> {
        const NOW: i64 = 50_000_000;
        let clock = Arc::new(AtomicI64::new(NOW));
        let tick = clock.clone();
        let db = Db::open_in_memory(Arc::new(move || tick.load(Ordering::SeqCst)))?;
        assert_eq!(db.capture_event_count_10m(), 0, "no events → zero");

        for (ts, source, hash) in [
            (NOW - CAPTURE_EVENTS_WINDOW_MS - 1, "capture", "just-outside"),
            (NOW - CAPTURE_EVENTS_WINDOW_MS, "capture", "window-start"),
            (NOW - 1_000, "capture", "recent"),
            (NOW, "capture", "now"),
            (NOW - 1_000, "screen_ocr", "ocr"),
            (NOW - 1_000, "gmail", "connector"),
        ] {
            db.capture(&event(ts, source, hash)).ok_or("insert failed")?;
        }
        // The window's start and the current instant are both inside; the older capture, the
        // OCR event and the connector event are not.
        assert_eq!(db.capture_event_count_10m(), 3);

        // Nothing new (as while capture is paused): events age out and the count reaches zero.
        clock.store(NOW + 1, Ordering::SeqCst);
        assert_eq!(db.capture_event_count_10m(), 2, "the window-start event aged out");
        clock.store(NOW + CAPTURE_EVENTS_WINDOW_MS + 1, Ordering::SeqCst);
        assert_eq!(db.capture_event_count_10m(), 0);
        Ok(())
    }
}

#[cfg(test)]
mod self_capture_tests {
    use super::*;

    #[test]
    fn hidden_cleanup_reports_bytes_and_preserves_nonself() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 5_000))?;
        let (event, _, _) = db.ingest_screen_ocr(Some("com.example.App"), None, "legacy self OCR", 1, None)
            .ok_or("fixture event missing")?;
        let frame = db.store_screen_frame(Some(event), None, Some("com.example.App"), None, None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"legacy")
            .ok_or("fixture frame missing")?;
        let (visible_event, _, _) = db.ingest_screen_ocr(Some("com.apple.Safari"), None, "normal OCR", 1, None)
            .ok_or("visible event missing")?;
        db.store_screen_frame(Some(visible_event), None, Some("com.apple.Safari"), None, None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"normal")
            .ok_or("visible frame missing")?;
        // Reproduce a legacy row without weakening the new-capture self-app boundary.
        db.with_conn_mut_reported("test.legacy_self", |conn| {
            conn.execute("UPDATE screen_frames SET app_bundle_id = 'shogun' WHERE id = ?1", [frame])
                .map_err(|error| error.to_string())?;
            Ok(())
        })?;
        assert_eq!(db.hidden_self_screen_frame_stats().count, 1);
        assert_eq!(db.hidden_self_screen_frame_stats().total_bytes, 6);
        assert_eq!(db.screen_frame_stats().total_bytes, 12);
        assert_eq!(db.delete_hidden_self_screen_frames()?, 1);
        assert_eq!(db.hidden_self_screen_frame_stats().count, 0);
        assert_eq!(db.screen_frame_stats().total_bytes, 6);
        assert!(db.get_screen_frame_summary(frame).is_none());
        assert_eq!(db.delete_hidden_self_screen_frames()?, 0);
        Ok(())
    }

    #[test]
    fn screen_text_survives_expiry_and_goes_on_off() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 5_000_000))?;
        let (ocr, _, _) = db
            .ingest_screen_ocr(Some("com.apple.Safari"), Some("Doc"), "frame bound zebrafish", 0, None)
            .ok_or("ocr event missing")?;
        db.store_screen_frame(Some(ocr), None, Some("com.apple.Safari"), Some("Doc"), None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"jpeg")
            .ok_or("frame missing")?;
        let (text, _, _) = db
            .ingest_screen_text(Some("com.apple.Safari"), Some("Doc"), "durable narwhal text", 0, None)
            .ok_or("screen_text event missing")?;
        let source = |id: i64| db.event_source(id).map(|(source, _)| source);
        assert_eq!(source(text).as_deref(), Some("screen_text"));

        // Retention expiry deletes the frame, never the durable copy.
        db.purge_screen_frames(0)?; // the clock is fixed: age 0 must already count as expired
        assert_eq!(db.screen_frame_stats().count, 0);
        assert_eq!(source(text).as_deref(), Some("screen_text"));

        // Off deletes it.
        db.purge_auto_screen_frames()?;
        assert!(source(text).is_none());
        Ok(())
    }

    #[test]
    fn deleting_a_frame_deletes_its_durable_text_copy() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 5_000_000))?;
        let (ocr, _, _) = db
            .ingest_screen_ocr(Some("com.apple.Safari"), Some("Doc"), "sensitive axolotl words", 0, None)
            .ok_or("ocr event missing")?;
        let frame = db
            .store_screen_frame(Some(ocr), None, Some("com.apple.Safari"), Some("Doc"), None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, b"jpeg")
            .ok_or("frame missing")?;
        let (copy, _, _) = db
            .ingest_screen_text(Some("com.apple.Safari"), Some("Doc"), "sensitive axolotl words", 0, None)
            .ok_or("copy missing")?;
        // Production links the copy to its frame by id right after storing it (capture_source).
        db.link_screen_text(frame, copy);
        let (other, _, _) = db
            .ingest_screen_text(Some("com.apple.Safari"), Some("Doc"), "unrelated durable heron", 0, None)
            .ok_or("other missing")?;
        assert!(db.delete_screen_frame(frame)?);
        assert!(db.event_source(copy).is_none(), "the frame's text copy goes with it");
        assert!(db.event_source(other).is_some(), "other durable text stays");
        Ok(())
    }

    #[test]
    fn own_app_frames_are_refused_at_storage_boundary() -> Result<(), Box<dyn std::error::Error>> {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 5_000))?;
        for bundle in [
            "com.syogun.shogunai",
            "com.syogun.shogunai.test",
            "dev.shogun.spike",
        ] {
            let (event, _, _) = db.ingest_screen_ocr(Some(bundle), None, bundle, 0, None)
                .ok_or("fixture event not inserted")?;
            assert_eq!(
                db.store_screen_frame(Some(event), None, Some(bundle), None, None, 1, 1, shogun_memory::screen_frames::OcrRegion::WholeFrame, &[1]),
                None
            );
        }
        assert_eq!(db.screen_frame_stats().count, 0);
        Ok(())
    }
}
