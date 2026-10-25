# Provider payouts

How a tool provider listed in the router gets paid: every settled run accrues to the provider named in its `provider.json`, a monthly job pays the balance through Stripe Connect Express, and the provider reads a statement per month. Plan: E29. Implementation: `apps/api/src/payouts.ts` (rules, in-memory store, monthly job), `apps/api/src/payouts-pg-store.ts` (Postgres, `apps/api/sql/provider-payouts.sql`), `apps/api/src/payouts-app.ts` (routes), `apps/api/src/billing/connect.ts` (Stripe Connect over `fetch`, no SDK); the router side is `apps/router/src/settlement.ts`. Tests: `apps/api/test/payouts.test.ts`, `apps/router/test/settlement.test.ts`. Amounts are integer micro-USD (1 USD = 1,000,000).

## 1. Accrual: the settlement seam

The router settles every run it reserved exactly once and sends the settlement to `POST /v1/internal/router/settlements` with `Authorization: Bearer <ROUTER_SETTLEMENT_TOKEN>`:

```json
{ "settlements": [
  { "format": "shogunagent-run-settlement/1", "run_id": "9f0c…", "ts": 1792411200000,
    "tool_id": "acme.lookup", "provider_id": "acme", "first_party": false,
    "payout_account_ref": "acct_1Acme", "outcome": "ok", "quantity": 1, "provider_micro_usd": 1000 } ] }
```

- Content-free by construction: no input, no output, no actor, no key. Unknown fields are rejected.
- `outcome` is `ok`, `empty`, `provider_error` or `transport_error`. Only `ok` carries a price; failures and empty results settle at $0 and count as runs.
- Idempotent by `run_id`: `{ "recorded": 1, "duplicates": 0 }`, then `{ "recorded": 0, "duplicates": 1 }` for a redelivery. Up to 100 per request; one invalid entry rejects the request and records nothing.
- `payout_account_ref` comes from the provider's `provider.json` (`apps/router/connectors/schema.md` §9).

Each settlement is added to the provider's accrual for its UTC day (`provider_accruals`): runs by outcome, the provider's price, and our take.

| Payee | When | Take | Paid out |
|---|---|---|---|
| `connect` | a third-party provider with a payout reference | 5% of the provider's price, rounded up (`RUN_MARKUP_BPS`, the same figure `/.well-known/pricing.json` publishes) | yes, through Stripe Connect |
| `first_party` | `first_party: true` or a `first-party:` reference | 0 (the revenue is ours) | never |
| `passthrough` | no payout reference (an imported marketplace catalog such as Monid) | 0 (the imported price already carries its markup) | never; settled on that marketplace's invoice |

An agent pays `provider price + take` for a third-party run; failures and empty results cost the agent nothing and earn the provider nothing.

### Delivery

The router's `HttpSettlementSink` retries a failed POST three times (the route is idempotent) and then counts the settlement as dropped. A durable outbox in the router is the production follow-up; until it exists a dropped settlement is an under-payment to the provider, never an over-payment.

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
| `ROUTER_SETTLEMENT_TOKEN` | secret, ≥ 32 characters; the router's bearer for the settlement route (same value in `apps/router`) |
| `PAYOUT_MIN_USD` | optional, default 10 |
| `STRIPE_SECRET_KEY` | the platform key; Connect onboarding and transfers use it |
| router `ROUTER_SETTLEMENT_URL` | where the router posts settlements |

Apply `apps/api/sql/provider-payouts.sql` (additive, idempotent) before enabling.
