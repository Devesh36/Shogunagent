# V37 — extracted knowledge entities (#436)

Adds nullable extraction keys to existing people/projects and new topic/tool/organization/self records with event provenance. No old records are rewritten, deleted or backfilled. About You retains the explicitly saved profile; separate self statements can only come from entire first-person notes in the existing `user`/`note` source, never from screen/web text. Existing search-document invalidation triggers continue to cover people/projects.

## Verification

Run `cargo test -p shogun-memory --locked --lib knowledge::tests` and the classification/API checks. V36 fixtures must retain existing state and provenance after upgrading; the normal startup path makes a pre-migration snapshot before applying V37.

## Rollback

Do not delete migration-history rows or launch a pre-V37 binary against an upgraded live database. Stop all Shogun processes, preserve the upgraded encrypted database and its matching Keychain key, and use the app's existing verified backup/restore workflow to restore the pre-upgrade snapshot before using an older binary. Restoring that snapshot omits writes made after the snapshot; retain the upgraded copy for recovery. No destructive downgrade SQL is supplied.

## Bounded rollout and backfill

The normal nightly classification window gains at most twelve entities per event and uses the existing company batch lane, source filtering, traceability and budget gates. Already-completed windows are not reset. In-flight older results without `entities` remain valid.

Backfill is deliberately not automated: first choose an explicit retained date window, preview eligible event count and estimated cost, and obtain confirmation before submitting through the same lane. Keep batches within the existing scheduler limits and persist provenance-backed results through the same idempotent path. No direct live reclassification has been performed for this change.

Exact name plus exact disambiguating context controls merging; fuzzy alias/entity resolution is deferred. This may conservatively split one entity whose context wording changes, but will not merge people merely because their names match. New kinds stay at medium confidence and are review-only, not automatically injected into agent context. They are searchable/filterable in Wiki; general search-document projection for new kinds is not added in this change.

Automatic pre-upgrade copies exist only within the configured [backup rollback window](verified-encrypted-memory-backups.md#rollback). Preserve the required copy outside app-managed rotation before rollback.
