-- V54: per-night Batch send telemetry (issue #286, OPEN-B2 in docs/batch-relay-design.md).
--
-- One row per (Dream cycle, Batch job): what the deterministic pre-filter
-- (shogun-core dreamcycle::prefilter) did to that night's window and how many input tokens the
-- request bodies were estimated at with and without it. The reduction rate is derived on read
-- from est_tokens_before / est_tokens_after, never stored twice.
--
-- **Counts only. There is no column for captured text, a line, or a hash of one** (CLAUDE.md
-- telemetry rule; same discipline as llm_usage / traceability_log). `line_drops_by_rule` is a
-- JSON object whose keys are fixed rule names from the code and whose values are integers.
--
-- `job` is free text (not an enum CHECK) so a later Batch job can record here without a table
-- rebuild. Today only 'consolidation' writes. No FK to dream_cycles: the counts must survive a
-- cycle row's lifecycle so a representative week stays readable.
--
-- Additive only (new table + index). Rollback: docs/migrations/V54-rollback.md.

CREATE TABLE dream_batch_send_stats (
    cycle_id                     TEXT    NOT NULL,
    job                          TEXT    NOT NULL CHECK (length(job) BETWEEN 1 AND 32),
    recorded_at                  INTEGER NOT NULL,
    items_considered             INTEGER NOT NULL CHECK (items_considered >= 0),
    items_sent                   INTEGER NOT NULL CHECK (items_sent >= 0),
    items_dropped_empty          INTEGER NOT NULL DEFAULT 0 CHECK (items_dropped_empty >= 0),
    items_dropped_duplicate      INTEGER NOT NULL DEFAULT 0 CHECK (items_dropped_duplicate >= 0),
    items_dropped_near_duplicate INTEGER NOT NULL DEFAULT 0 CHECK (items_dropped_near_duplicate >= 0),
    items_dropped_chrome_only    INTEGER NOT NULL DEFAULT 0 CHECK (items_dropped_chrome_only >= 0),
    lines_considered             INTEGER NOT NULL DEFAULT 0 CHECK (lines_considered >= 0),
    lines_dropped                INTEGER NOT NULL DEFAULT 0 CHECK (lines_dropped >= 0),
    line_drops_by_rule           TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(line_drops_by_rule)),
    chars_before                 INTEGER NOT NULL CHECK (chars_before >= 0),
    chars_after                  INTEGER NOT NULL CHECK (chars_after >= 0),
    est_tokens_before            INTEGER NOT NULL CHECK (est_tokens_before >= 0),
    est_tokens_after             INTEGER NOT NULL CHECK (est_tokens_after >= 0),
    PRIMARY KEY (cycle_id, job)
) STRICT;

CREATE INDEX idx_dream_batch_send_stats_recorded ON dream_batch_send_stats (recorded_at);
