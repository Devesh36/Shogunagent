# V71 rollback — full URL on web fetch traceability rows

V71 adds a nullable `traceability_log.url` column (issue #928). Only public web fetch rows
(`route = 'web'`) fill it, with the URL after `web_fetch::ledger_url` drops the fragment and
`user:pass@` userinfo and masks credential-looking query parameters and path segments. Every other
row, and every row written before V71, is NULL.

V1__init.sql's comment that the table has no text column predates V71. It is frozen, so the
correction lives here and in `shogun_memory::traceability`'s module docs.

## Impact

Additive. One nullable column with no default or CHECK. No existing column, index or trigger
changes. Older binaries name their columns on INSERT and SELECT, so they ignore `url` and write
NULL for it.

## Rollback

Keep the column (SQLite needs a table rebuild to drop one, and `traceability_log` is an audit
record). To remove the stored URLs and let an older build open the database:

```sql
BEGIN;
UPDATE traceability_log SET url = NULL;
DELETE FROM refinery_schema_history WHERE version = 71;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V71 build would try
`ADD COLUMN url` again and fail on the existing column; to go forward again, restore the V71
history row from a backup, or rebuild `traceability_log` without the column first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

Clearing `url` loses which page each web fetch read. The host, digest, size, time and route stay.
