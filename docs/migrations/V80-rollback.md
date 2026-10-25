# V80 rollback — `deferred_until` on commitments and open loops

V80 adds one nullable column, `deferred_until` (Unix ms), to `commitments` and to `open_loops`.
A row with a future value is snoozed: hidden from Today, the Overview and the briefs until then,
and listed under "Snoozed" on the Memory page. NULL means not snoozed.

## Impact

Additive. Two nullable columns with no default or CHECK. No existing column, index or trigger
changes. Older binaries name their columns on INSERT and SELECT, so they ignore them.

## Rollback

Keep the columns (SQLite needs a table rebuild to drop one, and these are user state tables).
Going back only loses the snooze: an older build shows snoozed rows again as ordinary open work.

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 80;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V80 build would try
`ADD COLUMN` again and fail on the existing columns; to go forward again, restore the V80 history
row from a backup, or rebuild both tables without the columns first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

None. Only snooze times are ignored by an older build.
