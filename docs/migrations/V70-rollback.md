# V70 rollback — send traceability destination names the service

V70 rewrites `traceability_log.destination` for rows whose `purpose` is one of the nine
`integration.*` send purposes (issue #924). Before V70, a confirmed send's row held the approval
card's destination: an email recipient's address, a calendar event's or document's title, a Slack
channel, an issue reference. After V70 those rows hold the service id the read path already
records (`gmail`, `gcal`, `gdrive`, `slack`, `notion`, `github`, `linear`).

| purpose | destination after V70 |
|---|---|
| `integration.send_email` | `gmail` |
| `integration.post_message`, `integration.add_reaction` | `slack` |
| `integration.create_calendar_event`, `integration.update_calendar_event` | `gcal` |
| `integration.post_comment` | `github` |
| `integration.create_document` | `gdrive` |
| `integration.update_document` | `notion` |
| `integration.change_issue_status` | `linear` |

No schema change. No row is deleted. `ts`, `route`, `purpose`, `chunk_bytes`, `chunk_xxh64`,
`third_party` and `kind` are untouched, so the viewer, the egress counts and the content
fingerprints read the same as before. Every other row (reads, Gmail drafts, Gmail label changes,
model calls, ASR, billing, support, web) is left alone.

## Rollback

No schema change, so there is nothing structural to undo. The old destinations are **intentionally
not recoverable**: they are the private data V70 removes.

The migration runner rejects a database whose history names a version it does not know, so an
older build will not open a V70 database until the history row is removed:

```sql
DELETE FROM refinery_schema_history WHERE version = 70;
```

The older build then works unchanged (same columns, same CHECKs) and writes the old style of
destination for its own new sends. Opening the database again with a V70 build reruns V70, which
is idempotent and rewrites those rows too. Verify with `PRAGMA quick_check;` before reopening.

A pre-V70 backup restored with its matching application version still holds the old
destinations; a V70 build rewrites them when it opens that backup.

## Residual copies

SQLite rewrites the pages in place but freed pages and the WAL can hold the old bytes until they
are reused or checkpointed. The database is encrypted at rest, so those bytes are never readable
without the key. The pre-upgrade snapshot the desktop takes before migrating, and any encrypted
backup made before V70, keep the old values until backup rotation replaces them.
