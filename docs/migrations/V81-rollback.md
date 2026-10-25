# V81 rollback — `event_grams_fts` (two-character search index)

V81 adds a second full-text index over captured events for the words the V1 trigram index cannot
match: two-character Japanese words (会議, 予算, 田中), okurigana variants folded to one key
(打合せ / 打ち合わせ → 打合), katakana pairs (バグ), and short Latin tokens (Q3, PR, CI). See
`docs/search-eval.md` for why and what it changed.

Objects added:

- `event_grams_fts` — FTS5, `detail=none`, `columnsize=0`, rowid = `event_log.id`, plus its
  shadow tables (`event_grams_fts_data`, `_idx`, `_content`, `_config`).
- `event_grams_pending` — ids whose grams must be (re)computed.
- `event_grams_backfill` — one-row cursor for indexing pre-V81 history.
- Triggers `event_log_grams_ai`, `event_log_grams_ad`, `event_log_grams_au` on `event_log`.

## Impact

Additive. No existing table, column, index or trigger changes. The migration itself does no
per-row work (it records `MAX(event_log.id)` as the back-fill start), so it is instant on any
store size. Rows are indexed on the write path and by the bounded hourly maintenance pass.

Size: the index stores each event's distinct grams (space-separated) and a `detail=none`
posting list. Measured (`gram_index_size_report` in `crates/shogun-core/tests/search_golden_eval.rs`,
3,000 rows each): kanji-dense Japanese text adds about as much as `event_fts` itself (≈ +43 % of
`event_log` + `event_fts`); English-only text adds ≈ 3 % of `event_fts` (≈ +2 %), since only
two-character tokens such as `q3` / `pr` are stored. Captured screen frames (V-series
`screen_frames`, up to 2 GiB) are unaffected. Details: `docs/search-eval.md`.

Older binaries: the triggers are plain SQL (no custom function), so an older build can still
insert, update and delete events with V81 in place; it simply does not drain
`event_grams_pending`, which grows by one small row per new event until a V81 build runs again.

## Rollback

```sql
BEGIN;
DROP TRIGGER IF EXISTS event_log_grams_ai;
DROP TRIGGER IF EXISTS event_log_grams_ad;
DROP TRIGGER IF EXISTS event_log_grams_au;
DROP TABLE IF EXISTS event_grams_fts;
DROP TABLE IF EXISTS event_grams_pending;
DROP TABLE IF EXISTS event_grams_backfill;
DELETE FROM refinery_schema_history WHERE version = 81;
COMMIT;
```

The migration runner rejects a database whose history names a version it does not know, so the
history delete is required before starting an older build. Going forward again re-runs V81 and
re-indexes through the back-fill. Verify with `PRAGMA quick_check;` before reopening.

## Data loss

None. The index is derived entirely from `event_log` and can be rebuilt at any time; dropping it
only makes two-character searches fall back to the trigram index (i.e. return nothing for them).
