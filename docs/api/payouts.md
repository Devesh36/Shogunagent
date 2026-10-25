# Provider payouts

How a tool provider listed in the router gets paid: every settled run accrues to the provider named in its `provider.json`, a monthly job pays the balance through Stripe Connect Express, and the provider reads a statement per month. Plan: E29. Implementation: `apps/api/src/payouts.ts` (rules, in-memory store, monthly job), `apps/api/src/payouts-pg-store.ts` (Postgres, `apps/api/sql/provider-payouts.sql`), `apps/api/src/payouts-app.ts` (routes), `apps/api/src/billing/connect.ts` (Stripe Connect over `fetch`, no SDK); the router side is `apps/router/src/settlement.ts`, `apps/router/src/wallet.ts` and `apps/router/src/outbox.ts`; the receiving routes are `apps/api/src/router-wallet.ts`. Tests: `apps/api/test/payouts.test.ts`, `apps/api/test/router-wallet.test.ts` (the router engine against the real wallet routes), `apps/router/test/{settlement,wallet,outbox}.test.ts`. Amounts are integer micro-USD (1 USD = 1,000,000).

## 1. Accrual: the settlement seam

The router settles every run it reserved exactly once and sends the settlement to `POST /v1/internal/router/settlements` with `Authorization: Bearer <ROUTER_SETTLEMENT_TOKEN>`:

```json
{ "settlements": [
  { "format": "shogunagent-run-settlement/2", "run_id": "9f0c…", "receipt_id": "rcpt_run_9f0c…", "ts": 1792411200000,
    "tool_id": "acme.lookup", "provider_id": "acme", "first_party": false,
    "payout_account_ref": "acct_1Acme", "marketplace": null, "outcome": "ok", "quantity": 1, "provider_micro_usd": 1000 } ] }
```

- Content-free by construction: no input, no output, no actor, no key. Unknown fields are rejected.
- `outcome` is `ok`, `empty`, `provider_error` or `transport_error`. Only `ok` carries a price; failures and empty results settle at $0 and count as runs.
- `receipt_id` (E32) names the caller's wallet hold for this run, `rcpt_run_<run_id>` or null for a $0 run that held nothing; any other value is rejected, so a settlement can settle only its own run. The hold is settled once (the receipt's settled flag), at provider price + one take for a third-party run, the provider price for a first-party run, $0 for failures and empty results, discounted under an active commit.
- `marketplace` (E32) tags a tool imported from a marketplace catalog (`"monid"`): that marketplace executes the run and invoices us; the provider is never paid through Connect.
- Idempotent by `run_id`: `{ "recorded": 1, "duplicates": 0, "wallet_settled": 1, "accrual": "on" }`, then `{ "recorded": 0, "duplicates": 1, "wallet_settled": 0, "accrual": "on" }` for a redelivery. Up to 100 per request; one invalid entry rejects the request and records nothing. The wallet is settled before the accrual is recorded, so a crash between the two leaves the accrual to the redelivery, never a provider credited for a run the caller was not charged for. Without `PAYOUTS_STATE_SECRET` the route still settles wallets (`"accrual": "off"`).
- Format `shogunagent-run-settlement/1` (E29, without `receipt_id` and `marketplace`) is still accepted: accrual only.
- `payout_account_ref` comes from the provider's `provider.json` (`apps/router/connectors/schema.md` §9).

### The single-charge path (E32)

A router run is charged by this gateway's wallet and nowhere else. The router holds no balance and adds no price:

```
router run ──► outbox.open(draft $0)                                   router Postgres
           ──► POST /v1/internal/router/reserve                         apps/api
                 { run_id, tool_id, provider_micro_usd, first_party }
                 X-Caller-Authorization: Bearer <the caller's key>      verified, never stored
                 → list = provider + RUN_MARKUP_BPS (third-party) | provider (first-party)
                 → caps, commit discount, balance; hold as rcpt_run_<run_id> (or 402)
           ──► provider call (the transport)
           ──► outbox.put(settlement)                                   router Postgres
           ──► POST /v1/internal/router/settlements                     apps/api
                 → settle rcpt_run_<run_id> once at the measured price + one take ($0 on failure)
                 → accrue the provider (provider price, take)
```

- **The take is in one place.** `RUN_MARKUP_BPS` is `packages/config/pricing.json` `run_markup_bps`. apps/api's rate table (the wallet, `/.well-known/pricing.json`) and the router's quotes (`apps/router/src/take.ts`) both read it. Only the provider's price crosses the wire; the router never sends a caller price, so the take cannot be applied twice.
- **The Monid passthrough.** Imported Monid tools are priced at Monid's own price, the figure Monid invoices us (Monid bills the router's account, never the caller). Before E32 the importer added 5% and the wallet would have added 5% again (1300 µ$ → 1365 → 1434). Now the caller sees and pays 1365: Monid's price + one take. Whatever platform fee Monid includes in its own price is part of the provider price.
- **Reads stay free.** A $0 tool (the first-party state reads) reserves nothing and calls the wallet not at all; its settlement only counts the run, delivered by the retry job.
- **Fails closed.** If the router cannot write the draft, or cannot reach the wallet, the run is refused (`503`, `next_action: retry_after`) before the provider is called. A 402 or a 401 from the wallet is passed to the agent unchanged in substance.
- **Idempotent reserve.** A retried reserve for the same `run_id` returns the same hold (`"replayed": true`); a `run_id` already settled, or held by another principal, is a 409.

