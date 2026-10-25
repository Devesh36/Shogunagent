# OpenAI Plugin directory submission (ChatGPT / Codex)

Plan: E23. Sources: `research_notes/ShogunAgent AIエージェント向けサービス戦略/monid_and_agent_tool_marketplaces.md`
("OpenAI Apps (ChatGPT)" and Key Question 5 "OpenAI Apps SDK", citing OpenAI's App submission
guidelines) and `agent_infra_and_protocols.md` (the app directory became a **Plugin directory on
2026-07-09**; plugins bundle skills, apps and app templates; OpenAI reviews every third-party
submission). Values come from files in this repository; anything the research did not confirm
from a primary page is marked **(confirm on the portal)**. Nothing here has been submitted.

## What OpenAI requires, mapped to our assets

| Requirement (submission guidelines) | Our answer | Source |
|---|---|---|
| Clear name, no "MCP" / "Plugin" suffix | `ShogunAgent` | `identity.ts` `DISPLAY_NAME` |
| Description | `identity.ts` `LONG_DESCRIPTION`; short form `SHORT_DESCRIPTION` | `identity.ts` |
| Example prompts | see "Example prompts" below | this file |
| Published privacy policy | `https://api.shogunagent.dev/privacy` (DRAFT until legal review) | `docs/legal/privacy-policy.md` |
| Terms | `https://api.shogunagent.dev/terms` (DRAFT until legal review) | `docs/legal/terms.md` |
| Identity / business verification in the OpenAI Platform dashboard | Select KK — **to do by the owner** | — |
| Support contact | `[TBD: support address]` | — |
| MCP server | `https://api.shogunagent.dev/mcp`, Streamable HTTP | `server.json` |
| Tool annotations: `readOnlyHint` / `destructiveHint` / `openWorldHint` booleans on every tool | yes, rendered per level (table below) | `apps/api/src/wellknown/tool-table.ts` |
| Transparent auth | OAuth 2.1 (PKCE S256, Client ID Metadata Documents, no DCR) and a consent page that lists the requested scopes in plain English **(confirm whether ChatGPT registers by CIMD or still expects DCR)** | `apps/api/src/oauth/`, `docs/api/oauth.md` |
| Demo account with sample data | see "Demo account procedure" | this file |
| Desktop and mobile tested | to do (consent page is responsive; `/privacy` and `/terms` too) | — |
| No trial or demo apps | the listing is the production service | — |
| Each public tool works on its own | yes: every core tool takes its own inputs; none needs a prior call except `actions.status` (needs an `approval_id`, documented) | `docs/api/core-tools.md` |
| No steering toward or away from other plugins | descriptions name exclusions by task ("not a web-search tool"), never by product | `docs/api/core-tools.md`, `SKILL.md` |
| Data minimization; no profiling without disclosure | ShogunAgent *is* a model of the user's work: say so in the description and the privacy policy (section 2) | `docs/legal/privacy-policy.md` |
| "MCP servers must not reconstruct the full chat log" | `memory.ingest` asks for the turns that changed something, not whole transcripts; for the ChatGPT listing, check the description of `memory.ingest` and `SKILL.md` keep saying that, and do not add a tool that uploads a conversation | `docs/api/core-tools.md` #20 |
| Commerce only for physical goods; no selling digital goods, subscriptions or **credits** | **Blocker — see below** | — |
| No ads | none | — |

### Tool annotations

| Tools | readOnlyHint | destructiveHint | openWorldHint |
|---|---|---|---|
| 18 reads: `memory.search`, `memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `memory.get_periods`, `memory.recall`, `calendar.upcoming`, `profile.whoami`, `state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`, `state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.status` | true | false | false |
| `memory.append_note`, `memory.ingest` (write to the user's own store, metered) | false | false | false |
| `actions.execute` (send / post / comment queue for human approval; L3) | false | true | true |

The same discrepancy as in `docs/directories/anthropic.md` applies: align the core's
`tools/list` (Rust, `docs/api/core-tools.md`) with the server card before submitting.

### Example prompts

1. "What did I commit to this week, and which of those are overdue?" → `state.commitments.list`
2. "Before you draft the reply to Aiko, recall what is still open with her." → `memory.recall`, `state.open_loops.list`
3. "Who am I working with most on the Atlas project?" → `state.projects.list`, `state.projects.get`
4. "Remember that the vendor review moved to Friday." → `memory.append_note`
5. "Email Ken the summary — but let me approve it first." → `actions.execute` (`send_email`, returns an approval URL; nothing is sent until approved)

## Blocker: credits cannot be sold inside ChatGPT

The guidelines allow commerce only for physical goods and forbid selling digital products,
subscriptions, tokens or **credits**. Our hosted service is a prepaid micro-USD wallet, and a
metered call without balance answers `402` with a top-up URL (`docs/api/402.md`). Before
submitting, decide one of:

- **Prepaid outside ChatGPT only.** For sessions from the ChatGPT client, `402` names the account
  page on `shogunagent.dev` without a purchase link inside the conversation, and reads stay free.
  Needs a way to recognise the client (the OAuth `client_id` of the ChatGPT connector — it is a
  CIMD URL we can match) and a variant of the 402 message. Not implemented.
- **Reads-only listing.** Offer only the 18 free read tools (and the free approval polling) in the
  ChatGPT listing; writes and runs stay on the MCP endpoint for agents. Needs a toolset filter by
  client. Not implemented.
- Ask OpenAI whether usage-billed API access paid on our own domain counts as selling credits.

Until one is chosen and built, do not submit. **(Owner decision.)**

## Demo account procedure

Same as the Anthropic one, with its own principal so the two reviews never share state:

1. `POST /v1/agents` (PoW) with `ref: "directory-review-openai"`.
2. Seed the fictional work history with `memory.ingest` under fixed idempotency keys (invented
   people and projects, `example.com` addresses only).
3. `POST /v1/admin/adjustments` (`kind: "credit"`, `reason: "service_credit"`,
   `Idempotency-Key: directory-review-openai-1`) so the reviewer never meets a `402` — and, given
   the blocker above, so no purchase prompt can appear during review.
4. Hand the `sa_live_…` key to the reviewer through the portal's private test-credentials field.
   The OAuth consent page asks for it once; ChatGPT holds the resulting tokens.
5. After review, revoke the key; its OAuth grants die with it.

## Review checklist

- [ ] OAuth connect from ChatGPT web, desktop and mobile; refresh works after 15 minutes; revoke
      works.
- [ ] Every tool returns usable output with the demo data, independently.
- [ ] No tool result contains a purchase link (blocker above) for the ChatGPT client.
- [ ] `actions.execute` never sends without approval; `actions.status` reflects the decision.
- [ ] Privacy policy and terms out of DRAFT and linked.
- [ ] Business verification complete in the OpenAI Platform dashboard.
- [ ] Descriptions do not mention other plugins or memory products.
- [ ] Spot-check logs for bodies during the review window: none.

## What must be true before submitting

1. The hosted core answers `/mcp` (today `503 upstream_unavailable` until E4 wires
   `mcpUpstream`).
2. OAuth on in production (`AGENT_OAUTH_SECRET`, `sql/agent-oauth.sql`), and the ChatGPT client's
   registration path confirmed (CIMD vs DCR; we deliberately have no DCR endpoint).
3. The credits blocker resolved by an owner decision and its implementation.
4. Legal pages out of DRAFT, subprocessors confirmed, support contact live.
5. Annotations aligned between the core and the server card.
6. OpenAI Platform business verification done.
