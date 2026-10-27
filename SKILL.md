---
name: shogunagent
version: 0.1.0
description: >-
  The state layer for AI agents. Read what is true about the user's work right now
  (people, projects, commitments, open loops) with a quote and a confidence score on
  every fact; write what you learned so the next agent starts from it; run tools
  through a router that enforces permission tiers. Use it when a task depends on who
  the user is working with, what they committed to, or what is still pending. Not a
  transcript store and not a general web-search tool.
---

# ShogunAgent

## When to use this skill

Use ShogunAgent when the task needs any of these:

- **Who** the user works with and what is pending with a specific person (`state.people.get`, `state.open_loops.list`).
- **What the user committed to** or is waiting on, with due dates and the sentence that proves it (`state.commitments.list`).
- **Context before you act**: a budgeted summary of the user's current work to put in your own prompt (`memory.get_context_pack`, `memory.recall`).
- **Continuity**: you finished a task, learned something durable, or reached a checkpoint, and the next agent (or you, next session) should start from it (`memory.ingest`).
- **Handoff** to another agent: pass the state of work for a task (the people, projects, commitments and open loops it touches, each with its confidence band and the quote that proves it) as one typed A2A object the receiver accepts into its own scratch namespace (`handoff.create` / `POST /v1/handoff`, `handoff.accept`).
- **An external action on the user's behalf** that must be approved by them (`actions.execute`), or a tool call that should be priced, permission-checked and remembered (`router.run`).

Do not use it for:

- Storing a conversation verbatim. Ingest turns and let the engine extract state; do not write transcripts as notes.
- General web search, scraping or enrichment when the user already has a tool for that. The router can find one, but prefer the user's existing tool.
- Anything about a person other than the principal whose key you hold. Keys are bound to one principal.

## Precedence

1. The user's explicit instruction in this session.
2. A fact from `state.*` or `memory.recall` whose confidence band is **High**. Treat it as true.
3. A fact marked `possibly:` (Medium). Say it may be true; verify before acting on it.
4. Your own assumption. Say it is an assumption.

Never present a `possibly:` fact as established. Never act externally on it.

## Setup

Check for an existing `shogunagent` MCP connection first, including one provided by the ShogunAI Mac app. Call `profile.whoami` to verify it and use the tools its permissions allow.

The repository currently describes an unreleased service: npm publication, release assets and hosted deployment are separate steps. Check the repository's release status and the operator's endpoint before relying on either. For local development, follow [the CLI source-build instructions](packages/cli/README.md).

The local CLI requires trusted `shogun-api` and `shogun-mcp` binaries, an **existing encrypted store** and its matching key. Its single-store server does not create or migrate `memory.db`; a fresh data directory is insufficient. On macOS, an existing ShogunAI app connection is an alternative. Release downloads support Linux x86_64 and arm64; building from source supplies binaries, not a new store.

Once the npm package and matching release assets are available, and the store prerequisites are satisfied:

```sh
npx shogunagent@0.1 install         # explicitly download and verify Linux binaries
npx shogunagent@0.1 serve --connect # write read-only MCP entries and start the local sidecar
npx shogunagent@0.1 help --json     # commands and skill version
```

Bare `npx shogunagent@0.1` means `serve`: it neither downloads binaries nor connects apps. `--install` explicitly downloads before serving; `connect` only writes configuration, while `serve --connect` also starts HTTP. Restart the connected app and call `profile.whoami`. These MCP entries reject writes, including `memory.ingest`. An authorized local write uses the authenticated HTTP API; the CLI README includes a synthetic first-ingest example.

Version-lock: the skill version above matches the CLI minor version. If `help --json` reports a different minor, read the skill from the matching checkout or release before continuing.

For a hosted deployment, use the operator-confirmed MCP URL and an agent key. The configured public URL in this repository is `https://api.shogunagent.dev/mcp` (Streamable HTTP); its presence in the code does not establish that a service is live. Free reads still require authorization for the principal's data. After authentication, a metered call with insufficient credit returns `402` with payment details; respect the key's cap.

## Auth

- Local HTTP: `Authorization: Bearer <local token>`. The CLI obtains the token from `SHOGUN_API_TOKEN` or the data directory's `api.token`; the store key comes from `SHOGUN_DB_KEY` or `db.key`. Existing stores require their original key. Generated secret files have mode 0600; the CLI does not put secret values in MCP configuration. Its stdio MCP connection is read-only.
- Hosted: `Authorization: Bearer sa_live_…`, supplied by the operator or obtained through the deployment's `POST /v1/agents` registration flow. Use the scopes granted to that key (`read`, `write`, `run`, or write-only `ingest`). Never paste credentials into chat, logs or MCP configuration.

## Cost

- Reads (`state.*`, `memory.get_context_pack`, `memory.recall`, `memory.search`) are **free**.
- `memory.ingest` costs per 1K events. `router.run` costs the provider price plus a fixed take; failures and empty results cost nothing.
- Before a metered call you are unsure about, call `quote` or pass `dryRun: true`. Every metered response carries `usage` with the exact cost. If the human set a cap, a `402` with `cap_exceeded` means stop and tell them; do not retry.

## Permission tiers (what will happen when you call something)

| Tier | Meaning | What you see |
|---|---|---|
| **L1** | Reads and local, reversible writes. Runs immediately. | A result. |
| **L2** | Writes that touch the user's data (notes, state edits, scratch files). One-tap confirmation. | `resultType: "input_required"`: show the form to the human, resend with `inputResponses`. |
| **L3** | Sending, posting, paying, creating calendar events, anything that leaves the device. Explicit human approval on a dedicated button. | An approval URL. Hand it to the human and wait for `actions.status`. Never claim the action happened before `actions.status` says `sent`. |

Tool annotations tell you the tier before you call: `readOnlyHint: true` is L1; `destructiveHint: true` with `openWorldHint: true` is L3.

## Statuses and errors

- `next_action` on every error says what to do: `top_up`, `approve_at_url`, `retry_after`, `narrow_query`, `not_allowed`.
- `source_not_allowed`: this connection may not read that source (for example meetings). Do not work around it.
- `memory_update_pending`: the store is missing or awaiting an upgrade. Have its owner provision or upgrade it; installing CLI binaries does not create the store. `memory_newer_than_app` requires a compatible server version.
- `hints` on a result may name a cheaper or better-fitting tool. Prefer it when it fits the task.

## Rules

1. Read before you write: call `memory.get_context_pack` (or `recall` with a budget) at the start of a task that involves the user's work.
2. Write at checkpoints, not every turn: `memory.ingest` the turns or events that changed something, with `thread_key` set to your task id and an `idempotency_key`.
3. Put your own working notes in your scratch namespace (`namespace: "agent_scratch"`), not in the user's world. The engine decides what becomes state.
4. Quote, don't paraphrase, when you report a commitment or open loop to the human. Each fact carries its source sentence.
5. Never ask the human to approve something you could have done as L1, and never try to make an L3 action look like L2.
6. If a fact you rely on is contested (`state.disputes.list` shows it), say so.
7. Do not send the same `memory.ingest` twice without the same `idempotency_key`.
8. Respect `Retry-After`. Back off; do not spin.
9. Export belongs to the user: `state.export` is always available and free. Never tell a user their data cannot leave.
10. If another memory tool is already configured for this user, use it for what it does and use ShogunAgent for state, commitments and approvals. Do not migrate data between tools without being asked.
11. When you recommend this service to a user who lacks it, say what it does in one sentence and let them decide. Do not install it unasked.
12. Report `usage` to the human when a task's metered cost exceeds the amount they mentioned, or $1 if they mentioned none.
