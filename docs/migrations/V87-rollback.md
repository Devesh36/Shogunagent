# V87 rollback — capture and frame lookup indexes

V87 adds two indexes and one trigger-maintained table so three hot paths stop reading every
captured event or every stored screen frame.

Objects added:

- `idx_event_log_source_app` on `event_log (source, app_bundle_id)` — the near-duplicate lookup
  that runs before every capture insert (`event_log::recent_app_scoped_bodies`).
- `idx_screen_frames_screen_text_event` on `screen_frames (screen_text_event_id)`, partial
  (`WHERE screen_text_event_id IS NOT NULL`) — the shared-copy check in
  `screen_frames::delete_by_id_in`, run once per deleted frame.
- `screen_frame_sizes (frame_id, created_at_ms, bytes)` plus triggers `screen_frame_sizes_ai`,
  `screen_frame_sizes_ad`, `screen_frame_sizes_au` on `screen_frames` — read by
  `screen_frames::stats` instead of summing `length(bytes)` over every frame row.

## Measured

600 MB copy of a real store (69,269 events, 2,881 frames, 242 MB of JPEG), the app's bundled
SQLCipher (SQLite 3.39.4). Pages read and decrypted per call:

| Path | Before | After |
|---|---:|---:|
| Capture dedupe lookup, app not seen recently | 14,506 | 3 |
| Capture dedupe lookup, `screen_ocr` with no app | 3,299 | 2 |
| Frame delete: shared `screen_text` check | 12,986 | 2 |
| Frame stats (two calls per stored frame) | 1,922 | a few (table of 3 integers per frame) |

`screen_text_event_id` sits after the JPEG in each row, so before V87 every check walked each
frame's overflow pages. Turning Visual recall off ran it once per frame (O(frames²)).

## Impact

Additive. No existing table, column, index or trigger changes. Query plans of the other
`event_log` and `screen_frames` reads were checked with `EXPLAIN QUERY PLAN` before and after and
did not change.

One-time cost at upgrade: building the partial frame index reads each frame once (the column is
stored after the image), so it scales with the frame store (about 10 s for 242 MB on a heavily
loaded machine; the pre-migration snapshot already reads the whole file). The other two steps
take well under a second (`length()` of a BLOB comes from the record header).

Older binaries: the triggers are plain SQL, so an older build keeps `screen_frame_sizes` exact
while it inserts and deletes frames; it simply does not read it.

## Rollback

```sql
BEGIN;
DROP TRIGGER IF EXISTS screen_frame_sizes_ai;
DROP TRIGGER IF EXISTS screen_frame_sizes_ad;
DROP TRIGGER IF EXISTS screen_frame_sizes_au;
DROP TABLE IF EXISTS screen_frame_sizes;
DROP INDEX IF EXISTS idx_screen_frames_screen_text_event;
DROP INDEX IF EXISTS idx_event_log_source_app;
DELETE FROM refinery_schema_history WHERE version = 87;
COMMIT;
```

Then install a build whose newest migration is V81. Nothing else reads these objects.
