-- Notch chat conversations (PR #303 TODO 4). Rust owns thread identity and persistence so
-- "Delete this chat" can remove one conversation without touching the user's other chats.

CREATE TABLE chat_threads (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

-- `ts` (unix ms, stamped by Rust when the message is appended) exists so the windowed privacy
-- delete ("last 1h / 24h") reaches the transcript like every other user table; without it, a
-- question about a since-deleted email would outlive the delete that removed the email.
CREATE TABLE chat_messages (
    thread_id INTEGER NOT NULL REFERENCES chat_threads(id) ON DELETE CASCADE,
    pos       INTEGER NOT NULL,
    role      TEXT NOT NULL CHECK (role IN ('me', 'shogun')),
    text      TEXT NOT NULL,
    citations TEXT,
    ts        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (thread_id, pos)
) STRICT, WITHOUT ROWID;

-- Singleton UI state. NULL means the user deleted the active chat and has not started another.
CREATE TABLE chat_state (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    active_thread_id INTEGER REFERENCES chat_threads(id) ON DELETE SET NULL
) STRICT;

INSERT INTO chat_state (id, active_thread_id) VALUES (1, NULL);
