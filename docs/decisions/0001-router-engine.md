# ADR 0001 — Router engine: TypeScript (Hono) on Cloudflare Workers, Monid-compatible connector format

- Status: accepted (E8-T1), 2026-10-25
- Deciders: owner; drafted for review by the E8 implementer
- Scope: the "OpenRouter for Agents" router (`discover` / `inspect` / `run`), `apps/router`
- Related: README.md §7 ("Build an OpenRouter for Agents"), §11; `docs/implementation-plan.md` §2.1, E8, §7.4, §9; research note *Monid and the "Tool Marketplace / Aggregator for AI Agents" category* (KQ1, KQ5)

## Context

The implementation plan left the router engine's language open (§2.1, §9): fork Monid's MIT connector engine (Deno/TypeScript) into `apps/router`, or re-implement in Rust next to the core. The plan names three criteria — connector-definition compatibility, whether it runs on Cloudflare Workers, and whether zero retention can be guaranteed — and this repository adds a fourth that matters for a cold-start marketplace: how much friction a connector contribution carries.

The router is the *front door*, not the product. README §7 fixes its economics (near-zero margin, provider price + 5%, failures and empty results free) and its role (its KPI is the share of router agents that start using the state layer). Everything body-bearing stays in the Rust core (invariants 1 and 2); the router is a zero-retention proxy that validates input, executes a connector, settles usage and emits a body-less run record.

## Decision

The router engine is **TypeScript on Hono, deployed as a Cloudflare Worker, sharing `apps/api`'s runtime (Hono 4, `nodejs_compat`, Vitest, Wrangler)**, in a new package `apps/router` (`@shogunagent/router`).

Connector definitions use a **Monid-compatible declarative format**: `connectors/<provider>/provider.ts` plus `endpoints/<name>.ts`, with provider metadata, endpoint id, an agent-readable description, a closed category list, input/output schemas in zod (exported as JSON Schema with `z.toJSONSchema`), a usage model (flat per call, per result, per unit), a declared health baseline and recorded fixtures for offline replay tests. The format is documented in `apps/router/connectors/schema.md`; a compile step validates every definition and emits a content-hashed catalog JSON.

Monid's hosted catalog is imported through a passthrough provider (`src/passthrough/monid.ts`) rather than by vendoring their engine. We do **not** fork Monid's repository: we adopt its *definition shape* so their endpoint definitions port with a mechanical rewrite, and we keep our own engine small enough to audit for retention.

Provider credentials are injected at the transport layer from an environment map; the engine, the scope check and connector hooks never receive them (invariant 7, E8-T9).

## Trade-off against a Rust re-implementation

| Criterion | TypeScript (Hono) on Workers — chosen | Rust re-implementation |
|---|---|---|
| Zero-retention guarantee (invariant 2) | Guaranteed by *structure*, not by language: the only logger accepts a closed set of typed fields (route, status, latency, tool id, outcome, counts) and a test asserts no request or response body ever reaches it. Same pattern `apps/api/src/logging.ts` already uses in production. Streams are relayed, not buffered. | Equally achievable. Rust's type system does not help here either; the guarantee is about what is written to logs and stores, which is a design discipline in both languages. No advantage. |
| Workers deployability | Native. `apps/api` already runs there with the same framework, build and test runtime; the router rides the same deploy, the same Workers Logs retention settings and the same `Mcp-Method`/`Mcp-Name` metering headers. One runtime to audit. | Possible via `wasm32` targets, but `reqwest`-style egress, the `nodejs_compat` surface and the Postgres driver do not carry over; we would be maintaining a second runtime story for a near-zero-margin component. |
| Connector-definition compatibility | Monid's definitions are TypeScript with zod input schemas and four hooks (`input.toRequest`, `usage.consolidate`, `output.fromResponse`, `auth.inject`). Keeping the engine in TypeScript lets a Monid connector port with file moves and small renames, and lets `z.toJSONSchema` produce the same JSON Schema an agent sees in `inspect`. | Every connector would need a JSON/TOML re-encoding or a scripting layer; hooks (`usage.consolidate` is arbitrary code over the raw vendor response) would need an embedded interpreter or a rewrite per connector. This is the criterion that decides it. |
| Contribution friction | A connector is one directory of TypeScript with fixtures, the shape the wider ecosystem already writes (Monid, Smithery `typescript` runtime, Apps SDK). A coding agent can draft one from `schema.md`. | Rust connectors raise the bar to contributors who are not Rust developers and slow the cold start README §7 says we must borrow. |
| Fit with invariant 1 (Rust core owns the data) | Preserved: the router holds no store. The run record goes to a pluggable sink that the state layer wires to `tool_runs` (V94) through the core; the idempotency cache's production home is the principal's store, not a hosted DB. | Same. |
| Egress rule (invariant 9) | Invariant 9 is a *crate* rule for the Rust workspace (`scripts/check-http-egress.py` inspects `cargo metadata`). The TypeScript router has exactly one egress module (`src/transport.ts`); nothing else calls `fetch`. | Would have to live inside `shogun-core` to satisfy the rule, coupling the marketplace release cadence to the core's. |
| Performance | The hot path is JSON validation plus one upstream HTTP call; Worker overhead is well under the provider latency we proxy. Not a factor at this stage. | Faster validation, irrelevant next to upstream latency. |

