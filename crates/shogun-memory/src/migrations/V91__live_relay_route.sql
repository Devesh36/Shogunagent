-- Rollback: docs/migrations/V91-rollback.md
-- non-additive-ok: preserve every column, row and index while extending route CHECK for #1101.
-- SQLite cannot extend CHECK in place. Run inside refinery's migration transaction.
CREATE TABLE traceability_log_live (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    route       TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'live_relay', 'mcp', 'composio', 'billing', 'asr', 'local_agent', 'support', 'web')),
    purpose     TEXT    NOT NULL,
    destination TEXT    NOT NULL,
    chunk_bytes INTEGER NOT NULL,
    chunk_xxh64 TEXT    NOT NULL,
    third_party INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'egress' CHECK (kind IN ('egress', 'retry', 'fell_back')),
    url TEXT
) STRICT;

INSERT INTO traceability_log_live SELECT * FROM traceability_log;
DROP TABLE traceability_log;
ALTER TABLE traceability_log_live RENAME TO traceability_log;
CREATE INDEX idx_traceability_ts ON traceability_log (ts);
CREATE TABLE llm_usage_live (
    id                 INTEGER PRIMARY KEY,
    ts                 INTEGER NOT NULL,
    route              TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'live_relay', 'mcp', 'composio', 'billing', 'asr', 'local_agent')),
    prompt_id          TEXT    NOT NULL,
    prompt_version     INTEGER NOT NULL,
    model              TEXT    NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    estimated          INTEGER NOT NULL DEFAULT 0,
    first_token_ms     INTEGER,
    total_ms           INTEGER NOT NULL DEFAULT 0,
    outcome            TEXT    NOT NULL CHECK (outcome IN ('ok', 'error', 'rate_limited', 'unauthorized', 'quota', 'parse'))
) STRICT;

INSERT INTO llm_usage_live SELECT * FROM llm_usage;
DROP TABLE llm_usage;
ALTER TABLE llm_usage_live RENAME TO llm_usage;
CREATE INDEX idx_llm_usage_ts ON llm_usage (ts);
CREATE INDEX idx_llm_usage_prompt ON llm_usage (prompt_id, ts);
