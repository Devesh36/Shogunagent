# @shogunagent/router

The ShogunAgent router — "OpenRouter for Agents" (README §7, §11): one base URL where an agent **discovers** a tool for a task, **inspects** its schema and price, and **runs** it with the user's permission tiers applied, metered usage settled and the result ready to become state. TypeScript on Hono, deployed as a Cloudflare Worker next to `apps/api` (decision: `docs/decisions/0001-router-engine.md`). E8 skeleton: the engine, the connector format, the first-party connector, the Monid passthrough and both faces are here; the wallet (E9), the conformant MCP transport (E4) and the `tool_runs` sink in the state layer are wired behind the interfaces this package exports.

## What it guarantees

- **Zero retention** (invariant 2). The only logger accepts a closed set of fields (route, status, latency, tool id, outcome, counts); the run record has no body field; `test/zero-retention.test.ts` drives sentinel strings through REST, MCP and the passthrough and asserts none reaches a log or a record.
- **No credential in the engine** (invariant 7). Provider keys are named by environment variable and injected in `src/transport.ts`, the only module that calls `fetch`. Hooks and the engine receive a request type with no credential field (`test/transport.test.ts`).
- **L1/L2/L3 on every run** (invariant 3). `src/scope.ts`: an operation not in the table is denied; sends and payments are L3 (approval URL, never executed directly); user-data writes are L2 (`input_required` once); reads are L1.
- **No self-preferencing** (invariant 13). First-party tools are scored by the same published formula, labeled `first_party`, with price inside `scoreBreakdown`. Task fit multiplies the quality terms, so a $0 first-party read cannot outrank a better-fitting paid tool on price alone.
- **Three faces, one behavior** (invariant 4). REST and MCP go through one dispatcher (`src/engine.ts`); the CLI face wraps REST (E13).
- **Every response is self-describing** (README §11). `usage`, `latency_ms`, `hints`, and on error `next_action`.

## Verbs

| Verb | REST | MCP tool | Auth | Price |
|---|---|---|---|---|
| discover | `POST /v1/router/discover` | `router.discover` | none | $0 |
| inspect | `POST /v1/router/inspect` | `router.inspect` | none | $0 |
| run | `POST /v1/router/run` | `router.run` | bearer for metered or first-party tools | the tool's quote; failures and empty results $0 |

Also: `GET /health`, `GET /v1/router/catalog` (the content-hashed manifest), `POST /mcp` (JSON-RPC: `initialize`, `ping`, `tools/list`, `tools/call`). Every response carries `Mcp-Method` and, for a tool call, `Mcp-Name`, which is what `apps/api` meters on without reading bodies (invariant 10).

### discover

```bash
curl -s -X POST http://localhost:8787/v1/router/discover \
  -H 'content-type: application/json' \
  -d '{"task":"what did the user commit to","limit":3}'
```

```json
{
  "result": {
    "candidates": [
      {
        "tool_id": "shogunagent.state.commitments.list",
        "provider_id": "shogunagent",
        "first_party": true,
        "summary": "Find what the user committed to: promises, deliverables and deadlines they made to others.",
        "categories": ["state", "memory"],
        "permission_level": "L1",
        "score": 0.787,
        "scoreBreakdown": { "task_fit": 0.8089, "price": 1, "health": 0.999, "latency": 0.8929, "personal": 0.9729 },
        "price": { "model": "per_call", "unit": "call", "cost_micro_usd": 0, "quote_micro_usd": 0 },
        "p50_ms": 40,
        "p95_ms": 120,
        "personal": { "value": 0.9729, "runs": 0, "successes": 0, "basis": "none", "level": null, "cluster": "commit user" }
      }
    ],
    "ranking": {
      "formula": "score = task_fit × (0.3825*price + 0.255*health + 0.2125*latency + 0.15*personal). …",
      "weights": { "price": 0.3825, "health": 0.255, "latency": 0.2125, "personal": 0.15 },
      "personal": { "formula": "personal = 0.6*success_rate + 0.25*price(cost_micro_usd / successes) + 0.15*hint_success_rate, …", "components": { "success_rate": 0.6, "cost_per_success": 0.25, "hint_success": 0.15 }, "min_runs": 5, "basis": "none", "cluster": "commit user" }
    },
    "catalog_hash": "…",
    "considered": 17
  },
  "usage": { "unit": "call", "quantity": 1, "cost_micro_usd": 0 },
  "latency_ms": 1,
  "hints": [],
  "next_action": null
}
```

