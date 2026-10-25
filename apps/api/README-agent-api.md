# Agent API (E3 + E9)

The agent-facing half of the gateway: `sa_` keys, a prepaid micro-USD wallet, hard caps, the
`402` contract, quotes, receipts, and the metered MCP proxy. Plan: `docs/implementation-plan.md`
E3 / E9 / §7. The licence-token routes (`/v1/batch`, `/v1/review`, …) are unchanged and documented
in `DEPLOY.md`.

Everything here is served only when `AGENT_API_ENABLED=1` (see `src/boot.ts` for the variables).
The store is `sql/agent-keys.sql` (apply like `shared-store.sql`; then `agent-keys-permissions.sql`).

Conventions:

- Auth: `Authorization: Bearer sa_live_…`. Scopes `read` / `write` / `run` / `admin` (`admin`
  implies all). Keys are stored as a salted PBKDF2-SHA256 hash plus a 10-character lookup
  prefix; the secret is shown once at registration (`src/agent-keys.ts` explains why PBKDF2 and
  not Argon2id on Workers).
- Amounts: integer micro-USD (`1 USD = 1_000_000`).
- Errors: `{ "error": { "code", "message", "next_action" } }` (§7.2). `message` says what to do
  next; branch on `code`.
- Nothing in a request or response body is logged or stored (`src/logging.ts` is the only console
  writer; the log line carries the route template, status, a fixed event word and a hash of the
  key *id*).