Each settlement is added to the provider's accrual for its UTC day (`provider_accruals`): runs by outcome, the provider's price, and our take.

| Payee | When | Take | Paid out |
|---|---|---|---|
| `connect` | a third-party provider with a payout reference | 5% of the provider's price, rounded up (`RUN_MARKUP_BPS`, the same figure `/.well-known/pricing.json` publishes) | yes, through Stripe Connect |
| `first_party` | `first_party: true` or a `first-party:` reference | 0 (the revenue is ours) | never |
| `passthrough` | `marketplace` set, or no payout reference (an imported marketplace catalog such as Monid) | 5% of the marketplace's price, rounded up (the one take the caller paid; E32). A format /1 settlement records 0: the E29 importer had baked its markup into the price | never; settled on that marketplace's invoice |

An agent pays `provider price + take` for a third-party run, once; failures and empty results cost the agent nothing and earn the provider nothing.

### Delivery (E33): the durable outbox

The router writes every settlement to `router_settlement_outbox` (`apps/router/sql/settlement-outbox.sql`, Postgres through the router Worker's `HYPERDRIVE` binding) **before** it sends it, and delivery is at-least-once against a receiver that is idempotent by `run_id` (accrual) and by `receipt_id` (wallet):

| State | Written when | Then |
|---|---|---|
| `open` | before apps/api is asked to hold money: a draft settlement (`transport_error`, $0) | replaced by the final settlement; deleted if the reservation is refused (402, 401). A draft older than 10 minutes means the run died after its hold: the retry job promotes it, and its delivery releases the hold at $0 |
| `pending` | the run's final settlement, before it is sent | one inline delivery for a run that held money; otherwise, and on any failure, the retry job |
| `delivered` | apps/api answered 2xx | pruned after 30 days |
| `dead` | apps/api answered 400 / 404 / 413 / 422 (the settlement itself is malformed; a batch is split so one bad row cannot block the rest) | kept, never pruned; `UPDATE router_settlement_outbox SET state='pending', next_attempt_at=0 WHERE state='dead'` re-queues after a fix |

- **Retry job.** The router's cron trigger (`* * * * *`, `scheduled` in `apps/router/src/worker.ts`) claims due rows (leased for 60 s, so overlapping runs do not double-send), posts them in batches of up to 100, and backs off per row: 30 s, 1 min, 2 min, … capped at one hour. Network errors, 401/403, 408, 429 and 5xx are retried forever.
- **Nothing is lost on a crash.** Before the hold: no money is held. Between the hold and the settlement: the draft releases it at $0 (an under-charge at worst, never an over-charge). After the settlement is written: it is delivered, eventually, exactly once in effect.
- **Fails closed.** A router with `ROUTER_WALLET_URL` but no `HYPERDRIVE` binding refuses metered runs (503) rather than charging without a durable settlement.
- **Content-free.** A row holds the settlement's fields and delivery bookkeeping (attempts, next attempt, last status, timestamps). No body column, no key, no actor. The router logs `kind=settlement` with a fixed word (`delivered`, `retry_scheduled`, `dead`, `deferred`, `outbox_unavailable`) and a count.

## 2. Onboarding (Stripe Connect Express)

1. When a listing PR is merged, an operator binds the provider to the principal whose agent keys act for it: `POST /v1/admin/providers {"provider_id": "acme", "owner_principal_id": "prn_…"}` (admin key).
2. The provider's agent calls `POST /v1/providers/me/connect` with its own key (not an OAuth token). The gateway creates an Express account once (`Idempotency-Key: connect:account:<provider_id>`) and answers `201 { url, expires_at, next_action: "approve_at_url" }`: a Stripe-hosted onboarding link a human at the provider opens.
3. Stripe returns the human to `GET /v1/providers/connect/return?state=…`, a page without JavaScript. The gateway reads the account from Stripe and, once details are submitted and payouts are enabled, marks it `active` and **binds `payout_account_ref` to the Connect account id**. If the link expired, Stripe sends the human to `GET /v1/providers/connect/refresh?state=…`, which mints a new link and redirects (302).
4. `state` is HMAC-signed with `PAYOUTS_STATE_SECRET`, names one provider and expires after 7 days; a forged or expired state is a 400 page.

`GET /v1/providers/me` (read) shows the account status (`unboarded`, `onboarding`, `active`, `restricted`), this month's runs and earnings, and the minimum payout; while onboarding is unfinished it refreshes the status from Stripe and offers the onboarding route again. A principal owning several providers passes `?provider_id=`.

## 3. Monthly payout

The sweep pays the previous UTC month once it has closed (one hour after it ends), and an operator can run a month with `POST /v1/admin/payouts/run {"month": "2026-10"}`. For each `connect` provider:

```
payable = earned through the month end − refunds − lost disputes − everything already paid (any month)
amount  = payable rounded down to whole cents
```

| Decision | When |
|---|---|
| `held` / `account_not_active` | no Connect account, or Stripe has not enabled payouts |
| `held` / `payout_ref_mismatch` | `provider.json` names an `acct_…` that is not the account this provider onboarded |
| `held` / `dispute_open` | any dispute against the provider is open |
| `below_minimum` | `amount` is under `PAYOUT_MIN_USD` (default $10); the balance carries to the next month |
| `pending` → `paid` | otherwise: `POST /v1/transfers` with `destination=<acct>`, `transfer_group` and `Idempotency-Key: payout:<provider_id>:<YYYY-MM>` |

Guarantees:

- **Once per provider-month.** A `paid` month is never decided again. A `pending` month (the transfer failed or the answer was lost) is resent with the same amount and the same Idempotency-Key, so Stripe returns the first transfer instead of making a second.
- **Never twice across months.** Payouts are cumulative: a held or carried balance is paid by a later month, and re-deciding an earlier month subtracts what later months already paid.
- **Refunds and disputes.** `POST /v1/admin/providers/{id}/adjustments` (admin, `Idempotency-Key` required) records `{"kind": "refund" | "dispute", "amount_micro_usd", "reason", "run_id"?}`. A refund is deducted. A dispute holds the provider's payout until `POST /v1/admin/providers/{id}/disputes/{entry}/resolve {"outcome": "provider_won" | "provider_lost"}`; a lost dispute is deducted.

## 4. Statement

`GET /v1/providers/me/statements?month=YYYY-MM` (read):

```json
{
  "schema": "shogunagent/provider-statement/v1", "provider_id": "acme", "month": "2026-10", "status": "closed",
  "currency": "USD", "amount_unit": "micro_usd", "payee": "connect",
  "totals": { "runs": 13, "ok_runs": 12, "empty_runs": 0, "failed_runs": 1,
              "provider_micro_usd": 12000000, "take_micro_usd": 600000, "charged_micro_usd": 12600000 },
  "days": [ { "day": "2026-10-20", "runs": 13, "ok_runs": 12, "empty_runs": 0, "failed_runs": 1, "provider_micro_usd": 12000000, "take_micro_usd": 600000, "charged_micro_usd": 12600000 } ],
  "adjustments": [],
  "payout": { "status": "paid", "reason": null, "earned_to_date_micro_usd": 12000000, "refunded_micro_usd": 0, "disputes_lost_micro_usd": 0,
              "previously_paid_micro_usd": 0, "payable_micro_usd": 12000000, "amount_micro_usd": 12000000, "transfer_id": "tr_…", "decided_at": "…" },
  "minimum_payout_micro_usd": 10000000, "take_bps": 500, "notes": "…",
  "hash": "sha256:…"
}
```

`hash` is SHA-256 over the canonical JSON of every other field (also in `X-Statement-Hash`).

## 5. What never leaves the ledger

The open catalog (`GET /v1/catalog`, `docs/api/catalog.md`) publishes `data_handling` per endpoint and nothing else from `provider.json`: legal names, contacts and payout references are not in the compiled catalog and appear on no public surface. Settlement rows carry no body and are deleted after 400 days; daily accruals and payout decisions are kept. Logs carry fixed words only (`settlements_recorded`, `payouts_run`, …), never an amount, a provider id or a reference.

## Configuration

| Variable | |
|---|---|
| `PAYOUTS_STATE_SECRET` | secret, ≥ 32 characters; turns the payout routes and the monthly job on |
| `ROUTER_SETTLEMENT_TOKEN` | secret, ≥ 32 characters; the router's bearer for `/v1/internal/router/reserve` and `/settlements` (same value in `apps/router`). Set = the router's wallet is served, with or without payouts |
| `PAYOUT_MIN_USD` | optional, default 10 |
| `STRIPE_SECRET_KEY` | the platform key; Connect onboarding and transfers use it |
| router `ROUTER_WALLET_URL` | apps/api's base URL; set = runs are reserved and settled through the wallet (E32) |
| router `HYPERDRIVE` binding | the Postgres holding `router_settlement_outbox` (E33); required with `ROUTER_WALLET_URL` |

Apply `apps/api/sql/provider-payouts.sql` (additive, idempotent) before enabling payouts, and `apps/router/sql/settlement-outbox.sql` then `settlement-outbox-permissions.sql` to the router's database before setting `ROUTER_WALLET_URL`.
