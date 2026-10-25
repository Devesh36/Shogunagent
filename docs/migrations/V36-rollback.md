# V36 rollback

-- non-additive-ok: SQLite cannot replace UNIQUE(app, text) in place; the transactional rebuild preserves every existing sample while adding recipient-scoped uniqueness.

## Decision and preservation

V36 creates the replacement table, copies every row's id, app, text and timestamp with an
empty recipient key, then replaces the old table and restores both indexes. No existing
sample is assigned an inferred recipient. The migration runner applies this transactionally.
The existing `recipient_migration_keeps_old_samples_unscoped` regression checks preservation
and verifies that the same text can subsequently be stored for a specific recipient.

The SQL has already been applied to local installations. Do not edit even its comments:
refinery hashes the complete SQL, and a changed checksum would reject existing databases.
The CI exception pins its exact SHA-256 in `scripts/check-migrations.py`; other migrations
and any changed V36 bytes are not covered by this decision.

## Rollback procedure

Back up the encrypted database before reverting. Prefer restoring the pre-V36 backup with the
matching application version. V36 changes writing-example uniqueness to include a recipient key;
old rows remain unscoped. Do not flatten recipient-scoped rows into general app examples on rollback:
that would lose their privacy boundary. If preserving post-migration data, export locally first and
retain only unscoped examples when rebuilding the V35 table. Existing memory and feedback tables are
not modified by V36.