## Routes

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/.well-known/pricing.json` | none | The rate table, caps and allowance, rendered from `src/pricing.ts` |
| GET | `/v1/agents/challenge` | none | A proof-of-work challenge for registration |
| POST | `/v1/agents` | PoW header, per-IP limit | Self-registration → key (`read,write,run`) + fresh principal + $5.00 wallet |
| GET | `/v1/agents/me` | `read` | The key's principal, scopes, expiry, caps |
| POST | `/v1/keys/{id}/revoke` | any (own key; `admin` for others) | Revoke a key |
| PATCH | `/v1/keys/{id}` | any (lower); `admin` (raise) | Set a key's hard caps |
| GET | `/v1/wallet` | `read` | Balance, reserved, spendable, caps, this day's / month's spend |
| POST | `/v1/wallet/topup` | `write` | Create a Stripe Checkout session (card rail) |
| POST | `/v1/stripe/webhook` | Stripe signature | Credit the wallet when a Checkout completes (idempotent by event id) |
| POST | `/v1/quote` | `read` | The exact cost of a call, nothing reserved |
| GET | `/v1/receipts?since=&limit=` | `read` | Per-call receipts, oldest first |
| POST | `/v1/receipts/{id}/dispute` | `write` | Open a dispute on a receipt (one per receipt) |
| ALL | `/mcp` | by unit (see below) | The metered MCP Streamable HTTP proxy |
| POST | `/v1/webhooks` | `write` | Register an approval webhook (`approval.created`, `approval.decided`) |
| GET | `/v1/webhooks` | `read` | The principal's approval webhooks (no secrets) |
| DELETE | `/v1/webhooks/{id}` | `write` | Remove one |
| GET / POST | `/approve/{token}` | the link (+ CSRF on POST) | The human approval page (E15) |
| POST | `/v1/slack/interactions` | Slack signature | Slack button presses on approval messages (E15) |
| POST | `/v1/approvals/notify` | the signed token | An approval was queued: post to Slack, deliver webhooks |

## 1. Register

```sh
BASE=https://api.shogunaios.com
CH=$(curl -s $BASE/v1/agents/challenge)            # {"challenge":"…","difficulty_bits":18,"expires_at":"…"}
CHALLENGE=$(echo "$CH" | jq -r .challenge)
BITS=$(echo "$CH" | jq -r .difficulty_bits)
# Find a nonce such that sha256(challenge + nonce) has $BITS leading zero bits (src/pow.ts `solvePow`
# does this in a loop; at 18 bits it takes well under a second).
NONCE=$(node -e '
  const {createHash}=require("node:crypto");const [c,b]=process.argv.slice(1);
  const lz=h=>{let n=0;for(const ch of h){const v=parseInt(ch,16);if(v===0){n+=4;continue;}n+=Math.clz32(v)-28;break;}return n;};
  for(let i=0;;i++){const n=i.toString(36);if(lz(createHash("sha256").update(c+n).digest("hex"))>=Number(b)){console.log(n);break;}}' "$CHALLENGE" "$BITS")

curl -s -X POST $BASE/v1/agents \
  -H "X-Agent-Pow: $CHALLENGE:$NONCE" \
  -H 'content-type: application/json' \
  -d '{"ref":"dev_alice"}'
```

```json
{
  "key": "sa_live_1ZqK…(43 chars)",
  "key_id": "ak_8c1f…", "prefix": "1ZqK7v0wQp",
  "principal_id": "prn_…", "tenant_id": "prn_…",
  "scopes": ["read", "write", "run"],
  "created_at": "2026-10-25T12:00:00.000Z", "expires_at": "2027-10-25T12:00:00.000Z", "ref": "dev_alice",
  "wallet": { "balance_micro_usd": 5000000, "reserved_micro_usd": 0 },
  "caps": { "key": { "day_micro_usd": 1000000, "month_micro_usd": 20000000 }, "principal": { "day_micro_usd": 5000000, "month_micro_usd": 100000000 } },
  "next_action": null
}
```

Body fields (all optional): `ref` (another key id or a developer id, `[A-Za-z0-9_.:-]{1,128}`),
`caps: { day_micro_usd, month_micro_usd }` (may only *lower* the key defaults).

Refusals: `401 pow_missing | pow_malformed | pow_invalid | pow_expired | pow_insufficient |
pow_reused` (`next_action: "solve_challenge"`), `429 rate_limited` (per IP per hour, with
`Retry-After`), `400 invalid_ref | invalid_caps | caps_above_default`.

## 2. Call MCP (metered)

The proxy reads three headers and never the body:

| Header | Meaning |
|---|---|
| `Mcp-Method` | the JSON-RPC method (`tools/call`, `initialize`, `tools/list`, …). Anything but `tools/call` is free `protocol` traffic |
| `Mcp-Name` | the tool name for `tools/call` |
| `Mcp-Quantity` | units in this call (events for `memory.ingest`); default `1`, max `1,000,000` |
| `Mcp-Dry-Run: true` | quote only: nothing reserved, nothing forwarded |
| `Mcp-Tool-Id` | for `router.run`: the tool whose provider price the router looks up |

Classification (`src/pricing.ts`):

| Unit | Price | Scope needed | Tools |
|---|---|---|---|
| `read` | $0 | `read` | `state.*`, `memory.search`, `memory.recall`, `memory.get_context_pack`, `memory.get_context`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `memory.get_periods`, `calendar.upcoming`, `profile.whoami`, `wiki.evidence`, `actions.status`, `approvals.*`, `router.discover`, `router.inspect` |
| `ingest` | 500 µ$ per event ($0.50 / 1,000) | `write` | `memory.ingest`, `memory.append_note` |
| `model_op` | 100 µ$ per call ($0.10 / 1,000) | `write` | every other `tools/call` |
| `run` | provider price × 1.05, rounded up | `run` | `router.run`, `actions.execute` (price from the router; from headers alone a run is refused `400 run_price_unavailable`) |
| `storage` | 100,000 µ$ per GB-month | — | booked by the storage sweep, not per call |
| `protocol` | $0 | any valid key | every non-`tools/call` method and `GET`/`DELETE /mcp` |

```sh
curl -s -X POST $BASE/mcp \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'Mcp-Method: tools/call' -H 'Mcp-Name: memory.ingest' -H 'Mcp-Quantity: 12' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory.ingest","arguments":{…}}}' -i
```

```
HTTP/1.1 200 OK
X-Receipt-Id: rcpt_…
X-Usage-Unit: ingest
X-Usage-Quantity: 12
X-Usage-Cost-Micro-Usd: 6000
X-Latency-Ms: 41
```

The body is the core's, relayed untouched. Sequence inside the gateway: verify key → classify →
quote → **reserve** (wallet and the four cap counters in one transaction) → forward → **settle**
once (2xx: the quote; anything else: 0, so a failed call costs nothing). A free call writes a
settled receipt at cost 0.

A dry run answers `200 { "dry_run": true, "quote": { "unit", "quantity", "cost_micro_usd" },
"sufficient": true, "balance_micro_usd": …, "usage": {…,"cost_micro_usd":0}, "latency_ms" }`.
The real call then settles at exactly `quote.cost_micro_usd`.

Refusals: `401` (key), `403 insufficient_scope`, `402 payment_required | cap_exceeded`
(`docs/api/402.md`), `503 upstream_unavailable | wallet_unavailable | capacity_exhausted`
(the company's global daily breaker; `Retry-After` to UTC midnight).

## 3. Quote

```sh
curl -s -X POST $BASE/v1/quote -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"method":"tools/call","name":"memory.ingest","quantity":250}'
```

```json
{ "quote": { "unit": "ingest", "quantity": 250, "cost_micro_usd": 125000 }, "method": "tools/call", "name": "memory.ingest",
  "balance_micro_usd": 5000000, "sufficient": true, "caps": { "…" }, "usage": { "unit": "ingest", "quantity": 0, "cost_micro_usd": 0 }, "next_action": null }
