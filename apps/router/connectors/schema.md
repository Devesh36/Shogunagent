# Connector definition format

How a tool gets into the ShogunAgent router (E8-T2). The format is Monid-compatible in spirit — a provider file, endpoint files, zod input schemas, a usage model, recorded fixtures — so an existing Monid connector ports with file moves and small renames, and a coding agent can draft one from this page. Decision record: `docs/decisions/0001-router-engine.md`.

Types: `connectors/types.ts`. Helpers: `connectors/define.ts`. Compiler: `src/catalog.ts`. Build: `pnpm --filter @shogunagent/router build:catalog`.

## 1. Layout

```
connectors/
  categories.ts                 closed category list (§3)
  types.ts · define.ts          the format and the authoring helpers
  index.ts                      the registry: every provider the catalog compiles (§6)
  schema.md                     this page
  <provider>/
    provider.ts                 ProviderDefinition (§2.1)
    provider.json               listing manifest: legal name, contact, payout reference, prices, data handling (§9)
    endpoints/<name>.ts         one EndpointDefinition per file (§2.2)
  monid/catalog.fixture.json    a Monid-style catalog imported by src/passthrough/monid.ts (§7)
```

The catalog `tool_id` of an endpoint is `<provider.id>.<endpoint.id>`, e.g. `shogunagent.state.commitments.list`, `monid.tinyfish.web-search`. Ids match `^[a-z0-9][a-z0-9_.-]*$` and are unique.

## 2. Definitions

### 2.1 Provider (`provider.ts`)

```ts
import { defineProvider } from "../define.js";

export const exampleProvider = defineProvider({
  id: "example",
  name: "Example Data",
  first_party: false,                 // true only for ShogunAgent's own connector; shown as a label, never a boost
  meta: { description: "…", docsUrl: "https://…", homepage: "https://…" },
  request: { baseUrl: "https://api.example.com", baseUrlEnv: "EXAMPLE_BASE_URL" }, // env var may override the base URL
  auth: { kind: "bearer", credential_env: "EXAMPLE_API_KEY" },                   // names a variable, never a value (§4)
  categories: ["data"],               // default for endpoints that omit theirs
  health: { p50_ms: 300, p95_ms: 900, success_rate: 0.99 }, // default baseline
  endpoints: [search, lookup],
});
```

`auth.kind` is one of `none`, `bearer`, `header` (`header` name), `query` (`param` name), `caller_bearer` (forward the calling agent's own key — used by the first-party connector so the state face scopes the read to that agent's principal) or `caller_connector` (`slot`, `credential_env`: act as the calling principal's own account at the vendor; the transport fetches that principal's token for `slot` from apps/api at run time, `docs/api/connector-tokens.md`, and reads `credential_env` only in development behind `ROUTER_DEV_SHARED_CREDENTIALS=1`). Endpoint fields override provider fields; `categories` and `health` are inherited when omitted.

### 2.2 Endpoint (`endpoints/<name>.ts`)

```ts
import { defineEndpoint, z, zodInput } from "../../define.js";

export const search = defineEndpoint({
  id: "search",
  meta: {
    summary: "Search public pages for a query and return links with snippets.",   // one line, the task
    description: "Use for: …  Free/metered: …",   // full text, written for ranking (§5); indexed by discover
    exclusions: "Not for reading a page's full text (use fetch).",                 // shown by inspect, NOT indexed
    notes: ["Rate limit 10 rps per key.", "Results are cached upstream for 60 s."], // one operational fact each
    categories: ["web_search"],
    examples: [{ input: { query: "state layer for agents" }, output: { items: [{ url: "https://…" }] } }],
    docsUrl: "https://…",
  },
  request: { method: "POST", path: "/v1/search", timeout_ms: 20_000 },
  input: {
    schema: zodInput(z.object({ query: z.string().min(1), limit: z.number().int().min(1).max(20).optional() }).strict()),
    toRequest: ({ input }) => ({ method: "POST", path: "/v1/search", body: input }),  // default: GET → query string, else JSON body
  },
  output: {
    fromResponse: ({ response }) => response.body,            // default: identity
    isFailure: (response) => response.status >= 400,           // default; override for vendors that fail inside a 2xx
    isEmpty: (output) => …,                                     // default: null / [] / {} / { items: [], cursor: null }
  },
  usage: { model: "per_call", cost_micro_usd: 1300 },          // §2.3
  scope: { class: "read" },                                    // §2.4 — required; no class, no catalog entry
  health: { p50_ms: 900, p95_ms: 2600, success_rate: 0.991 },
  fixtures: [                                                  // §2.5
    { scenario: "basic", input: { query: "x" }, response: { status: 200, body: { items: [{ url: "https://…" }] } }, expect: { usage: { quantity: 1, cost_micro_usd: 1300 } } },
    { scenario: "empty", input: { query: "zzz" }, response: { status: 200, body: { items: [] } }, expect: { usage: { quantity: 0, cost_micro_usd: 0 } } },
    { scenario: "vendor-500", input: { query: "x" }, response: { status: 500, body: null }, expect: { error: true } },
  ],
});
```

