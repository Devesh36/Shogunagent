-- Rewrite learning loop (#1066): what each inline rewrite became (kept / edited / reverted / sent),
-- the short style notes mined from that, and which notes each output was shown with.
-- Text stays in the encrypted store only, redacted before the write. Rollback: docs/migrations/V78-rollback.md.
CREATE TABLE rewrite_outcomes (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 app TEXT NOT NULL,
 partner TEXT,
 instruction TEXT NOT NULL DEFAULT '',
 input_digest TEXT NOT NULL,
 input_len INTEGER NOT NULL,
 input_text TEXT NOT NULL DEFAULT '',
 output_text TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 verdict TEXT NOT NULL DEFAULT 'unseen' CHECK(verdict IN ('kept','edited','reverted','unseen')),
 score REAL,
 checked_at INTEGER,
 observed_text TEXT,
 sent_text TEXT,
 sent_at INTEGER,
 sent_similarity REAL,
 sent_diff TEXT,
 mined_at INTEGER,
 mine_attempts INTEGER NOT NULL DEFAULT 0,
 superseded_at INTEGER
) STRICT;
CREATE INDEX rewrite_outcomes_scope ON rewrite_outcomes(app, partner, created_at DESC);
CREATE INDEX rewrite_outcomes_unmined ON rewrite_outcomes(mined_at, verdict);
CREATE TABLE rewrite_style_notes (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 scope TEXT NOT NULL CHECK(scope IN ('surface','partner','global')),
 app TEXT NOT NULL DEFAULT '',
 partner TEXT NOT NULL DEFAULT '',
 kind TEXT NOT NULL CHECK(kind IN ('stated','learned')),
 text TEXT NOT NULL,
 origin_outcomes TEXT NOT NULL DEFAULT '[]',
 impressions INTEGER NOT NULL DEFAULT 0,
 confirmations INTEGER NOT NULL DEFAULT 0,
 rejections INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL,
 invalidated_at INTEGER,
 user_deleted INTEGER NOT NULL DEFAULT 0 CHECK(user_deleted IN (0,1))
) STRICT;
CREATE INDEX rewrite_style_notes_scope ON rewrite_style_notes(app, partner);
CREATE TABLE rewrite_note_impressions (
 outcome_id INTEGER NOT NULL REFERENCES rewrite_outcomes(id) ON DELETE CASCADE,
 note_id INTEGER NOT NULL REFERENCES rewrite_style_notes(id) ON DELETE CASCADE,
 resolved INTEGER NOT NULL DEFAULT 0 CHECK(resolved IN (0,1)),
 PRIMARY KEY(outcome_id, note_id)
) STRICT;
