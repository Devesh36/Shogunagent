# State correction and deletion suppression

V106 scopes new suppression records to the affected state row's stored namespace. Correcting
an alpha scratch commitment cannot suppress a same-word commitment subsequently extracted into
world or beta scratch. Editing as the owner, or with a call destination different from the row's
namespace, still records the affected row's namespace. The existing mutation authorization checks
remain necessary: suppression scope does not grant permission to edit or delete a row.

The digest key remains `(state_table, trimmed extraction key)`: description for commitments and
open loops, `extraction_key` for people and projects. Suppression stores no raw wording. The new
table's uniqueness also includes namespace. Its cutoff is inclusive (`event.ts <= suppressed_at`),
and repeating a suppression never moves its cutoff backward. Later source evidence remains
eligible even when it has the same wording. A later processing clock does not make old evidence
new.

The extraction readers in `extract` and `knowledge` use the current writer's destination
namespace, matching their new-row inserts. The loop consolidator's new-row path uses its explicit
`WriteContext` and the source event timestamp returned by quote grounding. Its pre-existing
closed-source guard covers closed rows; the suppression check additionally covers hard-deleted
rows and a corrected row's previous wording. Both checks happen before inserting a replacement.

## Compatibility and limits

Pre-V106 suppression records have no namespace or state-row id. Their old global behavior and
original timestamps are preserved, without guessing a mapping from tombstones or surviving rows.
See [V106 migration and rollback](migrations/V106-rollback.md). A newer scoped deletion never
extends a legacy-global cutoff. Principal-wide deletion clears both suppression tables.

This change covers the suppression readers and new-row paths described above. It does not make
entity identity or all consolidator operations namespace-isolated: people/project extraction
keys still have global unique indexes, and consolidator existing-row lookup/re-list paths
(`same_text`, `same_source`, explicit ids) have separate namespace behavior. Suppression does not
alter existing-row re-listing. Tests for entity recreation and consolidator insertion use separate
fixtures for world and beta destinations; they do not establish independent identity resolution
when matching rows already coexist. Commitment/open-loop extraction tests additionally retain
same-word world, alpha and beta neighbors during correction/deletion.

The key is exact after trimming, not semantic: a differently worded paraphrase or a different
state table is a different key. Identity-created people without an extraction key still have no
suppression key. Those existing limitations are not expanded into inferred text or identity
reconstruction by this migration.
