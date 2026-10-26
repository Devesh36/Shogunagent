# Agent payments: mandates and spends

A **payment mandate** is a human's pre-approval for one agent to pay within bounds: an exact payee
or a payee class, a currency, a maximum per payment, a total cap, an expiry, a purpose and,
optionally, one payment only. With a mandate in place the agent pays without asking each time;
outside it, every payment waits for a human. Plan E46. Implementation: the Rust core
(`crates/shogun-agents/src/payment.rs` for the bounds, `crates/shogun-mcp/src/payments_api.rs`
for the flow, `crates/shogun-memory/src/payment_mandates.rs` and migration V105 for the store,
`crates/shogun-core/src/payment_rail.rs` for the rail seam).

**No real payment provider is integrated.** The only rail in this build is a stub that moves no
money; see [A real rail](#a-real-rail) for what one needs.

## Levels

A payment is L3 by type (invariant 3): `SendAction::Pay` is a send, and no L1 path can represent
it. A mandate does not lower that level; it moves *when* the L3 confirmation is given, the way a
standing approval does for an L2 change. The human confirms the bounds once, on the approval page;
each payment inside them carries that confirmation, and its record names the decision.

| Tool | Level | What happens | Cost |
|---|---|---|---|
| `payments.mandates.list` | read | The mandates, their bounds, spent and remaining, status (`active`, `expired`, `exhausted`, `revoked`) and the approval that granted each. With `mandate_id`: that mandate and its recent spends. | free (read) |
| `payments.mandates.request` | L3 | A new mandate, or wider / longer terms for an existing one, is queued for a human: the answer is `approval_required` with the approval URL. Nothing is granted until a human approves. Terms that only **narrow** an existing mandate apply at once. | `run`, free at the call |
| `payments.mandates.revoke` | L1 | Revoked at once, from any face. Revoking twice is harmless. | `run`, free at the call |
| `payments.spend` | L3 | Inside an active mandate granted to the caller: executes through the payment rail and is recorded against the mandate. Otherwise: queued as a per-payment L3 approval (URL) and executed only after a human approves. | `run` at the rail's fee (0 with the stub) |

- **Who grants.** Only a human decision (the signed approval link the gateway shows, or the
  operator's admin credential) releases a mandate grant; an agent's bearer on the decide route is
  `403 human_only`. The store then refuses to write a mandate unless the principal's approval
  ledger (V103) holds an `approve` decision for that approval id, and takes `granted_by` from that
  row: no code path stores a mandate a human did not approve.
- **What widens.** One comparison (`MandateTerms::widens`): a different payee scope or currency, a
  higher per-payment or total cap, a later expiry, a reusable mandate from a single-use one, or a
  changed purpose (the purpose is what the human read). Anything else is a narrowing.
- **Who spends.** Only the agent the mandate was granted to (the actor that requested it). A
  class mandate is checked against the class the **rail** reports for the payee, never a class the
  caller asserts.
- **Out of bounds.** A payment no mandate covers falls back to a per-payment approval and says why
  (`reason`: `no_mandate`, `over_per_payment`, `over_total_cap`, `payee_not_covered`,
  `currency_mismatch`, `single_use_spent`, `mandate_expired`, `mandate_revoked`,
  `mandate_not_yours`).

## Faces

The same four tools on every face, at the same level and with the same answer (invariant 4).

| Face | Call |
|---|---|
| MCP | `tools/call` with the arguments below (toolset `payments`, listed by default) |
| REST (sidecar) | `GET /v1/payments/mandates`, `POST /v1/payments/mandates/list`, `POST /v1/payments/mandates/request`, `POST /v1/payments/mandates/revoke`, `POST /v1/payments/spend`, the arguments as the body |
| REST (hosted gateway) | the same five routes at `https://api.shogunagent.dev`, with an agent key |
| CLI | `shogun payments mandates list [<json>]`, `shogun payments mandates request <json>`, `shogun payments mandates revoke <json>`, `shogun payments spend <json>` |
| SDKs | TypeScript `client.payments.{mandates.list, mandates.request, mandates.revoke, spend}`; Python `client.payments.{list_mandates, request_mandate, revoke_mandate, spend}` (MCP underneath; they never approve) |

The hosted gateway reaches them through `/mcp` and through its own REST routes of the same paths
(E51, `PAYMENT_ROUTES` in `apps/api/src/agent-app.ts`): each route is pinned to its tool, so the
scope (`read` for the list, `run` for the rest) and the price come from the path, never from a
header the caller sends; the request is handed to the principal's sidecar at the same path through
the `/mcp` relay, and neither body is read, logged or stored. A queued L3 answers `202` with the
approval URL exactly as the sidecar does.

### `payments.mandates.request`

```json
{
  "payee_class": "cloud",
  "currency": "USD",
  "max_per_payment_minor": 5000,
  "total_cap_minor": 20000,
  "expires_in_days": 30,
  "purpose": "Build minutes for the release",
  "single_use": false,
  "idempotency_key": "release-42-mandate"
}
```

Give exactly one of `payee` (an exact payee id) and `payee_class`. Amounts are integer **minor
units** of the ISO 4217 currency (`5000` USD = $50.00). `expires_in_days` (1–366) or
`expires_at_ms`. Add `mandate_id` to change an existing mandate. The new mandate's id is derived
from the caller and the idempotency key, so a retry maps to the same mandate (and, once granted,
answers it with `replayed: true`).

### `payments.spend`

```json
{ "payee": "cloud:hetzner", "currency": "USD", "amount_minor": 1250, "memo": "June invoice", "idempotency_key": "inv-2026-06" }
```

`mandate_id` is optional: without it the first active mandate of the caller that covers the
payment is used. The memo travels to the approval page and the rail and is never stored.

Inside a mandate:

```json
{ "tool": "payments.spend", "status": "settled", "authorization": "mandate", "mandate_id": "pmd_…",
  "spend": { "spend_ref": "psp_…", "actor": "agent:claude-code", "idempotency_key": "inv-2026-06",
             "approval_id": 7, "decided_by": "human:owner", "amount_minor": 1250, "currency": "USD",
             "status": "settled", "rail": "stub", "rail_ref": "stub_psp_…", … } }
```

Outside every mandate it is the pending envelope (`202`, `approval_id`, `url`,
`next_action: "approve_at_url"`) plus `spend_ref`, `authorization: "approval"` and `reason`. Poll
`actions.status`; once a human approves, the payment runs and the spend row settles. If the human
**rejects** it, or the approval window (10 minutes) elapses first, nothing is paid and the spend
reads `status: "rejected"` / `"expired"` with `decided_by` (the rejecting human, or
`system:approval-timeout`) and the `approval_id`; a retry with the same key answers that with
`next_action: "ask_human"` (E51). A retry
with the same key answers the recorded spend (`replayed: true`) and never pays twice. A payment
that reached the rail and failed answers `502 payment_failed` with the spend's `reason_class`.

## What is recorded

In the principal's own store (one SQLCipher file per principal, invariant 6), migration V105:

- `payment_mandates`: the terms, `active` / `revoked`, `grant_approval_id` and `granted_by` (the
  human decision), `revision` (each widening or narrowing), `revoked_by` / `revoked_at`.
- `payment_spends`: one row per payment asked for, unique on `(actor, idempotency_key)`, written
  **before** the rail is called: the actor, the authorization (`mandate` with the grant's
  approval id, or `approval` with the per-payment decision and who made it), payee, currency,
  amount, status (`awaiting_approval` → `executing`, or `authorized`, then `settled` / `failed`;
  `not_queued` when the approval could not be queued), a fixed failure class and the rail's
  reference. A per-payment approval that a human rejects or that times out closes its row
  (`awaiting_approval` → stored `failed` with `reason_class` `rejected` / `expired`, reported as
  that status; the V105 status set is frozen and `failed` counts nothing against a cap), recording
  who (`decided_by`), which approval and when. The decide route closes it at the decision; every
  `payments.mandates.list` and `payments.spend` first reconciles waiting rows against the
  approval ledger (a recorded `reject`) and the approval window, so a row the decide route could
  not close (a store fault, an approval that simply expired) still agrees with `actions.status`.
  Closing is idempotent, and a closed row can never be claimed for payment. A mandate's spent total counts `authorized`, `executing` and `settled` rows, re-read
  inside the reservation's transaction, so two concurrent payments cannot both pass the cap.
- The traceability log: every payment that may have reached the rail has a row written before the
  rail is called (`purpose` `payments.pay`, `destination` `payment_rail`, the body digested and
  dropped). No row, no payment.
- The audit log: every request, revoke and spend, and every decision, with actor, tier and outcome.

No memo, no rail response and no body is stored anywhere else. The gateway, router, logs and
metrics carry none (invariant 2).

## Billing

Reads are free (invariant 10). `payments.mandates.request` and `payments.mandates.revoke` are
`run` calls (they need the `run` scope: an agent that may spend) priced at 0: a request only queues
an L3 for a human and a revoke only narrows. `payments.spend` is a `run` priced at the rail's fee;
the stub rail has none, so it is 0 too (`FREE_RUN_TOOLS` in `apps/api/src/pricing.ts`). The amount
paid to the payee is never billed by ShogunAgent and never passes through its wallet.

## A real rail

The seam is `PaymentRail` in `crates/shogun-core/src/payment_rail.rs`, used through
`RailTransport` and `send_exec::execute_send` like every other L3 send. The operator selects a
rail with `SHOGUN_PAYMENT_RAIL` (only `stub` exists) or installs one at the composition root
(`payment_rail::install`). Without one, `payments.spend` answers `503 payment_rail_unavailable`
before anything is recorded. A real rail needs:

1. **A credential injected at the transport** (the principal's account with the provider),
   resolved per principal from the key provider or the connector token store, never visible to an
   agent, the router or a log (invariant 7). Its HTTP egress goes through `shogun-core`'s one
   client (invariant 9).
2. **Idempotency on `spend_ref`.** The rail must accept it as the provider's idempotency key, so a
   crash between "sent" and "settled" can be retried without paying twice. Until then, a spend left
   `executing` / `authorized` has an unknown outcome and is never re-run automatically.
3. **Payee classification from the provider's own data** (merchant category, verified account
   type) for class mandates; the stub reads the `<class>:` prefix of the payee id.
4. **Asynchronous settlement.** Most rails confirm later; the rail answers "accepted" and a signed
   provider callback settles the row. The gateway would relay that callback without reading its
   body, like the other webhooks.
5. **Refunds, reversals and disputes** as their own L3 actions, recorded against the spend.
6. **Pricing.** Remove `payments.spend` from `FREE_RUN_TOOLS` and price the run at the rail's fee
   (`runPricer`); the payment amount itself is never part of the run price.
7. **Compliance review** before any production use (see ADR 0002 on x402 for the same questions:
   money transmission, KYC of the principal, sanctions screening).

## Out of scope here

x402 (`docs/decisions/0002-x402.md`) is how an agent pays ShogunAgent; mandates are how a principal
lets its own agents pay others. They are separate, and nothing here changes the wallet, the caps
or the 402 flow.
