-- Rollback: docs/migrations/V57-rollback.md.
-- Keep FTS5 index leaves inside one SQLCipher page (#714). Config rows only: no DDL, no rewrite.
--
-- FTS5 writes `%_data` leaves of up to `pgsz` bytes; the default is 4050. SQLCipher 4 keeps the
-- 4096-byte page but reserves 80 bytes of each page for its IV + HMAC, so a table b-tree cell can
-- hold at most 4096 - 80 - 35 = 3981 bytes locally, 3977 of them leaf bytes after the 4-byte record
-- header. A full default leaf does not fit and spills the rest into an overflow page: on the #714
-- synthetic corpus 88.7% of `event_fts_data` pages were overflow pages. FTS5 can end a leaf a few
-- bytes past `pgsz` (varints appended after its "page full" check), so 3960 keeps 17 bytes of
-- headroom. At 3960 the same corpus had no overflow pages and the index was 8.7% smaller, with
-- identical match sets and bm25 order.
--
-- The value is fixed rather than derived from the page layout. Every shipped database is
-- SQLCipher with the default page size and reserve (the app sets neither). An unencrypted database
-- (tests, `open`, `open_in_memory`) has no reserve and could hold 4050; 3960 is valid there too,
-- just ~2% under the optimum. `sqlcipher_export` copies these rows, so a converted database keeps
-- the setting.
--
-- `pgsz` only governs leaves written from now on (new segments and merges). Readers accept any leaf
-- size, so existing segments stay valid and shrink as they get merged. Deliberately no
-- 'optimize' / 'rebuild' / 'merge' here: on a user's database that is one transaction with a WAL
-- the size of the whole index, run inside startup.
INSERT INTO event_fts(event_fts, rank) VALUES ('pgsz', 3960);
INSERT INTO search_documents_fts(search_documents_fts, rank) VALUES ('pgsz', 3960);
INSERT INTO knowledge_entities_fts(knowledge_entities_fts, rank) VALUES ('pgsz', 3960);
