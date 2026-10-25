# Load tests (plan E37)

Manual load tests for the gateway (`apps/api`) and the sidecar (`shogun-api --multi`). No
dependencies beyond Node ≥ 22 (`fetch`) and, for the self-hosted gateway, the `apps/api` dev
dependencies (`pnpm install`). Never run in CI; never point them at production.

| Script | Scenarios | Self-host |
|---|---|---|
| `gateway.mjs` | `auth` (401), `pay402` (402), `read` (free read proxied to the core), `discover` (public document) | `apps/api/scripts/local-gateway.ts`: in-memory store, fake core, tiny caps, throwaway keys |
| `sidecar.mjs` | `ingest`, `recall`, `status` | the `shogun-api` binary you pass with `--bin`, a temp principal root, the `env` key provider, a random key and token |

Common flags: `--concurrency N` (closed loop), `--duration S`, `--scenarios a,b`. Each scenario
prints one JSON line (statuses, rps, p50/p95/p99/max ms); the last line describes the machine.
The exit code is non-zero if any response had an unexpected status.

```bash
node bench/load/gateway.mjs --self-host
node bench/load/gateway.mjs --url https://staging.example --keys-file keys.txt
cargo build -p shogun-core --features daemon-server --bin shogun-api
node bench/load/sidecar.mjs --self-host --bin target/debug/shogun-api
node bench/load/sidecar.mjs --url http://127.0.0.1:7464 --token "$TOKEN"   # writes events!
```

Nothing about bodies, keys or responses is printed. Record runs in `docs/slo.md` with the
hardware caveat. The older Batch-lane load test is `apps/api/scripts/load-test.ts`.
