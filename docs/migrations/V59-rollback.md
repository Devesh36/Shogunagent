# V59 rollback — Morning Brief Batch metadata bridge

V59 adds nullable `expected_custom_id` and `submitted_at` columns to the existing V55
`brief_generation` table. It does not rewrite rows or remove V55 columns.

## Rollback

Do not delete migration-history rows or run an older binary against the upgraded database. Stop
Shogun, retain the upgraded encrypted database, then restore a verified pre-V59 snapshot using the
backup/restore workflow. No in-place destructive rollback SQL is supplied.

## Operational sequence and verification

1. Quit Shogun and stop any process using its memory database. Keep the current encrypted database
   and its WAL/SHM files together as an offline recovery copy; never copy a live SQLite file alone.
2. Select a verified backup created before this migration. Use the supported backup/restore flow
   with the matching older application version; do not edit `refinery_schema_history` by hand.
3. Open the restored copy and verify `PRAGMA quick_check` returns `ok`, then verify the migration
   history matches the restored binary before resuming capture. A newer migration history means
   the wrong backup was selected; keep the app stopped instead of trying destructive SQL.

## Data and provider impact

No source events or completed Briefs are rewritten. Restoring an older snapshot discards captures and job progress made after that snapshot; any submitted provider work may still be billed.

An in-place downgrade is intentionally unsupported: these migrations couple source-invalidation
triggers with durable paid-request state. Dropping columns or tables does not undo provider work.
