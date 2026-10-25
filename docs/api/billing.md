# Billing: statements, commits, adjustments and events

How the hosted gateway bills beyond the per-call wallet of `docs/api/402.md`: monthly statements,
annual commit contracts, refunds and credits, how a hosted `/mcp` call is relayed and settled,
the ShogunAI Pro bundle, and the two billing events.
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

## Hosted `/mcp` (E34)

`/mcp` on the hosted gateway is the agent's MCP endpoint (Streamable HTTP, MCP 2026-07-28). The
gateway authenticates, meters and relays; the principal's Rust sidecar (`shogun-api --multi`) runs
the call against the principal's store. Implementation: `apps/api/src/mcp-proxy.ts` (relay),
`apps/api/src/metering.ts` (`metered`, `settledQuantity`); tests: `apps/api/test/mcp-relay.test.ts`.

### Sequence

1. **Authenticate.** No valid agent key or OAuth access token: `401` with
   `WWW-Authenticate: Bearer realm="shogunagent", resource_metadata="<base>/.well-known/oauth-protected-resource/mcp"`
   (the pointer is present when OAuth is served), before any other header is looked at.
2. **Classify** from `Mcp-Method`, `Mcp-Name`, `Mcp-Quantity`. Region (`421 wrong_region`), scope
   (`403 insufficient_scope`), dry run. Missing or mismatched `Mcp-Method` / `Mcp-Name` are refused
   by the core (JSON-RPC `400 missing_header` / `*_mismatch`) before anything runs, and the refusal
   settles at 0; a `POST` without `Mcp-Method` is classified as free protocol traffic.
3. **Can this gateway serve the principal?** No sidecar configured: `503 upstream_unavailable`
   (`next_action: retry_after`). The principal's store lives on another replica (`SHOGUN_SHARD`):
   `421 wrong_shard`. No sidecar bearer for the principal: `503 principal_not_provisioned`
   (`next_action: contact_operator`). Nothing is reserved in any of these.
4. **Reserve** the quote (wallet or commit, the four caps, the global breaker; `402` as in
   `docs/api/402.md`).
5. **Relay** to `<SHOGUN_SIDECAR_URL>/mcp`, through the configured transport (global `fetch`, or a
   Workers binding with `SHOGUN_SIDECAR_BINDING`; options in `docs/deploy/production.md` §1): the request body is handed on as a stream and the reply
   body comes back as a stream; neither is read, buffered, logged or stored by the gateway
   (invariants 1 and 2). Request headers passed: `Content-Type`, `Accept`, `Mcp-Method`,
   `Mcp-Name`, `Mcp-Protocol-Version`, `Last-Event-ID`, `Idempotency-Key`; the agent's
   `Authorization` is replaced by the principal's sidecar bearer, everything else stays at the
   gateway. Reply headers passed: `Content-Type`, `Cache-Control`, `Mcp-Method`, `Mcp-Name`,
   `Mcp-Quantity`, `Mcp-Protocol-Version`, `Allow`, `Retry-After`. The sidecar refusing the
   gateway's bearer (`401`/`403`) is answered `503 core_rejected_gateway`, never as a `401` to the
   agent. No reply headers within 30 s, or no connection: `503 upstream_unavailable`, nothing charged.
6. **Settle** once, from the reply's headers only (below), and add `X-Receipt-Id`, `X-Usage-*`,
   `X-Latency-Ms`.

### The settlement contract (gateway ↔ sidecar)

The sidecar answers JSON-RPC errors, `isError` tool results and idempotent replays with HTTP
`200`. The gateway does not read the body to tell them apart; the sidecar says what it did in one
reply header:

| Reply | Settles at |
|---|---|
| non-`2xx` | 0 |
| `2xx` with `Mcp-Quantity: n` (a whole number) | `min(n, reserved quantity)` × the quote's unit price (commit discount applied as usual) |
| `2xx` without `Mcp-Quantity`, or with anything but a whole number | 0, logged as `event=settle_quantity_missing` |
| any reply to a free call (`read`, `protocol`) | 0, whatever the header says (invariant 10) |

The receipt records both figures (E48): `reserved_quantity` (the quote, also `quantity`) and
`settled_quantity`, the quantity it settled at under the table above (`0` when it settled at 0;
a free call is recorded at its reserved quantity). `GET /v1/receipts` returns both;
`settled_quantity` is `null` while the call is in flight and on receipts settled before the
gateway recorded it (`apps/api/sql/agent-receipts-settled-quantity.sql`). A router run's receipt
settles at the router's reported quantity for an `ok` run and 0 otherwise. An x402-paid call
settles at its whole quantity or 0.

What the sidecar MUST send on every reply to a billable `tools/call` (`ingest`, `model_op`,
`run` units):

- `Mcp-Quantity: <n>` where `n` is the units the core actually took: for `memory.ingest` the
  events/turns newly stored (`usage.quantity` of the result), for `memory.append_note` 1 when a
  note was written, for any other billable tool 1 when it ran;
- `Mcp-Quantity: 0` for a JSON-RPC error, an `isError` result, a refusal (L2 `input_required`,
  L3 `approval_required`) and an idempotent replay that stored nothing new.

