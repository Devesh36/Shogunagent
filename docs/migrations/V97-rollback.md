# V97 rollback — relations

V97 adds `relations`: typed rows between state entities (`subject_kind`/`subject_id`,
`predicate`, `object_kind`/`object_id`) with a validity interval, a 0..1 confidence, the
`event_log` row that evidences the relation (`provenance_event_id`), the writer and the namespace.
No existing table changes.

## Rollback

Do not delete migration-history rows or run a pre-V97 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V97 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the relation rows written after the snapshot; the state rows and their
`state_provenance` are unaffected. Nothing was sent to a provider by this migration.
