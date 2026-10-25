# V104 rollback — loop updates

V104 adds two tables for the outcomes of commitments and open loops (plan E49):

- `loop_updates` — one row per (state row, event, kind) where a later message in the row's thread
  says the item was done (`resolved`), called off (`cancelled`) or moved (`redated`, with the new
  `due_at` and the `previous_due_at`), with the exact quote, whether the local path applied it to a
  local candidate (`applied`), and when a loop consolidator call read it (`read_at`). Two indexes:
  by event, and unread by target row.
- `loop_update_scan` — one row: the last event id the outcome pass read.

No existing table, column, trigger or index changes. `loop_updates.quote` is captured text and is
deleted with its event (ON DELETE CASCADE), by "clear extracted state" and by "delete everything".

## Rollback

Do not delete migration-history rows or run a pre-V104 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V104 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data impact

Restoring loses the updates recorded after the snapshot and the effects the local path applied
with them: local candidates (always below 0.5, never shown as fact) that a later message closed or
re-dated are open again with their first due date. The state rows themselves were changed only in
`status`, `due_at`, `updated_at` and `actor_last`, plus one `state_provenance` link to the event
that said so (`batch_reviewed = 0`); no confidence was raised and nothing was deleted. Nothing was
sent anywhere by this migration or by the pass that writes these tables.
