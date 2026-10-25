# Batch relay on Cloudflare Workers — owner runbook

The relay (`apps/api`) runs as the Worker **`shogunai-relay`** behind **`api.shogunaios.com`**, with
its ledger in Postgres on the Supabase project, reached through **Hyperdrive**. Why this hosting
was chosen, and what must not regress, is `docs/unit-economics.md` §4 and §4.1; the design is
`docs/batch-relay-design.md`. The desktop app needs no change: same hostname, same HTTP API, same
tokens.

What the Worker is, in one paragraph: `src/worker.ts` reads its settings from bindings (the same
names `src/boot.ts` documents), opens the Postgres store (`src/pg-store.ts`) through the
`HYPERDRIVE` binding once per request, and serves the same Hono app as the Node entry. A Cron
Trigger (`*/15 * * * *`) runs the settlement sweep. A missing or malformed setting makes every
request a `503 {"error":"relay_misconfigured"}`, logged once with the variable's *name* —
fail-closed, because a Worker cannot exit.

Everything below is a one-time setup unless marked otherwise. Commands run from the repository
root with `pnpm --filter @shogun-ai/api exec wrangler …`, or from `apps/api` with `wrangler …`
after `pnpm install`. You need the Cloudflare account that owns the website Worker (the two share
the Workers Paid plan) and the Supabase project that holds the licence database.

## 1. The database: a relay-only role and the schema

The relay's six tables (`relay_*`) live in the same Supabase Postgres as the website's licence
tables, under a **role that owns only those tables and can see nothing else**. The website's role
and the relay's role never touch each other's tables (unit-economics §4.1, item 5).

Run as the project owner (Supabase SQL editor or `psql` with the owner URL):

```sql
-- 1. The role. Pick a long random password; it goes into the Hyperdrive config in §2, nowhere else.
CREATE ROLE shogun_relay LOGIN PASSWORD '<long-random-password>' NOINHERIT;
-- The per-statement ceiling the Node entry sets per connection (src/sql.ts). Hyperdrive may not
-- forward connection startup parameters, so for the Worker it is set on the role instead.
ALTER ROLE shogun_relay SET statement_timeout = '5s';

-- 2. Its own schema, so `relay_*` never shares `public` with the licence tables.
CREATE SCHEMA IF NOT EXISTS relay AUTHORIZATION shogun_relay;
ALTER ROLE shogun_relay SET search_path = relay;
REVOKE ALL ON SCHEMA public FROM shogun_relay;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM shogun_relay;
-- Nothing the website adds to `public` later becomes visible to the relay either.
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES FROM shogun_relay;
```

Then the tables, **as `shogun_relay`** so it owns them (the file is idempotent and additive-only):

```sh
psql "postgres://shogun_relay:<password>@db.<project>.supabase.co:5432/postgres?sslmode=require" \
  -v ON_ERROR_STOP=1 -c 'SET search_path = relay' -f apps/api/sql/shared-store.sql
```

Then, with the same connection, the permissions file (defence in depth, the same as
`asr-store-permissions.sql` for the speech tables): RLS on with no policy and no privilege for
`PUBLIC` / `anon` / `authenticated`. `shogun_relay` owns the tables, so the relay is unaffected.
**Existing installations run this once** (it is idempotent); nothing else in the deploy depends on it.

```sh
psql "postgres://shogun_relay:<password>@db.<project>.supabase.co:5432/postgres?sslmode=require" \
  -v ON_ERROR_STOP=1 -c 'SET search_path = relay' -f apps/api/sql/shared-store-permissions.sql
```

Check: `\dt relay.*` lists `relay_usage_days`, `relay_usage_idem`, `relay_usage_owners`,
`relay_spend_days`, `relay_spend_batches`, `relay_rate_buckets`, all owned by `shogun_relay`;
`SELECT relname FROM pg_class WHERE relname LIKE 'relay\_%' AND relkind = 'r' AND NOT relrowsecurity`
returns no row; and `SELECT count(*) FROM public.<any licence table>` as `shogun_relay` fails with
`permission denied`.

Where the website's tables share the database and have RLS enabled, nothing more is needed:
`shogun_relay` has no grant on them at all, and RLS only matters for roles that do. Do **not**
grant `shogun_relay` anything in `public`, and do not run the relay as the website's role or as
`postgres`.

Supabase note: the project must be on a plan that never pauses (the free tier pauses after seven
idle days, which would stop every submission). It already is, because the licence database lives
here — the relay adds no cost of its own.

## 2. Hyperdrive — query caching DISABLED

