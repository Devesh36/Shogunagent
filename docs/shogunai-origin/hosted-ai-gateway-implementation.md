# #1101 hosted AI implementation — live text relay

This is the first executable slice of the [gateway design](hosted-ai-gateway-design.md).
It is **not a completed #1101 rollout**. No production deployment or provider call was
performed by this change. Current hosting stays Cloudflare Workers + Supabase.

## Implemented

- Worker routing for `POST /v1/ai/{lane}` and `GET /v1/ai/usage`. `POST /v1/ai/form_decide`
  (#1144) is not a live lane and stays on the Batch relay app.
- Existing Ed25519 licence/trial authentication, including optional signed `acc`. As on
  every relay route, an ended trial answers 402 `plan_required` and the operator's
  emergency denylist (`RELAY_DENY_SUBJECTS`, audit 2026-10-20 L3) answers 401.
  Budgets are metered on the relay's payer subject, `meterSubject` in `src/auth.ts`, the
  same subject as every other relay budget (owner decision 2026-10-21): a trial issued
  through an account meters by that account across all its Macs, a device-only trial by
  its trial licence (`trial_<device>`), and a paid plan by its licence id even when the
  token also carries `acc`. This does not yet issue account sessions or add `acc` on the
  licence server.
- Nine server-configured live lanes. Rewrite requires `gpt-6-luna`, `low`, priority;
  dictation cleanup uses Groq; the other lanes use OpenAI Responses. Text inputs and
  function tools for chat/agent only. Remote/image attachments and hosted tools are
  refused until their own reviewed contracts are implemented.
- Explicit input/output prices, no free-price fallback, independent company live keys,
  `store:false`, HMAC safety/cache ids, body/output limits, no provider redirects.
- Postgres transaction/row-lock admission across Worker isolates: user daily/monthly
  budgets, provider daily breaker, 3 user streams / 2 device streams, 12 requests per
  user/lane/minute. No in-process counters or status cache.
- D9 caps, owner-approved 2026-10-21. Daily: trial $1, Standard $3, Pro $6; monthly Standard $40 /
  Pro $80. Trial monthly ceiling is seven daily allowances ($7). Reservations and
  actual use are included in the same balance. Batch retains its existing budgets.
- Stream settlement exactly once; unknown or interrupted usage keeps the reservation.
  A 240-second stream deadline precedes the 300-second concurrency lease. Usage above
  the reservation is recorded at actual cost. Cached input uses the full input price
  until a separately verified discount is configured in a later change.
- Bounded SSE framing, 15-second idle comments, cancellation propagation, fixed errors,
  no prompt/token/id in request logs. The DB connection remains open through settlement.
- Usage counters and request records expire after 400 days via the existing Worker cron.
- A transactional, audited `SqlLiveStore.block()` API. The next admission reads shared
  Postgres, even in another isolate. **No public admin endpoint is exposed**. Cloudflare
  Access admin UI/session revocation and active-stream interruption are still pending.

## Desktop live text connection

**Owner decision 2026-10-21:** SHOGUN, the company-key hosted lane, is the fixed default
source with no picker in the app (#1318 removed the notch Settings model card: model choice
is predetermined). `LlmSettings::default()` is `shogun`, so a Mac with no saved `llm.json`
choice runs on the gateway. A saved API-key provider or subscription delegate is an
explicit per-lane override and is kept; the app does not rewrite it. Subscription
delegation is still chosen under Settings > Intelligence (onboarding no longer has an AI
source step after #1314). MCP / REST / CLI keep `agent.source.set`
(`{"source":"shogun"}`, `shogun agent-source use shogun`) and `agent.source.get` reports
`selected_kind: "hosted"`, the gateway and the downstream provider. Settings > Privacy &
Security discloses the route (api.shogunaios.com to OpenAI, nothing saved) in all four UI
languages. Because the default now points at the gateway, a release carrying this change
needs the deployment prerequisites below in place; this PR is not a public rollout.

- Typed `LicenseBearer` constructors reuse the Responses parser and tool loop, with a
  fixed operator destination (only the existing loopback development override is allowed).
  Client model, effort, retention, continuation IDs and output caps never cross the wire.
- Chat, reply draft, inline rewrite, Scribe, meeting summary/translation, Quick Translate
  and other live text intents are connected. Batch tags cannot construct a hosted lane.
  Inline rewrite uses `/v1/ai/rewrite`; Scribe uses `/v1/ai/scribe`.
- Chat tools retain the existing local read/proposal/approval gates. Stateless follow-up
  turns replay function outputs and encrypted reasoning (including the required item ID;
  [official SDK contract](https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_reasoning_item_param.py)); the gateway admits only text and
  function tools (up to 32, enough for connected-service plus local-memory tools).
- Both buffered and streamed calls consume SSE. Incomplete streams fail; stopped chat
  requests cancel. Hosted failures are not automatically retried into new reservations.
  HTTP redirects are refused and responses have a 4 MiB / 260 s device bound.
- V91 preserves existing traceability/usage rows and adds the distinct `live_relay` route
  ([rollback](migrations/V91-rollback.md)). The trace screen identifies the operator hop and
  downstream OpenAI, and the whole-log operator-relay disclosure count includes it; text is
  never stored in these ledgers.
- The desktop TypeScript library is ES2022 to match existing `Array.at()` calls supported
  by the macOS 14+ webview; strict checks remain enabled.

- **Readiness gate.** SHOGUN is reported ready to the panel and the chat-tools line only after
  `GET /v1/ai/usage` answered this Mac's licence or trial token with 200. The check sends no
  user text, is traced as `live_relay` / `live_status` before it is sent, and reruns at launch
  and on each source change while SHOGUN is the source. While it is the source and the check
  fails, Privacy & Security shows an "unavailable" line under the disclosure.
- **Gateway refusals are SHOGUN's own copy.** `budget_exceeded` / `plan_required` end the chat
  turn with `hosted_limit`; `blocked`, `live_not_enabled`, `provider_budget_exceeded`,
  `upstream_failed` and other 402/502/503 answers with `hosted_unavailable`; per-minute and
  concurrency refusals reuse `rate_limited`. Gateway text is never shown.
- **No background spend.** The hourly style-note pass (`STYLE_NOTE_MINE`) never takes the
  hosted lane: work that runs without a user action does not draw on the company live budget.
- The trace screen names the downstream provider per lane (Groq for `dictation_cleanup`,
  OpenAI otherwise; none for the readiness check).

### Every lane gateway-first (2026-10-22)

The user's own keys stay as the override past the plan allowance (#1334); the company path
is tried first everywhere it exists.

- **Dictation cleanup** posts `{"instructions","input"}` to `/v1/ai/dictation_cleanup` with the
  licence or trial token and reads the Groq chat-completions stream
  (`openai_compat::complete_hosted_cleanup`, traced `live_relay` / `dictation_cleanup`). A
  refusal (allowance, unavailable) backs the gateway off for 5 minutes and uses the saved
  Groq key if there is one; without one the dictation keeps its local text silently, as with
  no key before. The Groq lane sends `reasoning_effort: "low"` and `include_reasoning: false`
  when the lane config sets `effort: "low"` (gpt-oss).
- **Deepgram** uses the company grant (`/v1/asr/grant`) first. The saved `deepgram-asr` key is
  looked up only when the grant is refused, then serves the rest of that stream
  (`deepgram::HostedThenOwn`). Dictation's passive path still reads the key without a dialog.
- **Jev form decisions** use the relay (`/v1/ai/form_decide`) first; the saved TypeSafe key is
  used when there is no bearer, or once after a `jev_relay_*` refusal.
- **A saved API-key provider with no key in the Keychain** runs chat on SHOGUN in memory;
  `llm.json` keeps the saved choice.

## Enablement configuration (not enabled by this change)

`LIVE_AI_ENABLED=1` activates the routes. Missing/invalid configuration or unavailable
storage fails closed. Existing Batch deployments without this flag keep working.

Secrets (via the existing Wrangler secret-input workflow, never inline in command
history): `OPENAI_LIVE_API_KEY`, `GROQ_LIVE_API_KEY`, `LIVE_HMAC_KEY` (at least 32
characters of random material). Never reuse `OPENAI_API_KEY` from the Batch project.

Required server vars:

- `LIVE_GLOBAL_DAILY_CAP_USD`: explicit cap **per provider**. Zero is the emergency stop.
  The sum of the two provider ceilings is the maximum configured live daily envelope.
- `LIVE_LANE_CONFIG_JSON`: every lane must have `model`, `maxOutputTokens` (1–16384),
  `inputMicrosPerToken`, `outputMicrosPerToken` (positive, finite), and optional `effort`
  (`none` or `low`). Values are micro-USD **per token**, numerically equal to USD per
  million tokens. Supply current contracted rates, including rewrite priority pricing.
  No invented pricing table or production key is shipped in this change.

Schema/grants and rollback: [relay-live-1101-rollback.md](migrations/relay-live-1101-rollback.md).
Configuration, schema and provider readiness are deployment prerequisites. The desktop
default is SHOGUN (owner decision 2026-10-21), so do not ship a desktop build with it before
the endpoint is provisioned.

## Verification commands

```
pnpm --filter @shogun-ai/api typecheck
pnpm --filter @shogun-ai/api test
pnpm --filter @shogun-ai/api check:bundle
```

For real PostgreSQL concurrency tests, apply `apps/api/sql/live-store.sql` to a disposable
database and set `RELAY_LIVE_TEST_DATABASE_URL` before running `vitest run test/live.test.ts`
in `apps/api`. The suite truncates **only that database's live tables** between tests.
Never point it at production. The default run uses a serialized SQLite SQL double;
it is not evidence of PostgreSQL cross-session locking on its own.

On this Mac, the existing Workers runner fails resolving `cloudflare:test-internal`
when the checkout path includes `α`; verify from an ASCII-path source copy if needed.
Keep the same source, lockfile, runtime configuration and tests. Do not disable tests.

## Remaining #1101 implementation / launch gates

- Phase 0: provision real Hyperdrive binding/secrets and verify live Batch/Brief recovery.
- Phase 1: account migrations, Google and passkey website sessions, PKCE loopback,
  licence `acc` issuance/linking, device/session revocation, trial uniqueness, recovery.
- Desktop: explicit per-lane BYOK/delegation overrides, a remaining-allowance display, and
  live usage/cap parity in MCP/REST/CLI.
- Deepgram: grant restoration, shared audio admission (owner-approved 20h/month), direct
  socket client and provider reconciliation. Audio is not proxied by this text relay.
- Connector OAuth broker and provider verification. SSO-protected admin operations,
  account deletion/usage cascade and reconciliation/auditing integration.
- JP/US latency and first-visible-token measurement, Hyperdrive cost/latency measurement,
  real-provider trials, privacy updates and provider ZDR/limits/rotation. No 15/40ms,
  1-second-token, or 2-second end-to-end block claim follows from local unit tests.
