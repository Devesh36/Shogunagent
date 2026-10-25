# V62 rollback — daily activity summary jobs

V62 adds durable lineage and Batch state for daily activity summaries. Generated prose remains in
`period_summaries`; the new table stores job state and source manifests.

It also clears legacy generated About You text and its raw-event source manifest before changing
invalidation to use daily summaries. That cached prose is regenerated from current evidence;
source events and user-declared profile fields are retained. In-flight and uncertain request
identities, handles and attempt metadata are preserved rather than resubmitted during upgrade.

## Rollback

Do not delete migration-history rows or drop the table. Stop Shogun and restore a verified
pre-V62 snapshot through the backup/restore workflow. Keep the upgraded encrypted database for
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

Restoring loses new daily-summary job progress and restores the older About You cache. Source events remain intact in the retained upgraded database. Submitted provider work cannot be undone by restoring a local file.

An in-place downgrade is intentionally unsupported: these migrations couple source-invalidation
triggers with durable paid-request state. Dropping columns or tables does not undo provider work.
