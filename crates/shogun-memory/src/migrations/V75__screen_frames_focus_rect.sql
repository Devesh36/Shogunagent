-- Rollback: docs/migrations/V75-rollback.md
-- V75: where the focused window sits inside a full-display Visual Recall preview (issue #969).
--
-- Previews now show the whole display the focused window is on, with excluded windows filtered
-- out at capture. OCR text still belongs to the focused window only, so a later "Re-read text"
-- must read just that rectangle, never the rest of the screen. The rectangle is in the stored
-- JPEG's pixels. All four NULL means a frame from before V75, whose JPEG is the focused window.
--
-- Additive only: four nullable columns, no default, no CHECK. Older binaries name their columns
-- on INSERT and SELECT, so they ignore them.

ALTER TABLE screen_frames ADD COLUMN focus_x INTEGER;
ALTER TABLE screen_frames ADD COLUMN focus_y INTEGER;
ALTER TABLE screen_frames ADD COLUMN focus_width INTEGER;
ALTER TABLE screen_frames ADD COLUMN focus_height INTEGER;
