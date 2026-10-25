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
import { SettlementDelivery, SqlSettlementOutbox } from "./outbox.js";
import { InMemoryPersonalStore, PersonalRanker } from "./personal.js";
import { InMemoryRunLogSink } from "./runlog.js";
import { createFetchTransport, createSettlementPoster, createWalletTransport, postgresConnector } from "./transport.js";
import { ApiWalletClient, UnavailableWalletClient } from "./wallet.js";

type App = ReturnType<typeof createApp>;
let cached: { env: RouterEnv; app: App; delivery: SettlementDelivery | undefined } | undefined;

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

function appFor(env: RouterEnv): { app: App; delivery: SettlementDelivery | undefined } {
  if (cached && cached.env === env) return cached;
  const catalog = compileCatalog(allProviders());
  // E20: per-isolate stub, like the other stores. Production: the principal's tool_runs (V94) via the sidecar.
  const personal = new PersonalRanker(new InMemoryPersonalStore(), catalog.index);
  const credentials = env as unknown as Record<string, string | undefined>;
  const delivery = deliveryFor(env);
  const app = createApp({
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
  cached = { env, app, delivery };
  return cached;
}

/** The parts of the Workers `ExecutionContext` this entry uses. */
interface WaitUntil {
  waitUntil(promise: Promise<unknown>): void;
}

export default {
  fetch(request: Request, env: RouterEnv): Response | Promise<Response> {
    return appFor(env).app.fetch(request);
  },
  /** The outbox retry job (E33): promote abandoned drafts, deliver due settlements with backoff,
   * prune delivered rows. Logs counts only. */
  scheduled(_controller: unknown, env: RouterEnv, ctx: WaitUntil): void {
    const { delivery } = appFor(env);
    if (delivery) ctx.waitUntil(delivery.flush());
  },
};
