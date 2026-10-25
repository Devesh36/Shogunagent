# V86 rollback — learned self names

V86 adds two tables. `self_name_evidence` holds the windows that showed a display name or handle is
the user's own (a one-to-one chat, a sent message's header, the hourly review); two windows, or one
sent message, make the name an alias of the user's `self` people row. `memory_markers` holds a scan
watermark and the one-time page refresh marker.

## Impact

Additive. Two new tables, no change to an existing table, no trigger, no data rewrite. Older
binaries never read them.

## Rollback

```sql
BEGIN;
DROP TABLE IF EXISTS self_name_evidence;
DROP TABLE IF EXISTS memory_markers;
DELETE FROM refinery_schema_history WHERE version = 86;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Verify with `PRAGMA quick_check;`
before reopening.

## Data loss

The learned evidence and markers. Aliases already written to the `self` people row stay; a person
row that was merged into it stays merged (its provenance moved with it).
