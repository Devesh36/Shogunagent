-- Durable Dream Cycle identity, frozen input range, and provider Batch lifecycle.
--
-- `job_runs` remains the per-job effect ledger. This cycle-level row exists before any cloud
-- submission, so a restart can recover the exact range and provider handle without creating a
-- second Batch. Expected custom ids are stored separately and atomically with the provider id so
-- result completeness can be checked without re-deriving what was submitted.

CREATE TABLE dream_cycles (
    cycle_id         TEXT    PRIMARY KEY,
    input_from_ts    INTEGER NOT NULL,
    input_to_ts      INTEGER NOT NULL,
    status           TEXT    NOT NULL CHECK (status IN ('running', 'failed', 'abandoned', 'done')),
    batch_stage      TEXT    NOT NULL CHECK (
        batch_stage IN (
            'pending', 'submitting', 'submitted', 'results_ready', 'local', 'abandoned', 'complete'
        )
    ),
    provider_batch_id TEXT,
    run_owner        TEXT,
    run_generation   INTEGER NOT NULL DEFAULT 0 CHECK (run_generation >= 0),
    run_lease_until  INTEGER,
    failure_count    INTEGER NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    coverage_carried INTEGER NOT NULL DEFAULT 0 CHECK (coverage_carried IN (0, 1)),
    snapshot_max_arrival_seq INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK (input_from_ts <= input_to_ts),
    CHECK (
        (batch_stage IN ('submitted', 'results_ready') AND provider_batch_id IS NOT NULL)
        OR (batch_stage NOT IN ('submitted', 'results_ready') AND provider_batch_id IS NULL)
        OR batch_stage IN ('abandoned', 'complete')
    ),
    CHECK (
        (run_owner IS NULL AND run_lease_until IS NULL)
        OR (run_owner IS NOT NULL AND run_lease_until IS NOT NULL)
    )
) STRICT;

CREATE INDEX idx_dream_cycles_unfinished
    ON dream_cycles (status, created_at, cycle_id);

-- event_log uses SQLite rowids without AUTOINCREMENT, so a deleted highest ID can be reused.
-- Give Dream its own monotonic arrival identity; this is what lets a later cycle distinguish a
-- genuinely late backfill from pre-existing history outside the first bounded lookback.
CREATE TABLE dream_event_arrivals (
    arrival_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id    INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_dream_event_arrivals_event
    ON dream_event_arrivals (event_id, arrival_seq DESC);

INSERT INTO dream_event_arrivals (event_id)
SELECT id FROM event_log ORDER BY id;

CREATE TRIGGER dream_event_arrivals_ai AFTER INSERT ON event_log BEGIN
    INSERT INTO dream_event_arrivals (event_id) VALUES (new.id);
END;

CREATE TRIGGER dream_event_arrivals_ad AFTER DELETE ON event_log BEGIN
    DELETE FROM dream_event_arrivals WHERE event_id = old.id;
END;

CREATE TABLE dream_batch_items (
    cycle_id  TEXT NOT NULL REFERENCES dream_cycles(cycle_id) ON DELETE CASCADE,
    custom_id TEXT NOT NULL,
    PRIMARY KEY (cycle_id, custom_id)
) STRICT;

-- The exact local inputs presented to classification. Content is already-sanitized event text,
-- copied locally only while the cycle is unfinished so canonical retention/mutation cannot erase
-- accepted Batch work. Explicit user deletion tombstones the content instead of processing it.
CREATE TABLE dream_cycle_inputs (
    cycle_id    TEXT    NOT NULL REFERENCES dream_cycles(cycle_id) ON DELETE CASCADE,
    arrival_seq INTEGER NOT NULL UNIQUE,
    event_id    INTEGER NOT NULL,
    event_ts    INTEGER NOT NULL,
    source      TEXT    NOT NULL,
    content     TEXT,
    disposition TEXT    NOT NULL DEFAULT 'active'
        CHECK (disposition IN ('active', 'deleted_explicit')),
    CHECK (
        (disposition = 'active' AND content IS NOT NULL)
        OR (disposition = 'deleted_explicit' AND content IS NULL)
    ),
    PRIMARY KEY (cycle_id, event_id)
) STRICT;

-- Ordinary retention/mutation must not invalidate an unfinished snapshot whose event id may be
-- used as state provenance. Explicit deletion first tombstones the snapshot, then these guards no
-- longer apply. Completing/resetting a cycle removes its snapshot and therefore its pins.
CREATE TRIGGER dream_cycle_inputs_pin_delete BEFORE DELETE ON event_log
WHEN EXISTS (
    SELECT 1
    FROM dream_cycle_inputs i
    JOIN dream_cycles c ON c.cycle_id = i.cycle_id
    WHERE i.event_id = old.id AND i.disposition = 'active'
      AND c.status IN ('running', 'failed')
)
BEGIN
    SELECT RAISE(ABORT, 'event pinned by unfinished Dream cycle');
END;

CREATE TRIGGER dream_cycle_inputs_pin_update BEFORE UPDATE OF ts, source, content, content_hash ON event_log
WHEN EXISTS (
    SELECT 1
    FROM dream_cycle_inputs i
    JOIN dream_cycles c ON c.cycle_id = i.cycle_id
    WHERE i.event_id = old.id AND i.disposition = 'active'
      AND c.status IN ('running', 'failed')
)
BEGIN
    SELECT RAISE(ABORT, 'event pinned by unfinished Dream cycle');
END;

-- Durable proof that MorningBrief's externally-visible effect and one-shot notice consumption
-- committed. It survives a later job-ledger Done write failure so retry reuses the exact effect.
CREATE TABLE dream_brief_effects (
    cycle_id  TEXT PRIMARY KEY REFERENCES dream_cycles(cycle_id) ON DELETE CASCADE,
    date      TEXT NOT NULL,
    notice_id INTEGER
) STRICT;

-- Rollback: DROP TRIGGER dream_event_arrivals_ad; DROP TRIGGER dream_event_arrivals_ai;
-- DROP TRIGGER dream_cycle_inputs_pin_update; DROP TRIGGER dream_cycle_inputs_pin_delete;
-- DROP TABLE dream_brief_effects; DROP TABLE dream_cycle_inputs; DROP TABLE dream_event_arrivals;
-- DROP TABLE dream_batch_items; DROP TABLE dream_cycles;
