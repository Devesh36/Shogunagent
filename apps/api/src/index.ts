/**
 * The Node entry (development). Production runs the same relay on Cloudflare Workers from
 * `worker.ts`; the settings both read are documented in `boot.ts`.
 *
 * Node-only env:
 * - PORT                     — listen port (default 8787).
 * - HOST                     — optional bind address (e.g. 127.0.0.1 for a loopback-only local
 *                              relay, P0-F2 workaround). Unset = all interfaces.
 * - RELAY_DATABASE_URL       — optional. Postgres URL of the relay's store (issue #29 FR-SVC-04;
 *                              schema `sql/shared-store.sql`; TLS on unless the URL says `sslmode=`
 *                              itself, sql.ts). When set, the ledger AND the rate buckets live
 *                              there — the same store production runs on. Unset = the in-memory
 *                              store (`InMemoryUsageStore` + `TokenBucketLimiter`): nothing
 *                              persists across a restart, which is said loudly at boot and is only
 *                              acceptable because this entry is never production.
 * - METRICS_LOG_INTERVAL_SEC — structured `metrics {…}` log line period (default 60; 0 = off).
 *   (E37: the OTLP span exporter, when configured in boot.ts, is flushed every 5 s here.)
 *
 * Every refusal here exits non-zero; the Worker answers 503 instead (it cannot exit). Nothing in
 * this file is imported by the Worker bundle.
 */
import { serve } from "@hono/node-server";

import { lookup } from "node:dns/promises";

import { InMemoryAgentStore } from "./agent-store.js";
import { InMemoryX402Store } from "./billing/x402.js";
import { InMemoryPayoutStore } from "./payouts.js";
import { InMemoryOAuthStore } from "./oauth/store.js";
import { InMemoryEnterpriseStore } from "./enterprise/store.js";
import { BootError, SERVER_TIMEOUTS, agentLaneLine, billingJobs, createRelay, formLaneLine, intFromEnv, postgresStore, relayConfigFromEnv, startPeriodicSweep, trialLaneLine, type RelayStore } from "./boot.js";
import { operational } from "./logging.js";
import { FLUSH_INTERVAL_MS } from "./observability.js";
import { TokenBucketLimiter } from "./ratelimit.js";
import { postgresClient } from "./sql.js";
import { InMemoryUsageStore, utcDate } from "./usage.js";

/** Print the refusal and stop. */
function refuse(message: string): never {
  operational.error(message);
  process.exit(1);
}

function bootConfig(): ReturnType<typeof relayConfigFromEnv> {
  try {
    return relayConfigFromEnv(process.env);
  } catch (e) {
    return refuse(e instanceof BootError ? e.message : "invalid relay configuration");
  }
}

/** A Node-only whole-number setting, or exit naming the variable. */
function nodeInt(name: string, fallback: number, min?: number, max?: number): number {
  try {
    return intFromEnv(process.env, name, fallback, min, max);
  } catch (e) {
    return refuse(e instanceof BootError ? e.message : `invalid ${name}`);
  }
}

const config = bootConfig();
const port = nodeInt("PORT", 8787, 1, 65_535);
const host = process.env.HOST?.trim() || undefined;
const metricsEverySec = nodeInt("METRICS_LOG_INTERVAL_SEC", 60, 0);
const databaseUrl = process.env.RELAY_DATABASE_URL;
operational.info(trialLaneLine(config));
operational.info(formLaneLine(config));
operational.info(agentLaneLine(config));

/** Every address a Client ID Metadata Document host resolves to, so the CIMD fetch can refuse a
 * public name that points into a private range (oauth/cimd.ts). Node only; on Workers the
 * platform's fetch cannot reach private networks. */
async function resolveAllAddresses(hostname: string): Promise<string[]> {
  return (await lookup(hostname, { all: true, verbatim: true })).map((a) => a.address);
}

