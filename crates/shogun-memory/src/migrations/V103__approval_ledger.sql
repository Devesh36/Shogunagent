-- Rollback: docs/migrations/V103-rollback.md
-- V103 (ShogunAgent E15 follow-up): the principal's approval ledger, durable across restarts.
--
-- `approval_decisions`: one row per remote decision on an approval of this principal — who decided
-- (`decided_by`, V92 actor vocabulary), how (`via`: a signed hosted-surface token or the
-- operator's admin credential), when, at which tier, and what came of carrying it out
-- (`execution`): `none` for a rejection, `pending` → `executing` → `sent` / `failed` for an
-- approval this sidecar executes, `unavailable` when no executor is built in. `reason_class` is a
-- fixed machine label for a failure (`not_connected`, `provider_error`, …), never a provider
-- message or a body. Ids, labels and times only: no recipient, no body.
CREATE TABLE approval_decisions (
    approval_id  INTEGER PRIMARY KEY,
    decision     TEXT    NOT NULL CHECK (decision IN ('approve', 'reject')),
    decided_by   TEXT    NOT NULL,
    decided_at   INTEGER NOT NULL,
    via          TEXT    NOT NULL CHECK (via IN ('token', 'admin')),
    level        TEXT    NOT NULL,
    execution    TEXT    NOT NULL DEFAULT 'none'
                 CHECK (execution IN ('none', 'pending', 'executing', 'sent', 'failed', 'unavailable')),
    reason_class TEXT,
    executed_at  INTEGER
) STRICT;

-- `approval_spent_nonces`: every decision-token nonce this principal's approvals consumed, kept
-- until the token expires, so a token replayed after a restart is still refused.
CREATE TABLE approval_spent_nonces (
    nonce       TEXT    PRIMARY KEY,
    approval_id INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    spent_at    INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX approval_spent_nonces_expiry ON approval_spent_nonces (expires_at);