Only constraints `z.toJSONSchema` can express are enforced (`.refine` is dropped, as in Monid); put cross-field rules in `notes`. The JSON Schema `inspect` shows is generated from the zod schema — never hand-written (invariant 12).

Hooks are plain functions of `{ input }`, `{ input, request }` or `{ input, response }`. **There is no auth hook**: credentials are injected by `src/transport.ts` from the environment map and no hook ever receives one (test/transport.test.ts asserts it).

### 2.3 Usage models

| model | fields | quantity at settlement | quote shown by discover/inspect |
|---|---|---|---|
| `per_call` | `cost_micro_usd` | 1 | `cost_micro_usd` |
| `per_result` | `cost_micro_usd_per_result`, `base_micro_usd?`, `typical_results`, `results?(output)` | result count (default: length of `items` or the only array field) | `base + per_result × typical_results` |
| `per_unit` | `unit`, `cost_micro_usd_per_unit`, `typical_units`, `units({ input, response })` | vendor-native units read from the raw response | `per_unit × typical_units` |

Costs are micro-USD integers. Whatever the model, **failures settle at 0** (the engine never reaches settlement) and **empty results settle at 0** (`output.isEmpty`). The quote is what `run` reserves and `dryRun` returns.

### 2.4 Scope class → permission level

Every endpoint declares what it does; the level follows from the class in exactly one place (`src/scope.ts`, a port of `shogun_mcp::scope`):

| class | meaning | level |
|---|---|---|
| `read` | reads the provider's data | L1, runs immediately |
| `user_data_write` | writes the user's own data (state rows, notes, drafts) | L2, `input_required` once |
| `service_state_change` | reversible change on the provider, delivers nothing to anyone | L2 |
| `external_send` | sends, posts, creates an event, provisions something others see | L3, approval URL, never auto-run |
| `payment` | spends money beyond the call price | L3 |

An endpoint without a class does not compile, and a `tool_id` absent from the catalog is denied by `run` — "an operation not in the table is denied; any external send or payment is L3" (CLAUDE.md invariant 3). `test/scope.test.ts` checks the whole compiled catalog.

### 2.5 Fixtures

A fixture is a recorded exchange: `scenario`, `input`, the vendor `response` (`status` + `body`; headers are never recorded) and optional `expect` (`output`, `usage`, `error: true`). The compiler validates every fixture's input against the endpoint's schema; `pnpm test` replays every fixture through the real engine with a replay transport and checks the expectation. Prefix synthetic scenarios with `synthetic-` while no real recording exists.

## 3. Categories

Closed list in `connectors/categories.ts`; a definition naming anything else does not compile.

`state`, `memory`, `approval` (ShogunAgent additions), `web_search`, `scrape`, `enrichment`, `communication`, `finance`, `social`, `ecommerce`, `gen_ai`, `seo`, `listings`, `files`, `calendar`, `issue_tracking`, `data`.

Categories are ranked (twice-weighted) by `discover`, filterable (`categories` argument) and used by `hints` to find cheaper alternatives. Monid's category names map onto this list in `src/passthrough/monid.ts` (`MONID_CATEGORY_MAP`).

## 4. Credentials

A definition names an **environment variable** (`credential_env`), never a value. The transport resolves it when it builds the outgoing request; the engine, the scope check, metering, the run log and every hook see a `RunRequest` that has no credential field at all. Missing variable → `503 provider_unavailable` with `next_action: "try_alternative"`. The calling agent's own bearer travels as a `Secret` (serialises as `[redacted]`) and is forwarded only to `caller_bearer` providers.

