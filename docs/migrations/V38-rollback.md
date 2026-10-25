# V38 — dashboard activity metadata

V38 adds content-free dashboard metadata only: retained-event-linked daily captured-time credits
and daily successful Scribe/quick-inline counters. It does not backfill or reinterpret historic
`event_log.dwell_ms`; a dedup touch has lifetime dwell but no trustworthy historical daily
interval. New data begins after this instrumentation ships.

Captured time is a capped successful foreground sample, not hours worked or attendance. App
identity stays on the canonical retained event. `first_recorded_on` means the oldest retained
captured credit, not a proof of coverage; successful edit counts may exist without a capture row.

## Forget and wipe

Event-linked credits cascade when their canonical event is removed. Each daily bucket also stores
only its latest UTC completion timestamp. Range forget removes any credit or edit bucket whose
latest completion is at or after the requested cutoff, regardless of the timezone that supplied
the bucket's presentation date. This conservatively drops an overlapping daily bucket, including
a late dedup credit whose canonical event predates the cutoff. Full wipe explicitly removes both
metadata tables.

## Rollback

Do not delete migration-history rows or run a pre-V38 binary against an upgraded live database.
Stop Shogun, preserve the upgraded encrypted database and matching Keychain key, then restore the
pre-upgrade snapshot through the existing verified backup/restore workflow before using an older
binary. The restored snapshot omits later data; retain the upgraded copy for recovery. No
destructive downgrade SQL is supplied.

Automatic pre-upgrade copies exist only within the configured [backup rollback window](verified-encrypted-memory-backups.md#rollback). Preserve the required copy outside app-managed rotation before rollback.