Ranking (`src/discover.ts`): `task_fit` is BM25 over id, summary (×2), categories (×2), description and notes, normalised by the query's bound to [0, 1), with exclusion text left out of the index; `price = 1 / (1 + ln(1 + quote_micro_usd / 1000))` ($0 → 1, strictly decreasing); `health` = observed success rate (declared baseline until 20 runs); `latency = 1 / (1 + p95_ms / 1000)`; `personal` is described below. Ties break on lower quote, then `tool_id`. `budget_micro_usd` drops tools whose quote exceeds it; `categories` restricts; `limit` ≤ 50.

#### Personal factor (E20)

`discover` called with an agent key as the bearer is personalised from that principal's own run history (`src/personal.ts`); without a key, or with an invalid one, it is the same for everyone.

- **What is learned.** Every `ToolRunRecord` (body-less, `actor` is a hash of the key) also goes to `PersonalRanker`, which keeps per principal a row per tool × task cluster: counted runs, successes, runs that followed a hint and how many of those succeeded, and cost spent. Counted outcomes are `ok` (success), `empty`, `provider_error`, `transport_error`; permission stops, input errors, auth/payment stops, replays and anonymous runs are not evidence about the tool.
- **Task clusters** use the same BM25 tokenizer as task fit: the cluster of a task phrase is its 3 most distinctive stemmed terms (highest catalog IDF), sorted — `"what did the user commit to"` → `"commit user"`. No model call.
- **Value.** `personal = 0.6·success_rate + 0.25·price(cost_micro_usd / successes) + 0.15·hint_success_rate` over the rows of the task's cluster when it has ≥ 5 runs, otherwise over the tool's rows across clusters when those have ≥ 5 runs (`level: "task_cluster" | "tool"`). `price(·)` is the same price curve as the ranking; `hint_success_rate` falls back to `success_rate` when no hint was followed.
- **Weight.** `PERSONAL_WEIGHT = 0.15`. The E8 mix (0.45 price, 0.30 health, 0.25 latency) is scaled by 0.85, so the published weights are 0.3825 / 0.255 / 0.2125 / 0.15 and `score = task_fit × (…)` as before.
- **Neutral.** With fewer than 5 counted runs (`basis: "none"`), `personal` equals the candidate's own `0.45·price + 0.30·health + 0.25·latency`, so its score is exactly the unpersonalised score — the term has no effect until there is evidence.
- **Explained.** Every candidate carries `personal: { value, runs, successes, basis: "principal" | "none", level, cluster }`, and `ranking.personal` publishes the formula, components and minimum.
- **Isolated.** `PersonalStore` (`record(principal, …)`, `profile(principal)`) has no cross-principal read. `InMemoryPersonalStore` is the development stub; in production the store is the principal's own ShogunAgent state — `tool_runs` (V94) in that principal's file, reached through the state sidecar — never a hosted shared database. `test/personal.test.ts` shows that principal A's history never changes principal B's ranking.
- **No self-preferencing.** The factor is computed the same way for first- and third-party tools; `test/personal.test.ts` shows a first-party tool with a worse history ranked below a third-party tool with a better one at equal fit and price.

### inspect

```bash
curl -s -X POST http://localhost:8787/v1/router/inspect \
  -H 'content-type: application/json' \
  -d '{"tool_id":"monid.strale.x402.url-to-text"}'
```

Returns the catalog entry (schemas, categories, notes, examples), `price` (`quote_micro_usd`, `failures_cost: 0`, `empty_results_cost: 0`), `permission` (class, level and what the level means for the caller), live `health` with its source, a `run_example`, and `hints` — cheaper endpoints in the same category, cheapest first, whoever provides them (here `monid.contextdev.web.scrape.markdown` at $0.000945 against $0.024948).

### run

```bash
curl -s -X POST http://localhost:8787/v1/router/run \
  -H 'content-type: application/json' -H 'authorization: Bearer sa_…' \
  -d '{"tool_id":"monid.tinyfish.web-search","input":{"query":"shogunagent"},"idempotency_key":"7b1c…","task_hint":"search the web for a company"}'
```

| Outcome | `resultType` | status | `next_action` |
|---|---|---|---|
| L1 executed | `result` (+ `run_id`, `replayed`) | 200 | `null` |
| `dryRun: true` | `quote` | 200 | per level |
| L2, not yet confirmed | `input_required` (MRTR shape: `inputRequests`, retry with `inputResponses: { confirm: true }`) | 200 | `provide_input` |
| L3 | `approval_required` (`approval.url`, 15-minute expiry) | 200 | `approve_at_url` |
| schema mismatch | `error` `invalid_input` (+ `details`) | 400 | `fix_input` |
| unknown tool / not in table | `error` `tool_not_found` / `operation_denied` | 404 / 403 | `discover` |
| no key for a metered or first-party tool | `error` `authentication_required` | 401 | `authenticate` |
| wallet short | `error` `payment_required` + x402-shaped `payment` | 402 | `top_up` |
| provider failed / unreachable | `error` `upstream_error` ($0) | 502 | `try_alternative` / `retry_after` |
| provider not configured | `error` `provider_unavailable` | 503 | `try_alternative` |
| repeated `idempotency_key`, same input | the stored `result`, `replayed: true`, usage 0 | 200 | `null` |
| repeated key, different input | `error` `idempotency_conflict` | 409 | `fix_input` |

