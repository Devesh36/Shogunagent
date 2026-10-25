# V90 rollback — dictionary auto-add source and group

V90 adds two columns to `dictionary_autoadd_corrections`: `source` (`local` or `model`, default
`local`) and `group_id` (the notch notice an addition belongs to, so one Undo removes the group).
Old rows read as `local` with no group. It also adds `dictionary_autoadd_settings.user_set`: auto-add
is on unless the user turned it off. A Mac where it was turned on and later off is marked as set
(stays off); every other Mac reads as never set, so auto-add is on after the upgrade.

## Rollback

Do not delete migration-history rows or run a pre-V90 binary against the upgraded database (the
older app refuses a database with a migration it does not know). Quit Shogun, keep the upgraded
encrypted database (with its WAL/SHM files) offline, and restore a verified pre-V90 snapshot
through the backup/restore workflow with the matching older app version. Then check
`PRAGMA quick_check` returns `ok` before resuming capture.

## Data and provider impact

Restoring loses only auto-add rows written after the snapshot (at most 24 hours of them are kept
anyway); the dictionary terms they created stay in `voice_terms` only if the snapshot has them.
Model checks already sent through the relay are not recalled; nothing was stored there.
