-- Rollback: docs/migrations/V67-rollback.md
-- V67: shared-service observability on the device (issue #29 §4-6 / §5,
-- docs/specs/issue-29-shared-service-capacity.md).
--
-- 1. traceability_log.kind — what a row records. Every existing row is an 'egress' (a chunk that
--    left the device), which is the default, so old rows keep their meaning untouched. Two new
--    markers ride on a shared-service route: 'retry' (the service asked the device to wait and it
--    will ask again) and 'fell_back' (the device gave the service up tonight and used the local
--    lane). Markers carry no chunk: chunk_bytes = 0 and chunk_xxh64 = ''. Egress counts
--    (FR-DC-06 "chunks sent") read only kind = 'egress'.
--
-- 2. shared_service_rtt — one row per shared-service HTTP round trip: route, milliseconds,
--    status. **No URL, no id, no content column** (same discipline as llm_usage /
--    traceability_log). `shogun metrics` reads the p95 per route from it, separately from the
--    NFR-SLO rows.
--
-- Additive only (ADD COLUMN with a constant default + one new table + one index). A future
-- traceability_log rebuild (the V18 / V46 pattern for extending the route CHECK) must carry the
-- kind column across. Rollback: docs/migrations/V67-rollback.md.

ALTER TABLE traceability_log
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'egress'
    CHECK (kind IN ('egress', 'retry', 'fell_back'));

CREATE TABLE shared_service_rtt (
    id      INTEGER PRIMARY KEY,
    ts      INTEGER NOT NULL,
    route   TEXT    NOT NULL CHECK (route IN ('batch_api', 'batch_relay', 'messages_api', 'mcp', 'composio', 'billing', 'asr', 'local_agent', 'support', 'web')),
    rtt_ms  INTEGER NOT NULL CHECK (rtt_ms >= 0),
    status  INTEGER NOT NULL CHECK (status BETWEEN 0 AND 999)
) STRICT;

CREATE INDEX idx_shared_service_rtt_ts ON shared_service_rtt (ts);
