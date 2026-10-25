# V92 rollback — event_log actor and namespace

V92 adds `event_log.actor` (nullable; the writer of the row, `human:owner` or `agent:<id>`) and
`event_log.namespace` (`world` by default, `agent_scratch:<agent_id>` for an agent's private rows),
with indexes on `(namespace, ts)` and `(actor, ts)`. Pre-V92 rows read as `actor = NULL` and
`namespace = 'world'`. No existing column, trigger or index changes.

## Rollback

Do not delete migration-history rows or run a pre-V92 binary against the upgraded database (the
older build refuses a database with a migration it does not know). Stop every process holding the
principal's store, keep the upgraded encrypted file (with its WAL/SHM files) offline, and restore a
verified pre-V92 snapshot with the matching older build. Then check `PRAGMA quick_check` returns
`ok` before resuming writes.

## Data and provider impact

Restoring loses the events written after the snapshot and the writer/namespace marks on every
event. Nothing was sent to a provider by this migration.