```

For a run pass `provider_price_micro_usd` (from `router.inspect`): `{"name":"router.run","provider_price_micro_usd":1999}` → `2099`.

## 4. Wallet and top-up

```sh
curl -s $BASE/v1/wallet -H "Authorization: Bearer $KEY"
```

```json
{ "principal_id": "prn_…", "balance_micro_usd": 4994000, "reserved_micro_usd": 0, "available_micro_usd": 4994000,
  "caps": { "…" }, "spend": { "key": { "day_micro_usd": 6000, "month_micro_usd": 6000 }, "principal": { "day_micro_usd": 6000, "month_micro_usd": 6000 } },
  "topup": { "rail": "card", "url": "https://api.shogunaios.com/v1/wallet/topup", "method": "POST", "min_micro_usd": 5000000, "max_micro_usd": 500000000 } }
```

```sh
curl -s -X POST $BASE/v1/wallet/topup -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'Idempotency-Key: topup-2026-10-25-1' -d '{"amount_micro_usd":10000000}'
```

```json
{ "rail": "card", "url": "https://checkout.stripe.com/c/pay/cs_…", "session_id": "cs_…", "amount_micro_usd": 10000000,
  "next_action": "approve_at_url", "instructions": "Open url in a browser and pay. …" }
```

Hand `url` to a human once. Stripe calls `POST /v1/stripe/webhook` (configure the endpoint for
`checkout.session.completed`; `Stripe-Signature` is verified with `STRIPE_WEBHOOK_SECRET`); the
wallet is credited `amount_total` once per event id. Then retry the `402`'d call unchanged.
Without `STRIPE_SECRET_KEY` the route answers `503 topup_unavailable`.

## 5. Caps

Defaults (`src/caps.ts`): key $1/day, $20/month; principal $5/day, $100/month. Checked at
reservation against booked estimates (so in-flight calls count), corrected at settlement.
UTC periods.

```sh
curl -s -X PATCH $BASE/v1/keys/$KEY_ID -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"day_micro_usd":250000}'                       # lower: any key; raise above default: admin only
curl -s -X POST  $BASE/v1/keys/$KEY_ID/revoke -H "Authorization: Bearer $KEY"
```

Exceeding a cap: `402 cap_exceeded`, `payment.cap = { scope, period, limit_micro_usd,
spent_micro_usd }`, `Retry-After` until the period resets. Deployment-wide defaults:
`AGENT_KEY_CAP_DAY_USD` and friends.

## 6. Receipts and disputes

```sh
curl -s "$BASE/v1/receipts?since=2026-10-25T00:00:00Z&limit=100" -H "Authorization: Bearer $KEY"
```

```json
{ "receipts": [
    { "id": "rcpt_…", "key_id": "ak_…", "method": "tools/call", "name": "memory.ingest", "unit": "ingest", "quantity": 12,
      "cost_micro_usd": 6000, "reserved_micro_usd": null, "settled": true, "ts": "2026-10-25T12:00:05.000Z" } ],
  "next_since": "2026-10-25T12:00:05.001Z" }
```

`since` is ISO-8601 or epoch milliseconds; page with `next_since`. `reserved_micro_usd` is
non-null only while a call is in flight.

```sh
curl -s -X POST $BASE/v1/receipts/rcpt_…/dispute -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"reason":"ingest counted 12 events, I sent 10"}'
```

`201 { "dispute_id": "dsp_…", "receipt_id": "rcpt_…", "status": "open", "ts": "…" }`; a second
dispute on the same receipt answers `200` with the existing one. Someone else's receipt is `404`.

## 7. Remote approvals (E15)

An L3 call (and an L2 one the core queues) answers with an approval URL on this gateway:
`https://<gateway>/approve/<token>`. The token is minted by the sidecar, signed with
`SHOGUN_APPROVAL_SIGNING_KEY` (the sidecar and the gateway hold the same value; nothing else
does), expires with the approval (10 minutes) and decides once. Hand the URL to a human; poll
`actions.status` until it reads `approved` or `rejected`. An agent never decides: there is no
tool for it, and the sidecar refuses an agent's bearer on the decide route (`403 human_only`).

- **Web page** (`GET /approve/{token}`): server-rendered, no JavaScript, the redacted summary
  (kind, tier, recipient class — never the recipient — a masked one-line preview, times) and
  Approve / Reject. The form carries a CSRF value bound to a `SameSite=Strict` cookie and to the
  link; a POST from another origin, without the cookie or with a wrong value is refused. The page
  sends `Referrer-Policy: no-referrer` and a `default-src 'none'` CSP. The decision is recorded as
  `web:approval-page`.
