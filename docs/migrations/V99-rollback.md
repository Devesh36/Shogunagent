# V99 rollback — ingest receipts

V99 adds the table `ingest_receipts` (`idempotency_key_hash` primary key, `actor`, `thread_key`,
`event_ids` as a JSON array, `created_at`) and an index on `created_at`. It holds the receipt of
every `memory.ingest` call so a repeated idempotency key replays the first outcome. No existing
table, column, trigger or index changes. The `memory_markers` rows the E4 build wrote under
`ingest:<hash>` keys are left in place and read as a fallback only.

## Rollback

Do not delete migration-history rows or run a pre-V99 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V99 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the receipts written after the snapshot: an agent that repeats an idempotency key
it used in that window appends its items again (the exact-duplicate collapse still folds identical
text). Nothing was sent to a provider by this migration.