async function openStore(): Promise<RelayStore> {
  if (databaseUrl) {
    // The production store (FR-SVC-04): every instance reserves against the same rows. Refuse to
    // serve without the schema rather than 503 every submit.
    const store = postgresStore(postgresClient(databaseUrl), config);
    try {
      await store.usage.checkSchema();
      if (config.agent) await store.agent.checkSchema();
      if (config.agent?.oauthSecret) await store.oauth.checkSchema();
      if (config.agent?.payouts) await store.payouts.checkSchema();
      if (config.agent?.x402) await store.x402.checkSchema();
      if (config.agent?.enterprise) await store.enterprise.checkSchema();
    } catch (e) {
      refuse(e instanceof Error ? e.message : "shared store unavailable");
    }
    return { ...store, resolveHost: resolveAllAddresses };
  }
  // Development only. The caps, the idempotency tombstones and the spend ledger live in this
  // process and vanish with it: a restart forgets every reservation and would let a replayed
  // client_request_id submit again. Never run this against the production provider key.
  operational.warn("ledger is IN-MEMORY and not persistent (RELAY_DATABASE_URL unset) — development only, never production");
  return {
    usage: new InMemoryUsageStore(),
    rateLimit: new TokenBucketLimiter(config.rateBurst, config.ratePerMin),
    // Keys, wallets and receipts vanish with the process too.
    ...(config.agent?.payouts ? { payouts: new InMemoryPayoutStore() } : {}),
    ...(config.agent?.x402 ? { x402: new InMemoryX402Store() } : {}),
    ...(config.agent ? { agent: new InMemoryAgentStore(), oauth: new InMemoryOAuthStore(), enterprise: new InMemoryEnterpriseStore(), resolveHost: resolveAllAddresses } : {}),
    close: () => Promise.resolve(),
  };
}

const store = await openStore();
const relay = createRelay(config, store);

// FR-SVC-06: one structured line per interval — route templates and numbers only, so a log-based
// alert (runbook §9-5) works without exposing /metrics at all.
if (metricsEverySec > 0) {
  const timer = setInterval(() => {
    void (async () => {
      const date = utcDate();
      const spend = config.spendBreaker
        ? { date, spentMicros: await store.usage.spentOn(date).catch(() => -1), capMicros: config.spendBreaker.capMicros }
        : undefined;
      operational.info(`metrics ${JSON.stringify(relay.metrics.snapshot(Date.now(), spend))}`);
    })();
  }, metricsEverySec * 1000);
  timer.unref();
}

// E37: the opt-in OTLP export. Spans are buffered by the middleware and posted from here on Node
// (the Worker flushes through `waitUntil`). A failed post drops the batch and counts it.
if (relay.traceExporter) {
  const exporter = relay.traceExporter;
  const timer = setInterval(() => { void exporter.flush(); }, FLUSH_INTERVAL_MS);
  timer.unref();
  operational.info("otlp_trace_export enabled");
}

// The periodic sweep (audit M9): store retention maintenance, then book the provider's actual
// usage for ended batches whose results the client never read, so the day's spend is real, not an
// upper bound. Counts only in the log.
const stopSettle = startPeriodicSweep(relay.settlementDeps, config.settleEverySec);
// The billing jobs (Pro bundle grants, commit overage reports, OAuth retention) on the same period;
// the Worker runs them inside `relay.sweep` from its Cron Trigger.
const billingTimer = config.agent && config.settleEverySec > 0
  ? setInterval(() => { void billingJobs(config, store, Date.now()); }, config.settleEverySec * 1000)
  : undefined;
billingTimer?.unref();
const stopSweep = (): void => { stopSettle(); if (billingTimer) clearInterval(billingTimer); };

// Shorter than Node's defaults so a trickled body cannot hold a connection for minutes (boot.ts).
const server = serve({ fetch: relay.app.fetch, port, ...(host ? { hostname: host } : {}), serverOptions: { ...SERVER_TIMEOUTS } }, (info) => {
  operational.info(`ai gateway listening on ${host ?? ""}:${info.port}`);
});

// Drain HTTP handlers before stopping their connection owner. An abrupt process death
// still relies on the durable claim/reservation, never on a cleanup that reopens it.
for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.once(signal, () => {
    stopSweep();
    server.close(() => { void store.close(); });
  });
}
