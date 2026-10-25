# Anthropic directory submission (Claude Code plugin + MCP connector)

Plan: E23. Sources: `research_notes/ShogunAgent AIエージェント向けサービス戦略/monid_and_agent_tool_marketplaces.md`
(Key Question 5, "Anthropic directory / Claude Code plugin") and `agent_infra_and_protocols.md`
(MCP 2026-07-28 authorization: CIMD replaces DCR). Every value below is taken from a file in this
repository; where the portal's exact field is not confirmed by a primary source it is marked
**(confirm on the portal)**. Nothing here has been submitted.

## Routes, and which one this is

There are three ways a Claude user gets ShogunAgent (Claude Code publish docs, as summarised in
the research note):

1. **Our own marketplace** — already live in the repository: `.claude-plugin/marketplace.json`.
   Users run `/plugin marketplace add Torutesu/Shogunagent` then
   `/plugin install shogunagent@shogunagent`. No submission, no review.
2. **Anthropic's directory** — submitted at `claude.ai/directory/manage` from a **paid claude.ai
   plan** (Pro/Max own account, or a Team/Enterprise Owner or a member with the Directory
   permission). One listing reaches claude.ai, Cowork and Claude Code (it loads as
   `shogunagent@synced`). Automated review first; an "Anthropic Verified" badge after an
   additional review. **This document is for this route.**
3. `claude-plugins-official` does not take submissions through the portal; not a target.

The portal accepts the **plugin** and, separately, the **MCP server as a connector**. Submit both:
the connector is what claude.ai web users add; the plugin carries the skill for Claude Code.

## Fields, filled from our assets

### Plugin

| Field | Value | Source |
|---|---|---|
| `name` (permanent, kebab-case) | `shogunagent` | `.claude-plugin/plugin.json` |
| Display name | `ShogunAgent` | `apps/api/src/wellknown/identity.ts` `DISPLAY_NAME` |
| `version` | `0.1.0` (one minor = one skill; bump with `SKILL.md` and `packages/cli`) | `plugin.json`, `SKILL.md`, `identity.ts` `VERSION` |
| `description` | "The state layer for AI agents: people, projects, commitments and open loops, with a quote and a confidence on every fact. Reads are free; writes and runs are metered; L3 actions wait for a human." | `plugin.json` |
| Long description | `identity.ts` `LONG_DESCRIPTION` (the same text as the MCP server card and the A2A card) | `identity.ts` |
| `author` | Select KK, https://shogunagent.dev | `plugin.json` |
| `homepage` / `repository` / `license` | https://shogunagent.dev / https://github.com/Torutesu/Shogunagent / Apache-2.0 | `plugin.json` |
| `keywords` | state, memory, commitments, open-loops, mcp, agents, provenance, approvals | `plugin.json` |
| Category | productivity **(confirm the portal's category list)** | `marketplace.json` |
| Skill | the repository-root `SKILL.md` (`"skills": ["./"]`) | `plugin.json` |
| MCP servers | `shogunagent` (HTTP, `https://api.shogunagent.dev/mcp`) and `shogunagent-local` (`npx -y shogunagent@0.1 mcp`, read-only stdio) | `plugin.json` |
| README | `README.md` | repository root |

### Connector (the remote MCP server)

| Field | Value | Source |
|---|---|---|
| Server URL | `https://api.shogunagent.dev/mcp` (Streamable HTTP, protocol `2026-07-28`) | `server.json` `remotes`, `identity.ts` `HOSTED_MCP_URL` |
| Name / title | `io.github.torutesu/shogunagent` / ShogunAgent | `server.json` |
| Short description (≤ 100 chars) | "The state layer for AI agents: people, projects, commitments and open loops, with provenance." | `identity.ts` `SHORT_DESCRIPTION` |
| Authentication | **OAuth 2.1** with Client ID Metadata Documents: protected resource metadata at `/.well-known/oauth-protected-resource/mcp`, authorization server metadata at `/.well-known/oauth-authorization-server`, PKCE S256, no Dynamic Client Registration. A `401` on `/mcp` names the metadata URL in `WWW-Authenticate`. Bearer `sa_live_…` keys keep working for agents that self-register. | `apps/api/src/oauth/`, `docs/api/oauth.md` |
| Scopes offered | `read`, `write`, `run` (never `admin`) | `oauth/app.ts` `OAUTH_SCOPES` |
| Tools and annotations | the 21 core tools from the server card (below) | `GET /.well-known/mcp/server-card.json`, `docs/api/core-tools.md` |
| Privacy policy URL | `https://api.shogunagent.dev/privacy` (DRAFT until legal review) | `docs/legal/privacy-policy.md` |
| Terms URL | `https://api.shogunagent.dev/terms` (DRAFT until legal review) | `docs/legal/terms.md` |
| Support / contact | `[TBD: support address]` | — |
| Logo | `[TBD: square logo asset]` **(confirm required sizes on the portal)** | — |

### Tool annotations as published

The server card renders annotations from each tool's level (`apps/api/src/wellknown/tool-table.ts`
`annotationsFor`); `docs/api/core-tools.md` is the Rust-generated wire contract.

