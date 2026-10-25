# V77 rollback — `screen_text` event link on Visual Recall frames

V77 adds one nullable column to `screen_frames`: `screen_text_event_id`, the durable
`screen_text` event the frame produced or touched. Deleting the frame deletes that event too
unless another frame still references it. NULL on frames from before V77 and on frames whose
text Accessibility already covered.

## Impact

Additive. One nullable column with no default or CHECK. No existing column, index or trigger
changes. Older binaries name their columns on INSERT and SELECT, so they ignore it.

## Rollback

Keep the column (SQLite needs a table rebuild to drop one, and the frames are encrypted user
data). Going back only loses the link: an older build deletes a frame without its `screen_text`
copy, which then stays until Visual recall is turned Off.

```sql
BEGIN;
DELETE FROM refinery_schema_history WHERE version = 77;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V77 build would try
`ADD COLUMN` again and fail on the existing column; to go forward again, restore the V77 history
row from a backup, or rebuild `screen_frames` without the column first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

None.
