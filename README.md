# ShogunAgent

**The state layer for AI agents.**

> Memory remembers. State knows.

[日本語版はこちら / Japanese version](./README.ja.md)

This repository holds the strategy, research, and (soon) the code for ShogunAgent: the state-inference and permission engine inside [ShogunAI](https://github.com/Torutesu/ShogunAI-) offered as a service to AI agents instead of humans. The decisions below are current as of October 2026 and supersede earlier drafts in `reports/`.

---

## 1. What it is

For teams running many AI agents, ShogunAgent is the shared state layer that gives every agent one calibrated model of the user's work (people, projects, commitments, open loops) with provenance on every fact and a human gate on every consequential action.

Unlike memory APIs that store free text in a vendor cloud, it is **typed, permission-tiered, local-first, and portable**.

It is not a memory API. "Agent memory" is a saturated category (Mem0, Zep, Letta, supermemory, Cognee, Engram) and the labs now ship personalization memory for free (OpenAI Dreaming, Anthropic Managed Agents memory, Google Memory Bank, AWS AgentCore, Microsoft Copilot). ShogunAgent names a different category: not what an agent remembers, but what is true now.

## 2. Value proposition (three pillars, no more)

| Pillar | Claim | Proof in the codebase | What competitors lack |
|---|---|---|---|
| **Typed state, not text memory** | Every fact is a typed row with a verbatim quote from its source event and a confidence score. Newer facts supersede older ones, contradictions are explicit, confidence decays over time. | Writes with empty `state_provenance` are rejected at the repository layer. High / Medium / Low bands enforced in one place (`shogun-fusion/confidence.rs`). The loop consolidator stores nothing without an exact quote match and confidence ≥ 0.84. | supermemory states it is "not a system of record: no SQL, no joins, no primary keys." Mem0 and Zep are free-text fact stores. |
| **Permission tiers on every action** | Reads run immediately (L1). Writes touching user data need one-tap confirmation (L2). External sends and payments need explicit human approval (L3). Enforced by type, the same for every agent. | `SendAction` is L3 by construction in `shogun-agents`. Maps 1:1 onto MCP `readOnlyHint` / `destructiveHint` / `openWorldHint` and MRTR `input_required` / URL elicitation. | 93% of agent permission prompts are approved without reading. Competitors expose binary read/write. |
| **Local-first, open, portable** | Data stays on the device by default. The cloud path retains no bodies. The engine is open source. State exports in an open format. | Zero-retention relay (traceability log has no body column). Secret redaction: recall 1.0, 0 false positives. User-held keys. | Every funded vendor is cloud-first. supermemory's OSS is clients only; the engine is closed. 7 of 7 memory products tested lock users in. |

**Delivery line for the router:** one endpoint for your agents: discover tools, run them under permission tiers, and keep what they learned as state you own.

**Pricing line:** reads are free; you pay for what you ingest and what you run.

### Message ladder

- **Three words:** The state layer for agents.
- **One sentence:** Calibrated truth about your work (people, projects, commitments, open loops) with provenance on every fact and a human gate on every send, shared across all your agents.
- **Developer, 30s:** Your agents start every session from zero and act without asking. ShogunAgent gives them one typed, provenance-backed model of the user's work over MCP, enforces three permission tiers on every action, keeps data on the device by default, and exports everything in an open format. Reads are free; you pay for what you ingest and run.
- **CTO, 30s:** Agent failures are context failures and permission failures. ShogunAgent is a system of record for what agents believe about your work, with a quote behind every fact, a confidence score on every row, and a human approval on every external send. Nothing leaves the device unless you say so, and nothing is locked in.

### Words we do not use

"AI OS", "world model" (collides with robotics usage), "memory" as our category name, "benchmark #1" (the term has lost trust; we publish same-harness results only), region-specific compliance framing.

## 3. How state is acquired and delivered

![Figure 1. Raw data to agent context through the ShogunAgent engine](docs/diagrams/architecture.svg)

*Figure 1. Raw data enters one encrypted store per principal; the Dream Cycle turns it into typed state with a quote and a confidence on every row; agents receive it as a context pack, as files or as tools, and every action passes the L1/L2/L3 gate. Agent writes and router runs flow back as events.*

Five acquisition paths, all landing in the same append-only `event_log` (dedup, redaction, instruction-shaped gate):

| Path | Content | Requires |
|---|---|---|
| Agent ingest (primary) | Conversation turns, tool results, task progress via `memory.ingest` and Anthropic memory-tool-compatible `/memories` writes | Nothing. Any agent. |
| Connector pull | Gmail, Calendar, Slack, GitHub, Linear, Notion over OAuth | `shogun-integrations` |
| Router run log | tool × task × outcome for every run through the router | Owning the router |
| Device capture (premium) | Accessibility-API text, Visual recall OCR, meeting transcripts | The ShogunAI Mac app |
| Import | ChatGPT / Claude / Cursor exports | `shogun-memory-import/v1` |

Inference is the ShogunAI pipeline unchanged: hourly Dream Cycle (sessionize, prefilter, schema-enforced extraction) → state tables with quote and confidence → consolidator → entity pages and period summaries.

Delivery takes three shapes, because that is what agents call in 2026: a budgeted context pack (`get_context_pack`: High as fact, Medium as "possibly", Low excluded), a virtual filesystem (`/memories/people/<name>.md`, `/memories/open_loops.md`, readable by Anthropic's memory tool with zero integration), and MCP tools (`state.*`, `recall`, `search`) plus an A2A Agent Card.

ShogunAgent is model-agnostic. Our own LLM use is extraction and consolidation only, in cheap batch lanes.

## 4. Relationship to the model layer

**Where the labs will win and we do not compete:** personalization memory inside their own products, session state inside their own APIs, storage at the cost floor.

**Where they structurally will not go and we stand:** cross-vendor state (no lab becomes the neutral record for another lab's agents), portability (lock-in is their revenue model), human-approval governance across agents from different vendors, device-level truth.

**How we relate:** customer, supplier, never competitor. Anthropic's memory tool is storage-agnostic, so we are its backend; we list in the Claude Code plugin marketplace, the Anthropic directory, and the MCP Registry. OpenAI: Plugin directory plus an Agents SDK middleware. Google: ADK and Vertex memory interfaces. AWS: the external store behind AgentCore's self-managed memory tier. Microsoft Copilot's communication memory is the closest comparable; we offer the same to agents outside M365.

**Watch:** Anthropic Cowork / Claude Desktop reaching into device context; OpenAI Dreaming extending from personalization to work state; Microsoft opening communication memory through Foundry. Each is an extension inside one vendor; the more vendors a user mixes, the more we are worth.

## 5. Competitors by layer

| Layer | Players | Our difference | Stance |
|---|---|---|---|
| Memory APIs | Mem0, Zep, Letta, supermemory, Cognee, Engram | Typed state vs free text; three tiers vs read/write; local-first vs cloud-first; open engine vs closed | Direct competitors; we name a different category |
| Routers and registries | Monid, Composio, Arcade + Smithery, Glama, Coinbase Agentic Market | No permission tiers, no state | We build our own router as the front door and list on all of them |
| Enterprise context platforms | Microsoft Copilot, Glean, Jedify, Interloom | Suite-bound or data-platform-bound; not agent-neutral; no human gate | The real comparables; where we must win |
| Capture layer | Screenpipe, Limitless (Meta), Rewind (stopped) | Capture without state inference | Mostly exited. Lesson: nobody pays for raw recordings |

## 6. Moat (strongest first, honestly)

1. **Position in a two-sided market.** Tool suppliers connect to the router; agents connect over MCP. More connectors → more agents → more run data → better `discover` → more agents. This loop needs the state layer; a router alone does not have it.
2. **Compounding state.** The longer it runs, the more accurate the state; leaving loses calibrated confidence and learned patterns even though facts export freely. Continuity, not lock-in.
3. **Trust.** Zero retention, provenance on every row, six eval harnesses, OWASP ASI06 mitigations in the write path. Enterprise trust is earned through audits and time and cannot be bought.
4. **Device-level truth.** Accessibility and Visual recall capture that neither labs nor memory vendors have. Grows as platforms are added.
5. **Standards.** Bring the export format and the permission-tier taxonomy to the W3C memory interop CG and MCP annotations; be the reference implementation.
6. **Engineering lead.** ~330k lines of Rust, calibration rules, 91 migrations. A lead, not a wall.

**Not a moat:** router code (MIT, 780 forks), the shape of a memory API, benchmark scores, model access.

## 7. Product decisions

### Build an "OpenRouter for Agents", as the front door

Owner decision. The research warnings (commodity connector code, $0.0015 median call price, consolidation by Arcade and Stripe, conflict of interest when ranking your own product, memory not fitting a spot market) remain true and become design constraints:

1. **Two layers.** Router (discover / inspect / run, metered, zero-retention proxy) may run at near-zero margin. Revenue is measured on ingest and state. Router KPI: share of router agents that start using the state layer.
2. **Borrow the cold start.** Monid-compatible connector definitions so existing endpoint definitions import; pass through Monid's hosted catalog. First-party connectors only where they differentiate: memory and state, and writes that need approval.
3. **L1/L2/L3 on every run.** Reads L1, user-data writes L2, external sends and payments L3. Lift `scope.rs` ("deny anything not in the table, ExternalSend is always L3") as is.
4. **The router learns in user-owned state.** tool × task × outcome recorded in ShogunAgent state; `discover` personalized per user and tenant; portable, not centralized.
5. **No self-preferencing.** Publish the score breakdown with price included, label first-party tools, keep directory submissions separate from the router's SKILL.md.

### Pricing: pure usage-based

No fixed subscription. Prepaid credits, `402` when exhausted, per-key and per-agent hard caps from day one.

| Metered item | Unit price (all estimates) | Notes |
|---|---|---|
| Tool run, third-party provider | provider price + 5% | Failures and empty results $0 |
| Tool run, first-party connector | $0.002 / call | Writes pass L2/L3 at the same price |
| Ingest | $0.50 / 1K events | Includes extraction LLM cost |
| Model-invoking op | $0.10 / 1K | Consolidation, AI search, entity pages |
| State reads | $0 | `state.*`, `get_context_pack`, `recall`, `search` |
| Remote L2/L3 approvals | $0 | |
| Hosted storage | $0.10 / GB-month | Delete and export always free |
| Local (OSS) | $0 | |
| Enterprise | same rates, 10–25% off with annual commit | BYOC / ZDR / SSO / audit included; no seat fees |

Margin targets: ingest at 3–5× cost; runs near break-even at a 5% take. Revise from design partners' real usage.

### Do not build

A general router competing on price alone; our own payment rails or an x402 wallet (Stripe plus the 402 convention; decide x402 later); a graph DB (typed relation rows in SQLite suffice); a Postgres port (reuse drops from 60–70% to 25%); in-house embedding research; a chat-personalization memory API; LoCoMo score-chasing; Windows/mobile capture before the ingest API; consumer apps or a browser extension; exposing all 158 tools (about 100 desktop-only tools stay in ShogunAI).

## 8. Go-to-market: developers and individuals first, OSS as the engine

**Who, in order:** (1) power users of coding agents (Claude Code, Codex, Cursor), (2) developers building agent products (Agents SDK, Vercel AI SDK, Mastra, LangGraph), (3) small teams running many agents, (4) enterprises promoted from 2–3. Consumer memory products all exited; we sell to people who run agents daily, not to general consumers. Existing ShogunAI Mac users are the day-one cohort: their agents already have read connections auto-installed.

**Open:** the engine (state tables, confidence, consolidator, Dream Cycle), local MCP / REST / CLI, connector definitions and the router engine, SKILL.md, the export format, all six eval harnesses. License: Apache-2.0. **Closed:** hosted sync, remote approvals, wallet and billing, the hosted router catalog and provider contracts. Funnel: same API locally and hosted, switch `baseURL`. Unlike supermemory, the engine itself is open.

**OSS launch mechanics:** a README GIF that works in ten seconds (`npx shogunagent`, and your coding agent knows your open loops); Show HN built around a reproducible claim (same-harness results, audit-style posts), not a feature list; the benchmark harness published so competitors' numbers can be reproduced; an "open-loop tracking" benchmark offered to the community; connector definitions as the low-friction contribution path.

**Agent-native distribution:** SKILL.md, `npx shogunagent help --json`, a docs MCP, `llms.txt`. Agents install it themselves and spend within their own prepaid budget.

**Platform shelves, all in English, from the start:** Claude Code plugin marketplace → Anthropic directory; official MCP Registry; Cursor / Windsurf MCP configs; OpenAI Plugin directory; Glama, Smithery, Composio toolkit, Monid once a memory category exists.

**One-line integrations:** `withShogunAgent()` for Vercel AI SDK, OpenAI Agents SDK, Claude Agent SDK, Mastra, LangGraph, CrewAI. This is what drove Mem0's 5× API growth.

**Not doing:** paid ads, SEO-first, conference booths, enterprise sales first.

**Conversion ladder:** OSS local ($0) → hosted sync (second device or agent) → wallet top-up (connector ingest, router runs) → team (shared state, remote approvals) → enterprise (BYOC, ZDR, audit). The first two rungs are free and build trust and volume; revenue starts at the third.

## 9. Roadmap (sequence only, no durations)

Development runs AI-first. Phases are defined by order and exit criteria; no day or month counts.

| Phase | Deliverables | Exit criteria (estimates) |
|---|---|---|
| **1. Core and router skeleton** | `shogunagent-core` standalone on Linux (per-tenant SQLite); Streamable HTTP MCP with the 21 frozen core tools; API-key auth; `/memories` projection; export format v1; OSS release. Router: discover / inspect / run, Monid-compatible connectors, hosted passthrough, memory/state category, per-key caps, L1/L2/L3 on every run. Prepaid credits and 402. SKILL.md, Claude Code plugin, MCP Registry and Glama listings. Same-harness results published. ShogunAI Mac Memory API rebranded. | Sustained `tools/call` traffic from external agents; design partners signed; one published benchmark; router-to-state conversion measurable |
| **2. Approvals, SDKs, billing, catalog** | Remote L2/L3 approvals (web, Slack, webhooks); A2A Agent Card; TS/Python SDKs with `withShogunAgent()`; agent-scratch namespaces with ACL and audit; Stripe usage billing and wallet; personalized `discover`; self-serve provider listing; Anthropic and OpenAI directories; local embeddings | Growing paid wallet users; ingest gross margin ≥ 3× cost; LLM cost ≤ 15% of revenue; improving router-to-state conversion |
| **3. Enterprise, standards, payments** | Enterprise commit contracts (BYOC, SSO/SCIM, audit export, ZDR); W3C memory-interop CG and export format v2; public open-loop benchmark v1; provider payouts and open catalog; x402 decision; fundraising | Enterprise contracts; NRR > 110%; top-3 on same-harness multi-session and temporal categories; rising first-party connector share |

## 10. What carries over from ShogunAI

Roughly 30–40% by lines of code and 70%+ of the hard parts.

Reuse: `shogun-memory` (state tables, `state_provenance`, loops, entity pages, federated search, sanitize, traceability, import; keep V1–V91 on SQLite, add `tenant_id` / `uid` / `actor` by additive migration), `shogun-fusion`, `shogun-agents`, `shogun-redact`, `shogun-license` (verify), `shogun-mcp` (`MemoryBackend`, `ApiLevel`, toolsets, source policy, rate limit, REST routing; replace the Keychain-bound settings and launcher check with env vars and `apps/api` tokens), platform-neutral `shogun-core` (dreamcycle, llm, ai_search, context, `shogun_api` / `shogun_mcp` binaries), `apps/api` (Ed25519 auth, reserve-then-settle metering, zero-retention logger).

Does not carry over: the macOS capture layer, Keychain-bound secrets, the Tauri desktop, the single-tenant assumption.

Shortest path: multiplex the `shogun_api` binary across per-tenant SQLCipher files with `apps/api` in front for auth and metering.

## 11. Agent-native commerce: designed from day one to be chosen by agents

The buyer is increasingly an agent, not a person. An agent discovers a service, compares it on price, latency, health and trust, onboards itself, pays within a budget, and re-selects or abandons it based on how the first calls went. Every one of those steps is designed for, from the first release.

### Principles

1. **Zero-config first call.** An agent with no key can call `discover`, `inspect`, and a tightly scoped read. The first write or metered run returns `402` with everything needed to proceed. Nothing requires a human before the agent has seen value.
2. **Every response is self-describing.** Each result carries `usage` (cost, units), `latency_ms`, `next_action` on error (what to do, not just what failed), and `hints` (a cheaper or better-fitting alternative when one exists, including a competitor's connector inside the router). Agents' selection loops penalize opaque failures; honesty is the retention mechanism.
3. **Machine-readable at every well-known location.** The same identity, tool names and schemas appear everywhere an agent might look, and the human-facing docs are derived from the machine-facing ones, never the reverse.
4. **Descriptions are written for ranking, not for marketing.** Registries and routers rank on description text. Tool descriptions name the tasks an agent is trying to do ("find what the user committed to", "what is pending with this person", "run a tool and keep the result as state") and the exclusions ("not for storing chat transcripts verbatim"). Each description is tested, not guessed (see selection eval).
5. **One human approval, at most, and only for money.** A card is attached once through URL-mode elicitation; after that the agent spends within its cap. Reads never ask. Permission tiers (L1/L2/L3) govern actions on the user's world; budget caps govern spend. The two are separate and both are visible to the agent before it acts.
6. **Stability is a feature agents can measure.** Frozen wire names, a published deprecation policy, idempotency keys on every write, `ttlMs` on lists, honored `Retry-After`, and a public incident history. An agent that was burned once does not come back.

### Day-one artifacts

| Surface | What ships | Why an agent needs it |
|---|---|---|
| `/.well-known/mcp/server-card.json` | MCP Server Card: endpoint, auth, tool list with annotations | Progressive discovery without a handshake |
| `server.json` in the MCP Registry | Canonical registry entry | Where Claude Code, Cursor and gateways look first |
| `/.well-known/agent-card.json` | Signed A2A Agent Card | Cross-organization handoff and identity |
| `llms.txt` and `llms-full.txt` | Agent-readable docs index | Agents read docs before humans do |
| `SKILL.md` | Trigger conditions and non-trigger exceptions, setup, auth, precedence rules, cost warnings, status semantics, L2/L3 meaning, numbered rules | Agents install and operate it themselves |
| `openapi.json` | OpenAPI 3.1 for the REST face | Code-generating agents |
| `/.well-known/pricing.json` | Machine-readable rate card, free allowance, caps | Price comparison before the first call |
| `/health` and `/.well-known/status.json` | Live p50 / p95, error rate, incident history | Health-weighted ranking |
| `benchmarks.json` | Same-harness results with reader, judge, seeds, cost per answer | Trust signal that can be re-run |
| `quote` endpoint and `dryRun` on every metered call | Exact cost before committing | Budgeted agents refuse unknown costs |
| 402 payload | Amount, units, accepted rails, human-approval URL, `Retry-After` | Self-service top-up with no support ticket |
| `POST /v1/agents` | Agent self-registration: returns a scoped key and the free allowance, rate-limited and abuse-gated, no email | Onboarding without a form |
| Referral in the key | `ref` attribute credited to the referring agent's or developer's wallet | Agents become the channel |

### Payment design

Prepaid wallet settled through Stripe. The `402` response follows the x402 payment-required shape so wallet-carrying agents already know how to read it, even before we accept on-chain settlement. Per-key and per-agent hard caps (daily and monthly) are mandatory and default to conservative values. Receipts are machine-readable per call. Failed and empty results cost nothing. Disputes are an API, not an email.

### Selection eval: the metric that matters

A harness that runs from CI: given a task description, which tool does Claude, GPT, Gemini and an open model pick when ShogunAgent sits alongside Mem0, supermemory, Zep and the router's own alternatives, across the MCP Registry listing, the Claude Code plugin description, the OpenAI Plugin listing and the router's `discover`? The result is a selection rate per surface per model. Description changes ship only when this number does not fall. This is the agent-era equivalent of conversion-rate optimization, and nobody in the category runs it.

### Being chosen inside our own router

The router ranks third-party tools and our own connectors by the same published score (task fit, price, health, latency), with first-party items labeled and a written no-self-preferencing policy. The state layer is not listed as a competing tool; it is the substrate every run writes into. Agents choose tools in the router; they choose the router because the results persist as their user's state.

## Repository layout

```
README.md                  This document (English)
README.ja.md               This document (Japanese)
CLAUDE.md                  Rules for working in this repository
crates/                    Rust workspace migrated from ShogunAI (engine, MCP/REST/CLI, evals)
apps/api/                  Gateway and relay (Hono on Cloudflare Workers + Postgres)
packages/config/           Shared config
scripts/                   Invariant guards and the feature-matrix runner
docs/implementation-plan.md  Epics and tasks per phase, with acceptance criteria
docs/MIGRATION.md          What was migrated from ShogunAI, what was not, and why
docs/migrations/           Rollback notes per schema migration
docs/mcp/                  Memory API architecture notes
docs/shogunai-origin/      Reference copies of ShogunAI design docs
reports/                   Full strategy report (Markdown and bilingual HTML)
research_notes/            Seven research notes with inline sources
```

## Open questions

- Do developers without the Mac app get enough state from the ingest API alone?
- Do router agents convert to the state layer at a rate that justifies the router?
- Which export format wins the W3C memory-interop process, and can ours be it?

All three are answered by design partners' real usage, not by further research.
