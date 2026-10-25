# V39 — indexed knowledge-context lookup

V39 adds an external-content FTS5 trigram index over `knowledge_entities.name` and
`knowledge_entities.context`. Existing rows are indexed during migration. Insert, update, and
delete triggers keep the index in the same transaction as canonical writes. Quotes and source
event text are not copied into the index.

## Rollback

Do not delete migration-history rows or run a pre-V39 binary against an upgraded live database.
Stop Shogun, preserve the upgraded encrypted database and matching Keychain key, then restore the
pre-upgrade snapshot through the existing verified backup/restore workflow before using an older
binary. The restored snapshot omits later data; retain the upgraded copy for recovery. No
destructive downgrade SQL is supplied.

Automatic pre-upgrade copies exist only within the configured [backup rollback window](verified-encrypted-memory-backups.md#rollback). Preserve the required copy outside app-managed rotation before rollback.