The sidecar meets this (E34, `crates/shogun-mcp/src/mcp_http.rs` `settled_quantity`): every
`tools/call` reply carries `Mcp-Quantity`, including `0` for every read (a read is never billed,
whatever the gateway's table says, invariant 10), a declined L2 confirmation (`ran: false`), a
write recorded as a pending intent, and a `tools/call` sent as a notification (no id: nothing
runs, HTTP 202). Replies to other methods carry none. A call refused for its bearer is HTTP `401`
with `WWW-Authenticate: Bearer` (the JSON-RPC `-32001` error stays the body), which the gateway
answers as `503 core_rejected_gateway`. A reply that is a JSON-RPC error also names its code in
`Mcp-Error-Code` (E44), so the gateway tells a store that could not serve (`-32000`, worth a
retry) from a refused argument (`-32602`, final) without reading the body; the `/mcp` relay passes
the header on.

**`actions.execute` on `/mcp`.** The call is free: it runs `local_search` (a read), refuses the
desktop-only kinds, and only *queues* a send, post or comment for a human's approval (L3), and
queueing and remote approvals are free (invariant 10). It stays a `run` (it needs the `run` scope)
priced at $0 (`FREE_RUN_TOOLS` in `apps/api/src/pricing.ts`), so the gateway no longer refuses it
with `400 run_price_unavailable` and asks the router for nothing; `POST /v1/quote` answers $0 for
it too. The external send happens later, when a human approves, in the principal's core with the
principal's own connector credentials (`approval_exec`), not on this request; it is billed where
it executes, which today means it carries no charge of its own (a send that goes through the router
is a router `run`, E32). A per-send fee would be a new line in `packages/config/pricing.json` booked
at execution, an owner pricing decision not taken here.

The gateway bills only what the core confirmed: a sidecar that omits the header is never billed
more than 0 for the call. An x402-funded call (`docs/api/402.md`) is captured only when `n`
equals the reserved quantity, because an `exact` payment cannot be captured in part.

### How the gateway names the principal to the sidecar

The sidecar resolves a bearer to a principal and an actor. The gateway presents one of:

- **A listed token** — `SHOGUN_SIDECAR_PRINCIPAL_TOKENS`, the same `principal:token[:agent_id],…`
  list the sidecar is given as `SHOGUN_PRINCIPAL_TOKENS`. Works with the current sidecar; suited
  to a fixed set of principals (BYOC, evaluation).
- **A derived bearer** — with `SHOGUN_SIDECAR_BEARER_SECRET` (≥ 32 characters, the same value on
  both sides): `sgw1.<principal_id>.<key_id>.<mac>`, `mac` = base64url (no padding) of
  HMAC-SHA256(secret, `"shogun/sidecar-bearer/v1\n" + principal_id + "\n" + key_id`). The sidecar
  must verify the MAC in constant time, take the principal from the token and record writes as
  `agent:<key_id>`. Any self-registered principal works without provisioning. The sidecar does
  this (E34, `crates/shogun-core/src/tenancy/derived_bearer.rs`) when it is given the same
  `SHOGUN_SIDECAR_BEARER_SECRET`, for both REST and `/mcp`; a shared test vector is asserted on both
  sides (`apps/api/test/mcp-relay.test.ts`, `derived_bearer::tests`). With it the sidecar needs no
  `SHOGUN_PRINCIPAL_TOKENS` at all, so no principal bearer sits in its environment (threat model
  R4).

A listed token wins over a derived one. Sharding (`deploy/README.md` §5.2): each gateway talks only
to its own pod's loopback sidecar; with `SHOGUN_SHARD=<index>/<count>` it refuses a principal whose
`fnv1a64(principal_id) mod count` is another replica, so a misrouted request never creates a stray
store. Routing a principal to its replica stays the ingress's job.

## Ingest hooks (E44)

Provider webhooks into memory (`docs/api/ingest-sources.md`, `apps/api/src/ingest-hooks/app.ts`).
A verified delivery is a `memory.ingest_webhook` call on the principal's core through the same
relay, metered as **ingest** (500 µ$ per stored event, the `memory.ingest` rate) against the
principal's wallet and caps, with the hook id as the receipt's `key_id`:

| what arrives | reserved | settled |
|---|---|---|
| a verified delivery | 20 events (`MAX_EVENTS_PER_DELIVERY`) | the core's `Mcp-Quantity`: the events newly stored |
| a delivery the core answers as a change notice (Calendar, Gmail, Notion) or ignores | 20 | 0 |
| a replay of a delivery id already stored | 20 | 0 (the core's idempotent replay) |
| the core cannot store it now (`Mcp-Error-Code: -32000`, 5xx) | 20 | 0; the provider is answered 503 and retries |
| a handshake: GitHub `ping`, Slack `url_verification`, Notion's `verification_token`, Calendar `sync`, a Gmail push no hook claims | nothing | — (free, no receipt) |
| a bad signature, a stale Slack timestamp, an unknown hook | nothing | — (free; counts against a per-IP and per-hook budget of 30 failures in 10 minutes, then `429`) |

Managing hooks (`POST/GET/DELETE /v1/ingest-hooks…`) is free. An empty wallet answers the provider
`402`; the provider's own retry policy decides whether the delivery comes back after a top-up.

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

## Agent payments (E46)

`payments.*` ([payments.md](payments.md)) never moves money through this wallet: a payment goes
from the principal's own rail account to the payee, executed by the principal's core. What this
gateway meters is the call: `payments.mandates.list` is a free read; `payments.mandates.request`,
`payments.mandates.revoke` and `payments.spend` are `run` calls (scope `run`) priced at $0 at the
call (`FREE_RUN_TOOLS`). A request only queues an L3 for a human and a revoke only narrows; a spend
would be priced at the rail's fee, and the only rail today is a stub with none. The gateway relays
these calls over `/mcp` without reading or storing their bodies (no payee, amount or memo in a
receipt, a log or a metric).

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