A **separate** Hyperdrive configuration from the website's. The one thing that must be right:
**query caching off**. The default cache (60 s) would let the batch-ownership check (who may read
a batch's results) and the idempotency `SELECT … FOR UPDATE` be answered from a stale copy. The
2026-10-10 audit judged those reads correct because they are live; a cached Hyperdrive would make
them wrong (unit-economics §4.1, item 1).

```sh
wrangler hyperdrive create shogunai-relay-store --caching-disabled \
  --connection-string "postgres://shogun_relay:<password>@db.<project>.supabase.co:5432/postgres?sslmode=require"
```

Use the **direct** host (`db.<project>.supabase.co:5432`) or the **session** pooler, not the
transaction pooler: Hyperdrive does its own pooling. `sslmode=require` is the Hyperdrive→origin
leg (item 5); the Worker→Hyperdrive leg stays inside Cloudflare's network. Hyperdrive refuses an
origin that cannot do TLS.

Paste the `id` it prints into `apps/api/wrangler.jsonc` → `hyperdrive[0].id`, then verify:

```sh
wrangler hyperdrive get <id>        # must show  caching: { disabled: true }
```

If `disabled` is not `true`, fix it before the first deploy:
`wrangler hyperdrive update <id> --caching-disabled`.

## 3. Secrets and variables

Secrets go into the Worker's own store, never into the repository, GitHub, or `vars`:

```sh
wrangler secret put ANTHROPIC_API_KEY --config apps/api/wrangler.jsonc   # the operator's Anthropic key (the brief; the background classes under BATCH_PROVIDER=anthropic) — the ONLY copy (CLAUDE.md invariant 5)
wrangler secret put OPENAI_API_KEY    --config apps/api/wrangler.jsonc   # the operator's OpenAI key for the background classes on gpt-6-luna (owner decision 2026-10-13) and the hourly review (`/v1/review`); always required
wrangler secret put DEEPGRAM_API_KEY  --config apps/api/wrangler.jsonc   # company speech grants (§ Company-funded speech rollout); unset = /v1/asr/* answer 503 speech_unavailable
wrangler secret put METRICS_TOKEN     --config apps/api/wrangler.jsonc   # optional: bearer for GET /metrics; unset = 404
wrangler secret put TYPESAFE_API_KEY  --config apps/api/wrangler.jsonc   # the operator's TypeSafe key for Jev form decisions (`POST /v1/ai/form_decide`, issue #1144); unset = that route answers 503
```

### `TYPESAFE_API_KEY` (Jev form decisions, issue #1144 / #1101 D12)

The company TypeSafe key that Macs without their own TypeSafe key use for Scribe's "fill this
form" and Memory API form fills. It lives only in the Worker's secret store: never in the
repository, a GitHub secret, `vars`, `.env` or a shell history (pipe it on stdin). It is optional
at boot so the Batch lane deploys without it; until it is set, the boot log says
`form_decide_lane closed (TYPESAFE_API_KEY unset)` and the route answers 503 (the Mac fills
nothing). After setting it, the next isolate logs `form_decide_lane open`.

- **Limits** (code, not vars): 30 requests per licence per UTC hour, 2 in flight per device, and
  the existing daily spend breaker and per-licence budget. TypeSafe is priced at $0.042 per
  million input tokens (output free) in `src/spend.ts`; a reservation is request bytes ÷ 4 tokens,
  settled from TypeSafe's `usage.input_tokens`. If `SPEND_PRICE_TABLE_JSON` is set, add a
  `"jev-1.13.0": {"input":0.042,"output":0,"cacheRead":0.042,"cacheWrite":0.042}` row to it,
  or form decisions are reserved at the table's worst-case input rate.
- **Spend alerts:** set a hard limit and a billing alert in the TypeSafe console (the final wall,
  like the other providers). In Workers Logs, alert on any `ALERT typesafe_key_rejected` line
  (the company key was refused: rotate it) and on a run of `event=upstream_overloaded` /
  `event=upstream_rate_limited` on `/v1/ai/form_decide`.
- **Rotation:** create the new key in the TypeSafe console, `wrangler secret put
  TYPESAFE_API_KEY` with it, push one authenticated form decision through and see
  `POST /v1/ai/form_decide 200 … provider=typesafe model=jev-1.13.0` in Workers Logs, then revoke
  the old key. On a suspected leak, revoke right after the put. Nothing changes on the Macs.
  Full procedure: `docs/ai-gateway-runbook.md` §9-9.
- **Pre-launch check (owner call, open):** TypeSafe's retention / zero-data-retention terms for
  text sent on the company key (`docs/ai-gateway-launch-checklist.md` §3).

Non-secrets are `vars` in `wrangler.jsonc` and ship with each deploy (so a change is a PR, and
the dashboard shows what is in force). Every name is documented in `src/boot.ts`:

| var | default in code | note |
|---|---|---|
| `LICENSE_PUBKEY_B64` | required | the `ed25519` value of `packages/config/license-public-key.json` (public, not a secret); boot refuses any other key. During a rotation also set `LICENSE_PUBKEY_NEXT_B64`. |
| `DAILY_SPEND_CAP_USD` | **required** | the global breaker. Formula below. `0` is the emergency stop for the whole Batch lane. |
| `LICENSE_DAILY_SPEND_CAP_USD` | `0.50` | per licence per UTC day (unit-economics §3.3) |
| `TRIAL_DAILY_SPEND_CAP_USD` | `0.25` | per trial **payer** per UTC day: the account for a trial issued through an account (all of its devices share it), the device for a device-only trial (audit 2026-10-20 H1); never above the licence budget |
| `DAILY_CHUNK_CAP` / `TRIAL_DAILY_CHUNK_CAP` | `2000` / `500` | chunk caps, metered on the same payer |
| `TRIAL_SPEND_SHARE` | `0.5` | the share of the global breaker trials may reach — Batch, review and speech alike |
| `RATE_BURST` / `RATE_PER_MIN` | `12` / `12` | per payer + device + route |
| `SETTLE_INTERVAL_SECONDS` | `900` | on Workers only `0` matters: it switches the sweep off without touching the cron |
| `SPEND_PRICE_TABLE_JSON` | unset | price rows **merged over** the shipped table (`src/spend.ts` `priceTableWithOverride`): a row you set replaces that model's price, every other model keeps its shipped row. `gpt-6-luna` is in the shipped table ($0.05 / $0.25 Batch). A model in neither is priced at the most expensive card, never below the hard-coded `UNKNOWN_MODEL_FLOOR` (Sonnet-class Batch rates), so a typo here can over-reserve but never make a model cheap |
| `RELAY_DENY_SUBJECTS` | unset | **emergency only** (audit 2026-10-20 L3): comma-separated licence ids and/or `account:<acc>` subjects refused with 401 on every route. See "Revoking a token early" in §8. Set it in the dashboard (ids are not committed); `keep_vars: true` keeps it across deploys |
| `BATCH_PROVIDER` | `openai` | `openai` (the four background classes on `gpt-6-luna`) or `anthropic` (Claude Haiku 5.5 rows). The brief follows the same switch since 2026-10-16: luna under `openai`, Sonnet 5.5 under `anthropic` (`src/models.ts`) |

### Budget defaults and the global breaker

Per-licence `$0.50/day` and per-trial `$0.25/day` are the defaults in `src/boot.ts` and the values
in `wrangler.jsonc` (unit-economics §3.3: about twice a typical night's actual cost, about 30% of
the plan price over a month; a trial costs the company at most $1.75 — per account for a trial issued through an account
(audit 2026-10-20 H1), per device id on the device-only path). Reservations are a
`bytes/2` over-estimate, so a licence hits its cap at two to three times its real usage; the
settlement sweep writes the actual figure back within 15 minutes of the batch ending.

The global breaker is sized from active licences:

```
DAILY_SPEND_CAP_USD ≈ active licences × 0.50 × 1.5
```

e.g. 20 active licences → `15.00` (the value shipped in `wrangler.jsonc`), 100 → `75.00`. Revisit
at the start of each month (the licence database has the count). A breaker that is too low fails
safe (429 `spend_cap` for the rest of the UTC day, nothing sent); one that is too high only
matters if the per-licence budgets are also wrong.

### Provider-side hard limits — MANDATORY before launch (audit 2026-10-20 M3)

The breaker and the per-licence budgets are the relay's own arithmetic: a bug in them, a wrong
price row, or a credential used outside the relay (a Deepgram grant JWT, a leaked key) is not
stopped by them. Each provider's own console is the final wall. **Do every row below before real
users' traffic reaches that provider, and record it (screenshot or export, with the date) in
`docs/ai-gateway-launch-checklist.md` §1-12.** Re-check monthly with the breaker formula above.

A useful ceiling for a provider's monthly budget is what the relay can book on it in a month:
`DAILY_SPEND_CAP_USD × 31`, plus headroom you are willing to lose to a bug (the breaker is shared,
so no single provider can legitimately exceed it).

| provider | what to set | where |
|---|---|---|
| **OpenAI** | **A separate project per lane**, each with its own key: the Batch lane (the background classes on `gpt-6-luna` and the hourly `/v1/review`, which share `OPENAI_API_KEY` today) and, when #1101 ships them, each live lane. Per project: a **monthly budget** with enforcement on (requests stop at the budget — the hard limit, not just a notice); **low rate limits** for the models the lane uses (Project → Limits: RPM/TPM a small multiple of the expected peak, every model the lane does not use at its lowest or disallowed); **billing alerts** at 50 / 80 / 100%. If the organisation is on prepaid credit, keep auto-recharge off or capped, so the balance is a wall too. | platform.openai.com → Projects → (project) → Limits / Billing |
| **Groq** | The same before the dictation-cleanup lane ships (#1101; this relay does not call Groq yet): its own project and key, a **monthly spend limit** that stops requests, low per-model rate limits, alerts at 50 / 80 / 100%. ZDR on (launch checklist §3). | console.groq.com → Settings → Billing / Limits |
| **Anthropic** | A dedicated workspace for the relay's `ANTHROPIC_API_KEY` (`BATCH_PROVIDER=anthropic` and the fallback), with a **workspace monthly spend limit** and low rate limits; organisation spend notifications at 50 / 80 / 100%. | console.anthropic.com → Settings → Workspaces / Limits |
| **Deepgram** | A **dedicated project** for company speech (the relay refuses to mint unless the key sees exactly one project), with a **hard monthly budget**: fund it only with prepaid credit up to the month's budget and keep auto top-up off, so the balance stops usage; a usage/balance alert at 50 / 80%. The key is Member, scoped to that project only. Since 2026-10-22 `ASR_ALLOWED_SUBJECTS` is unset and every paid licence and trial is admitted, so this prepaid cap is the hard wall: a grant JWT can call any Deepgram API the project allows, and the relay cannot bind a tag or a limit to it until the speech socket is proxied. | console.deepgram.com → (project) → Billing / Settings |

Console labels and plan features change; the "where" column is a starting point, not a promise.
If a provider offers only alerts and no enforced stop on your plan, the wall is prepaid credit with
auto top-up off (or capped), and the checklist entry says so. An alert alone does not count.

## 4. The custom domain and the cron

Both are in `wrangler.jsonc` and are applied by `wrangler deploy`:

- `routes: [{ pattern: "api.shogunaios.com", custom_domain: true }]` — Cloudflare creates the DNS
  record and certificate. The zone `shogunaios.com` must be on this account (it is; the website
  uses it). If a hand-made `A`/`CNAME` for `api.shogunaios.com` still exists, delete it first or
  the deploy reports a conflict. `workers_dev` and `preview_urls` are off: the custom domain is the
  only hostname, so the rate-limit rule below cannot be bypassed.
- `triggers.crons: ["*/15 * * * *"]` — the settlement sweep (`settleOnce`, audit M9).

After the first deploy, add one **WAF rate-limiting rule** in the dashboard (Security → WAF → Rate
limiting rules) on hostname `api.shogunaios.com`, **path starting with `/v1/`** — every relay route:
`/v1/batch*`, `/v1/review` and `/v1/asr/*`, not only the Batch lane (audit 2026-10-20 L1): e.g. 60
requests per minute per IP, action Block for 1 minute. The relay's own limiter is per payer + device
+ route; this one is the per-IP layer a single-server deployment did not have (unit-economics §4.1),
and the only one in front of signature verification. If an existing rule matches `/v1/batch` only,
widen it to `/v1/`. `/healthz` stays outside the rule.

Workers Logs is on (`observability.enabled`). Set the dataset's **retention short** (the shortest
the plan allows): only `src/logging.ts` writes to the console, so lines are route templates, status
codes, counters and variable names — but shorter is still better (item 4). **Invocation logs stay
off** (`observability.logs.invocation_logs: false` in `wrangler.jsonc`): the runtime's per-request
record carries request metadata including headers, and every relay request's `Authorization` is a
licence bearer. If the dashboard shows invocation logs on, turn them off there too; the relay's own
`POST /v1/batch 202 …` lines are the request log.

## 5. Deploy

Deploys are deliberate: the workflow **Deploy batch relay (Cloudflare Workers)**
(`.github/workflows/deploy-relay.yml`) runs only from the Actions tab (`workflow_dispatch`, Linux
only). Unchecked, it verifies (typecheck, the Node suite, the Workers-runtime suite, the bundle
check); with **deploy** checked it also refuses a `wrangler.jsonc` that still holds a `REPLACE_WITH`
placeholder, reads the Hyperdrive id out of it and fails unless `wrangler hyperdrive get` reports
`caching.disabled: true` (§2), then uploads the Worker and confirms `/healthz`. It uses the
repository secrets `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` the website's `deploy.yml`
already has (the token needs Hyperdrive read for the guard) — the Anthropic key is never in GitHub.
Every action in it is pinned to a commit SHA.

From a laptop (same thing, your own Cloudflare login):

```sh
pnpm --filter @shogun-ai/api typecheck
pnpm --filter @shogun-ai/api test          # Node suite + Workers-runtime suite (workerd)
pnpm --filter @shogun-ai/api check:bundle  # dry run; refuses a bundle that carries the Node entry's server or a Node-only module
pnpm --filter @shogun-ai/api deploy
```

**Deploy order against a device release.** The relay attaches a JSON schema to every Batch item
(`src/schemas.ts`, `output_config.format`), and the decoder is bound by it: a field the device
prompt asks for but the schema does not list is never emitted, silently, after the batch is
billed. So whenever a device build changes a Batch contract (a new field, a renamed one), deploy
the relay that carries the matching schema **before or together with** that build — never after.
An old relay under a new device loses the new field for every batch until it is updated (the
sessionised consolidation anchors and the mail triage `commitments` / `open_loops` of PR #1022
are the standing example); a new relay under an old device is harmless, since the device parsers
ignore fields they do not read and the schemas keep every field the old prompts asked for.

## 6. First-deploy smoke

1. **Boot.** `curl -sS https://api.shogunaios.com/healthz` → `{"ok":true}` (HTTP 200).
   A `503 {"error":"relay_misconfigured"}` means a variable or binding failed validation: Workers
   Logs has one line naming it (`missing required env: …`, `invalid …`, `missing required binding:
   HYPERDRIVE`). Fix and redeploy; the Worker never serves half-configured.
2. **Store.** `wrangler tail --config apps/api/wrangler.jsonc` and wait for the next quarter hour:
   the cron logs `settlement_sweep {"checked":0,…}`. A `shared store table missing or unreadable:
   relay_…` line means §1 was not applied under this role/schema. To force a tick without waiting:
   `curl "http://localhost:8787/__scheduled?cron=*/15+*+*+*+*"` against `pnpm --filter
   @shogun-ai/api dev:worker` (needs a local Postgres with the schema; see `localConnectionString`).
3. **A probe batch with a test token.** Issue a licence token for a test licence from the licence
   API (the same `v1.<payload>.<sig>` the app caches), then:

   ```sh
   TOKEN='v1.…'
   curl -sS -D - -o /tmp/probe.json https://api.shogunaios.com/v1/batch \
     -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{"client_request_id":"probe-'"$(date +%s)"'","purpose":"consolidation","model_class":"classify","items":[{"custom_id":"probe","chunk":"ping"}]}'
   ```

   Expect `202` and a `batch_id`. Then `GET /v1/batch/<batch_id>` with the same bearer polls it;
   `GET` by *another* test licence must be `404` (the ownership check — the reason Hyperdrive
   caching is off). In the database, `SELECT * FROM relay.relay_usage_days` shows one row for the
   probe licence with `chunks = 1`. The probe costs a fraction of a cent on the operator key.
4. **Caps.** Repeat the probe past `RATE_BURST` within a minute → `429` with `Retry-After`.
5. **Logs.** In `wrangler tail`, every line is `POST /v1/batch 202 12ms chunks=1 … license=<12 hex>`
   or similar. If you can read a chunk, a token or an id in there, stop and open an issue: it is a
   regression of the redacting logger.

## 7. Rollback

Workers keep every uploaded version. To go back to the previous one without rebuilding:

```sh
wrangler versions list    --config apps/api/wrangler.jsonc        # find the previous version id
wrangler rollback <version-id> --config apps/api/wrangler.jsonc   # instant, no build
```

Secrets and the Hyperdrive config are not part of a version; a rollback changes code and `vars`
only. The ledger is in Postgres and is **never** rolled back: a row that disappears authorises a
duplicate paid submission (`sql/shared-store.sql`). If a bad deploy is spending money, set the
breaker first — `DAILY_SPEND_CAP_USD` to `0` in `wrangler.jsonc` and deploy — then diagnose.

## 8. Operating notes

- **Metrics.** `GET /metrics` with `Authorization: Bearer <METRICS_TOKEN>` is per isolate (each
  Workers isolate keeps its own five-minute window); the ledger in Postgres is the authority on
  spend: `SELECT date, micros FROM relay.relay_spend_days ORDER BY date DESC LIMIT 7`.
- **Scaling.** Nothing to do: every request reserves against the same Postgres rows, so more
  isolates never multiply a cap. The single-instance Node ledger (SQLite file, instance lock,
  machine-id pin) was removed on 2026-10-12; Postgres is the only persistent store.
- **Retention.** The cron tick runs `SqlUsageStore.maintenance` before settling: day counters,
  spend days and spend batches older than 45 days, device-less legacy owner rows older than 7
  days, and rate buckets idle for 10 minutes are deleted then — never inside a request. Request
  tombstones (`relay_usage_idem`) are never deleted. The settlement pass polls at most
  `SETTLE_MAX_PER_TICK` (500) batches per tick and leaves a still-processing batch alone for
  `SETTLE_RECHECK_SECONDS` (900); `src/boot.ts` has the ceiling math (48,000 polls/day).
- **The hourly review's bill (audit 2026-10-20 C1).** `/v1/review` reserves for both of its
  attempts (the first call and one retry on an empty / invalid answer) and closes the reservation
  on what OpenAI billed: the summed `usage` of every attempt, whether the answer was usable or
  not (cut off, filtered, refused, invalid JSON). When an attempt's bill is unknown (timeout,
  dropped connection, 5xx) the whole reservation stays booked and the log says
  `review_bill_unknown (reservation kept)`. The day's chunk is returned only when nothing reached
  the provider.
