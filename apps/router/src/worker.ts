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
import { OpsMetrics, OtlpTraceExporter, otlpSettingsFromEnv, type RouterOps } from "./observability.js";
import { InMemoryMeteringClient } from "./metering.js";
import { InMemoryPersonalStore, PersonalRanker } from "./personal.js";
import { InMemoryRunLogSink } from "./runlog.js";
import { HttpSettlementSink } from "./settlement.js";
import { createFetchTransport, createMetricsAuthorizer, createOtlpPoster, createSettlementPoster, parseOtlpHeaders } from "./transport.js";

type App = ReturnType<typeof createApp>;
let cached: { env: RouterEnv; app: App; ops: RouterOps } | undefined;

/** E37: the isolate's metrics window, the `/metrics` bearer check and, when configured, the span
 * exporter. A malformed OTLP setting turns export off (logged once by name) rather than failing
 * the router: telemetry is never a reason to refuse traffic. */
function opsFor(env: RouterEnv, credentials: Record<string, string | undefined>): RouterOps {
  const metrics = new OpsMetrics();
  const ops: RouterOps = { metrics, authorize: createMetricsAuthorizer(credentials) };
  try {
    const settings = otlpSettingsFromEnv(env as Record<string, string | undefined>);
    if (settings) {
      parseOtlpHeaders(credentials["OTEL_EXPORTER_OTLP_HEADERS"]);
      ops.exporter = new OtlpTraceExporter(settings, createOtlpPoster({ env: credentials, url: settings.tracesUrl }), metrics);
      consoleLogger.log({ kind: "catalog", outcome: "otlp_export_enabled" });
    }
  } catch {
    consoleLogger.log({ kind: "catalog", outcome: "otlp_config_invalid" });
  }
  return ops;
}

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
  const ops = opsFor(env, credentials);
  const app = createApp({
    ops,
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
  cached = { env, app, ops };
  return app;
}

/** The parts of the Workers `ExecutionContext` the router uses. */
interface WorkerContext {
  waitUntil(promise: Promise<unknown>): void;
  passThroughOnException(): void;
}

export default {
  fetch(request: Request, env: RouterEnv, ctx?: WorkerContext): Response | Promise<Response> {
    // The context reaches Hono so the telemetry middleware can `waitUntil` a span flush.
    return ctx ? appFor(env).fetch(request, env, ctx as Parameters<App["fetch"]>[2]) : appFor(env).fetch(request);
  },
};
