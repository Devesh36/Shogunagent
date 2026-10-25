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
import { InMemoryRunLogSink } from "./runlog.js";
import { createFetchTransport } from "./transport.js";

type App = ReturnType<typeof createApp>;
let cached: { env: RouterEnv; app: App } | undefined;

function appFor(env: RouterEnv): App {
  if (cached && cached.env === env) return cached.app;
  const catalog = compileCatalog(allProviders());
  const app = createApp({
    catalog,
    transport: createFetchTransport({ env: env as Record<string, string | undefined> }),
    metering: new InMemoryMeteringClient({ topUpUrl: env.ROUTER_TOPUP_URL ?? DEFAULTS.ROUTER_TOPUP_URL }),
    runLog: new InMemoryRunLogSink(),
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
