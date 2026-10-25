-- Keep existing people/projects; additional entity types use one bounded schema.
-- Rollback: docs/migrations/V37-rollback.md.
ALTER TABLE people ADD COLUMN extraction_key TEXT;
ALTER TABLE projects ADD COLUMN extraction_key TEXT;
CREATE UNIQUE INDEX people_extraction_key ON people(extraction_key) WHERE extraction_key IS NOT NULL;
CREATE UNIQUE INDEX projects_extraction_key ON projects(extraction_key) WHERE extraction_key IS NOT NULL;

CREATE TABLE knowledge_entities (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('topic','tool','organization','self','employment','education','activity')),
    name TEXT NOT NULL,
    context TEXT NOT NULL,
    extraction_key TEXT NOT NULL UNIQUE,
    confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 0.6),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE knowledge_provenance (
    entity_id INTEGER NOT NULL REFERENCES knowledge_entities(id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL REFERENCES event_log(id) ON DELETE CASCADE,
    quote TEXT,
    PRIMARY KEY(entity_id,event_id)
) STRICT;
CREATE INDEX knowledge_provenance_event ON knowledge_provenance(event_id);
-- Removing the last retained source must not leave extracted text behind as an orphan.
CREATE TRIGGER knowledge_without_evidence AFTER DELETE ON knowledge_provenance BEGIN
    DELETE FROM knowledge_entities WHERE id=old.entity_id
      AND NOT EXISTS(SELECT 1 FROM knowledge_provenance WHERE entity_id=old.entity_id);
END;

CREATE VIEW knowledge_nodes AS
SELECT 'person-' || id AS key, 'person' AS kind, display_name AS name,
       COALESCE(relationship_summary,'') AS detail, confidence FROM people
UNION ALL SELECT 'project-' || id, 'project', name, COALESCE(summary,''), confidence FROM projects
UNION ALL SELECT 'knowledge-' || id, kind, name, context, confidence FROM knowledge_entities;
CREATE VIEW knowledge_links AS
SELECT CASE state_table WHEN 'people' THEN 'person-' ELSE 'project-' END || state_id AS key, event_id
FROM state_provenance WHERE state_table IN ('people','projects')
UNION ALL SELECT 'knowledge-' || entity_id, event_id FROM knowledge_provenance;
