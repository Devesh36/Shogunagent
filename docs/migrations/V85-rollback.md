# V85 rollback — project class

V85 adds one nullable column, `projects.project_class` (`user_project` | `media_or_public` |
`not_a_project`, NULL = unknown). It is the project twin of V84's `people.person_class`: the hourly
review and a page rewrite set it; lists hide `not_a_project` rows and rank `media_or_public` last.

## Impact

Additive. One nullable column, no CHECK, no index, no trigger, no data rewrite. Older binaries name
their columns on INSERT and SELECT, so they ignore it.

## Rollback

Keep the column (SQLite needs a table rebuild to drop one, and `projects` is a user state table).
Going back loses only the classification: an older build lists the rows it hid again.

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 85;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V85 build would try
`ADD COLUMN` again and fail on the existing column; to go forward again, restore the V85 history
row from a backup, or rebuild the table without the column first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

None. The class is ignored by an older build.
