-- User-authored memory, with reversible edits. Historical versions never enter search.
-- Rollback: docs/migrations/V76-rollback.md.
CREATE TABLE user_memory_notes (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 text TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision > 0),
 forgotten INTEGER NOT NULL DEFAULT 0 CHECK(forgotten IN (0,1)),
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE user_memory_note_versions (
 note_id INTEGER NOT NULL REFERENCES user_memory_notes(id) ON DELETE CASCADE,
 revision INTEGER NOT NULL,
 text TEXT NOT NULL,
 forgotten INTEGER NOT NULL CHECK(forgotten IN (0,1)),
 changed_at INTEGER NOT NULL,
 PRIMARY KEY(note_id, revision)
) STRICT;
CREATE VIRTUAL TABLE user_memory_notes_fts USING fts5(text, tokenize='trigram');
-- Keep index leaves within one encrypted page, matching the existing search indexes.
INSERT INTO user_memory_notes_fts(user_memory_notes_fts, rank) VALUES ('pgsz', 3960);
CREATE TRIGGER user_memory_notes_ai AFTER INSERT ON user_memory_notes WHEN new.forgotten = 0 BEGIN
 INSERT INTO user_memory_notes_fts(rowid,text) VALUES(new.id,new.text);
END;
CREATE TRIGGER user_memory_notes_au AFTER UPDATE ON user_memory_notes BEGIN
 DELETE FROM user_memory_notes_fts WHERE rowid=old.id;
 INSERT INTO user_memory_notes_fts(rowid,text) SELECT new.id,new.text WHERE new.forgotten=0;
END;
CREATE TRIGGER user_memory_notes_ad AFTER DELETE ON user_memory_notes BEGIN
 DELETE FROM user_memory_notes_fts WHERE rowid=old.id;
END;
