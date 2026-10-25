-- Rollback: docs/migrations/V102-rollback.md
-- V102 (ShogunAgent E24): which embedding model produced each stored vector, so a model change can
-- never silently mix vector spaces. `embedding_space` is the store's one active space (the model
-- the embed job writes and search reads); `vector_models` records, per vector row, the model that
-- wrote it. Search only considers rows of the active model, and the embed job re-embeds every row
-- of another model (or of none: vectors written before V102) instead of comparing across spaces.
-- Ids and model names only: no text, no vector bytes.
--
-- Written with IF NOT EXISTS so `embedding_space::ensure_schema` can re-run it after the search
-- projection is rebuilt (`search_documents::recreate_projection_and_rebuild` drops and recreates
-- `search_documents`, which drops the triggers on it).
CREATE TABLE IF NOT EXISTS embedding_space (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    model_id TEXT    NOT NULL,
    dim      INTEGER NOT NULL,
    since_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS vector_models (
    vec_table TEXT    NOT NULL CHECK (vec_table IN ('event_vec', 'search_document_vec', 'cold_embeddings')),
    row_id    INTEGER NOT NULL,
    model_id  TEXT    NOT NULL,
    PRIMARY KEY (vec_table, row_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS vector_models_by_model ON vector_models (vec_table, model_id, row_id);

CREATE TRIGGER IF NOT EXISTS vector_models_event_ad
AFTER DELETE ON event_log BEGIN
    DELETE FROM vector_models
     WHERE vec_table IN ('event_vec', 'cold_embeddings') AND row_id = old.id;
END;

CREATE TRIGGER IF NOT EXISTS vector_models_document_ad
AFTER DELETE ON search_documents BEGIN
    DELETE FROM vector_models WHERE vec_table = 'search_document_vec' AND row_id = old.id;
END;

CREATE TRIGGER IF NOT EXISTS vector_models_document_au
AFTER UPDATE OF content_digest, is_dirty ON search_documents
WHEN new.content_digest <> old.content_digest OR new.is_dirty <> 0 BEGIN
    DELETE FROM vector_models WHERE vec_table = 'search_document_vec' AND row_id = old.id;
END;
