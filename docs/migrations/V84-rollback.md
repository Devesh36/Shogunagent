# V84 rollback — people and projects as pages

V84 adds to `people` and `projects`: `mention_count`, `last_mentioned_at`, `page_dirty`, `page`
(JSON summary + claims), `engagement`, `page_failures`, `page_blocked_until`; and `person_class` to
`people`. It fills `mention_count` / `last_mentioned_at` once from existing provenance and marks rows
named in two or more hours for a page rewrite.

## Impact

Additive. Nullable columns or NOT NULL with a default, no CHECK, no index, no trigger. No existing
column changes. Older binaries name their columns on INSERT and SELECT, so they ignore them.

## Rollback

Keep the columns (SQLite needs a table rebuild to drop one, and both are user state tables). Going
back loses only the page behaviour: an older build lists every row again, ordered as before, and
shows the description column as it was last written (a written page's rendered text).

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 84;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V84 build would try
`ADD COLUMN` again and fail on the existing columns; to go forward again, restore the V84 history
row from a backup, or rebuild both tables without the columns first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

None. The page JSON and counts are ignored by an older build; the description column keeps its text.
