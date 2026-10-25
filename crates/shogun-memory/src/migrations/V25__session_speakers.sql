-- Real names for a meeting's speakers (FR-MT-15's "next stage"). Additive: one new table.
--
-- `transcript_segments.speaker` stores capture-derived keys only: 'me', 'other', or a diarized
-- slot 'other:N'. A slot is a per-session label, not an identity — this table is where a slot
-- becomes a person's name, and only through a user-confirmed assignment (source='user') or a
-- candidate the user accepted from calendar attendees / people (source='calendar' / 'people').
-- Nothing writes here on inference alone: an unnamed slot stays unnamed (never guessed).
--
-- One name per (session, speaker key): re-assigning replaces, like a document, not a log.

CREATE TABLE session_speakers (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER NOT NULL REFERENCES sessions (id),
    -- 'me' | 'other' | 'other:N' — the exact key transcript_segments stores.
    speaker_key  TEXT    NOT NULL,
    display_name TEXT    NOT NULL,
    -- Where the accepted name came from: 'user' (typed), 'calendar', 'people'.
    source       TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (session_id, speaker_key)
) STRICT;
