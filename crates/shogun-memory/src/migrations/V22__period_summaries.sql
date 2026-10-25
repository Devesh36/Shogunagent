-- Additive hierarchical period summaries. Canonical events remain source of truth; every hour
-- cites events and every coarser level cites child summaries plus selected event evidence.

CREATE TABLE period_summaries (
    id                INTEGER PRIMARY KEY,
    level             TEXT NOT NULL CHECK (level IN ('hour', 'day', 'week', 'month')),
    period_start_ms   INTEGER NOT NULL,
    period_end_ms     INTEGER NOT NULL,
    title             TEXT NOT NULL,
    content           TEXT NOT NULL,
    generation_mode   TEXT NOT NULL CHECK (generation_mode IN ('extractive', 'model')),
    generation_model  TEXT,
    generated         INTEGER NOT NULL CHECK (generated IN (0, 1)),
    content_digest    TEXT NOT NULL,
    is_dirty          INTEGER NOT NULL DEFAULT 0 CHECK (is_dirty IN (0, 1)),
    dirty_at_ms       INTEGER,
    generated_at_ms   INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    CHECK (period_start_ms < period_end_ms),
    CHECK ((generation_mode = 'model' AND generation_model IS NOT NULL AND generated = 1)
        OR (generation_mode = 'extractive' AND generation_model IS NULL AND generated = 0)),
    UNIQUE (level, period_start_ms, period_end_ms)
) STRICT;

CREATE INDEX idx_period_summaries_dirty
    ON period_summaries (is_dirty, level, period_start_ms, period_end_ms);

CREATE TABLE period_summary_shard_runs (
    id              INTEGER PRIMARY KEY,
    level           TEXT NOT NULL CHECK (level IN ('hour', 'day', 'week', 'month')),
    period_start_ms INTEGER NOT NULL,
    period_end_ms   INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'running', 'done', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    failure_class   TEXT,
    updated_at_ms   INTEGER NOT NULL,
    CHECK (period_start_ms < period_end_ms),
    UNIQUE(level, period_start_ms, period_end_ms)
) STRICT;

CREATE INDEX idx_period_summary_shard_runs_resume
    ON period_summary_shard_runs (state, level, period_start_ms, period_end_ms);

CREATE TABLE period_summary_event_provenance (
    summary_id INTEGER NOT NULL REFERENCES period_summaries (id) ON DELETE CASCADE,
    event_id   INTEGER NOT NULL REFERENCES event_log (id) ON DELETE CASCADE,
    weight     REAL NOT NULL DEFAULT 1.0 CHECK (weight > 0.0 AND weight <= 1.0),
    PRIMARY KEY (summary_id, event_id)
) WITHOUT ROWID, STRICT;

CREATE TABLE period_summary_child_provenance (
    summary_id       INTEGER NOT NULL REFERENCES period_summaries (id) ON DELETE CASCADE,
    child_summary_id INTEGER NOT NULL REFERENCES period_summaries (id) ON DELETE CASCADE,
    weight           REAL NOT NULL DEFAULT 1.0 CHECK (weight > 0.0 AND weight <= 1.0),
    CHECK (summary_id <> child_summary_id),
    PRIMARY KEY (summary_id, child_summary_id)
) WITHOUT ROWID, STRICT;

-- Canonical mutation dirties every containing period. This is deliberately bottom-up: hours are
-- selected alongside their ancestors, so repair may regenerate hour/day/week/month in order.
CREATE TRIGGER period_summary_event_log_bu BEFORE UPDATE ON event_log BEGIN
    UPDATE period_summaries SET is_dirty = 1, dirty_at_ms = CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE (old.ts >= period_start_ms AND old.ts < period_end_ms)
        OR (new.ts >= period_start_ms AND new.ts < period_end_ms);
    UPDATE period_summary_shard_runs SET state='pending', failure_class=NULL,
      updated_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE (old.ts >= period_start_ms AND old.ts < period_end_ms)
        OR (new.ts >= period_start_ms AND new.ts < period_end_ms);
END;

CREATE TRIGGER period_summary_event_log_bd BEFORE DELETE ON event_log BEGIN
    UPDATE period_summaries SET is_dirty = 1, dirty_at_ms = CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE old.ts >= period_start_ms AND old.ts < period_end_ms;
    UPDATE period_summary_shard_runs SET state='pending', failure_class=NULL,
      updated_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE old.ts >= period_start_ms AND old.ts < period_end_ms;
END;

CREATE TRIGGER period_summary_event_log_ai AFTER INSERT ON event_log BEGIN
    UPDATE period_summaries SET is_dirty=1,
      dirty_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE new.ts >= period_start_ms AND new.ts < period_end_ms;
    UPDATE period_summary_shard_runs SET state='pending', failure_class=NULL,
      updated_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE new.ts >= period_start_ms AND new.ts < period_end_ms;
END;

-- A changed child invalidates only parents that actually cited it.
CREATE TRIGGER period_summary_child_au
AFTER UPDATE OF content_digest, is_dirty ON period_summaries BEGIN
    UPDATE period_summaries SET is_dirty = 1, dirty_at_ms = new.updated_at_ms
     WHERE id IN (
       SELECT summary_id FROM period_summary_child_provenance WHERE child_summary_id = new.id
     ) AND id <> new.id;
    UPDATE period_summary_shard_runs SET state = 'pending', failure_class = NULL,
      updated_at_ms = new.updated_at_ms
     WHERE EXISTS (
       SELECT 1
         FROM period_summaries parent
         JOIN period_summary_child_provenance provenance ON provenance.summary_id = parent.id
        WHERE provenance.child_summary_id = new.id
          AND parent.id <> new.id
          AND parent.level = period_summary_shard_runs.level
          AND parent.period_start_ms = period_summary_shard_runs.period_start_ms
          AND parent.period_end_ms = period_summary_shard_runs.period_end_ms
     );
END;
