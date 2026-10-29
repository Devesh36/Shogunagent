# V106 rollback — namespace-scoped state suppressions

V106 adds `state_suppressions_scoped`, keyed by state table, namespace and match digest.
New corrections and deletions write only this table, using the affected row's stored namespace
inside the mutation transaction. Extraction checks its destination namespace, which can differ
from the source event's namespace. Evidence captured at or before the suppression timestamp stays
suppressed; evidence captured later is still allowed. Repeated suppressions retain the latest
timestamp. Neither suppression table stores the removed wording.

The V49 `state_suppressions` table stays unchanged. Its rows contain neither a namespace nor a
state-row id. Deleted rows and their provenance are gone; export tombstones do not associate a
digest with a namespace. No reliable backfill is possible. Readers therefore retain those rows
as legacy-global suppressions with their original timestamps. This deliberately preserves old
hard-delete protection, including the historical possibility of suppressing matching old evidence
in another namespace. New scoped rows never broaden or extend a legacy-global suppression.
The principal-wide `delete_all` operation clears both tables.

## Rollback

Do not drop either suppression table or remove migration-history rows. A pre-V106 binary ignores
the new table and can resurrect beliefs corrected or deleted after upgrade. Stop all writers,
retain the upgraded encrypted database and its WAL/SHM files offline, and restore a verified
pre-V106 snapshot with its matching older build. Check `PRAGMA quick_check` returns `ok` before
resuming writes. Restoring loses all changes after the snapshot, including deletion/correction
intent; review those changes before resuming extraction. Prefer a forward fix when that intent
must be retained.

## Verification

The suppression regression tests cover destination namespace isolation, all four state tables,
old versus fresh evidence and repeated cutoffs. The V105 upgrade-lineage regression preserves
legacy rows byte-for-byte, checks legacy and scoped behavior after reopen, and repeats the upgrade
from a restored synthetic pre-V106 snapshot. No user data or external providers are involved.
