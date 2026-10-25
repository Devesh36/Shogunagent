-- Rollback: docs/migrations/V87-rollback.md
-- V87: stop three hot paths from reading every screen frame or every captured event.
--
-- Measured on a 600 MB store (69k events, 2.9k frames) with the app's bundled SQLCipher; numbers
-- are pages read and decrypted per call (docs/migrations/V87-rollback.md has the table).
--
-- 1. `event_log::recent_app_scoped_bodies` runs before every capture insert:
--    `WHERE source = ?1 AND app_bundle_id IS ?2 ORDER BY id DESC LIMIT ?3`. Only `(source)` was
--    indexed, so an app not seen recently walked back through every capture row (14,506 pages).
--    `(source, app_bundle_id)` finds the newest rows of that app directly (3 pages).
--
-- 2. `screen_frames::delete_by_id_in` asks whether another frame still references a frame's
--    `screen_text` event. `screen_text_event_id` (V77) had no index, and it sits after the JPEG in
--    the row, so every check read every frame image (12,986 pages; once per frame when Visual
--    recall is turned off). Partial index: most frames never get one.
--
-- 3. `screen_frames::stats` (two calls per stored frame, under the store lock) summed
--    `length(bytes)` over every frame row (1,922 pages and growing toward the 2 GiB ceiling).
--    `screen_frame_sizes` keeps each frame's byte length and timestamp, maintained by triggers,
--    so the totals read a few small pages. An expression index cannot do this: the bundled
--    SQLite (3.39) does not use an index on `length(bytes)` to answer `sum(length(bytes))`.
--
-- Additive only: no existing table, column, index or trigger changes.

CREATE INDEX IF NOT EXISTS idx_event_log_source_app ON event_log (source, app_bundle_id);

CREATE INDEX IF NOT EXISTS idx_screen_frames_screen_text_event
    ON screen_frames (screen_text_event_id) WHERE screen_text_event_id IS NOT NULL;

CREATE TABLE screen_frame_sizes (
    frame_id      INTEGER PRIMARY KEY,
    created_at_ms INTEGER NOT NULL,
    bytes         INTEGER NOT NULL
) STRICT;

-- `length()` of a BLOB comes from the record header; this does not read the images.
INSERT INTO screen_frame_sizes (frame_id, created_at_ms, bytes)
SELECT id, created_at_ms, length(bytes) FROM screen_frames;

CREATE TRIGGER screen_frame_sizes_ai AFTER INSERT ON screen_frames BEGIN
    INSERT INTO screen_frame_sizes (frame_id, created_at_ms, bytes)
    VALUES (new.id, new.created_at_ms, length(new.bytes));
END;

CREATE TRIGGER screen_frame_sizes_ad AFTER DELETE ON screen_frames BEGIN
    DELETE FROM screen_frame_sizes WHERE frame_id = old.id;
END;

CREATE TRIGGER screen_frame_sizes_au AFTER UPDATE OF bytes, created_at_ms ON screen_frames BEGIN
    UPDATE screen_frame_sizes SET created_at_ms = new.created_at_ms, bytes = length(new.bytes)
     WHERE frame_id = new.id;
END;
