-- Rollback: docs/migrations/V105-rollback.md
-- V105 (ShogunAgent E46): agent payment mandates and the spends recorded against them.
--
-- `payment_mandates`: one row per mandate a human granted an agent (`agent`, V92 actor id): an
-- exact payee or a payee class, the currency (ISO 4217), the per-payment and total caps in minor
-- units, the expiry, the purpose the human read, and whether it is single-use. A row is written
-- only after a human `approve` decision on its grant is recorded in `approval_decisions` (V103):
-- `grant_approval_id` names that decision and `granted_by` the human who made it. Widening or
-- extending replaces the terms under a new decision (`revision` + 1); narrowing replaces them
-- directly; revoking sets `status = 'revoked'` with who and when. Nothing is deleted.
--
-- `payment_spends`: one row per payment an agent asked for, under (actor, idempotency_key), so a
-- retry replays. `authorization` says what stood in for the L3 confirmation: `mandate` (the
-- mandate's grant decision, `approval_id` = `grant_approval_id`) or `approval` (a per-payment
-- human decision). `spend_ref` is the id the payment action carries through the approval queue
-- and the rail. Labels, ids, amounts and times only: no memo text, no rail response body.
--
-- Additive only: two new tables and their indexes; no existing table, column, trigger or index
-- changes.

CREATE TABLE payment_mandates (
    mandate_id            TEXT    PRIMARY KEY,
    agent                 TEXT    NOT NULL,
    payee_kind            TEXT    NOT NULL CHECK (payee_kind IN ('payee', 'class')),
    payee                 TEXT    NOT NULL,
    currency              TEXT    NOT NULL CHECK (length(currency) = 3),
    max_per_payment_minor INTEGER NOT NULL CHECK (max_per_payment_minor > 0),
    total_cap_minor       INTEGER NOT NULL CHECK (total_cap_minor >= max_per_payment_minor),
    expires_at            INTEGER NOT NULL,
    purpose               TEXT    NOT NULL,
    single_use            INTEGER NOT NULL CHECK (single_use IN (0, 1)),
    status                TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    grant_approval_id     INTEGER NOT NULL,
    granted_by            TEXT    NOT NULL,
    granted_at            INTEGER NOT NULL,
    revision              INTEGER NOT NULL DEFAULT 1,
    revoked_by            TEXT,
    revoked_at            INTEGER,
    updated_at            INTEGER NOT NULL
) STRICT;
CREATE INDEX payment_mandates_agent ON payment_mandates (agent, status);

CREATE TABLE payment_spends (
    id              INTEGER PRIMARY KEY,
    spend_ref       TEXT    NOT NULL UNIQUE,
    actor           TEXT    NOT NULL,
    idempotency_key TEXT    NOT NULL,
    mandate_id      TEXT    REFERENCES payment_mandates (mandate_id),
    authorization   TEXT    NOT NULL CHECK (authorization IN ('mandate', 'approval')),
    approval_id     INTEGER,
    decided_by      TEXT,
    payee           TEXT    NOT NULL,
    currency        TEXT    NOT NULL CHECK (length(currency) = 3),
    amount_minor    INTEGER NOT NULL CHECK (amount_minor > 0),
    status          TEXT    NOT NULL CHECK (status IN
                        ('awaiting_approval', 'authorized', 'executing', 'settled', 'failed', 'not_queued')),
    reason_class    TEXT,
    rail            TEXT    NOT NULL,
    rail_ref        TEXT,
    created_at      INTEGER NOT NULL,
    settled_at      INTEGER,
    UNIQUE (actor, idempotency_key)
) STRICT;
CREATE INDEX payment_spends_mandate ON payment_spends (mandate_id, status);