- **Revoking a token early (audit 2026-10-20 L3).** A relay token is a signed bearer credential,
  verified locally with no call to the licence API, and it stays valid until its `exp`: **up to 26
  hours after issue** (a trial's never past the trial's end). Revoking a licence at the licence
  API stops new tokens, not the ones already issued. When that window matters (a leaked token, an
  abusive account), add the licence id or `account:<acc>` to `RELAY_DENY_SUBJECTS` in the Worker
  dashboard: every route, Batch, review and speech, answers 401 from the next request. Remove the
  entry once 26 hours have passed since the licence was revoked. Until then the blast radius of a
  leaked token is its payer's daily budgets.
- **Which limits fail open (audit 2026-10-20 L4).** The rate limiter (`SqlRateLimiter`) **fails
  open** on a database error and logs `ratelimit_unavailable (failing open)`: pacing is not the
  spend control. The ledger right behind it — chunk caps, per-licence and global spend — **fails
  closed**: a reservation it cannot write is a `503` with `Retry-After: 1` and no provider call,
  and speech grants mint nothing. So a database outage can let requests through the limiter, but
  never past the ledger to a provider.
- **Key rotation.** Follow `src/auth.ts`: add `LICENSE_PUBKEY_NEXT_B64` to `vars` when the shipped
  file gains `ed25519_next`, deploy, and remove it when the rotation completes. Boot refuses a
  mismatch either way.
