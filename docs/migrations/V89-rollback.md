# V89 rollback — meeting crash recovery

V89 adds `meeting_recovery`: one row per crash-closed meeting or call, with flags for whether its
recap was attempted and whether the notch showed the recovery notice. It changes no existing table,
and transcripts and notes are never modified by it.

## Rollback

Do not delete migration-history rows or run a pre-V89 binary against the upgraded database (the
older app refuses a database with a migration it does not know). Quit Shogun, keep the upgraded
encrypted database (with its WAL/SHM files) offline, and restore a verified pre-V89 snapshot
through the backup/restore workflow with the matching older app version. Then check
`PRAGMA quick_check` returns `ok` before resuming capture.

## Data and provider impact

Restoring loses only the recovery flags: a meeting closed by recovery after the snapshot may get
one more recap attempt and notice. A recap Batch already submitted is not recalled. Dropping the
table in place does not undo provider work, so no destructive rollback SQL is supplied.
