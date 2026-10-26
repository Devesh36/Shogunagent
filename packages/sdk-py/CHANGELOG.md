# Changelog — shogunagent (PyPI)

Format and rules: `docs/release.md` (Keep a Changelog sections, SemVer, newest first). The
`## <version>` heading must match `pyproject.toml` and `SDK_VERSION`;
`scripts/check-release-versions.mjs` fails the publish otherwise.

## Unreleased

## 0.2.0 — 2026-10-26

### Added

- `client.payments` (docs/api/payments.md), on the sync and the async client:
  `list_mandates(mandate_id=None, limit=None)`, `request_mandate(...)` (a `PaymentOutcome` of type
  `approval_required` with the URL to hand to the human, or `narrowed` / `replayed`),
  `revoke_mandate(mandate_id)` and `spend(payee=..., currency=..., amount_minor=..., memo=None,
  mandate_id=None, idempotency_key=None)` (`settled`, `approval_required` with `spend_ref` and
  `reason`, or `not_settled` for a replay that is awaiting a human, rejected, expired or failed).
  An idempotency key is generated when you pass none and returned as `idempotency_key`: reuse it
  to retry, and a retry never pays twice. The SDK never approves anything.
- `client.connections.status()` (`connections.list`, free) and `client.connections.disconnect(service)`.
- `PaymentOutcome` is exported.

### Changed

- `accept_handoff()` calls the core's `handoff.accept` (E35) instead of assembling `memory.ingest`
  calls itself, with `Mcp-Quantity` set to the events the core will store. It returns
  `{"handoff", "accepted", "envelope"}` in place of `{"handoff", "ingested"}`.

### Fixed

- LangGraph adapter: the wrapped node's `config` parameter is annotated `Optional[RunnableConfig]`,
  so LangGraph passes the run config and turns are ingested under the graph's `thread_id`. With the
  previous `Any` annotation LangGraph withheld the config and every thread was ingested under
  `langgraph:default` (found by the real-framework integration tests, E36b).

### Added

- `integration` extra and `tests/integration`: the adapters inside real LangGraph and OpenAI Agents
  SDK runs, offline.

## 0.1.0 — not yet published

### Added

- First release candidate: the sync and async clients and `with_shogun_agent()` for the OpenAI
  Agents SDK, Claude Agent SDK, LangGraph and CrewAI.
