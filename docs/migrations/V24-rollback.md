# V24 rollback: Visual Recall Accessibility link

V24 is additive, but the migration runner rejects a database whose history contains a newer
version. Before launching an older binary, stop every SHOGUN process, back up the encrypted
database, then run:

```sql
DROP INDEX IF EXISTS idx_screen_frames_accessibility_event;
DELETE FROM refinery_schema_history WHERE version = 24;
```

SQLite cannot drop this additive column safely on every supported version. Leave it in place;
it contains only an optional foreign-key id and no copied user text. Verify with
`PRAGMA foreign_key_check;` and `PRAGMA quick_check;` before reopening the older binary.
