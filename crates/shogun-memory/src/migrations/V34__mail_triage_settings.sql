-- V34: the mail triage master switch (issue #319, docs/specs/issue-319-mail-triage-spec.md §9).
--
-- V33 shipped triage with per-label enable/disable and no way to stop the whole thing short of
-- disconnecting Gmail — which also stops the mail reaching memory at all, a much larger thing
-- than "stop sorting my inbox". This is that missing switch.
--
-- One row, enforced by the primary-key CHECK: a settings table that can hold two rows is a
-- settings table that will eventually disagree with itself. Seeded ON so an existing install
-- upgrades into exactly the behaviour it already had — a migration that silently turned a
-- working feature off would read as a bug, not a new option.
--
-- Additive only: no existing table changes. Rollback: docs/migrations/V34-rollback.md.

CREATE TABLE mail_triage_settings (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1))
) STRICT;

INSERT INTO mail_triage_settings (id, enabled) VALUES (1, 1);
