# Billing: statements, commits, adjustments and events

How the hosted gateway bills beyond the per-call wallet of `docs/api/402.md`: monthly statements,
annual commit contracts, refunds and credits, the ShogunAI Pro bundle, and the two billing events.
Implementation: `apps/api/src/billing/` (`ledger.ts` rules, `routes.ts` routes, `jobs.ts` sweep
jobs, `notify.ts` events); tests: `apps/api/test/billing.test.ts`. Plan: E19 and E14-T3. Amounts
are integer micro-USD (1 USD = 1,000,000). Reads, export and deletion are free (invariant 10).

## Statements

`GET /v1/statements?month=YYYY-MM&format=json|csv` (`read` scope; default: this month, JSON; an
`Accept: text/csv` header also selects CSV).

- **Usage per unit**: `calls`, `quantity`, `list_micro_usd`, `charged_micro_usd`,
  `discount_micro_usd`, `commit_funded_micro_usd`, `wallet_funded_micro_usd`, `free`. Free units
  (`read`, `protocol`) are listed with their call counts at `0`.
- **Credits** (Stripe top-ups, the signup allowance, the Pro bundle), **adjustments** (operator
  credits, refunds) and **disputes** with their resolution (`open` / `resolved` / `rejected`,
  `credit_micro_usd`). A dispute's free-text reason never appears in a statement.
- **Commit**: the commit in force during the month, with its drawdown.
- **`hash`**: `sha256:` over the canonical JSON (RFC 8785) of every other field; the same value is
  in the `X-Statement-Hash` header and the CSV's last row.

A month **closes** one hour after it ends (UTC). The first request after that stores the
statement; every later request returns the stored copy byte for byte, whatever happens to the
ledger afterwards. Receipts are deleted after 400 days; closed statements are kept.

```sh
curl -s "$BASE/v1/statements?month=2026-10" -H "Authorization: Bearer $KEY"
curl -s "$BASE/v1/statements?month=2026-10&format=csv" -H "Authorization: Bearer $KEY" -o statement-2026-10.csv
```

CSV columns: `section,key,calls,quantity,list_micro_usd,amount_micro_usd,discount_micro_usd,status,ref,ts`
with sections `usage`, `credit`, `adjustment`, `dispute`, `total` and `hash`.

## Annual commits

