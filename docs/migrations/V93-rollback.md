# V93 rollback — state tables actor_last and namespace

V93 adds `actor_last` (nullable; the last writer) and `namespace` (`world` by default) to the four
state tables `people`, `projects`, `commitments` and `open_loops`, with one index per table on
`namespace`. Pre-V93 rows read as `actor_last = NULL` and `namespace = 'world'`. No existing
column, trigger or index changes; `state_provenance` is untouched.

## Rollback

Do not delete migration-history rows or run a pre-V93 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V93 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses state changes after the snapshot and the writer marks on every state row. Nothing
was sent to a provider by this migration.
