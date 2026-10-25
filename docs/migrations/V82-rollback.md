# V82 rollback — `aliases` on projects

V82 adds one nullable column, `aliases` (JSON array of strings), to `projects`. It holds the other
names a project goes by, matched when a new mention arrives so a case or wording variant does not
create a second row. NULL means no aliases.

## Impact

Additive. One nullable column with no default or CHECK. No existing column, index or trigger
changes. Older binaries name their columns on INSERT and SELECT, so they ignore it.

## Rollback

Keep the column (SQLite needs a table rebuild to drop one, and `projects` is a user state table).
Going back only loses the alias matching: an older build matches a project by its name alone.

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 82;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V82 build would try
`ADD COLUMN` again and fail on the existing column; to go forward again, restore the V82 history
row from a backup, or rebuild `projects` without the column first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

None. Only the stored aliases are ignored by an older build.
