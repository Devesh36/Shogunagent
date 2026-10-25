# V60 rollback — About You generation

V60 adds the singleton `about_profile_generation` table and source-invalidation triggers. Editable
profile fields remain in `memory_api.json`.

## Rollback

Do not delete migration-history rows or drop the table. Stop Shogun and restore a verified
pre-V60 snapshot through the backup/restore workflow. Keep the upgraded encrypted database for
recovery; no in-place destructive rollback SQL is supplied.

## Operational sequence and verification

1. Quit Shogun and stop any process using its memory database. Keep the current encrypted database
   and its WAL/SHM files together as an offline recovery copy; never copy a live SQLite file alone.
2. Select a verified backup created before this migration. Use the supported backup/restore flow
   with the matching older application version; do not edit `refinery_schema_history` by hand.
3. Open the restored copy and verify `PRAGMA quick_check` returns `ok`, then verify the migration
   history matches the restored binary before resuming capture. A newer migration history means
   the wrong backup was selected; keep the app stopped instead of trying destructive SQL.

## Data and provider impact

The generated profile cache and its source references are lost when restoring the snapshot. User-declared profile fields live separately and are not removed by this migration.

An in-place downgrade is intentionally unsupported: these migrations couple source-invalidation
triggers with durable paid-request state. Dropping columns or tables does not undo provider work.
