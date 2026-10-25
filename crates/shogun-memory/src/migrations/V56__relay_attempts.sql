-- Rollback: docs/migrations/V56-rollback.md.
-- Content-free attempts for relay callers without a domain-specific job ledger (meeting recap).
-- Pending/unknown work retains its identity across restart. Only confirmed terminal work may
-- create a new attempt for the same fingerprint; no prompt or transcript is retained.
CREATE TABLE relay_attempts (
    fingerprint TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    provider_id TEXT,
    finished INTEGER NOT NULL DEFAULT 0 CHECK (finished IN (0,1))
) STRICT;
