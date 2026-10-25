# Changelog — @shogunagent/sdk

Format and rules: `docs/release.md` (Keep a Changelog sections, SemVer, newest first). The
`## <version>` heading must match `package.json` and `SDK_VERSION`; `scripts/check-release-versions.mjs`
fails the publish otherwise.

## Unreleased

### Changed

- `shogunAgentMiddleware()` is assignable to the AI SDK's `LanguageModelMiddleware` under `strict`
  and `exactOptionalPropertyTypes`, and `withShogunAgent(model)` returns the model's own type, so
  both work with `wrapLanguageModel` / `generateText` without casts (found by the real-framework
  integration tests, E36b).

## 0.1.0 — not yet published

### Added

- First release candidate: the client (recall, ingest, state reads, context packs, actions,
  router, quotes, typed A2A handoffs) and `withShogunAgent()` for the Vercel AI SDK, OpenAI
  Agents SDK, Claude Agent SDK, Mastra and LangGraph.js.
