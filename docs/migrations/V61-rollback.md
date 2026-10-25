# V61 rollback — About You Batch attempt metadata

V61 adds Batch attempt metadata and trigger behavior for preserving an in-flight About You request
when cited source evidence changes.

## Rollback

Do not delete migration-history rows or run an older binary against the upgraded database. Stop
Shogun and restore a verified pre-V61 snapshot through the backup/restore workflow. Retain the
upgraded encrypted database for recovery; no in-place destructive rollback SQL is supplied.

## Operational sequence and verification

1. Quit Shogun and stop any process using its memory database. Keep the current encrypted database
   and its WAL/SHM files together as an offline recovery copy; never copy a live SQLite file alone.
2. Select a verified backup created before this migration. Use the supported backup/restore flow
   with the matching older application version; do not edit `refinery_schema_history` by hand.
3. Open the restored copy and verify `PRAGMA quick_check` returns `ok`, then verify the migration
   history matches the restored binary before resuming capture. A newer migration history means
   the wrong backup was selected; keep the app stopped instead of trying destructive SQL.

## Data and provider impact

Restoring loses newer request identities and provider handles. Already submitted work may still complete at the provider; do not manually retry those requests as part of rollback.

An in-place downgrade is intentionally unsupported: these migrations couple source-invalidation
triggers with durable paid-request state. Dropping columns or tables does not undo provider work.
