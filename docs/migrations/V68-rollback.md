# V68 — indexes for event-linked lookups (#780)

## Scope

V68 adds four indexes and changes no data:

```sql
CREATE INDEX idx_screen_frames_event ON screen_frames (event_id);
CREATE INDEX idx_period_summary_event_provenance_event
    ON period_summary_event_provenance (event_id);
CREATE INDEX idx_period_summaries_end
    ON period_summaries (period_end_ms, period_start_ms);
CREATE INDEX idx_period_summary_shard_runs_end
    ON period_summary_shard_runs (period_end_ms, period_start_ms);
```

Why (issue #780):

- **Deletes.** `screen_frames.event_id` and `period_summary_event_provenance.event_id` are foreign
  keys to `event_log`, and neither had an index that leads with them. Every deleted event scanned
  both tables, so "Delete last hour / 24 hours" over a few thousand events held the only database
  connection for minutes, longer under SQLCipher. Capture, notch reads, search and chat waited
  behind it, and Memory API writers in other processes hit `SQLITE_BUSY`.
- **Chat and notch context.** Each evidence hit looks up its saved screen by `event_id`, a full
  `screen_frames` scan per hit with Visual recall on.
- **Every capture write.** The V22 triggers mark containing periods dirty on each `event_log`
  insert, update and delete with `ts >= period_start_ms AND ts < period_end_ms`. Neither table had
  an index that could serve that condition, so each write re-read every summary row, a cost that
  grew with each night of history. An index leading on `period_end_ms` reads only the periods that
  end after `ts`.

## Impact

Query results are unchanged; only plans change. The indexes hold one or two integers per row, small
next to the rows themselves. Building them on upgrade reads each row of the four tables once.

## Rollback SQL

Stop Shogun and back up the encrypted database first.

```sql
DROP INDEX IF EXISTS idx_screen_frames_event;
DROP INDEX IF EXISTS idx_period_summary_event_provenance_event;
DROP INDEX IF EXISTS idx_period_summaries_end;
DROP INDEX IF EXISTS idx_period_summary_shard_runs_end;
DELETE FROM refinery_schema_history WHERE version = 68;
```

An older build does not reference these indexes and works without them, with the scans this
migration removed.

## Data loss

None. The indexes hold copies of columns that stay in their tables.

## Notes

- No foreign keys or constraints are added, so there is no drop order to follow.
- The migration runner rejects a database whose history names a version it does not know, so the
  `refinery_schema_history` delete above is required before starting an older build.
- Verify with `PRAGMA quick_check;` before reopening the older build.
