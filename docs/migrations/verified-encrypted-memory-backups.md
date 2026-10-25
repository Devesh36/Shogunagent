# Verified encrypted memory backups

This change adds encrypted SQLite Online Backup snapshots before Dream mutation and before any
pending refinery migration. It does not add or alter a migration.

## Storage and retention

- Snapshots live in `<app-data>/backups/` (`0700`). Temporary and published database files are
  `0600`.
- Files are immutable `backup-<unix-ms>-<nonce>.db` generations. Temporary files remain hidden and
  are never recovery candidates.
- SQLCipher keys the destination before `sqlite3_backup_init`. No plaintext SQL dump, key file,
  copied WAL, or copied SHM file is produced.
- A snapshot closes, syncs, reopens read-only with `SQLITE_OPEN_NOFOLLOW`, consumes SQLCipher and
  SQLite integrity checks, requires zero foreign-key violations, and reads refinery history before
  atomic publication.
- Rotation starts only after verified, directory-synced publication. One verified
  replacement remains (owner decision 2026-09-27). Old copies rotate without re-reading their contents;
  a failed directory listing or deletion reports `rotation_pending`. Launch and periodic sweeps
  retry unfinished count rotation, keeping a verified copy before deleting extras. Replacement
  temporarily needs space for both the old and new full database copies. An upgrade retains one
  additional pre-upgrade baseline within the configured age cap; during replacement this can
  temporarily require three copies. Inconclusive verification may retain extra young files.
- Published nonces include a `G<sequence>` suffix. Cleanup prefers publication order over
  wall-clock time, including after interrupted rotation and restart. Legacy names remain readable;
  timestamps still determine age expiry. The suffix stays alphanumeric so older builds still
  recognize these backups for listing and deletion.

## Dream and migration gates

Mutation-free Dream gate checks do not create backups. A scheduled full/degraded run or manual
degraded run must have one verified snapshot from the same local day whose recorded schema version
matches the current live schema. Failure leaves Dream pending and does not change capture/search
health.

An existing encrypted store opens keyed without create or migration. Any refinery history other
than the current one requires a verified snapshot first; failure aborts before refinery writes.
That old-schema snapshot never satisfies the post-migration Dream gate. Only an upgrade (older
history) takes a tagged pre-upgrade baseline: its filename records the target migration as
`M<version>` before `G<sequence>`. A newer history (a downgraded app) or an unreadable one takes a
plain verified snapshot, exactly as before tagged baselines existed
(`shogun_memory::backup::StartupSnapshot`). Retries for that target reuse the original verified
baseline even after partial migration only while it is the newest publication; that is also the
baseline rotation protects. A newer daily copy starts a new upgrade attempt. A tagged copy at or
above the target schema (left by an earlier build on a downgraded store) is not a retry baseline
and is replaced by a fresh copy, never an error.
A baseline that fails verification permanently (content that does not decrypt or pass the
integrity pragmas, wrong mode, extra hard links) is renamed with an `R` suffix before `G`,
preserving its bytes and SQLite side files for one configured retention window from detection,
and one content-free log line records it. A fresh verified snapshot must then succeed before
migration proceeds; that publish rotates nothing away, so an older verified copy left by an
interrupted rotation survives it. Failure leaves startup blocked but preserves the uncertain
recovery copy. A transient failure (I/O, locking, memory) does not set the baseline aside: the
upgrade aborts and the next launch verifies the same baseline again. Recovery copies survive count rotation and restart, expire under this
grace period, and are removed by Delete everything. Normal rotation and sweeps preserve the latest upgrade baseline until its configured age cap. A newer
upgrade can replace that protection; this is not an indefinite archive of all old schemas.
Delete everything includes tagged copies. Earlier app versions recognize the names for deletion,
but do not understand their rotation protection.

## Recovery behavior

Missing/malformed/unavailable Keychain state plus any existing `memory.db`, `memory.db-wal`, or
`memory.db-shm` artifact enters fixed `recovery_required`. Wrong key, corruption, I/O failure, or
integrity failure does the same. Startup does not rename a live artifact, mint a replacement key,
or create a blank store.

Use **Open backup folder** to preserve/export immutable generations before support work. Recovery
must happen with no active database handle and must revalidate the selected generation with the
existing Keychain key. Automatic live-file replacement is forbidden.

Current limitation: the explicit confirmed restore command (revalidate generation, quarantine
DB/WAL/SHM, publish a verified restore temp, then restart) is not included because production code
for that destructive replacement requires separate explicit user authorization. Until authorized,
support-assisted restoration is required; do not manually overwrite an open `memory.db`.

## Rollback

This backup change itself has no schema migration. For a schema rollback, stop all app/sidecar
processes and preserve the verified pre-upgrade copy outside app-managed rotation before launching
an older binary. The automatic rollback window is bounded by backup retention (default seven
days), later upgrades, and explicit deletion. Do not promise every historical generation exists.
Use the matching migration's rollback instructions; never overwrite an open live database.

## Verification and latency limits

Runtime backups open a separate keyed, read-only source connection. Copying no longer holds the
shared capture/search mutex. A read transaction pins the copy so new captures cannot restart it;
it ends and its connection closes before verification. Full deletion waits for the copy reader
to close before deleting rows and running VACUUM, so it cannot pin pre-wipe pages in the WAL.
Pre-migration copying still happens before capture starts. The 120-second deadline bounds copying, not post-copy integrity verification. SQLite I/O and WAL
checkpoint contention still require device measurements.

Local release-mode probe (2026-09-29), 64 MiB encrypted fixture and 100 capture/search pairs:
capture p50/p95 = 0.280/0.544 ms; search p50/p95 = 0.426/0.685 ms. Every sampled operation
was below 500 ms. This synthetic probe also checks snapshot consistency during concurrent writes;
it does not establish latency for a real multi-GiB store. Run it with
`cargo test -p shogun-core --features db --test backup --release --locked capture_and_search_remain_available_during_backup_copy -- --ignored --nocapture`.

An inconclusive verification is reported separately from unlink failure. Young unverified newer
copies are retained alongside the verified keeper. The desktop sweep backs off to at most daily
rechecks; a successful verification resets the normal 30-minute interval. Age cleanup may be
delayed during that backoff; publication and explicit retention changes still attempt cleanup.

SQLCipher 4.5.3's whole-file integrity scan uses a 32-bit offset intermediate. Above 4 GiB the
app emits a content-free, once-per-process notice and checks active pages with quick_check and
foreign_key_check instead. Unused/freelist pages are not HMAC-checked in this fallback.
Follow-up (#706): upgrade the bundled SQLCipher/rusqlite dependency, prove encrypted-file and
rollback compatibility, and run a real >4 GiB integrity fixture before removing the guard.

## Device validation still required

Before reliability sign-off on a signed macOS build:

1. Measure maximum capture/search wait during the bounded source-copy window.
2. Kill the app during copy and after publication; confirm a prior verified generation survives.
3. Exercise active WAL writes, Keychain denial, disk full, read-only backup directory, and corrupt
   live store without background prompts or content-bearing logs.
4. Check backup directory/file modes and confirm neither plaintext headers nor captured text appear
   in backup bytes.
