# Changelog — @shogunagent/sdk

Format and rules: `docs/release.md` (Keep a Changelog sections, SemVer, newest first). The
`## <version>` heading must match `package.json` and `SDK_VERSION`; `scripts/check-release-versions.mjs`
fails the publish otherwise.

## Unreleased

## 0.2.0 — 2026-10-26

### Added

- `client.payments` (docs/api/payments.md): `mandates.list({ mandateId?, limit? })`,
  `mandates.request(terms)` (answers `approval_required` with the URL to hand to the human, or
  `narrowed` / `replayed`), `mandates.revoke(mandateId)` and `spend({ payee, currency, amountMinor,
  memo?, mandateId?, idempotencyKey? })` (answers `settled`, `approval_required` with `spendRef` and
  `reason`, or `not_settled` for a replay that is awaiting a human, rejected, expired or failed). An
  idempotency key is generated when you pass none and returned as `idempotencyKey`: reuse it to
  retry, and a retry never pays twice. The SDK never approves anything: a mandate or an
  out-of-mandate payment waits for a human at the approval URL.
- `client.connections.status()` (`connections.list`, free) and `client.connections.disconnect(service)`.
- Exported types `PendingApproval`, `MandateRequestOutcome`, `SpendOutcome`, `PaymentMandate`,
  `PaymentSpend`, `SpendStatus` and `ConnectionsStatus`.

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
