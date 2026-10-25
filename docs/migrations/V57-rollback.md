# V57 — FTS5 leaf size that fits a SQLCipher page (#714)

## Scope

V57 writes one configuration row per FTS5 table: `pgsz = 3960` in `event_fts_config`,
`search_documents_fts_config` and `knowledge_entities_fts_config`. There is no DDL and no index
rewrite.

Why: SQLCipher 4 reserves 80 bytes of each 4096-byte page, so a cell holds at most 3,981 bytes
locally. FTS5's default leaf size (4050) does not fit, and every full leaf spilled into an overflow
page. At 3960 the leaves fit and the index is about 9% smaller.

## Impact

The value only limits the size of index leaves FTS5 writes from then on (new segments and merges).
It does not change what is indexed or how queries match or rank. The on-disk format does not depend
on the value: FTS5 readers accept leaves of any size, so leaves written at 3960 stay valid under
4050 and vice versa. Existing leaves keep their size until a merge rewrites them.

## Rollback SQL

Stop Shogun and back up the encrypted database first.

1. Optional: return to the FTS5 default. Harmless, and not required for an older build to read the
   index.

   ```sql
   INSERT INTO event_fts(event_fts, rank)                           VALUES ('pgsz', 4050);
   INSERT INTO search_documents_fts(search_documents_fts, rank)     VALUES ('pgsz', 4050);
   INSERT INTO knowledge_entities_fts(knowledge_entities_fts, rank) VALUES ('pgsz', 4050);
   ```

2. Only to run a pre-V57 binary: refinery refuses a database that records a migration the binary
   does not know, so remove the history row. V57 changed nothing else, so no other cleanup is
   needed.

   ```sql
   DELETE FROM refinery_schema_history WHERE version = 57;
   ```

   The next start of a V57 build re-applies V57, which writes 3960 again.

Restoring a pre-upgrade snapshot through the verified backup/restore workflow also works, but it
drops everything captured since the upgrade and is not needed for this migration.

## Data loss

None. Neither direction touches indexed content, and search results are the same under either
value. Leaves already written at 3960 stay valid after going back to 4050.

## Notes

- No foreign keys or tables are involved, so there is no drop order.
- Do not run `'optimize'` or `'rebuild'` to "apply" either value. On a real database that is one
  transaction with a WAL about the size of the index.