- **Slack** (`SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APPROVAL_CHANNEL`): when an
  approval is queued the gateway posts the summary with Approve / Reject buttons to the channel
  (or a user's DM) with `chat.postMessage`. A press reaches `POST /v1/slack/interactions`, which
  verifies Slack's signature (`v0`, five-minute window) before parsing, decides as
  `slack:<user id>`, and replaces the message with the outcome.
- **Webhooks**: `POST /v1/webhooks {"url": "https://…", "events": ["approval.created",
  "approval.decided"]}` → `201 {id, url, events, secret}` (the secret is shown once; it is derived
  from `WEBHOOK_SIGNING_SECRET` and never stored). Each delivery is signed
  `X-Shogun-Signature: t=<unix>,v1=hex(HMAC-SHA256(secret, "<t>.<raw body>"))` with
  `X-Shogun-Event` and `X-Shogun-Delivery`; a network error, 429 or 5xx is retried after 1 s, 5 s
  and 30 s. The body is the redacted summary only:

  ```json
  { "id": "whd_…", "event": "approval.created", "created_at": "…", "principal_id": "prn_…",
    "approval": { "approval_id": 9, "status": "pending", "level": "L3", "kind": "Send email",
                  "recipient_class": "email_recipient", "preview": "…", "created_at": "…", "expires_at": "…" } }
  ```

How the gateway learns of a new approval without reading a body: the core's answer to the call
that queued it carries `X-Shogun-Approval-Token`; the `/mcp` proxy reads that header (and drops
it from the relayed answer) and fans out. A core that is not behind `/mcp` posts the token to
`POST /v1/approvals/notify`. All three surfaces reach the sidecar through one client
(`src/approvals/sidecar-client.ts`): `GET /v1/approvals/{id}` and
`POST /v1/approvals/{id}/decide` with the token in `X-Shogun-Approval-Token`. The sidecar verifies
the token again, spends its nonce (`409 token_replayed` on a second use, `410 token_expired`
after expiry), records who decided and when, and writes the decision to the principal's audit
log.

## Licence tokens and agent keys together (E3-T4)

The Ed25519 licence token stays the credential of the Mac app's routes. Both credentials meter
into the same string space (`auth.ts` `meterSubjectOf`: a licence id or `account:<acc>` for a
licence, `principal:<id>` for an agent key), and cost-bearing agent calls book their quote
against the same global daily breaker row the Batch lane uses (`relay_spend_days`), so the
company's spend ceiling covers both. The wallet, caps and receipts are the agent side's own
tables.

## Operations

- Provision: `psql … -f apps/api/sql/agent-keys.sql` then `-f apps/api/sql/agent-keys-permissions.sql`.
- Secrets (`wrangler secret put`): `AGENT_POW_SECRET`, `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`;
  for remote approvals `SHOGUN_APPROVAL_SIGNING_KEY` (≥ 32 characters, the sidecar's value),
  `WEBHOOK_SIGNING_SECRET` (≥ 32 characters) and optionally `SLACK_BOT_TOKEN` +
  `SLACK_SIGNING_SECRET`; vars `SHOGUN_SIDECAR_URL` and `SLACK_APPROVAL_CHANNEL`. Provision
  `apps/api/sql/approval-webhooks.sql`. The sidecar needs `SHOGUN_APPROVAL_SIGNING_KEY`,
  `SHOGUN_APPROVAL_BASE_URL=https://<gateway>` and, for the operator's CLI, `SHOGUN_ADMIN_TOKEN`.
- Vars: `AGENT_API_ENABLED=1`, `PUBLIC_BASE_URL`, optionally `AGENT_POW_BITS`,
  `AGENT_REGISTER_PER_HOUR`, `AGENT_KEY_PBKDF2_ITERATIONS`, the four `AGENT_*_CAP_*_USD`.
- Boot log: `agent_api open (card top-ups on)` / `agent_api closed (AGENT_API_ENABLED unset)`.
- Log events to alert on: `event=payment_required` runs (a client looping on 402),
  `event=capacity_exhausted` (the breaker closed the day), `event=webhook_rejected` (a bad
  Stripe signature), `stripe_failure status=…`.
- The periodic sweep deletes expired PoW rows and settled receipts older than 400 days;
  unsettled receipts are never deleted (they hold money).

Not in this slice: the core behind `/mcp` (E4 wires `mcpUpstream`; until then `/mcp` answers
`503 upstream_unavailable` before reserving), the router's `runPricer`, storage billing (the
rate is published; the sweep that books it comes with the core's usage report), and OAuth/CIMD
(E3-T2).
