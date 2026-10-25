-- Rollback: docs/migrations/V84-rollback.md
-- V84: people and projects become pages (owner decision 2026-10-14, Goldfish parity).
--
-- mention_count       distinct clock hours the row was named in (one per hour, however many sightings)
-- last_mentioned_at   newest event that named it (ms). Not last_evidence_at, which drives confidence age.
-- page_dirty          new mentions since the page was last written
-- page                JSON {summary, claims:[{text, basis, weight, event_ids, at}], seen}; NULL = never written
-- engagement          ranking score from the page's user_action / user_said claims (see entity_pages.rs)
-- page_failures       consecutive failed rewrites; page_blocked_until holds the next try (ms)
-- person_class        people only: contact | public_figure | not_a_person (NULL = unknown, older rows)
--
-- Additive only: nullable columns or NOT NULL with a default, no CHECK. Older binaries name their
-- columns on INSERT and SELECT, so they ignore them. The UPDATEs fill the counts once from the
-- existing provenance, so rows named in two or more hours are rewritten as pages over the next runs.

ALTER TABLE people   ADD COLUMN mention_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE people   ADD COLUMN last_mentioned_at INTEGER;
ALTER TABLE people   ADD COLUMN page_dirty INTEGER NOT NULL DEFAULT 0;
ALTER TABLE people   ADD COLUMN page TEXT;
ALTER TABLE people   ADD COLUMN engagement REAL NOT NULL DEFAULT 0;
ALTER TABLE people   ADD COLUMN page_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE people   ADD COLUMN page_blocked_until INTEGER;
ALTER TABLE people   ADD COLUMN person_class TEXT;
ALTER TABLE projects ADD COLUMN mention_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN last_mentioned_at INTEGER;
ALTER TABLE projects ADD COLUMN page_dirty INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN page TEXT;
ALTER TABLE projects ADD COLUMN engagement REAL NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN page_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN page_blocked_until INTEGER;

UPDATE people SET
    mention_count = (SELECT COUNT(DISTINCT e.ts / 3600000) FROM state_provenance s JOIN event_log e ON e.id = s.event_id
                      WHERE s.state_table = 'people' AND s.state_id = people.id),
    last_mentioned_at = (SELECT MAX(e.ts) FROM state_provenance s JOIN event_log e ON e.id = s.event_id
                          WHERE s.state_table = 'people' AND s.state_id = people.id);
UPDATE projects SET
    mention_count = (SELECT COUNT(DISTINCT e.ts / 3600000) FROM state_provenance s JOIN event_log e ON e.id = s.event_id
                      WHERE s.state_table = 'projects' AND s.state_id = projects.id),
    last_mentioned_at = (SELECT MAX(e.ts) FROM state_provenance s JOIN event_log e ON e.id = s.event_id
                          WHERE s.state_table = 'projects' AND s.state_id = projects.id);
UPDATE people   SET page_dirty = 1 WHERE mention_count >= 2;
UPDATE projects SET page_dirty = 1 WHERE mention_count >= 2;
