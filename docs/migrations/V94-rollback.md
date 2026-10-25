# V94 rollback — tool_runs

V94 adds `tool_runs`, the router's run ledger (timestamp, actor, tool id, optional task hint,
outcome, cost in micro-dollars, latency, whether a hint was followed). It holds no input or output.
No existing table changes.

## Rollback

Do not delete migration-history rows or run a pre-V94 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V94 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the run ledger rows after the snapshot; `discover` falls back to the published
score without the personal term. Tool runs already settled with a provider are not reversed.
