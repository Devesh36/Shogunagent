-- Lossless candidate index for request-time knowledge context matching.
-- Rollback: docs/migrations/V39-rollback.md.
CREATE VIRTUAL TABLE knowledge_entities_fts USING fts5(
    name,
    context,
    content='knowledge_entities',
    content_rowid='id',
    tokenize='trigram'
);

CREATE TRIGGER knowledge_entities_fts_ai AFTER INSERT ON knowledge_entities BEGIN
    INSERT INTO knowledge_entities_fts(rowid, name, context)
    VALUES (new.id, new.name, new.context);
END;
CREATE TRIGGER knowledge_entities_fts_ad AFTER DELETE ON knowledge_entities BEGIN
    INSERT INTO knowledge_entities_fts(knowledge_entities_fts, rowid, name, context)
    VALUES ('delete', old.id, old.name, old.context);
END;
CREATE TRIGGER knowledge_entities_fts_au AFTER UPDATE OF name, context ON knowledge_entities BEGIN
    INSERT INTO knowledge_entities_fts(knowledge_entities_fts, rowid, name, context)
    VALUES ('delete', old.id, old.name, old.context);
    INSERT INTO knowledge_entities_fts(rowid, name, context)
    VALUES (new.id, new.name, new.context);
END;

-- Existing canonical rows become searchable in the same migration transaction.
INSERT INTO knowledge_entities_fts(knowledge_entities_fts) VALUES ('rebuild');