## 5. Writing descriptions for ranking

`discover` ranks lexically (BM25 over id, summary ×2, categories ×2, description, notes). So:

- **Name the tasks** an agent is trying to do, in the agent's words ("what did the user commit to", "read the text content of a web page URL").
- **Put exclusions in `exclusions`**, not in `description`: text that names the task you refuse would otherwise attract you to it. (Imported descriptions that contain "Not for …" sentences have those sentences stripped before indexing.)
- **State the cost class** ("free read", "metered per result").
- One operational fact per `notes` entry.

Description changes must not lower the selection-eval rate (E11, CLAUDE.md code rules).

## 6. Compile and publish

`connectors/index.ts` lists every provider. `src/catalog.ts` compiles them into entries with a SHA-256 content hash each and a catalog hash over all of them; `scripts/build-catalog.ts` writes `dist/catalog.json` and `dist/catalog.<hash12>.json`. Compile errors are listed all at once (duplicate ids, unknown categories, missing class or health, bad usage numbers, fixtures failing their schema). `server-card.json`, `pricing.json` and the human docs derive from this manifest (invariant 12).

Compiled entry fields: `tool_id, provider_id, provider_name, first_party, summary, description, exclusions, notes, categories, input_schema, output_schema, usage, quote_micro_usd, health, scope{class, level}, examples, docs_url, data_handling, hash`. `data_handling` is the public part of `provider.json` (§9); legal name, contact and payout reference are never in the catalog.

## 7. Importing a Monid-style catalog

`src/passthrough/monid.ts` turns a Monid catalog JSON (`providers[]`, `endpoints[]` with `id`, `provider`, `summary`, `description`, `categories`, `inputSchema` (JSON Schema), `pricing{model: PER_CALL|PER_RESULT|PER_UNIT, priceUsd, …}`, `health{p50Ms, p95Ms, successRate}`, `verified`, `sideEffects`, `provisions`, `fixtures`) into one provider `monid` whose endpoints all execute through `POST /v1/run` on `api.monid.ai`. Prices are Monid's **+ 5%** (`toMicroUsd`). `sideEffects: true` → `external_send` (L3); `provisions: true` → `payment` (L3); otherwise `read`. JSON Schema inputs are validated by `src/jsonschema.ts` (object/properties/required, string/number/integer/boolean/array constraints, enum, const, anyOf). The fixture `connectors/monid/catalog.fixture.json` holds 12 endpoints from 11 upstream providers; the hosted router refreshes it from Monid's catalog API.

## 8. Differences from Monid's format, on purpose

- No `auth.inject` hook; auth is a declarative spec applied by the transport (invariant 7).
- `scope.class` is required; Monid has no permission tiers.
- `exclusions` is a separate field so it can be shown without being ranked.
- Categories add `memory`, `state`, `approval`.
- A `provider.json` listing manifest (§9) and the `listing:check` pipeline (§10) replace Monid's email-and-hand-review step.
- Ids are not locked in a separate file; the content hash of each entry is the stability signal, and wire-name freezing (invariant 11) applies to the three verbs, not to catalog entries.

## 9. `provider.json` — the listing manifest (E21)

Every listed provider directory carries a `provider.json` next to `provider.ts`, imported as the provider's `manifest` (`import manifest from "./provider.json" with { type: "json" }`). The catalog compiler validates it (`src/listing/manifest.ts`); a provider without one does not pass `listing:check`. Passthrough catalogs (`connectors/monid`) carry none.

```json
{
  "format": "shogunagent-provider/1",
  "provider_id": "example",
  "legal_name": "Example Data Ltd",
  "contact": { "email": "listings@example.com", "url": "https://example.com/support" },
  "payout_account_ref": "acct_1Nq2Example",
  "pricing": [
    { "endpoint": "search", "model": "per_result", "cost_micro_usd_per_result": 100, "base_micro_usd": 0 },
    { "endpoint": "lookup", "model": "per_call", "cost_micro_usd": 1000 },
    { "endpoint": "tts", "model": "per_unit", "unit": "1k_chars", "cost_micro_usd_per_unit": 50 }
  ],
  "data_handling": {
    "retention": "none",
    "retention_days": 0,
    "zero_retention": true,
    "regions": ["eu"],
    "statement": "Requests are processed in memory and not stored."
  }
}
```

