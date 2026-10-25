/**
 * Cloudflare Workers entry. Builds the production dependency set from the Worker environment:
 * the compiled catalog (first-party + Monid passthrough), the fetch transport over the env map
 * (the only place `env` secrets are read), and — until E9 wires the wallet and the state layer
 * wires `tool_runs` — in-memory metering, idempotency, health and run-log stubs.
 *
 * NOTE: a Worker isolate's memory is per-isolate and short-lived, so the in-memory stores give
 * no cross-request guarantee in production. That is deliberate for the E8 skeleton: wrangler.jsonc
 * binds no route (`workers_dev: false`), so nothing serves real traffic from these stubs.
 */
import { allProviders } from "../connectors/index.js";
import { createApp } from "./app.js";
import { compileCatalog } from "./catalog.js";
import type { RouterEnv } from "./env.js";
import { DEFAULTS } from "./env.js";
import { InMemoryHealthStore } from "./health.js";
import { InMemoryIdempotencyStore } from "./idempotency.js";
import { consoleLogger } from "./logging.js";
import { InMemoryMeteringClient } from "./metering.js";
import { InMemoryPersonalStore, PersonalRanker } from "./personal.js";
import { InMemoryRunLogSink } from "./runlog.js";
import { HttpSettlementSink } from "./settlement.js";
import { createFetchTransport, createSettlementPoster } from "./transport.js";

type App = ReturnType<typeof createApp>;
let cached: { env: RouterEnv; app: App } | undefined;

function appFor(env: RouterEnv): App {
  if (cached && cached.env === env) return cached.app;
  const catalog = compileCatalog(allProviders());
  // E20: per-isolate stub, like the other stores. Production: the principal's tool_runs (V94) via the sidecar.
  const personal = new PersonalRanker(new InMemoryPersonalStore(), catalog.index);
  const credentials = env as Record<string, string | undefined>;
  // E29: every settled run is forwarded to apps/api's provider ledger when the URL is set. The
  // token is read by the transport module at send time, never here.
  const settlements = env.ROUTER_SETTLEMENT_URL
    ? new HttpSettlementSink(createSettlementPoster({ env: credentials, url: env.ROUTER_SETTLEMENT_URL }))
    : undefined;
  const app = createApp({
    catalog,
    transport: createFetchTransport({ env: credentials }),
    metering: new InMemoryMeteringClient({ topUpUrl: env.ROUTER_TOPUP_URL ?? DEFAULTS.ROUTER_TOPUP_URL, ...(settlements ? { settlements } : {}) }),
    runLog: new InMemoryRunLogSink(),
    personal,
    idempotency: new InMemoryIdempotencyStore(),
    health: new InMemoryHealthStore(),
    logger: consoleLogger,
    approvalBaseUrl: env.APPROVAL_BASE_URL ?? DEFAULTS.APPROVAL_BASE_URL,
  });
  consoleLogger.log({ kind: "catalog", count: catalog.manifest.entry_count });
  cached = { env, app };
  return app;
}

export default {
  fetch(request: Request, env: RouterEnv): Response | Promise<Response> {
    return appFor(env).fetch(request);
  },
};
