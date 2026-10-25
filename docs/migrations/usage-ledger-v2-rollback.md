# Gateway usage ledger schema 2: migration and rollback

This is the gateway ledger, not a desktop memory-database migration. The versioned migration is
`SqliteUsageStore.migrate` in `apps/api/src/usage.ts`; `usage_meta(id=1, version=2)` is its commit marker.
No user content or credentials enter these tables.

## Cutover

1. Stop every gateway writer using JSON or schema-1 SQLite. Keep submissions disabled during cutover.
2. Back up the authoritative `USAGE_FILE.sqlite` with SQLite's backup mechanism, or stop writers and
   preserve the database plus committed WAL state. Preserve `USAGE_FILE.initialized` and legacy JSON.
3. Start the schema-2 image on the same volume. Under one `BEGIN IMMEDIATE`, startup prefers the
   existing schema-1 `ledger(id=1).state`; only an uninitialized store may import legacy JSON.
4. Migration inserts `usage_days` (date/licence primary key), `usage_idem` (licence/device/purpose/request
   primary key and indexed relay/provider alias) and `usage_owners` (batch primary key, date index).
   It preserves bound, pending, submitting/unknown and retired identities, counters, handles and devices.
   The external initialized guard is fsynced before committing version 2. The obsolete blob table is
   dropped in that same transaction, so older code cannot read an apparently valid but stale snapshot.
5. Verify migrated counters, known-job recovery and same-device ownership before enabling submissions.
   Migration tests cover malformed-input rollback, old SQLite precedence over stale JSON, preservation
   of all identity states, and loss of initialized metadata. No live Fly cutover is claimed by these tests.

Malformed data rolls back the SQL migration and retains the original blob for offline repair.
If a fresh import loses power after guard creation but before commit, the guard intentionally blocks
JSON reimport. Inspect and restore authoritative state offline; do not remove the guard to reset it.

## Rollback

A code rollback is safe only to an image that understands schema 2. Earlier SQLite images use the
single-blob table and are incompatible, just like JSON-only images. Keep submissions disabled if no
compatible rollback image is available.

A pre-cutover backup may be restored only before any post-cutover reservation or provider submission.
After new work is accepted, restoring that backup loses spend, permanent identities and jobs; it can
permit duplicate paid submissions. Repair or forward-fix the current schema-2 database instead.
Never export back to JSON, delete tombstones, remove the initialized guard or restore away unknown jobs
as a retry mechanism. With writers stopped, restore only a current authoritative backup and its guard.

The gateway still needs one shared local volume; normalized SQLite does not share state across machines.
Permanent tombstones continue to consume disk, but normal requests no longer read or rewrite all history.
Monitor volume space and preserve tombstones; expiry applies only to old counters and unkeyed owners.
