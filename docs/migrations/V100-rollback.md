# V100 rollback — scratch files

V100 adds the table `scratch_files` (`namespace` + `path` primary key, `content`, `actor`,
`created_at`, `updated_at`) and an index on `(namespace, updated_at)`: the files an agent keeps
under `/memories/scratch/<agent_id>/` through `memory.fs`. The `namespace` is always an
`agent_scratch:<agent_id>` value (a CHECK enforces it); the world half of the virtual filesystem is
a projection of the state tables and stores nothing. No existing table, column, trigger or index
changes.

## Rollback

Do not delete migration-history rows or run a pre-V100 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V100 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses every scratch file an agent wrote after the snapshot (its own working notes; the
world model is untouched). Nothing was sent to a provider by this migration.
