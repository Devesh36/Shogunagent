-- Link a retained Visual Recall frame to the normal Accessibility capture
-- persisted during the same focus poll. The OCR event remains the frame's
-- owner; the Accessibility link is optional and follows its independent
-- retention policy.
ALTER TABLE screen_frames
    ADD COLUMN accessibility_event_id INTEGER
        REFERENCES event_log (id) ON DELETE SET NULL;

CREATE INDEX idx_screen_frames_accessibility_event
    ON screen_frames (accessibility_event_id)
    WHERE accessibility_event_id IS NOT NULL;