| field | rule |
|---|---|
| `provider_id` | equals the provider's `id` |
| `legal_name` | the contracting legal entity, 2–200 characters |
| `contact` | `email` and/or `url`; at least one |
| `payout_account_ref` | an **opaque** id in the payout system (e.g. a Stripe Connect account id), `[A-Za-z0-9_:.-]{3,128}`. Never bank data: an IBAN, a run of 8+ digits or a bank field name (`iban`, `routing`, `swift`, `bic`, `sort code`, `account number`) is rejected |
| `pricing` | exactly one declaration per endpoint, equal to the endpoint's `usage` model (model, unit and costs in micro-USD; `typical_*` stay in the endpoint) |
| `data_handling.retention` | `none` (not stored), `transient` (memory or cache for the call only), `days` (stored up to `retention_days`), `user_account` (the call creates content the user owns in their account on the service — a draft, an issue — kept until the user deletes it) |
| `data_handling.retention_days` | > 0 only with `days`, else 0 |
| `data_handling.zero_retention` | `true` only with `none` or `transient` |
| `data_handling.regions` | where request data is processed: ISO 3166 codes or `eu` / `us` / `global` (`global`: not region-pinned) |
| `data_handling.statement` | one plain-English paragraph (20–600 characters): what is stored, where, for how long, who can see it |

The catalog publishes `data_handling` per entry; legal name, contact and payout reference stay out of every agent-readable surface.

## 10. Listing pipeline (`listing:check`)

```bash
pnpm --filter @shogunagent/router listing:check connectors/<provider>     # one provider directory
pnpm --filter @shogunagent/router listing:check --all --out report.json   # every directory under connectors/
```

The command prints one JSON document (`{ "format": "shogunagent-listing-reports/1", "ok", "reports": [...] }`, one `shogunagent-listing-report/1` per directory with `checks`, `problems` and an `endpoints` summary) and exits 1 when any report has an error. Under GitHub Actions it appends a markdown rendering to the job summary; `.github/workflows/router-listing.yml` runs it on every PR that touches `apps/router/connectors/**`. The checks (`src/listing/check.ts`):

| check | errors when |
|---|---|
| `schema` | `provider.json` is missing or invalid (§9), or the provider does not compile (§6) |
| `fixtures` | an endpoint has no fixture or no successful one; a fixture's request hook throws; a fixture marked `error` is treated as a success (or the reverse); the mapped output or the settled usage differs from `expect`; replayed through the real `run` engine, an L1 run does not return the result, an L2 run does not ask `input_required` once before reaching the transport, or an L3 run reaches the transport instead of returning an approval URL |
| `scope` | an endpoint has no class; its id names a send (`send`, `post`, `publish`, `invite`, `notify`, `reply`, …) but it is not `external_send`; it names a payment or provisioning but is not L3; it names a write (`create`, `update`, `delete`, …) or uses `DELETE`/`PUT`/`PATCH` but is declared `read` |
| `descriptions` | `description` > 400 characters or `summary` > 160; no `Use for: …` (the tasks); no `exclusions`; no cost class (`free`, `$…`, `metered`, `L1/L2/L3`); a marketing superlative (`best`, `leading`, `revolutionary`, `seamless`, `powerful`, `AI-powered`, … — `MARKETING_SUPERLATIVES`) |
| `price` | the `provider.json` declaration differs from the usage model; a replayed fixture settles a cost the declared model does not produce (empty results must settle 0) |

Warnings (shown, not blocking): no failure fixture; an exclusion sentence inside `description`; an L2/L3 endpoint whose description does not say its level; a priced endpoint with no fixture that settles a non-zero cost; a quote over 10× or under 1/10 of the observed fixture cost; a typical run over $1.

To list a provider: add `connectors/<provider>/` (§1–§2, §9), register it in `connectors/index.ts`, run `listing:check`, `pnpm --filter @shogunagent/router test` and the selection eval (§5), and open a PR. The workflow's report is the review's starting point; a passing report is required, not sufficient.