An enterprise contract: an amount of spend over a period (at most 367 days) at 10–25% off
(`discount_bps` 1000–2500), for one principal or a whole tenant (a principal's own commit wins over
its tenant's). Created by an operator:

```sh
curl -s -X POST $BASE/v1/admin/commits -H "Authorization: Bearer $ADMIN_KEY" -H 'Idempotency-Key: acme-2027' \
  -H 'content-type: application/json' \
  -d '{"subject":"tenant","subject_id":"ten_acme","amount_micro_usd":12000000000,"discount_bps":2000,
       "starts_at":"2027-01-01T00:00:00Z","ends_at":"2028-01-01T00:00:00Z","stripe_customer_id":"cus_…"}'
```

`201 { commit, created: true }`; the same Idempotency-Key answers `200` with the same commit; a
period overlapping another commit of the same subject is `409 commit_overlap`.

While a commit is active:

- Every metered call is charged `list − floor(list × bps / 10000)`. Quotes (`POST /v1/quote`,
  `Mcp-Dry-Run`) show `cost_micro_usd` (discounted), `list_cost_micro_usd`, `commit_id` and
  `discount_bps`; the real call settles at exactly the dry-run figure. `/mcp` responses add
  `X-Usage-List-Cost-Micro-Usd`; receipts keep both prices.
- A call is **commit-funded** while the commit has headroom, or past it when overage billing is
  configured: nothing is held on the wallet, the discounted cost is drawn down from the commit.
  Past the amount without overage billing, calls fall back to the wallet, still discounted.
- **Overage** (drawn above the amount) is reported to Stripe as usage records on the customer's
  metered subscription item (`STRIPE_COMMIT_OVERAGE_PRICE_ID`, one unit = one cent), in whole
  cents, by the sweep. Each report's id is its Stripe Idempotency-Key.
- Hard caps still apply: a commit is not a blank cheque for a leaked key.

`GET /v1/commit` (`read`) returns the active commit: `discount_bps`, `amount_micro_usd`,
`drawn_micro_usd`, `remaining_micro_usd`, `overage_micro_usd`, `overage_billing`, the period.

## Refunds, credits and disputes

- `POST /v1/admin/adjustments` (`admin`, Idempotency-Key required):
  `{ principal_id, kind: "credit" | "refund", amount_micro_usd, reason, charge_id?, ref? }`.
  `reason` is one of `goodwill`, `dispute`, `service_credit`, `refund`, `chargeback`,
  `correction`. A credit adds to the wallet; a refund without `charge_id` is a ledger debit; with
  `charge_id` it also refunds the card through Stripe. A debit never takes the balance below zero
  (the entry records what it applied).
- `charge.refunded` (Stripe webhook) debits the increase of the charge's refunded total. The admin
  route and the webhook converge on that total, so a refund is debited once whichever arrives
  first, and a later dashboard refund debits only the difference.
- `checkout.session.expired` closes a top-up session that was never paid; a completed session is
  never marked expired.
- `POST /v1/admin/disputes/{id}/resolve` (`admin`): `{ outcome: "credit" | "reject",
  amount_micro_usd? }`. A credit defaults to the receipt's cost and is the ledger entry
  `dispute:<id>`. Resolving twice changes nothing.

Admin routes take an `admin` agent key, never an OAuth access token.

## Router runs (E32)

A router `run` is billed by this wallet, once, like any other metered call: the receipt is
`rcpt_run_<run_id>` (`unit: "run"`, `name: "router.run"`), it counts against the same caps, and an
active commit discounts it. The router reserves the run's price through
`POST /v1/internal/router/reserve` and settles it through `POST /v1/internal/router/settlements`
(contract and diagram: `docs/api/payouts.md` §1, "The single-charge path").

- **Price.** Third-party tool: the provider's measured price plus one take, `RUN_MARKUP_BPS`
  (5%, rounded up; set once in `packages/config/pricing.json`). First-party tool: its listed
  price, no take. The router sends only the provider's price; the take is added here.
- **Marketplace imports.** A tool imported from Monid is priced at Monid's own price (what Monid
  invoices us) plus the same one take. There is no second markup.
- **Free.** Failures and empty results settle at $0; a $0 tool (the state reads) holds nothing.
- **Durability.** The router's settlement outbox (E33) redelivers until this gateway acknowledges,
  so a statement is never missing a run that was charged, and a run that died mid-flight is
  released at $0.

## ShogunAI Pro bundle

`POST /v1/agents/me/licence` with `X-Shogun-Licence: <the ShogunAI licence token>` links the
licence's subject (the licence id, or `account:<acc>`) to the calling principal. A **Pro** licence
earns `$20` of credit per UTC month, keyed `grant:shogunai-pro:<subject>:<YYYY-MM>`: granted at
link time and by the sweep for every Pro link verified in the last 35 days, once per subject per
month whichever principal it is linked to. Takes the principal's own key (not an OAuth token).

## Events

There is no per-principal webhook module yet (E15 brings webhooks); events go to a pluggable
`Notifier` (`apps/api/src/billing/notify.ts`). The default keeps the newest events per principal in
memory and never logs them. Ids are deterministic, so a notifier dedupes and a redelivery is safe.

```json
{ "type": "wallet.low_balance", "id": "wallet.low_balance:prn_…:2026-10-25", "principal_id": "prn_…",
  "created_at": "2026-10-25T12:00:05.000Z",
  "data": { "balance_micro_usd": 500000, "threshold_micro_usd": 1000000, "topup_url": "https://…/v1/wallet/topup" } }
```

Emitted when a wallet-funded settlement takes the spendable balance below the threshold ($1) from
at or above it; at most once per principal per UTC day.

```json
{ "type": "cap.reached", "id": "cap.reached:key:ak_…:2026-10-25", "principal_id": "prn_…",
  "created_at": "2026-10-25T12:00:05.000Z",
  "data": { "scope": "key", "period": "day", "period_key": "2026-10-25", "limit_micro_usd": 1000000,
            "spent_micro_usd": 999600, "key_id": "ak_…", "retry_after": 43200 } }
```

Emitted when a hard cap refuses a call (`402 cap_exceeded`); once per counter per period.
