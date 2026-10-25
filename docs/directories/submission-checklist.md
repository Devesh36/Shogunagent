# Directory submissions: what each one needs vs. what exists (E42)

Status as of 2026-10-25, at commit `515e408` plus the E41/E42 documents. **Nothing has been
submitted.** Each submission happens only after the owner confirms it. This file is the diff; the
per-directory field mappings stay in `docs/directories/anthropic.md` and
`docs/directories/openai.md`, the OpenAI commerce question in
`docs/directories/openai-credit-rule.md`, the Monid listing in `integrations/monid/`.

Legend: **BLOCKER** = the submission cannot pass review or cannot be made until this is done;
**gap** = should be done before submitting but does not by itself fail review; **ok** = exists.

## 1. Cross-cutting blockers (every hosted listing)

| # | Item | Needed | Exists now | Status |
|---|---|---|---|---|
| X1 | Hosted `/mcp` answers | a live Streamable HTTP endpoint at the URL every listing names | `apps/api` serves `/mcp` with `503 upstream_unavailable` until E34 wires the core behind it; the Worker → sidecar network path is still open (`docs/deploy/production.md` §1) | **BLOCKER** |
| X2 | One public hostname | the URL in listings is the one the Worker serves | listings, `server.json`, `plugin.json`, `smithery.yaml`, `identity.ts` say `api.shogunagent.dev`; `apps/api/wrangler.jsonc` routes only `api.shogunaios.com` (`docs/deploy/production.md` §1.1) | **BLOCKER** (owner decision) |
| X3 | Production deploy with OAuth | `AGENT_API_ENABLED`, `AGENT_OAUTH_SECRET`, `agent-oauth.sql` applied, consent flow verified by hand from each client | runbook ready (`docs/deploy/production.md`), nothing deployed | **BLOCKER** |
| X4 | Annotations on every tool | MCP `readOnlyHint` / `destructiveHint` / `openWorldHint` as explicit booleans on every tool in the core's `tools/list`, identical to the server card | server card (`apps/api/src/wellknown/tool-table.ts`) renders all of them; the core (`docs/api/core-tools.md`, wire-frozen) lists only the true hints and **none** for `memory.append_note` / `memory.ingest`. E34 fixes this with new names and a deprecation entry (invariant 11) | **BLOCKER** |
| X5 | Privacy policy | published, out of DRAFT | `docs/legal/privacy-policy.md` served at `/privacy` with a DRAFT banner; `[TBD]` operator address, company number, privacy@ and security@ addresses, subprocessors, transfer mechanism, log retention | **BLOCKER** (counsel) |
| X6 | Terms | published, out of DRAFT | `docs/legal/terms.md` at `/terms`, DRAFT; `[TBD]` dispute window, refund conditions, deletion grace period | **BLOCKER** (counsel) |
| X7 | Legal entity and contact | one entity, one support contact, one security contact | "Select KK" (`plugin.json`, `docs/legal/*`) vs "Select, Inc." with `https://shogunai.si` (`apps/router/connectors/*/provider.json`); support `[TBD]`; no `SECURITY.md` (E38) | **BLOCKER** (owner) |
| X8 | Demo account | a working principal with fictional seed data and a credited wallet, credentials handed privately | procedure written (`anthropic.md`, `openai.md` "Demo account procedure"); needs X1 and X3 | **BLOCKER** (after deploy) |
| X9 | Logo | square logo asset (sizes per portal) | none in the repository (only `docs/diagrams/architecture.svg`) | **BLOCKER** where required (Anthropic, OpenAI) |
| X10 | Screenshots / demo media | what each portal asks for | none in the repository | gap (confirm on each portal; may be required) |
| X11 | Conformance against the deployed URL | `packages/conformance` green against the hosted `/mcp` | green against a local sidecar in CI; never run against a deployed URL | gap (runs once X1 is done; `docs/deploy/production.md` §7.4) |
| X12 | Logs carry no bodies during review | spot-check `wrangler tail` | logger is closed-field by construction; no production logs yet | gap (do during review) |

## 2. Per directory

### 2.1 Anthropic directory: MCP connector (claude.ai, Cowork, Claude Code)

| Requirement | Exists now | Status |
|---|---|---|
| Server URL, Streamable HTTP | `https://api.shogunagent.dev/mcp` named; not served (X1, X2) | **BLOCKER** |
| OAuth 2.1, CIMD, discovery from a `401` | implemented (`apps/api/src/oauth/`, `docs/api/oauth.md`); off until deployed (X3) | **BLOCKER** |
| Annotations | X4 | **BLOCKER** |
| Privacy policy / terms URLs | X5, X6 | **BLOCKER** |
| Support contact, logo | X7, X9 | **BLOCKER** |
| Demo credentials in the private field | X8 | **BLOCKER** |
| Short and long description | `identity.ts` `SHORT_DESCRIPTION` / `LONG_DESCRIPTION` | ok |
| Scopes shown on consent | `read`, `write`, `run` (`OAUTH_SCOPES`) | ok |
| Paid claude.ai account with the Directory permission to submit from | owner | **BLOCKER** (owner) |
| Tool descriptions name task, exclusions, cost class; no other products | `docs/api/core-tools.md`, selection eval (E11) | ok |

