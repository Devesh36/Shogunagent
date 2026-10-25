-- Rollback: docs/migrations/V82-rollback.md
-- V82: aliases on projects, like people.aliases (V1).
--
-- A project is known by more than one name ("Shogun", "ShogunAI", the repo handle). The aliases
-- are matched, normalised, against a new mention before a second row is created, and are shown
-- to the hourly review so the model can map a mention to the existing project. JSON array of
-- strings, NULL = none.
--
-- Additive only: one nullable column, no default, no CHECK. Older binaries name their columns on
-- INSERT and SELECT, so they ignore it.

ALTER TABLE projects ADD COLUMN aliases TEXT;
