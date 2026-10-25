# V95 rollback — export_tombstones

V95 adds `export_tombstones` (`kind`, `id`, `deleted_at`, `reason`), the deletion markers an
incremental export or sync peer reads to retire its copy of a deleted row. It never holds the
deleted content. No existing table changes.

## Rollback

Do not delete migration-history rows or run a pre-V95 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V95 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the markers written after the snapshot, so a sync peer may keep a copy of a row
deleted in that window until the deletion is replayed. Nothing was sent to a provider by this
migration.
