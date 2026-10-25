# V69 rollback — state provenance quotes

V69 adds nullable `state_provenance.quote` and `batch_reviewed` (default false). Existing links remain NULL: their precise supporting
span is unknown. Source IDs, raw events, weights, state IDs, paid-request identities, and pins are
unchanged. No backfill invents a quote or model review, and no source rows are deleted.

The added fields are structurally compatible with older SQL that names its columns, but older
binaries neither preserve new quotes on every write nor apply the live quote checks. Do not treat
an older reader as providing the new evidence guarantees. No destructive in-place downgrade is
provided; never delete migration-history rows to force a downgrade.

To restore an older application/database pair, stop Shogun, retain the upgraded encrypted database
and its WAL/SHM together offline, and use the supported backup/restore workflow with a verified
pre-V69 backup and its matching application version. Do not copy a live SQLite database alone.
Verify the restored database's migration history and `PRAGMA quick_check` before resuming capture.
Keep the upgraded copy for recovery of newer captures and quotes. Restoring a backup does not undo
provider work; retain its paid-request records and do not replay work merely because an older copy
lacks the latest ledger entries.

The unpublished QA branch originally numbered this change V67. Main now owns V67 for
shared-service observability, and V68 for event-link indexes. Databases created by that older QA build have incompatible
migration history: keep them separate and use a fresh QA data suffix for this build. Do not
relabel their history or use this renumbering as a repair for an existing database.

Review follow-up: V69 also adds `wording_edited` to the four state tables. Existing pins are
conservatively marked as wording edits because older rows lack field-level edit history. New
date/status-only edits preserve live source links. No source content or pin is removed.

New brief payloads may omit a provenance event for a user-authored correction. Older binaries
that require that field cannot decode these payloads. Roll back the matching application and
verified pre-upgrade backup together, never just the binary or migration ledger. As V69 is
still unpublished, QA databases from earlier revisions of V68 or V69 also need a fresh isolated data
suffix or a matching backup; their checksum must not be rewritten.

V69 also adds `idx_state_provenance_source` so readers can stop at the earliest valid link
without sorting all linked source bodies. Every provenance link remains stored; complete
search evidence still validates every link.
