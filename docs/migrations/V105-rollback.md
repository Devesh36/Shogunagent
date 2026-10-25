# V105 rollback — payment mandates

V105 adds two tables for agent payment mandates (E46, `docs/api/payments.md`):

- `payment_mandates` — one row per mandate a human granted an agent: the agent, an exact payee or a
  payee class, the currency, the per-payment and total caps (integer minor units), the expiry, the
  purpose, single-use, `active` / `revoked`, the approval id and the human (`granted_by`) whose
  decision granted it, the revision, who revoked it and when, plus an index on `(agent, status)`.
- `payment_spends` — one row per payment an agent asked for: its `spend_ref`, the actor and
  idempotency key (unique together), the mandate or per-payment approval that authorized it, who
  decided, the payee, currency and amount, the status (`awaiting_approval`, `authorized`,
  `executing`, `settled`, `failed`, `not_queued`), a fixed failure class, the rail and the rail's
  reference, plus an index on `(mandate_id, status)`.

No existing table, column, trigger or index changes. Neither table holds a memo, a rail response
or a body: labels, ids, amounts and times only.

The migration number is V105 because V104 was taken on the integration branch (open-loop
outcomes, `V104__loop_updates`) at the same time; the two merge as V104 then V105.

## Rollback

Do not delete migration-history rows or run a pre-V105 binary against the upgraded database. Stop
every process holding the principal's store, keep the upgraded encrypted file (with its WAL/SHM
files) offline, and restore a verified pre-V105 snapshot with the matching older build. Then check
`PRAGMA quick_check` returns `ok` before resuming writes.

## Data and provider impact

Restoring loses every mandate and every spend recorded after the snapshot. The older build has no
`payments.*` tools, so no agent can spend after the restore; a mandate granted after the snapshot
must be requested and approved again on the newer build.

Before restoring, export the `payment_spends` rows whose status is `settled` or `executing`: those
payments may have reached the payment rail, and the rail (not this store) is then the record of
what was paid. A spend left `executing` has an unknown outcome; check it with the rail by its
`rail_ref` / `spend_ref` before paying again. With the stub rail (the only rail this build ships)
no money moved.
