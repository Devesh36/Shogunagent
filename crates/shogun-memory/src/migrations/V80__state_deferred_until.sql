-- Rollback: docs/migrations/V80-rollback.md
-- V80: snooze for commitments and open loops.
--
-- `deferred_until` (Unix ms) hides a row from Today, the Overview and the briefs until that time,
-- and groups it under "Snoozed" on the Memory page. NULL = not snoozed. It is the user's own
-- choice: set and cleared only through `state.update`, never by extraction or decay.
--
-- Additive only: two nullable columns, no default, no CHECK. Older binaries name their columns on
-- INSERT and SELECT, so they ignore them.

ALTER TABLE commitments ADD COLUMN deferred_until INTEGER;
ALTER TABLE open_loops  ADD COLUMN deferred_until INTEGER;
