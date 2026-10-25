# V58 — covering index for the Visual Recall timeline (#727)

## Scope

V58 adds one index and changes no data:

```sql
CREATE INDEX idx_screen_frames_timeline
    ON screen_frames (created_at_ms, id, event_id, accessibility_event_id, app_bundle_id);
```

Why: `screen_frames::timeline_count` / `timeline_page` used to run the full-text MATCH once per
frame (a correlated `EXISTS`), ~0.37 ms per frame on the #714 harness and seconds at a few thousand
frames. They now evaluate the MATCH once per request and test each frame's OCR and Accessibility
links against that hit set. This index carries every column that test and the time/app/keyset
filters read, so the per-frame check stays inside the index instead of reading frame rows, whose
JPEG BLOB spills into overflow pages. `id` is the second column so the page order
(`created_at_ms`, `id`) comes straight off the index.

## Impact

Query results are unchanged (parity tests: `crates/shogun-memory/src/screen_frames_timeline_tests.rs`).
The index holds five integers/short strings per retained frame, so it is small next to the frames
themselves. Building it on upgrade reads each frame row once.

## Rollback SQL

Stop Shogun and back up the encrypted database first.

```sql
DROP INDEX IF EXISTS idx_screen_frames_timeline;
DELETE FROM refinery_schema_history WHERE version = 58;
```

An older build does not reference the index and works without it. Its timeline queries then plan
off `idx_screen_frames_created` again: same results, with the per-frame cost this migration removed.

## Data loss

None. The index holds copies of columns that stay in `screen_frames`; dropping it loses nothing.

## Notes

- No foreign keys are added, so there is no drop order to follow.
- The migration runner rejects a database whose history names a version it does not know, so the
  `refinery_schema_history` delete above is required before starting an older build.
- Verify with `PRAGMA quick_check;` before reopening the older build.