- **Local development** stays on Node: `pnpm --filter @shogun-ai/api dev` runs the in-memory
  store (nothing persists across a restart; the boot log says so) unless `RELAY_DATABASE_URL`
  points at a Postgres with the schema applied; `RELAY_ALLOW_NO_SPEND_CAP=1` for a breaker-less
  dev relay. `dev:worker` runs the Worker in miniflare against the `localConnectionString` in
  `wrangler.jsonc`.


## Company-funded speech rollout (#1101 D2)

The operator's `DEEPGRAM_API_KEY` belongs only in the relay Worker Secret. Do not
copy it to a Mac, source file, GitHub secret, or build environment. The key needs
Deepgram Member permissions to mint grants. `ASR_ENABLED=1` enables the route;
it is **off by default**, independently of Batch/review availability.
**Who gets company speech (owner decision 2026-10-22).** With `ASR_ALLOWED_SUBJECTS` unset, every
valid **paid** licence and every live **trial** is admitted. This reverses audit 2026-10-20 H3's
trial refusal: trials now start only with a card at Checkout, a trial's speech reserves against
the `TRIAL_SPEND_SHARE` of `DAILY_SPEND_CAP_USD`, and the per-subject monthly allowance and the
Deepgram project's prepaid budget still bound the rest. The known exposure stays until the speech
socket is proxied: each admitted subject holds, for up to 60 s per grant, a JWT for the company
project that can call any Deepgram API the project allows. Keep that project dedicated and
prepaid with auto top-up off.

