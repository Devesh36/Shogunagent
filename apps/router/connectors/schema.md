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

`auth.kind` is one of `none`, `bearer`, `header` (`header` name), `query` (`param` name) or `caller_bearer` (forward the calling agent's own key — used by the first-party connector so the state face scopes the read to that agent's principal). Endpoint fields override provider fields; `categories` and `health` are inherited when omitted.

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

`state`, `memory`, `approval` (ShogunAgent additions), `web_search`, `scrape`, `enrichment`, `communication`, `finance`, `social`, `ecommerce`, `gen_ai`, `seo`, `listings`, `files`, `data`.

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

Compiled entry fields: `tool_id, provider_id, provider_name, first_party, summary, description, exclusions, notes, categories, input_schema, output_schema, usage, quote_micro_usd, health, scope{class, level}, examples, docs_url, hash`.

## 7. Importing a Monid-style catalog

`src/passthrough/monid.ts` turns a Monid catalog JSON (`providers[]`, `endpoints[]` with `id`, `provider`, `summary`, `description`, `categories`, `inputSchema` (JSON Schema), `pricing{model: PER_CALL|PER_RESULT|PER_UNIT, priceUsd, …}`, `health{p50Ms, p95Ms, successRate}`, `verified`, `sideEffects`, `provisions`, `fixtures`) into one provider `monid` whose endpoints all execute through `POST /v1/run` on `api.monid.ai`. Prices are Monid's **+ 5%** (`toMicroUsd`). `sideEffects: true` → `external_send` (L3); `provisions: true` → `payment` (L3); otherwise `read`. JSON Schema inputs are validated by `src/jsonschema.ts` (object/properties/required, string/number/integer/boolean/array constraints, enum, const, anyOf). The fixture `connectors/monid/catalog.fixture.json` holds 12 endpoints from 11 upstream providers; the hosted router refreshes it from Monid's catalog API.

## 8. Differences from Monid's format, on purpose

- No `auth.inject` hook; auth is a declarative spec applied by the transport (invariant 7).
- `scope.class` is required; Monid has no permission tiers.
- `exclusions` is a separate field so it can be shown without being ranked.
- Categories add `memory`, `state`, `approval`.
- Ids are not locked in a separate file; the content hash of each entry is the stability signal, and wire-name freezing (invariant 11) applies to the three verbs, not to catalog entries.
