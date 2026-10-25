# V98 rollback — user notes actor and namespace

V98 adds `user_memory_notes.actor` (nullable; the writer, `human:owner` or `agent:<id>`) and
`user_memory_notes.namespace` (`world` by default, `agent_scratch:<agent_id>` for an agent's
private notes), `user_memory_note_versions.actor` (the writer of each revision), and an index on
`(namespace, updated_at)`. Pre-V98 rows read as `actor = NULL` and `namespace = 'world'`. No
existing column, trigger or index changes; the notes' FTS table is untouched.

## Rollback

Do not delete migration-history rows or run a pre-V98 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V98 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the notes written after the snapshot and the writer marks on every note. Nothing
was sent to a provider by this migration.