`ASR_ALLOWED_SUBJECTS` is now the operator's way to **narrow** company speech again without a
deploy: set it to licence ids, or `account:<acc>` for a trial issued through an account,
comma-separated (a present but empty list admits nobody). Set it in the Worker dashboard (licence
ids are not committed); `keep_vars: true` in `wrangler.jsonc` keeps a `wrangler deploy` from
deleting it, so to open speech to everyone, delete it in the dashboard.

Before enabling, apply `sql/asr-store.sql` and `sql/asr-store-permissions.sql` as
`shogun_relay` with `search_path=relay`. Rollback: `../../docs/migrations/relay-asr-rollback.md`.
These additive tables hold identifiers, time and counters only. Enable RLS on all
four tables with no public policies and revoke access from `anon`/`authenticated`;
the existing relay owner role performs the internal queries. Rollback disables
`ASR_ENABLED` and retains accounting tables. Never drop them to reset a cap.

`POST /v1/asr/grant` verifies the existing signed licence token. It reserves up to
one hour per stream, within 20 stream-hours per UTC month, at most two streams per
licence, and one grant request per device per 20 seconds. A two-channel meeting
uses one grant and accounts both channels. Audio remains Mac → Deepgram directly
with `mip_opt_out=true` and an HMAC subject tag. No audio is sent to the relay.
The Mac closes supported sessions at their allotted deadline. It reports closure
at `/v1/asr/finish`; accounting uses server elapsed time, not a reported duration.
Abandoned sessions keep their reservation; a duplicated finish never refunds twice.
A finish never refunds the window in which the session's latest JWT could still open a socket
(60 s after the grant or the latest refresh; audit 2026-10-20): a finish sent right after a grant
is billed that minute per stream, not one second. Sockets kept open beyond it are invisible to the
relay until the speech socket is proxied; the provider-usage floors and the project's hard budget
bound them. A grant or refresh inside the 20-second pacing window is refused (429) before the
provider usage is read, so it costs no Deepgram call.