| Tools | Level | readOnlyHint | destructiveHint | idempotentHint | openWorldHint |
|---|---|---|---|---|---|
| `memory.search`, `memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`, `memory.get_wrap`, `memory.get_periods`, `memory.recall`, `calendar.upcoming`, `profile.whoami`, `state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`, `state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.status` (18) | read (L1, free) | true | false | true | false |
| `memory.append_note`, `memory.ingest` | L1 write (metered) | false | false | true (the server card; both take an idempotency key) | false |
| `actions.execute` | L1 / L3 by kind | false | true | false | true |

**Discrepancy to settle before submitting:** `docs/api/core-tools.md` (generated by the Rust
`wire_freeze` test) lists no annotations for the two L1 writes, while the server card sends
`idempotentHint: true, destructiveHint: false, readOnlyHint: false`. Reviewers read what
`tools/list` returns from the core. Align the two (E4 exports the tool table; the fixture is
hand-maintained until then — `docs/api/agent-surfaces.md`).

## Demo account procedure

Reviewers need a working account with realistic data that cannot run out of money mid-review.

1. **Principal.** Register a dedicated principal (`POST /v1/agents` with the PoW header,
   `ref: "directory-review-anthropic"`). Record `principal_id` and `key_id`; keep the key in the
   operator's secret store, never in a ticket.
2. **Seed data.** Ingest a fixed, fictional work history (`memory.ingest`, one idempotency key per
   batch): two projects, four people, five commitments with due dates, three open loops, a
   calendar week. Use invented names and the `example.com` domain only. Re-running the seed is a
   no-op because of the idempotency keys.
3. **Money.** Credit the wallet so the review never hits `402`: `POST /v1/admin/adjustments`
   `{"principal_id": "<id>", "kind": "credit", "amount_micro_usd": 20000000, "reason": "service_credit"}`
   with `Idempotency-Key: directory-review-anthropic-1`. Lower the key's caps to the defaults
   (`PATCH /v1/keys/{id}`) so a runaway loop is bounded.
4. **Credentials for the reviewer.** Give the reviewer the `sa_live_…` key through the portal's
   private credentials field (never in the public listing). It is what the consent page asks for
   when Claude connects over OAuth; it can also be used directly as a bearer.
5. **L3 path.** Note in the review instructions that `actions.execute` with `send_email` returns an
   approval URL and never sends; the reviewer can approve or reject at that URL.
6. **After review.** Revoke the key (`POST /v1/keys/{id}/revoke`) — every OAuth token minted from
   it dies with it — and issue a new one for the next review.

## Review checklist

- [ ] `claude plugin validate --strict` passes on the repository root.
- [ ] Test install from our own marketplace in a clean Claude Code profile; the skill loads and
      `profile.whoami` answers.
- [ ] Connect the hosted server from claude.ai through OAuth end to end: discovery from the `401`,
      the consent page, token exchange, a refresh after 15 minutes.
- [ ] Every tool in `tools/list` carries `readOnlyHint` / `destructiveHint` (and the discrepancy
      above is closed).
- [ ] Tool descriptions name the task, the exclusions and the cost class, and do not mention or
      steer away from other products (invariant 13; selection eval E11 must not drop).
- [ ] The privacy policy and terms are live, out of DRAFT, and linked from the listing.
- [ ] Reads work for a reviewer without spending; writes are metered from the credited wallet.
- [ ] `402` responses are plain English and name the next action.
- [ ] No request body appears in any log line during the review session (spot-check
      `wrangler tail`).

## What must be true before submitting

1. **The hosted core answers `/mcp`.** Today `apps/api` serves `/mcp` with `503
   upstream_unavailable` until E4 wires `mcpUpstream` (`apps/api/README-agent-api.md`, "Not in
   this slice"). A directory review against a 503 fails.
2. **OAuth is on in production:** `AGENT_OAUTH_SECRET` set, `sql/agent-oauth.sql` applied, and
   the flow verified from Claude Code and Cursor by hand (E3-T2's acceptance is a manual review).
3. **Legal pages out of DRAFT** after counsel's review, placeholders filled, subprocessors confirmed.
4. **A paid claude.ai account** with the Directory permission is available to submit from.
5. **Annotations consistent** between the core's `tools/list` and the server card.
6. **Logo and support contact** exist.
7. Directory submissions stay separate from the router's `SKILL.md` (README §7, "No
   self-preferencing"): the listing describes ShogunAgent, not the router's ranking.
