# V103 rollback — approval ledger

V103 adds two tables for remote approvals (E15) and their execution:

- `approval_decisions` — one row per decision on an approval of this principal: the approval id,
  `approve` / `reject`, who decided (`decided_by`, an actor id), when, how (`token` or `admin`),
  the tier, and the execution state (`none`, `pending`, `executing`, `sent`, `failed`,
  `unavailable`) with a fixed failure class and the time it settled.
- `approval_spent_nonces` — every decision-token nonce consumed, with its approval id and the
  token's expiry, plus an index on the expiry.

No existing table, column, trigger or index changes. Neither table holds a recipient, a body or a
provider message: ids, labels and times only.

## Rollback

Do not delete migration-history rows or run a pre-V103 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V103 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the decisions and spent nonces recorded after the snapshot. The older build keeps
them in process memory only, so an unexpired decision token whose nonce was spent after the
snapshot could be presented once more until it expires (at most the 10-minute approval window);
rotate `SHOGUN_APPROVAL_SIGNING_KEY` when restoring to close that window. An approval whose
execution was recorded after the snapshot is not re-run by the older build (it has no executor);
check the provider for the action's outcome before asking for it again. Nothing was sent to a
provider by this migration.
