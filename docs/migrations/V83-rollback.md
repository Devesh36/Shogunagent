# V83 rollback — loop signals, horizon, aging and merges

V83 adds the `loop_signals` table (what the hourly review reports, append-only), the
`loop_signal_reads` ledger (which signals the consolidator has read) and three
nullable columns to `commitments` and to `open_loops`: `horizon` (set by the loop consolidator),
`aged_at` (the horizon timer retired the row) and `merged_into` (the row a duplicate was folded
into).

## Impact

Additive. Two new tables, one index, six nullable columns. No existing column, index or
trigger changes. Older binaries name their columns on INSERT and SELECT, so they ignore them.

## Rollback

Keep the columns and the tables (SQLite needs a table rebuild to drop a column, and these are user
state tables). Going back loses the consolidator: an older build writes review items straight into
the state tables again. Aged and merged rows keep the `cancelled` / `closed` status they were given,
so they stay off the lists of an older build too.

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 83;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V83 build would try
`CREATE TABLE loop_signals` and `ADD COLUMN` again and fail; to go forward again, restore the V83
history row from a backup, or drop `loop_signal_reads` and `loop_signals` and rebuild both
state tables without the columns first. Verify with `PRAGMA quick_check;` before reopening.

## Data loss

None. Signals are derived from the event log and are re-created by the next reviews.
Rows V83 builds retired below 0.5 stay there under an older build (it only reads them).
