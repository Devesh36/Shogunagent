# V101 rollback — embedding spaces

V101 adds two tables and three triggers for the local embedding lane (E24):

- `embedding_space` — at most one row (`id = 1`): the model id and dimension the store's vectors
  are built in (the "active space"), and since when.
- `vector_models` — `(vec_table, row_id) → model_id` for every vector row in `event_vec`,
  `search_document_vec` and `cold_embeddings`, plus an index on `(vec_table, model_id, row_id)`.
- Triggers that drop a `vector_models` row when its `event_log` or `search_documents` row is
  deleted, or when a search document is re-projected (`content_digest` / `is_dirty` change).

No existing table, column, trigger or index changes. The vectors themselves stay in the V2 / V4 /
V23 tables; nothing here holds text or vector bytes.

## Rollback

Do not delete migration-history rows or run a pre-V101 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V101 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses only the model bookkeeping written after the snapshot. Vectors written after it are
lost with the snapshot too; the older build treats any it keeps as un-attributed, exactly as before
V101. A later V101 build re-embeds every vector that has no `vector_models` row in its active
model, so nothing is mis-scored across models. Nothing was sent to a provider by this migration:
embeddings are computed locally (feature `embed-local`) or not at all.
