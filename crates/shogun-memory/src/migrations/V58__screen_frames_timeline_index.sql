-- Rollback: docs/migrations/V58-rollback.md.
-- #727: a covering index for the Visual Recall timeline (`screen_frames::timeline_page` /
-- `timeline_count`). The timeline now evaluates its full-text MATCH once per request and tests each
-- frame's two links against that hit set; with every filtered column here (time window, keyset id,
-- OCR link, Accessibility link, app), the per-frame test never reads the frame row, whose JPEG BLOB
-- spills into overflow pages. `id` sits second so the page's `ORDER BY created_at_ms, id` comes
-- straight off the index.
--
-- Additive only: `idx_screen_frames_created` stays (other readers use it). No data changes.
CREATE INDEX idx_screen_frames_timeline
    ON screen_frames (created_at_ms, id, event_id, accessibility_event_id, app_bundle_id);
