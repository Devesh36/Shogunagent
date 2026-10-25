-- Rebuildable local semantic index for distilled memory. Canonical search_documents remain the
-- source of truth; vector rows are disposable and never hold user text.

CREATE TABLE IF NOT EXISTS search_document_embeddings (
    document_id    INTEGER PRIMARY KEY REFERENCES search_documents(id) ON DELETE CASCADE,
    content_digest TEXT NOT NULL,
    embedded_at_ms INTEGER NOT NULL
) STRICT;

CREATE VIRTUAL TABLE IF NOT EXISTS search_document_vec
USING vec0(embedding float[384]);

CREATE TRIGGER IF NOT EXISTS search_document_vectors_ad
AFTER DELETE ON search_documents BEGIN
    DELETE FROM search_document_vec WHERE rowid = old.id;
END;

CREATE TRIGGER IF NOT EXISTS search_document_vectors_au
AFTER UPDATE OF content_digest, is_dirty ON search_documents
WHEN new.content_digest <> old.content_digest OR new.is_dirty <> 0 BEGIN
    DELETE FROM search_document_vec WHERE rowid = old.id;
    DELETE FROM search_document_embeddings WHERE document_id = old.id;
END;
