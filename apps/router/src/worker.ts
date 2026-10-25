/**
 * Cloudflare Workers entry. Builds the production dependency set from the Worker environment:
 * the compiled catalog (first-party + Monid passthrough), the fetch transport over the env map
 * (the only place `env` secrets are read), and the wallet:
 *
 * - `ROUTER_WALLET_URL` + `HYPERDRIVE` (production, E32/E33): `ApiWalletClient`. Every metered run
 *   is reserved and settled once by apps/api's wallet; every settlement goes through the durable
 *   Postgres outbox, and the `scheduled` handler (cron, wrangler.jsonc) is its retry job.
 * - `ROUTER_WALLET_URL` without `HYPERDRIVE`: misconfigured; metered runs are refused (503
 *   wallet_unavailable) rather than charged without a durable settlement. $0 runs proceed.
 * - neither (development): the in-memory wallet; nothing is forwarded.
 *
 * Ops telemetry (E37): body-free metrics at `GET /metrics` behind `ROUTER_METRICS_TOKEN`, and
 * opt-in OTLP span export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
 *
 * The idempotency, health, run-log and personal stores are still per-isolate stubs.
 */
import { allProviders } from "../connectors/index.js";
import { createApp } from "./app.js";
import { compileCatalog } from "./catalog.js";
import type { RouterEnv } from "./env.js";
import { DEFAULTS } from "./env.js";
import { InMemoryHealthStore } from "./health.js";
import { InMemoryIdempotencyStore } from "./idempotency.js";
import { consoleLogger } from "./logging.js";
import { InMemoryMeteringClient, type MeteringClient } from "./metering.js";
import { OpsMetrics, OtlpTraceExporter, otlpSettingsFromEnv, type RouterOps } from "./observability.js";
import { SettlementDelivery, SqlSettlementOutbox } from "./outbox.js";
import { InMemoryPersonalStore, PersonalRanker } from "./personal.js";
import { InMemoryRunLogSink } from "./runlog.js";
import { createFetchTransport, createMetricsAuthorizer, createOtlpPoster, createSettlementPoster, createWalletTransport, parseOtlpHeaders, postgresConnector } from "./transport.js";
import { ApiWalletClient, UnavailableWalletClient } from "./wallet.js";

type App = ReturnType<typeof createApp>;
let cached: { env: RouterEnv; app: App; delivery: SettlementDelivery | undefined; ops: RouterOps } | undefined;

/** E37: the isolate's metrics window, the `/metrics` bearer check and, when configured, the span
 * exporter. A malformed OTLP setting turns export off (logged once by name) rather than failing
 * the router: telemetry is never a reason to refuse traffic. */
function opsFor(env: RouterEnv, credentials: Record<string, string | undefined>): RouterOps {
  const metrics = new OpsMetrics();
  const ops: RouterOps = { metrics, authorize: createMetricsAuthorizer(credentials) };
  try {
    const settings = otlpSettingsFromEnv(env as unknown as Record<string, string | undefined>);
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

/** The outbox delivery, when the wallet is wired with its database. */
function deliveryFor(env: RouterEnv): SettlementDelivery | undefined {
  if (!env.ROUTER_WALLET_URL || !env.HYPERDRIVE?.connectionString) return undefined;
  const credentials = env as unknown as Record<string, string | undefined>;
  const outbox = new SqlSettlementOutbox(postgresConnector(env.HYPERDRIVE.connectionString));
  const url = `${env.ROUTER_WALLET_URL.replace(/\/+$/, "")}/v1/internal/router/settlements`;
  return new SettlementDelivery(outbox, createSettlementPoster({ env: credentials, url }), consoleLogger);
}

function walletFor(env: RouterEnv, delivery: SettlementDelivery | undefined): MeteringClient {
  if (!env.ROUTER_WALLET_URL) return new InMemoryMeteringClient({ topUpUrl: env.ROUTER_TOPUP_URL ?? DEFAULTS.ROUTER_TOPUP_URL });
  if (!delivery || !env.HYPERDRIVE?.connectionString) {
    consoleLogger.log({ kind: "metering", outcome: "outbox_not_configured" });
    return new UnavailableWalletClient(consoleLogger);
  }
  const credentials = env as unknown as Record<string, string | undefined>;
  return new ApiWalletClient({
    wallet: createWalletTransport({ env: credentials, baseUrl: env.ROUTER_WALLET_URL }),
    outbox: new SqlSettlementOutbox(postgresConnector(env.HYPERDRIVE.connectionString)),
    delivery,
    logger: consoleLogger,
  });
}

function appFor(env: RouterEnv): { app: App; delivery: SettlementDelivery | undefined; ops: RouterOps } {
  if (cached && cached.env === env) return cached;
  const catalog = compileCatalog(allProviders());
  // E20: per-isolate stub, like the other stores. Production: the principal's tool_runs (V94) via the sidecar.
  const personal = new PersonalRanker(new InMemoryPersonalStore(), catalog.index);
  const credentials = env as unknown as Record<string, string | undefined>;
  const delivery = deliveryFor(env);
  const ops = opsFor(env, credentials);
  const app = createApp({
    ops,
    catalog,
    transport: createFetchTransport({ env: credentials }),
    metering: walletFor(env, delivery),
    runLog: new InMemoryRunLogSink(),
    personal,
    idempotency: new InMemoryIdempotencyStore(),
    health: new InMemoryHealthStore(),
    logger: consoleLogger,
    approvalBaseUrl: env.APPROVAL_BASE_URL ?? DEFAULTS.APPROVAL_BASE_URL,
  });
  consoleLogger.log({ kind: "catalog", count: catalog.manifest.entry_count });
  cached = { env, app, delivery, ops };
  return cached;
}

/** The parts of the Workers `ExecutionContext` this entry uses. */
interface WorkerContext {
  waitUntil(promise: Promise<unknown>): void;
  passThroughOnException(): void;
}

export default {
  fetch(request: Request, env: RouterEnv, ctx?: WorkerContext): Response | Promise<Response> {
    // The context reaches Hono so the telemetry middleware can `waitUntil` a span flush.
    const { app } = appFor(env);
    return ctx ? app.fetch(request, env, ctx as Parameters<App["fetch"]>[2]) : app.fetch(request);
  },
  /** The outbox retry job (E33): promote abandoned drafts, deliver due settlements with backoff,
   * prune delivered rows. Logs counts only. */
  scheduled(_controller: unknown, env: RouterEnv, ctx: Pick<WorkerContext, "waitUntil">): void {
    const { delivery, ops } = appFor(env);
    if (delivery) ctx.waitUntil(delivery.flush());
    if (ops.exporter) ctx.waitUntil(ops.exporter.flush());
  },
};
