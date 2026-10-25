-- Rollback: docs/migrations/V81-rollback.md
-- V81: a second, additive full-text index for the words the trigram index cannot match.
--
-- `event_fts` (V1) is a trigram index: a query term shorter than three characters matches
-- nothing. Japanese is full of two-character words (会議, 予算, 請求, 田中), and English queries
-- carry short tokens (Q3, PR, CI, v2), so those searches returned nothing or matched only by
-- accident (docs/search-eval.md). Owner-approved 2026-10-14: add an index and accept the size.
--
-- `event_grams_fts` holds, per event, the distinct two-character units derived in Rust by
-- `shogun_memory::search` (terms.rs `index_grams`): kanji bigrams with okurigana folded out
-- (打ち合わせ and 打合せ both store 打合), katakana / hangul bigrams, and two-character Latin
-- tokens. The text is space-separated whole tokens, so `unicode61` only splits on the spaces.
-- `detail=none`, `columnsize=0`: only "which events contain this token" is stored — no positions,
-- no per-row lengths (nothing ranks with bm25 on this table) — which is all
-- a single-token match needs and keeps the index small. rowid = event_log.id.
--
-- Population is not done here: deriving grams needs Rust, and a migration must stay fast on a
-- large store. New events are indexed on the write path; `event_grams_pending` queues ids whose
-- grams are missing or stale (every insert by any path, every content/title change), and a
-- bounded maintenance pass drains the queue and back-fills existing history newest-first via
-- `event_grams_backfill`. Search treats the gram lane as additive, so a row not yet indexed is
-- still found through the trigram index exactly as before.
--
-- Additive only: new tables and triggers; no existing table, column, index or trigger changes.

CREATE VIRTUAL TABLE event_grams_fts USING fts5(
    grams,
    detail = 'none',
    columnsize = 0,
    tokenize = 'unicode61 remove_diacritics 0'
);

-- Same SQLCipher leaf-size setting as V57 gave the other FTS indexes.
INSERT INTO event_grams_fts(event_grams_fts, rank) VALUES ('pgsz', 3960);

-- Ids whose grams must be (re)computed. Filled by triggers, drained by Rust.
CREATE TABLE event_grams_pending (
    event_id INTEGER PRIMARY KEY
) STRICT;

-- Back-fill cursor for rows that existed before V81: every id <= next_event_id is still to do,
-- walked downward so recent memory becomes short-word searchable first. 0 = done.
CREATE TABLE event_grams_backfill (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    next_event_id INTEGER NOT NULL
) STRICT;
INSERT INTO event_grams_backfill (id, next_event_id)
VALUES (1, COALESCE((SELECT MAX(id) FROM event_log), 0));

CREATE TRIGGER event_log_grams_ai AFTER INSERT ON event_log BEGIN
    INSERT OR IGNORE INTO event_grams_pending (event_id) VALUES (new.id);
END;

CREATE TRIGGER event_log_grams_ad AFTER DELETE ON event_log BEGIN
    DELETE FROM event_grams_fts WHERE rowid = old.id;
    DELETE FROM event_grams_pending WHERE event_id = old.id;
END;

-- Same column scope as V41's `event_log_au`: a dedup touch (last_seen_at / dwell_ms) fires nothing.
CREATE TRIGGER event_log_grams_au AFTER UPDATE OF content, window_title ON event_log BEGIN
    DELETE FROM event_grams_fts WHERE rowid = old.id;
    INSERT OR IGNORE INTO event_grams_pending (event_id) VALUES (new.id);
END;
