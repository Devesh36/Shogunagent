-- In-person meetings (issue #262, FR-MC-01): a session started by hand for a room, not a call.
--
-- Online, the capture channels decide the speaker — the microphone is `me`, the system tap is
-- everyone else. In a room every voice is on the microphone, so the session records that it was
-- in person: the audio lane skips the system tap and lets diarization label the mic instead of
-- stamping every line `me`, and the list / recap can say "In person".
--
-- Additive: one nullable-by-default column. Old binaries ignore it; old rows read as 0 (online).
ALTER TABLE sessions ADD COLUMN in_person INTEGER NOT NULL DEFAULT 0;
