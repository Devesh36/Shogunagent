-- Rollback: docs/migrations/V77-rollback.md
-- V77: the durable `screen_text` event a Visual Recall frame produced (or touched).
--
-- Deleting a frame must delete that OCR copy too. Matching by identical text breaks once
-- near-duplicate collapse rewrites one of the rows, so the frame points at the event by id.
-- NULL means no copy (Accessibility covered the text) or a frame from before V77.
--
-- Additive only: one nullable column, no default, no CHECK. Older binaries name their columns
-- on INSERT and SELECT, so they ignore it.

ALTER TABLE screen_frames ADD COLUMN screen_text_event_id INTEGER;