Each run emits one `ToolRunRecord { ts, actor, tool_id, task_hint, outcome, cost_micro_usd, latency_ms, hint_followed }` to the `RunLogSink` (no bodies; `actor` is a hash of the key).

### MCP

```bash
curl -s -X POST http://localhost:8787/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"router.discover","arguments":{"task":"read a web page"}}}'
```

`tools/list` returns the three tools with input schemas generated from `src/verbs.ts` and annotations (`readOnlyHint` true for discover/inspect, `openWorldHint` true for run). `tools/call` returns `content[0].text` and `structuredContent` with the same body the REST face returns; `isError` mirrors the HTTP status.

## Connectors

Format: `connectors/schema.md`. Registry: `connectors/index.ts`. Build the content-hashed catalog with `pnpm --filter @shogunagent/router build:catalog` (writes `dist/catalog.json` and `dist/catalog.<hash12>.json`; the compiler rejects duplicate ids, unknown categories, missing scope class or health, bad usage numbers and fixtures that fail their schema).

- `shogunagent` (first-party, `caller_bearer` auth to the state REST face; base URL from `SHOGUNAGENT_API_BASE_URL`): `state.commitments.list`, `state.open_loops.list`, `state.people.get`, `memory.recall` — reads, L1, $0 — and `actions.execute` — `external_send`, L3, $0.002.
- `monid` (passthrough, `src/passthrough/monid.ts`): endpoints imported from a Monid-style catalog JSON, executed through Monid's `POST /v1/run` with the router's `MONID_API_KEY` injected at the transport, priced at Monid's price + 5%. The fixture catalog holds 12 endpoints from 11 upstream providers (search, scrape, enrichment, SEO, image generation, and one L3 `provision-numbers`).

## Listing pipeline (E21)

`pnpm --filter @shogunagent/router listing:check <provider-dir> | --all` validates a provider directory for listing: `provider.json` (legal name, contact, opaque payout reference, per-endpoint price declarations, data handling) and the catalog compile, offline fixture replay through the connector hooks and through `run` with its permission tier, the scope classification (sends and payments are L3; a write is never `read`), the description lint (tasks named, exclusions, cost class, ≤ 400 characters, no superlatives) and price sanity (declared = usage model = what the fixtures settle). It prints a JSON report and exits 1 on any error; `.github/workflows/router-listing.yml` runs it on PRs touching `connectors/**` and posts the markdown report as the job summary. Details: `connectors/schema.md` §9–§10.

## Pluggable seams (what apps/api and the state layer wire later)

| Interface | Default | Production home |
|---|---|---|
| `MeteringClient` (`identify`, `reserve`, `settle`) | `InMemoryMeteringClient` — `sa_` keys, $1 default balance, 401 for anonymous metered runs, x402-shaped 402 | apps/api wallet (E9) |
| `RunLogSink` | `InMemoryRunLogSink` | `tool_runs` (V94) in the principal's store |
| `PersonalStore` (behind `PersonalRanker`, E20) | `InMemoryPersonalStore` | the principal's `tool_runs` (V94) via the sidecar — never a hosted shared DB |
| `IdempotencyStore` | `InMemoryIdempotencyStore` (24 h TTL, bounded) | the principal's store — never a hosted DB (invariant 2) |
| `HealthStore` | `InMemoryHealthStore` (observed after 20 runs) | shared status store (`/.well-known/status.json`) |
| `Transport` | `createFetchTransport({ env })` | same; `createReplayTransport` for tests |
| `Logger` | `consoleLogger` (closed field set) | Workers Logs, invocation logs off |

## Develop

```bash
pnpm install
pnpm --filter @shogunagent/router typecheck
pnpm --filter @shogunagent/router test
pnpm --filter @shogunagent/router build:catalog
pnpm --filter @shogunagent/router dev          # wrangler dev on :8787 (no route bound in wrangler.jsonc; workers_dev: false)
```

Secrets are set with `wrangler secret put` and documented in `src/env.ts`; non-secrets are `vars` in `wrangler.jsonc`. Nothing in this package is live until E9 wires the wallet and a route is bound.
