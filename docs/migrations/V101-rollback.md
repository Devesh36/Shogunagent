# V101 rollback — audit log and access grants

V101 adds three tables and one index (ShogunAgent E18):

- `audit_log` (`id`, `ts`, `actor`, `principal`, `route`, `level`, `outcome`, `request_id`) and
  the index `audit_log_ts` on `(ts, id)`: one row per write and per approval decision. It holds no
  body, argument value or row text.
- `scratch_acl` (`grantee`, `namespace`, `access`, `granted_by`, `granted_at`; primary key
  `(grantee, namespace)`): which agent may read or write another agent's `agent_scratch:<id>`
  namespace.
- `principal_grants` (`grantee_principal`, `scope`, `granted_by`, `granted_at`; primary key
  `(grantee_principal, scope)`): which other principal of the same team may read this principal's
  `world` state (Team).

No existing table, column, trigger or index changes.

## Rollback

Do not delete migration-history rows or run a pre-V101 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V101 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses the audit records written after the snapshot and every grant made after it: an
agent that was granted another agent's scratch, and a team member granted read access to this
principal's world state, lose that access until it is granted again (the safe direction). Nothing
was sent to a provider by this migration.