### 2.2 Anthropic directory: Claude Code plugin

| Requirement | Exists now | Status |
|---|---|---|
| `plugin.json` (name, version, description, author, license, skills, MCP servers) | `.claude-plugin/plugin.json`, `.claude-plugin/marketplace.json` | ok |
| `claude plugin validate --strict` passes | not recorded as run | gap |
| Skill | `SKILL.md` at the root | ok |
| Hosted MCP server in the plugin | X1, X2 | **BLOCKER** |
| Local MCP server `npx -y shogunagent@0.1 mcp` | the npm package `shogunagent` is not published (E40 workflows ready, publish awaits the owner) | **BLOCKER** (owner: first npm publish, `docs/release.md`) |
| Author entity | X7 | **BLOCKER** |
| Our own marketplace (`/plugin marketplace add Torutesu/Shogunagent`) | live in the repository; no submission needed | ok |

### 2.3 OpenAI Plugin directory (ChatGPT / Codex)

| Requirement | Exists now | Status |
|---|---|---|
| No selling of credits / digital goods inside the plugin | prepaid wallet, `402` with a top-up URL; options and recommendation in `openai-credit-rule.md` (Option A: free read-only toolset for the ChatGPT client) — not decided, not built | **BLOCKER** (owner decision + build) |
| `readOnlyHint` / `destructiveHint` / `openWorldHint` booleans on every tool | X4 (under Option A only the 18 reads are listed, which narrows the fix to explicit `false` values on reads) | **BLOCKER** |
| ChatGPT's client registration path | we support CIMD and deliberately have no DCR; whether ChatGPT registers by CIMD is unconfirmed | **BLOCKER** (confirm) |
| Business / identity verification in the OpenAI Platform dashboard | owner | **BLOCKER** (owner) |
| Published privacy policy, support contact | X5, X7 | **BLOCKER** |
| Demo account with sample data | X8 (own principal `directory-review-openai`) | **BLOCKER** |
| Example prompts | five in `openai.md`; prompts 4 and 5 use metered tools and must change under Option A | gap |
| Desktop and mobile tested | not done; consent page, `/privacy`, `/terms` are responsive | gap (after X1) |
| No trial or demo app | the listing is the production service | ok |
| Each public tool works on its own | yes; `actions.status` needs an `approval_id` (document, or drop it from the ChatGPT set) | ok / decide |
| Does not reconstruct the full chat log | `memory.ingest` asks for changed turns only; under Option A it is not listed at all | ok |
| No ads, no steering against other plugins | descriptions name exclusions by task, never by product | ok |
| Logo | X9 | **BLOCKER** |

### 2.4 Official MCP Registry

| Requirement | Exists now | Status |
|---|---|---|
| `server.json` valid against the registry schema | yes; `pnpm registry:validate` in CI | ok |
| Namespace `io.github.torutesu/*` | publishing under it needs the `Torutesu` GitHub account's authentication in the registry's publisher tool (owner action) | **BLOCKER** (owner) |
| `remotes[0].url` live | X1, X2 | **BLOCKER** |
| `packages[0]` npm `shogunagent@0.1.0` exists on npm | not published (E40) | **BLOCKER** (owner) |
| Version matches the package and the server | `0.1.0` in `server.json`, `identity.ts`, `packages/cli` (checked by tests) | ok |

### 2.5 Smithery

| Requirement | Exists now | Status |
|---|---|---|
| `smithery.yaml` (name, type `http`, URL, config schema) | yes (repository root) | ok |
| `/.well-known/mcp-config` served | implemented (`apps/api/src/wellknown/mcp-config.ts`); served only with the agent API on (X3) | **BLOCKER** |
| Endpoint live | X1, X2 | **BLOCKER** |
| Exact current Smithery schema | the research confirms Smithery as a surface, not every field (`smithery.yaml` header) | gap (confirm on the portal) |

### 2.6 Glama

| Requirement | Exists now | Status |
|---|---|---|
| Listing source | named as a target in `README.md` §7; nothing prepared; how Glama ingests (repository crawl vs. form) not researched | gap (research before planning) |
| Endpoint live | X1, X2 | **BLOCKER** for a hosted listing |

### 2.7 Monid (router listing, E31)

Prepared and blocked on answers from Monid and on our side; see `integrations/monid/CHECKLIST.md`
("Blocking: answers from Monid", "Blocking: our side"). Not repeated here.

## 3. Order that clears the most blockers

1. Owner decisions: entity and contacts (X7), hostname (X2), credit-rule option (2.3).
2. E34 lands (X1, X4); E38 adds `SECURITY.md` (X7).
3. Counsel clears the legal drafts (X5, X6).
4. Production deploy per `docs/deploy/production.md` with OAuth on (X3); conformance against the URL (X11).
5. Demo principals for each review (X8); logo and screenshots (X9, X10).
6. First npm publish of `shogunagent` (2.2, 2.4).
7. Submit in this order: MCP Registry and Smithery (fewest human gates) → Anthropic connector and
   plugin → OpenAI after the Option A build (or OpenAI's answer to Option C).