What we give up: the Rust permission types (`Level`, `OpClass`, `SendAction`) are not shared at the type level. The router carries a **port** of the scope rule in `src/scope.ts` ("an operation not in the table is denied; any external send or payment is L3; writes touching user data are L2; reads are L1"), with its own invariant test. Two copies of one rule is a cost; it is bounded because the rule is four lines and both copies are tested against the same statement in CLAUDE.md invariant 3.

## Consequences

- `apps/router` is a sibling of `apps/api`, not a dependency of it. `apps/api` later wires the real wallet through the router's `MeteringClient` interface and the real run-log sink through `RunLogSink`; until then both have in-memory stubs.
- The three verbs are exposed over REST (`POST /v1/router/{discover,inspect,run}`) and as MCP tools (`router.discover`, `router.inspect`, `router.run`) from one handler set, so invariant 4 ("three faces, one behavior") holds for the router from the first commit. The CLI face is E4/E13 work and wraps the same REST calls.
- First-party connectors (`shogunagent`) are ranked by the same published score as third-party ones and labeled `first_party` (invariant 13); price sits inside `scoreBreakdown`.
- Monid's catalog is consumed as data (a catalog JSON), so a change in their engine does not break us; a change in their catalog shape is absorbed in one importer file.

## What would trigger a revisit

1. **A retention incident or audit finding** that the TypeScript engine wrote a body anywhere (log, metric, error report, cache) despite the structural logger. The fix is still structural, but if a second incident shows the discipline cannot be held in this codebase, re-evaluate moving the proxy into `shogun-core` where the `Secret`/redaction tooling already exists.
2. **Workers stops being the hosting target** for `apps/api` (for example, a move to a VM to host per-principal SQLite files next to the API). Sharing a runtime was a major reason to choose TypeScript; without it, a Rust router inside the core binary becomes the simpler deployment.
3. **Connector hooks become the dominant cost.** If more than a handful of connectors need arbitrary code in `usage.consolidate` or `output.fromResponse` that cannot be expressed declaratively, and Monid compatibility stops mattering (e.g. they change format or we stop importing their catalog), the compatibility argument weakens and the single-language argument strengthens.
4. **The router needs direct access to the principal's store** (personalised `discover` in Phase 2 reading `tool_runs` locally for the OSS build). A local-only router would then be better served by the Rust CLI/MCP binary; the hosted router can keep the TypeScript engine and read ranking signals through the state API instead. Decide per deployment at that point, not now.

## Rejected alternatives

- **Fork Monid's engine wholesale.** Their engine carries Deno task tooling, an id lock file, Apify scaffolding and a `resources` model we do not need yet; forking makes us track their repo instead of their *format*. We take the format and the four-hook contract, not the code.
- **Rust now, TypeScript later.** Loses the cold start we are trying to borrow and couples the router to the core's release cadence for a component whose revenue role is deliberately small.
- **Deno runtime** (what Monid uses). `apps/api` is on Workers; a second JavaScript runtime buys nothing.
