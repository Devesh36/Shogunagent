# Service levels and load tests

Plan E37. Target SLOs for the hosted service, how they are measured, and the latest local load
run. The scripts are in `bench/load/` (see its README); they run against a server you start, never
in CI.

## Target SLOs

Measured at the gateway (server-side, `shogun_gateway_http_request_duration_ms` in `/metrics`),
over 28 days, excluding requests refused for the caller's own reasons (4xx other than 429).

| Surface | Availability | Latency target (server-side) |
|---|---|---|
| Gateway auth / 402 refusal (`/mcp` before the core) | 99.9% | p95 < 50 ms |
| Free reads through the gateway (`state.*`, `memory.recall`, `memory.search`) | 99.9% | p95 < 300 ms, p99 < 800 ms |
| `memory.ingest` | 99.9% | p95 < 500 ms |
| Model-invoking tools, router `run` | 99.5% | first byte p95 < 1 s (provider time excluded from the budget, reported separately) |
| Agent-readable documents (`/.well-known/*`, `/llms.txt`) | 99.95% | p95 < 50 ms |
| Sidecar recall (one principal, warm store) | — | p95 < 500 ms (the local search SLO) |
| Sidecar ingest (one principal) | — | p95 < 300 ms at ≤ 4 concurrent writers |

Error budget alerts: 5xx rate > 1% over 5 minutes, or p95 over target for 15 minutes, per route
template. Both come from the closed-set labels (`route`, `method`, `status`, `mcp_method`,
`mcp_name`); nothing else is available, by design.

## Latest local run

**Hardware caveat.** These numbers come from a shared 4-vCPU Linux container (Intel Xeon
@ 2.10 GHz, 16 GB, Node 22.22), with other build jobs running on the same machine, load generator
and server on the same host. They show relative cost and catch regressions; they are not
production capacity. Re-run on the target hardware before quoting them.

Date: 2026-10-25. Commit: the E37 branch on top of ae8d067.

### Gateway (`node bench/load/gateway.mjs --self-host --duration 10 --concurrency 16`)

In-memory store, fake core answering after 5 ms, Node entry (`@hono/node-server`).

| Scenario | What it exercises | Requests | rps | p50 ms | p95 ms | p99 ms |
|---|---|---|---|---|---|---|
| `auth` | unknown key → 401 | 40,195 | 4,019 | 3.2 | 7.8 | 11.5 |
| `pay402` | capped key, `memory.ingest` → 402 | 40,332 | 4,033 | 3.3 | 8.0 | 10.6 |
| `read` | `memory.search` (free) proxied to the core → 200 | 20,336 | 2,033 | 7.2 | 11.9 | 15.6 |
| `discover` | `GET /.well-known/pricing.json` → 200 | 60,945 | 6,094 | 1.8 | 6.8 | 10.3 |

All responses had the expected status; no network errors.

### Sidecar (`node bench/load/sidecar.mjs --self-host --bin <shogun-api> --duration 10 --concurrency 8`)

`shogun-api --multi` with the `env` key provider, one principal, SQLCipher on local disk. Binary
built with `cargo build -p shogun-core --features daemon-server --bin shogun-api` at opt-level 2
without debug info. Recall runs after ingest, over the ~6,000 events it wrote.

| Scenario | What it exercises | Requests | rps | p50 ms | p95 ms | p99 ms |
|---|---|---|---|---|---|---|
| `ingest` | 5 events per call, fresh idempotency key | 1,245 | 122 | 35.4 | 195.9 | 261.8 |
| `recall` | `budget_tokens: 800` | 424 | 42 | 187.4 | 312.9 | 377.4 |
| `status` | liveness, no store work | 71,171 | 7,117 | 0.9 | 2.2 | 5.0 |

Reading: one principal's writes serialise on its file, so ingest p95 at 8 concurrent writers is
over the 4-writer target; that is threat-model risk R7 (per-principal write queue). Recall meets
the 500 ms local search SLO at this store size.

## How to reproduce

```bash
pnpm install
node bench/load/gateway.mjs --self-host
cargo build -p shogun-core --features daemon-server --bin shogun-api
node bench/load/sidecar.mjs --self-host --bin target/debug/shogun-api
```

Paste the JSON lines into the tables above with the machine line and the date.
