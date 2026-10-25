# Changelog — @shogunagent/sdk

Format and rules: `docs/release.md` (Keep a Changelog sections, SemVer, newest first). The
`## <version>` heading must match `package.json` and `SDK_VERSION`; `scripts/check-release-versions.mjs`
fails the publish otherwise.

## Unreleased

### Changed

- `acceptHandoff()` calls the core's `handoff.accept` (E35) instead of assembling `memory.ingest`
  calls itself: the core validates the handoff again and stores it in your `agent_scratch`, with
  `Mcp-Quantity` set to the events it will store. It returns `{ handoff, accepted, envelope }`
  (`accepted` is the `AcceptHandoffResult` receipt) in place of `{ handoff, ingested }`.
  `handoff()` keeps its route and shape; the hosted gateway now relays it to the core.
- `shogunAgentMiddleware()` is assignable to the AI SDK's `LanguageModelMiddleware` under `strict`
  and `exactOptionalPropertyTypes`, and `withShogunAgent(model)` returns the model's own type, so
  both work with `wrapLanguageModel` / `generateText` without casts (found by the real-framework
  integration tests, E36b).

## 0.1.0 — not yet published

### Added

- First release candidate: the client (recall, ingest, state reads, context packs, actions,
  router, quotes, typed A2A handoffs) and `withShogunAgent()` for the Vercel AI SDK, OpenAI
  Agents SDK, Claude Agent SDK, Mastra and LangGraph.js.