The shared `DAILY_SPEND_CAP_USD` also covers speech reservations, conservatively
at $1 per stream-hour (a reservation estimate, not a vendor price quote). A trial
reserves only up to its `TRIAL_SPEND_SHARE` of it, as on the Batch lane (audit
2026-10-20 H3), so speech never lets trials into the paid users' part of the day.
Speech minutes are metered per payer: an account's trial devices share one monthly
allowance (`account:<acc>`), a paid licence keeps its licence id. Normal
finishes return unused reservations. Before every grant and refresh the relay reads
Deepgram aggregate usage (never request logs): project-wide daily counters and the
subject-tagged month. Missing permissions, ambiguous projects, invalid counters, or
provider outages refuse new tokens. Apply `sql/asr-provider-usage.sql` to upgrade
an existing installation. Provider month counters form a nondecreasing admission
floor. Daily provider usage is charged additionally at the reservation rate; this
intentionally over-reserves rather than trusting client closure. This is
aggregate reconciliation with provider reporting delay, not real-time revocation. The schema must exist before enabling the
route; a missing/unavailable ledger mints nothing. Contents, grants and provider
error bodies must not be logged. Counters/session metadata expire after 400 days (month counters at month granularity).

**Non-speech usage (audit 2026-10-20 H3).** A grant can call TTS / agent / text APIs too, and the
usage tag is whatever the client sends, so the relay can only attribute what is tagged:

