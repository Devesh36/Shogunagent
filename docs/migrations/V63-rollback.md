# V63 rollback — Morning Brief source lifecycle

V63 replaces the V55 brief invalidation triggers and adds cleanup of expired provider handles. It
preserves the V55 ledger, source manifests, and existing provider IDs. Completed briefs remain
stable for uncited historical inserts; cited-source edits and deletes still invalidate generated
prose.

## Rollback

Do not delete migration-history rows or run a pre-V63 binary against the upgraded database. Stop
Shogun, retain the upgraded encrypted database, then restore a verified pre-V63 snapshot through
the backup/restore workflow. No in-place destructive rollback SQL is supplied.

## Operational sequence and verification

1. Quit Shogun and stop any process using its memory database. Keep the current encrypted database
   and its WAL/SHM files together as an offline recovery copy; never copy a live SQLite file alone.
2. Select a verified backup created before this migration. Use the supported backup/restore flow
   with the matching older application version; do not edit `refinery_schema_history` by hand.
3. Open the restored copy and verify `PRAGMA quick_check` returns `ok`, then verify the migration
   history matches the restored binary before resuming capture. A newer migration history means
   the wrong backup was selected; keep the app stopped instead of trying destructive SQL.

## Data and provider impact

Restoring reinstates the previous source-invalidation behavior. Keep the upgraded database offline so its newer captures and completed results can be recovered separately.

An in-place downgrade is intentionally unsupported: these migrations couple source-invalidation
triggers with durable paid-request state. Dropping columns or tables does not undo provider work.
