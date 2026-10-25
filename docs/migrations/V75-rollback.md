# V75 rollback — focused-window rectangle on Visual Recall frames

V75 adds four nullable columns to `screen_frames` (issue #969): `focus_x`, `focus_y`,
`focus_width`, `focus_height`. Full-display previews fill them with the focused window's rectangle
in the stored JPEG's pixels, so "Re-read text" reads only that window. Frames written before V75
keep NULL: their JPEG is the focused window itself.

## Impact

Additive. Four nullable columns with no default or CHECK. No existing column, index or trigger
changes. Older binaries name their columns on INSERT and SELECT, so they ignore the new ones and
write NULL for them.

## Rollback

Keep the columns (SQLite needs a table rebuild to drop one, and the frames are encrypted user
data). An older build reads every frame as a whole-window JPEG, so a full-display preview would
be re-read as the focused window's text. Delete the full-display previews before going back:

```sql
BEGIN;
DELETE FROM screen_frames WHERE focus_width IS NOT NULL;
DELETE FROM refinery_schema_history WHERE version = 75;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Reopening with a V75 build would try
`ADD COLUMN focus_x` again and fail on the existing column; to go forward again, restore the V75
history row from a backup, or rebuild `screen_frames` without the columns first. Verify with
`PRAGMA quick_check;` before reopening.

## Data loss

The deleted previews are gone, as if their retention had expired. The OCR and Accessibility text
linked to them stays in `event_log`.
