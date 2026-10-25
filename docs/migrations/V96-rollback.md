# V96 rollback — principal_meta

V96 adds `principal_meta` (`key`, `value`), small keyed values about the principal's file other
than the schema version: `export_cursor`, `sync_cursor`, the principal id, the import source.
Never a secret, never content. No existing table changes.

## Rollback

Do not delete migration-history rows or run a pre-V96 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V96 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring resets the export and sync cursors to the snapshot's values, so the next incremental
export or sync re-sends what came after it (idempotent on the receiving side). Nothing was sent
to a provider by this migration.
