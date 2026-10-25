# OpenAI directory: the rule against selling credits, and how our listing complies (E42)

Status: **proposal for an owner decision.** Nothing has been submitted, built or asked of OpenAI.
Companion documents: `docs/directories/openai.md` (the submission mapping, where this was first
recorded as a blocker) and `docs/directories/submission-checklist.md`.

## 1. The rule, as our research recorded it

From `research_notes/ShogunAgent AIエージェント向けサービス戦略/monid_and_agent_tool_marketplaces.md`
("OpenAI Apps (ChatGPT)" and Key Question 5), quoting OpenAI's App submission guidelines
(<https://developers.openai.com/apps-sdk/app-submission-guidelines>), and
`agent_infra_and_protocols.md` (the app directory became a Plugin directory on 2026-07-09; OpenAI
reviews every third-party submission):

- "Currently, plugins may conduct commerce only for physical goods."
- "Selling digital products or services — including subscriptions, digital content, tokens, or
  credits — is not allowed."
- Commerce that is allowed goes through an external checkout on the developer's own domain; no ads.
- Also relevant to how we comply: a demo account is required, "trial or demo plugins will not be
  accepted", each public tool must work on its own, and MCP servers "must not reconstruct the full
  chat log".

These quotes were collected for the strategy research and were **not re-checked against the live
page for this document** (no browsing was done). Before choosing, the owner should re-read the
current guidelines page, because the wording on digital goods has moved before (the directory itself
was renamed in July 2026).

## 2. Why ShogunAgent is caught by it

- The hosted service is a **prepaid micro-USD wallet** (`docs/api/402.md`, `docs/legal/terms.md` §4).
  Writes (`memory.append_note`, `memory.ingest`) are metered as `ingest`, `actions.execute` as a
  `run`; a call without balance answers `402 payment_required` with `next_action: "top_up"` and a
  `card` rail whose `url` is `POST /v1/wallet/topup` (Stripe Checkout on our domain).
- A `402` body reaches the model inside the conversation, so a ChatGPT user would be offered a way to
  buy **credits** for a digital service from inside the plugin — exactly what the rule forbids.
- The wallet also emits `wallet.low_balance` events carrying a `topup_url` (`docs/api/billing.md`),
  and the signup allowance ($5, `FREE_ALLOWANCE_MICROS` in `apps/api/src/pricing.ts`) and the
  ShogunAI Pro bundle ($20 a month for a linked Pro licence) are credits too, though granted rather
  than sold.
- What is **not** caught: reads. `state.*`, `memory.recall`, `memory.search`,
  `memory.get_context_pack`, the other read tools and `actions.status` are free by invariant 10
  (`apps/api/src/pricing.ts` `FREE_TOOLS`), never reserve against the wallet, and cannot produce a
  `402`.

## 3. Options

### Option A — A free, read-only toolset for the ChatGPT client (recommended)

The ChatGPT listing exposes only the free tools: the 18 core reads (`memory.search`,
`memory.get_context`, `memory.get_context_pack`, `memory.get_events`, `memory.read_session`,
`memory.get_wrap`, `memory.get_periods`, `memory.recall`, `calendar.upcoming`, `profile.whoami`,
`state.commitments.list`, `state.open_loops.list`, `state.people.list`, `state.projects.list`,
`state.people.get`, `state.projects.get`, `wiki.evidence`, `actions.status`). No tool in it is
metered, so no `402` and no purchase link can ever appear in a ChatGPT conversation. Billing,
writes and runs stay where they are for agents outside ChatGPT, and account management (balance,
top-ups, statements) is a plain link to our own site in the listing's description, never in a tool
result.

How it would be built, within the invariants:

- **Recognise the client at OAuth time.** ChatGPT registers through a Client ID Metadata Document,
  so its `client_id` is a URL we can match (`apps/api/src/oauth/`; `oauth_tokens.client_id` is
  already stored). A token minted for that client carries a toolset marker (e.g. `free-reads`);
  OAuth tokens already carry scopes, and this client would get `read` only, which is the cleanest
  form of the marker.
- **List narrowly in the core.** The sidecar already narrows `tools/list` by toolset
  (`SHOGUN_MCP_TOOLSETS`, `crates/shogun-mcp`; group ids are a listing convenience, not a wire
  contract — `docs/api/deprecation-policy.md`). The gateway would pass the marker as a header so the
  core lists only the free group for that session. The gateway itself never parses `tools/list`
  bodies (invariants 1 and 2).
- **Refuse by header at the gateway.** Any `tools/call` whose `Mcp-Name` is not in `FREE_TOOLS` from
  a `free-reads` token is refused before reserving, with a plain-English error that names no price
  and no link (`not_allowed`). The gateway already classifies calls from `Mcp-Method` / `Mcp-Name`
  without reading bodies (`classifyCall`), so this is the same mechanism.
- **Same behaviour on every face** (invariant 4): the tools that are listed behave exactly as on REST
  and CLI; nothing is renamed and no tool changes level. Only the listed set is narrower for one
  client, as `SHOGUN_MCP_TOOLSETS` already allows.

Cost to us: one OAuth client rule, one header to the core, one refusal path, tests. The
"each public tool works on its own" requirement holds (every read takes its own inputs;
`actions.status` needs an `approval_id`, which a reader may simply not have — document it, or leave
it out of the ChatGPT set).

Trade-off: ChatGPT users cannot write notes or queue L3 actions from ChatGPT. The state they read is
built by their other agents and the ShogunAI Mac app, which is how the product is positioned anyway
("many agents read and write"; ChatGPT becomes one more reader). Example prompts 4 and 5 in
`docs/directories/openai.md` must be replaced by read prompts.

### Option B — A separate listing whose writes are funded only by included allowances

The ChatGPT client gets reads plus writes, but its wallet can only be funded by grants that are not
sold in ChatGPT: the signup allowance and the ShogunAI Pro bundle. For that client the `402` variant
carries no `accepts` rail and no URL (`next_action: "not_allowed"`, "This account's included usage is
used up for this month"), and `wallet.low_balance` carries no `topup_url`. A user who wants more buys
a ShogunAI plan on our website, outside ChatGPT, through no link in a tool result.

Risks: it may still read as selling a subscription that unlocks a digital service, which the quoted
rule names explicitly; the allowance runs out mid-task, which a reviewer may experience as a "trial
or demo" limitation; and it adds a second wallet behaviour that every billing test must cover. Worth
doing only if OpenAI confirms in writing that access tied to a subscription bought elsewhere is
allowed.

### Option C — Ask first, list elsewhere meanwhile

Submit to the Anthropic directory, the MCP Registry and Smithery first (no comparable rule is
recorded for them), and ask OpenAI through the developer portal whether usage-billed API access,
paid on our own domain and never offered inside ChatGPT, counts as selling credits. Decide A or B
on the answer. No engineering until then, but no ChatGPT listing either, and the answer is not
guaranteed to be specific.

## 4. Recommendation

**Option A**, with the Option C question asked in parallel.

- It is compliant on its face: no tool in the listing can charge, so there is nothing to sell and
  nothing to argue about with a reviewer, whatever the exact current wording on digital goods.
- It follows from rules we already hold: reads are free (invariant 10), toolsets narrow the list
  without changing behaviour, and the gateway refuses by header without reading bodies.
- It does not fork billing; the wallet, `402` and receipts stay one implementation.
- If OpenAI later confirms Option B is acceptable, widening the toolset is a configuration change,
  not a redesign.

Owner decisions needed to proceed: choose the option; if A, whether `actions.status` stays in the
ChatGPT set; and who asks OpenAI the Option C question, under which account.

## 5. What Option A adds to the submission checklist

- [ ] OAuth client rule for the ChatGPT CIMD `client_id` (confirm ChatGPT uses CIMD, not DCR — we
      have no DCR endpoint), minting `read`-scope tokens only.
- [ ] Core lists only the free group for that client; gateway refuses non-free `Mcp-Name`s from it
      before reserving, with no price or link in the error.
- [ ] Tests: a ChatGPT-client token sees exactly the free tools in `tools/list`, a metered call is
      refused without a `402`, and no response body for that client contains a `topup_url`.
- [ ] Listing description says reads are free and links account management on our site.
- [ ] `docs/directories/openai.md` example prompts replaced with read-only ones.
