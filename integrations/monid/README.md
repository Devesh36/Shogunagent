# Listing ShogunAgent on Monid (E31)

**Status: prepared, not submitted.** Nothing in this directory has been sent to Monid, no pull request has been opened against `github.com/monid-ai/monid`, and nobody at Monid has been contacted. Submitting is an owner decision, gated on the open questions below.

Plan: `docs/implementation-plan.md` E31 ("per-call listing on Monid, if a memory category exists"). Background: research note *monid_and_agent_tool_marketplaces.md* (Key Question 1 "How to get listed", Key Question 5 "Monid connector").

## What is here

```
integrations/monid/
  README.md                       this page
  CHECKLIST.md                    what must be true before submitting, item by item
  ids.lock.fragment.json          entries for Monid's connectors/ids.lock.json
  connectors/shogunagent/
    provider.ts                   the provider: metadata, base URL, caller-provided bearer auth
    endpoints/state-commitments-list/   endpoint.ts · schema/inputs.ts · endpoint.test.ts · fixtures/*.json
    endpoints/state-open-loops-list/    (same)
    endpoints/memory-recall/            (same)
```

Three read endpoints, the ones that answer what other tools in Monid's catalog cannot — the user's own state:

| Monid id | ShogunAgent wire name | Request | Price |
|---|---|---|---|
| `shogunagent/state-commitments-list` | `state.commitments.list` | `GET /v1/state/commitments` | per call, $0 |
| `shogunagent/state-open-loops-list` | `state.open_loops.list` | `GET /v1/state/open_loops` | per call, $0 |
| `shogunagent/memory-recall` | `memory.recall` | `POST /v1/memory/recall` | per call, $0 |

- **Format.** Monid's connector format as its DEVELOPMENT.md describes it: `connectors/<provider>/provider.ts`, `endpoints/<name>/endpoint.ts` with the four hook contracts (`input.toRequest`, `usage.consolidate`, `output.fromResponse`, `auth.inject`), zod input schemas in `schema/inputs.ts` (only `z.toJSONSchema`-expressible constraints), recorded fixtures with the `synthetic-` prefix until real recordings exist (headers are never recorded), and an offline `endpoint.test.ts`. The import path (`../../engine/mod.ts`) and helper names (`defineProvider`, `defineEndpoint`, `utils.money.fromDollars`, `utils.query.fromObject`, `replayFixtures`) are our reading of that document; they are checked against the reference connector `connectors/exa/` as the first step of submission (CHECKLIST item 1). These files are not compiled or tested in this repository.
- **Descriptions.** Summaries, descriptions and input schemas are the router's first-party definitions (`apps/router/connectors/shogunagent/endpoints/`), with exclusions moved into `notes` (Monid has no `exclusions` field). They are the texts the selection eval measures (E11); any wording change here must not lower the selection rate.
- **Pricing.** Per call, $0. Reads of the user's own state are free on every ShogunAgent surface (CLAUDE.md invariant 10), and a listing elsewhere does not change that. Revenue comes from ingest and model operations on our own surfaces; a Monid listing is distribution. If Monid requires a non-zero price or applies a platform fee to $0 endpoints, that is a reason not to list (CHECKLIST item 5).
- **Auth: the caller's own key.** Every call must carry the calling user's ShogunAgent agent key (`sa_live_…`), so the read is scoped to that user's principal (one principal = one store, invariant 6). `auth.inject` puts it in `Authorization: Bearer`; it comes from the run's auth params, **never from `input`**, because Monid stores and returns run inputs and outputs. A vendor-level key held by Monid would read one principal — Monid's own — and is not offered.

## Submission steps (when the owner decides to submit)

1. Clear every item in `CHECKLIST.md`, in particular the open questions below. Record the answers in this README.
2. Fork `monid-ai/monid`; study `connectors/exa/` and DEVELOPMENT.md; copy `connectors/shogunagent/` into the fork and align import paths and helper names with the reference connector.
3. If Monid accepts a `memory` (or `state`) category, add it to `connectors/categories.ts` in the same PR; otherwise use the closest existing category agreed with Monid.
4. Merge `ids.lock.fragment.json` into `connectors/ids.lock.json`; run `deno task ids:check`.
5. Record real fixtures against a test principal with `deno task record <endpoint-id> <scenario>` (headers are never captured), keep one `synthetic-` failure fixture per endpoint, then run `deno task check && deno task lint && deno task test` offline until green; `deno task test:live` with a test key if Monid's CI runs it.
6. Open the PR. Email support@monid.ai with the PR link, the API docs link (`https://shogunai.si/docs/router/shogunagent`) and the company name, as Monid's README asks. Monid reviews every connector by hand; no review time is published.
7. After listing: watch the run counts Monid attributes to `shogunagent/*`, and the share of those callers that go on to ingest (the router KPI in README §7).

## Open questions (blocking)

1. **Per-end-user credentials.** Monid's connector model injects *Monid's* vendor credential in the transport and meters it per call; it assumes a vendor-level key, not per-end-user OAuth. ShogunAgent's reads are per principal and must use the caller's own key. Does Monid support a caller-provided credential passed per run (and kept out of stored run records), or a per-user OAuth integration on their side (`auth.credentials` beyond `{ apiKey }`, or their "OAuth: let your users sign in with Monid" integration mode extended to provider accounts)? If neither exists, the listing cannot work as designed, and we do not list with a shared key.
2. **No memory category.** Monid's closed category list (search, social, web scraping, lead gen, e-commerce, gen AI, SEO & GEO, listings, …) has no memory or state category, and its catalog lists no memory, context or state tool. The plan's condition for E31 is "if a memory category exists". We ask Monid to add `memory`; if they will not, decide whether a listing under a generic category is worth it.
3. **Retention of run bodies.** Monid's runs API stores and returns each run's input and output, and as of August 2026 no terms or privacy policy were published. A commitment list or a recall excerpt is the user's private state; CLAUDE.md invariant 2 says bodies never persist outside the principal's store. We need Monid to exclude these endpoints' inputs and outputs from run storage (zero retention for the provider), in writing, before listing.
4. **Price floor and fees.** Monid raises x402 runs to a $0.01 floor and secondary press reports a 10% platform fee while its own pages say 0% markup. Confirm that a $0 per-call endpoint is allowed, stays $0 for the caller, and is not floored on the x402 path (or is excluded from x402).
5. **Ranking neutrality.** Monid's own Simple FS sits in its rankings, and an independent review found its ranking ignored price. Ask whether listing order is published; a free state read should not be buried, but we also make no request for preferential placement.

## Checklist

See `CHECKLIST.md`.
