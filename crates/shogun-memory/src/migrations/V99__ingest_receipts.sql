-- Rollback: docs/migrations/V99-rollback.md
-- V99 (ShogunAgent E4 follow-up / E5-T1): the receipts of `memory.ingest`. One row per
-- idempotency key an agent has used, keyed by the SHA-256 of the key (never the key itself, so an
-- odd or long key fits and is not readable back as text): who wrote (`actor`), the thread the
-- items joined (`thread_key`, the event log's derived key), the ids the first call produced
-- (`event_ids`, a JSON array of integers in request order) and when. A repeated key replays this
-- row and appends nothing. Replaces the `memory_markers` workaround E4 shipped with; markers
-- written by that build are read once more as a fallback and never written again.
CREATE TABLE ingest_receipts (
    idempotency_key_hash TEXT    PRIMARY KEY,
    actor                TEXT    NOT NULL,
    thread_key           TEXT,
    event_ids            TEXT    NOT NULL,
    created_at           INTEGER NOT NULL
) STRICT;
CREATE INDEX ingest_receipts_created_at ON ingest_receipts (created_at);