- Non-speech usage under a **subject's own tag** blocks that subject (`relay_asr_subjects.blocked`)
  and logs `ALERT speech_subject_blocked reason=tagged_non_speech_usage`.
- Non-speech usage on the **project** that no tag attributes logs
  `ALERT speech_project_non_speech_usage scope=project …` on every grant/refresh while it shows,
  and **blocks nobody**. (Before this change it latched a project-wide block: any one grant holder
  could switch speech off for every user.) Alert on that line in Workers Logs.
- The **operator stop** is unchanged and manual: it refuses every grant and refresh at once.

  ```sql
  -- stop company speech for everyone (as shogun_relay, search_path=relay)
  INSERT INTO relay_asr_provider_usage(scope,period,seconds) VALUES('blocked','global',1) ON CONFLICT DO NOTHING;
  -- resume after investigating
  DELETE FROM relay_asr_provider_usage WHERE scope='blocked' AND period='global';
  ```

  A row written by an older relay's automatic latch looks the same: if `SELECT * FROM
  relay_asr_provider_usage WHERE scope='blocked'` shows one after this deploy, investigate the
  project's usage before deleting it. `ASR_ENABLED=0` is the other off switch.

**What a grant cannot carry.** Deepgram's `POST /v1/auth/grant` takes only `ttl_seconds`
(`src/deepgram.ts`); it does not bind a usage tag, a model, an endpoint scope or a spending
limit to the JWT, and the JWT does not end an already-open stream at expiry. Re-check Deepgram's
token API before each release; if it gains tag or limit binding at mint time, use it. Until the
speech socket is proxied, the real ceiling is the **dedicated Deepgram project with a hard monthly
budget** (§3, "Provider-side hard limits") plus the allowlist.

### Gates before public activation

- Record the Deepgram no-retention written confirmation required by CLAUDE.md
  invariant 3 / `docs/ai-gateway-launch-checklist.md`. `mip_opt_out=true` alone is
  not proof of zero retention.
- Verify the real key can mint a 60-second grant and complete a WebSocket
  handshake, without sending user audio during setup.
- Verify provider-side usage reconciliation and misuse handling. A Deepgram JWT
  can be reused for other connections/APIs, its tags are client-supplied, and an
  already-open stream survives JWT expiry. These admission/client limits are NOT
  an adversarial hard spending ceiling. Do not describe them as one.
- Record the dedicated Deepgram project's hard monthly budget (§3, "Provider-side hard
  limits") and an alert on the `ALERT speech_` lines in Workers Logs.
- Exercise no-BYOK dictation and dual-channel meetings on the signed Mac build,
  including provider refusal, deadline, cancellation, network outage/reconnect,
  and expiry. Hosted REST replay is disabled to avoid paying twice on ambiguity;
  meeting reconnect refreshes an expired grant against the same active reservation; refresh cannot extend the session deadline or replenish quota.
- Verify final-revision CI before merging and ship the server before the Mac app.

### Operator verification (2026-10-17)

- Owner confirmed written no-retention approval in this task; the document location
  has not yet been supplied. Keep the operator's original confirmation with release evidence.
- Replacement company key: real grant 200 / 60 seconds, direct WebSocket accepted,
  finish 200. Two simultaneous Japanese synthetic streams transcribed the expected
  words with `mip_opt_out=true`; PCM was generated and streamed in memory only.
- Aggregate usage API and accounting gate also admitted the real two-stream smoke.
- Signed Mac UI: no personal speech key configured. Dictation inserted the expected Japanese sentence; an in-person meeting saved both Japanese sentences in its transcript. The existing English meeting setting was changed to automatic for this test; new installations now default to automatic recognition.
- The ignored Rust live smoke passed with both dictation and meeting streams, no dropped meeting samples, then a reconnect after 65 seconds. Refresh kept the original quota deadline and closure completed normally.
- Real PostgreSQL concurrency checks passed for admission, finish, idempotent usage reconciliation and reservation above the provider month floor.
- Final CI, distribution and public activation evidence is tracked in PR #1158 and its associated release. The results above describe staged verification.

Owner decision 2026-10-22: `ASR_ALLOWED_SUBJECTS` is removed and company speech is open to every paid licence and trial (the audit 2026-10-20 H3 trial refusal is reversed; see "Who gets company speech" above). Proxying the speech socket through the relay remains the follow-up that closes the grant-JWT exposure.

## 9. The agent API (plan E3 / E9) — off until switched on

The `sa_` keys, wallet, 402, quote, receipts and the metered `/mcp` proxy (`README-agent-api.md`)
are served only with `AGENT_API_ENABLED: "1"` in `vars`. Before the switch:

1. Provision the tables, as the relay role with `search_path=relay`:
   `psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/agent-keys.sql` then
   `-f apps/api/sql/agent-keys-permissions.sql`. Idempotent, additive only.
2. Secrets: `wrangler secret put AGENT_POW_SECRET` (any ≥16-character random string; rotating it
   only invalidates unspent registration challenges) and, for card top-ups, `STRIPE_SECRET_KEY`
   and `STRIPE_WEBHOOK_SECRET` (both or neither; the webhook endpoint is
   `https://api.shogunaios.com/v1/stripe/webhook`, event `checkout.session.completed`).
3. Vars: `AGENT_API_ENABLED: "1"`, `PUBLIC_BASE_URL: "https://api.shogunaios.com"`; optional
   `AGENT_POW_BITS` (18), `AGENT_REGISTER_PER_HOUR` (5), `AGENT_KEY_PBKDF2_ITERATIONS` (20000),
   `AGENT_KEY_CAP_DAY_USD` / `AGENT_KEY_CAP_MONTH_USD` / `AGENT_PRINCIPAL_CAP_DAY_USD` /
   `AGENT_PRINCIPAL_CAP_MONTH_USD` (1 / 20 / 5 / 100).

The boot log says `agent_api open (card top-ups on)` or `agent_api closed (…)`. Cost-bearing
agent calls book against the same `DAILY_SPEND_CAP_USD` day as the Batch lane (E3-T4), so size
the breaker with agent traffic in mind once the lane opens; the wallet, caps and receipts are the
agent side's own tables. `/mcp` answers `503 upstream_unavailable` before reserving anything
until the core (E4) is wired behind it.
