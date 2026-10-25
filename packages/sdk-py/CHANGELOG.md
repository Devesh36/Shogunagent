# Changelog — shogunagent (PyPI)

Format and rules: `docs/release.md` (Keep a Changelog sections, SemVer, newest first). The
`## <version>` heading must match `pyproject.toml` and `SDK_VERSION`;
`scripts/check-release-versions.mjs` fails the publish otherwise.

## Unreleased

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
