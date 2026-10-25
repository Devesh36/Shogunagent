-- Rollback: docs/migrations/V85-rollback.md
-- V85: what a project row is to the user (2026-10-15), the twin of V84's people.person_class.
--
-- project_class   user_project | media_or_public | not_a_project (NULL = unknown, older rows)
--                 not_a_project rows leave every list; media_or_public rows rank after the
--                 user's own work (entity_pages.rs). Set by the hourly review and a page rewrite.
--
-- Additive only: one nullable column, no CHECK, no index. Older binaries name their columns on
-- INSERT and SELECT, so they ignore it.

ALTER TABLE projects ADD COLUMN project_class TEXT;
