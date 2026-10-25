# What is open, what is hosted

ShogunAgent ships the engine as open source under Apache-2.0 and keeps the hosted service closed. The line is drawn so that the local product is real (same engine, same API, same quality as hosted) and the hosted product is paid for what only a hosted service can do: sync, remote approvals, a wallet, and a shared router catalog.

## Open (this repository, Apache-2.0)

| Area | Contents |
|---|---|
| Engine | `crates/shogun-memory` (schema, event log, state tables, provenance, loops consolidator, entity pages, search), `crates/shogun-fusion` (confidence bands, context assembly, budget), `crates/shogun-agents` (L1/L2/L3, approval queue), `crates/shogun-redact`, the platform-neutral parts of `crates/shogun-core` (Dream Cycle, LLM clients, ai_search, context, the `shogun_api` / `shogun_mcp` binaries) |
| Faces | MCP server, REST routing and CLI in `crates/shogun-mcp` and `crates/shogun-cli` |
| Router engine | `apps/router`: connector format, catalog compiler, `discover` / `inspect` / `run`, scope table, first-party connectors |
| Agent surfaces | `SKILL.md`, `server.json`, well-known document generators, `llms.txt` generation, the export format and its JSON Schema |
| Evals | `crates/memory-bench`, `pii-bench`, `dedup-bench`, `extract-eval`, `fusion-eval`, `selection-eval`, the open-loop tracking benchmark |
| Guards and CI | `scripts/check-*.py`, `check-feature-matrix.sh`, the GitHub Actions workflows |

Running locally: one principal, one SQLCipher file, the full tool set over local MCP / REST / CLI, extraction through your own model key or the ShogunAI Mac app's relay. No telemetry unless you opt in.

## Hosted (closed)

| Area | Why it is hosted-only |
|---|---|
| Sync between devices and agents | Needs a server and user-held-key encryption at rest |
| Remote L2/L3 approvals (web, Slack, webhooks) | Needs a server that can reach the person |
| Wallet, 402, caps, Stripe, receipts, disputes | Payment rails and liability |
| Hosted router catalog, provider contracts, Monid passthrough credentials, provider payouts | Contracts and keys |
| Tenant KMS, BYOC deployment tooling, SSO/SCIM, audit export | Enterprise operations |

The gateway code that implements these (`apps/api`) is in this repository because its auth, metering and zero-retention logging are part of the engine's contract and are tested here; the deployed service, its secrets and its provider agreements are not.

## The funnel

The same API runs locally and hosted; a client switches by changing `baseURL`. The engine is not a demo of the hosted product. If a capability exists in hosted and could run locally, it runs locally.

## Contributions

Connector definitions under `apps/router/connectors/` are the lowest-friction contribution (see `CONTRIBUTING.md`). Engine changes follow `CLAUDE.md`: additive migrations with rollback notes, clippy deny, no `unwrap`, every face in step, provenance and actor on every write.
