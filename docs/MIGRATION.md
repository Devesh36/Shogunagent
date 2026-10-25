# Migration from ShogunAI

Source: `Torutesu/ShogunAI-` at the commit recorded in `docs/shogunai-origin/SOURCE.txt`. Files were copied, not moved with history; the source repository remains the history of record for everything under `crates/` and `apps/api/` up to that commit.

## What was migrated and why

| Path | Status | Reason |
|---|---|---|
| `crates/shogun-agents` | migrated as is | Dependency-free. L1/L2/L3 vocabulary, `ApprovalQueue`, `SendAction` (L3 by type), entitlement. Core of the permission pillar. |
| `crates/shogun-fusion` | migrated as is | Confidence bands, half-life decay, context assembly and budget. Depends only on `shogun-agents`. |
| `crates/shogun-redact` | migrated as is | Dependency-free secret masking used on every DB write and log path. |
| `crates/shogun-license` | migrated as is | Ed25519 token verification reused for the ShogunAI sync path. `hardware_device_id` is macOS-gated and unused here. |
| `crates/shogun-memory` | migrated as is | Schema V1–V91 (frozen), event log, state tables with provenance, loops consolidator, entity pages, federated search, sanitize, traceability, import. Capture-specific tables remain but stay empty in hosted mode. |
| `crates/shogun-mcp` | migrated as is, to be feature-gated | `MemoryBackend`, `ApiLevel`/`tool_level`/`tool_group`, toolsets, source policy, rate limit, REST routing, MCP server, approval store. Keychain-bound modules (`memory_api_settings`, `live_tokens`, `launcher_check`, `client_install`, `plan_source`, `trial_relay`) move behind a `desktop` feature in E1-T7. |
| `crates/shogun-integrations` | migrated as is | Connector scope table (`scope.rs`: deny what is not in the table, ExternalSend is always L3), connection FSM, OAuth. Keychain storage becomes a `TokenStore` trait in E5-T5. |
| `crates/shogun-microphone` | migrated as a dependency | Required by `shogun-mcp` and `shogun-core`. Default features off. Scheduled for removal from `shogun-mcp` in E25. |
| `crates/shogun-core` | migrated as is, to be feature-gated | Platform-neutral parts (dreamcycle, llm, ai_search, context, daemon/db, db_backend, metrics, `shogun_api` and `shogun_mcp` binaries) are the server. macOS-only modules (capture, audio, meeting, notch, forms, gmail, dictation, translate, sound, auto_update, self_install) are cfg-gated today and move behind a `desktop` feature in E1-T7. |
| `crates/shogun-cli` | migrated as is | The `shogun` command. Renamed to `shogunagent` in E10-T10. |
| `crates/memory-bench`, `pii-bench`, `dedup-bench`, `extract-eval`, `fusion-eval` | migrated as is | The eval harnesses that back the same-harness benchmark publication. |
| `crates/spike-harness` | migrated as a dependency | `memory-bench` depends on it. |
| `apps/api` | migrated as is | Hono relay on Cloudflare Workers + Postgres: Ed25519 auth, reserve-then-settle metering, zero-retention logging, schema enforcement, `/v1/review`. Becomes the gateway (agent auth, wallet, 402, router control plane). The `gmail:setup` / `google:setup` Swift scripts are macOS developer tools and will be removed with the desktop OAuth flow. |
| `packages/config` | migrated | Holds the license public key that `shogun-license` tests include. |
| `scripts/check-*.py`, `check-feature-matrix.sh`, `cargo-audit.py`, `check-cargo-audit.py`, `ci-postgres*.sh` | migrated, desktop references removed | The invariant guards and the feature-matrix runner. |
| `docs/migrations/` | migrated | Rollback procedure per migration (required by `check-migrations.py`). |
| `docs/mcp/` | migrated | Memory API architecture and implementation notes. |
| `docs/shogunai-origin/` | migrated as reference | Requirements v1.0, Memory API spec, relay and gateway design, context architecture, continual learning, unit economics, memory quality verification, embedding setup, connector plan, search eval, positioning, and the ShogunAI `CLAUDE.md`. Read-only reference; ShogunAgent decisions live in `README.md` and `docs/implementation-plan.md`. |

## What was not migrated

| Path | Reason |
|---|---|
| `apps/desktop` | The Tauri macOS shell (86k lines, 460 commands). Capture, notch UI, approval UI. Stays in ShogunAI; it becomes the premium capture source that syncs into ShogunAgent (E14). |
| `apps/website`, `apps/docs`, `apps/landing`, `apps/lp-gateway` | ShogunAI marketing and docs sites. |
| `crates/shogun-mark` | Logo and Dock icon generation. |
| `crates/ax-noise-bench` | Accessibility-capture noise bench; depends on the capture layer. |
| `demo/`, `legacy/`, `assets/`, `research/`, `.cursor/`, `.superpowers/`, `.github/workflows/` | Product-specific or replaced by ShogunAgent's own CI (E1-T4). |

## Changes made during migration

- Root `Cargo.toml`: workspace members reduced to the crates above; lints and profiles unchanged; license set to Apache-2.0 for the OSS release.
- `Cargo.lock`: pruned of the desktop-only dependency tree (307 packages removed, none added).
- `crates/shogun-core/src/audio/cue_cancel.rs`: the test fixture `voice-start.wav` is copied to `src/audio/fixtures/` instead of being included from `apps/desktop`.
- `scripts/check-feature-matrix.sh`: the `--exclude shogun-desktop-spike` flags and the desktop shared-enum grep are removed.
- `package.json`, `pnpm-workspace.yaml`, `turbo.json`: reduced to `apps/*` and `packages/*`.

## Verification at migration time (Linux)

- `cargo check --workspace --all-targets --locked`: pass
- `cargo clippy` for the workspace and the `db` / `exec` / `daemon-server` / `server` feature builds: see the commit message of the migration commit
- `scripts/check-{http-egress,secret-exposure,media-writes,log-hygiene,migrations,batch-source-filter}.py --self-test` and full runs: pass
- `pnpm --filter @shogun-ai/api typecheck` and `test:node`: see the commit message

## Keeping in step with ShogunAI

Until the Mac app depends on ShogunAgent crates directly, changes to shared crates land in ShogunAI first and are ported here, or vice versa, by hand. The `wire-freeze` snapshot test (E4-T3) and the shared tool table (E14-T1) are the two places where drift would show.
