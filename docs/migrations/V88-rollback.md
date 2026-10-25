# V88 rollback — Evening Wrap generation ledger

V88 adds `wrap_generation` (the Morning Brief's `brief_generation` ledger, for the Evening Wrap's
Batch-written lines) and four triggers: a fresh request id per insert, handle/prose cleanup on
expiry, and invalidation when a cited `event_log` row is edited or deleted. It changes no existing
table.

## Rollback

Do not delete migration-history rows or run a pre-V88 binary against the upgraded database. Quit
Shogun, keep the upgraded encrypted database (with its WAL/SHM files) offline, and restore a
verified pre-V88 snapshot through the backup/restore workflow with the matching older app version.
Then check `PRAGMA quick_check` returns `ok` before resuming capture.

## Data and provider impact

Only generated Evening Wrap lines and their source manifests are lost by restoring; the local Wrap
sections are computed live and are unaffected. An in-flight relay Batch for a wrap is abandoned
(it is never resubmitted). Dropping the table in place does not undo provider work, so no
destructive rollback SQL is supplied.
