import { createLiveApp } from "./live-app.js";
import { liveConfig } from "./live-config.js";
import { SqlLiveStore } from "./live-store.js";
/**
 * The Cloudflare Workers entry (production; docs/unit-economics.md §4 / §4.1). The Node entry is
 * `index.ts`; both wire the same relay through `boot.ts`.
 *
 * What is different here, and nothing else:
 * - Settings arrive as bindings on `env` (secrets via `wrangler secret put`, non-secrets as
 *   `vars` in wrangler.jsonc) under the SAME names `boot.ts` documents.
 * - The store is Postgres only (`pg-store.ts`), reached through the `HYPERDRIVE` binding's
 *   `connectionString`. The Hyperdrive config MUST have query caching disabled (wrangler.jsonc,
 *   DEPLOY.md): ownership and idempotency reads must never be served stale. A client is opened
 *   per request — the runtime forbids sharing a socket across requests — and left to the runtime
 *   to close, because a results read settles its spend only after the response body has streamed.
 * - The periodic sweep (store maintenance, then settlement) runs from the Cron Trigger
 *   (`scheduled`), never from `setInterval`.
 * - Boot cannot `process.exit`. A missing or malformed setting is logged ONCE per isolate (variable
 *   names only) and every request answers 503 with a fixed body until a deploy fixes it; the
 *   sweep does nothing in that state. Fail-closed, like the Node entry's exit.
 *
 * This bundle must contain nothing of the Node entry (`@hono/node-server`, `node:fs`,
 * `node:child_process`, `node:worker_threads`); only `index.ts` imports those. `pnpm check:bundle`
 * greps a dry-run bundle for exactly that.
 *
 * Logging: `logging.ts` `operational` is the only console writer, so Workers Logs sees route
 * templates, status codes, counters and env-variable names — never a body, header, id or value.
 */
import type { ExecutionContext } from "hono";

import { BootError, agentLaneLine, createRelay, formLaneLine, newMetrics, newTraceExporter, postgresStore, relayConfigFromEnv, trialLaneLine, type EnvRecord, type RelayConfig, providerGateways } from "./boot.js";
import type { AnthropicGateway } from "./gateway.js";
import { operational } from "./logging.js";
import type { RelayMetrics } from "./metrics.js";
import type { OtlpTraceExporter } from "./observability.js";
import { postgresClient } from "./sql.js";

/** The `HYPERDRIVE` binding, as far as the relay reads it. */
export interface HyperdriveBinding {
  connectionString: string;
}

/** Bindings: string vars and secrets under the names `boot.ts` documents, plus `HYPERDRIVE`. */
export interface WorkerEnv {
  HYPERDRIVE?: HyperdriveBinding;
  [name: string]: unknown;
}

/** The Workers `ExecutionContext`, as Hono already types it (`waitUntil`, `passThroughOnException`,
 * `props`) — so the Node typecheck needs no Workers type package. */
export type WorkerExecutionContext = ExecutionContext;

export interface WorkerScheduledController {
  readonly scheduledTime: number;
  readonly cron: string;
}

/** The fixed 503 body while the Worker is misconfigured. Says nothing about which setting. */
export const MISCONFIGURED_BODY = JSON.stringify({ error: "relay_misconfigured" });

/** The string bindings only: a Hyperdrive (or any other) object binding is not a setting. */
export function envRecord(env: WorkerEnv): EnvRecord {
  const record: EnvRecord = {};
  for (const [name, value] of Object.entries(env)) if (typeof value === "string") record[name] = value;
  return record;
}

interface Booted {
  config: RelayConfig;
  metrics: RelayMetrics;
  gateway: AnthropicGateway;
  orphanSweep?: () => Promise<number>;
  hyperdrive: HyperdriveBinding;
  /** E37: the isolate's span buffer, when OTLP export is configured. */
  traceExporter?: OtlpTraceExporter;
}

type BootState = { ok: true; booted: Booted } | { ok: false };

let state: BootState | undefined;

/** Validate the bindings once per isolate. Vars and secrets only change with a deploy, which
 * starts new isolates, so the outcome (good or bad) is final for this one. */
function boot(env: WorkerEnv): BootState {
  if (state) return state;
  try {
    const config = relayConfigFromEnv(envRecord(env));
    const hyperdrive = env.HYPERDRIVE;
    if (!hyperdrive || typeof hyperdrive.connectionString !== "string" || hyperdrive.connectionString === "") {
      throw new BootError("missing required binding: HYPERDRIVE (the relay's Postgres store; apps/api/DEPLOY.md)");
    }
    operational.info(trialLaneLine(config));
    operational.info(formLaneLine(config));
    operational.info(agentLaneLine(config));
    const provided = providerGateways(config);
    const metrics = newMetrics(config);
    const traceExporter = newTraceExporter(config, metrics);
    state = { ok: true, booted: { config, metrics, gateway: provided.gateway, ...(provided.orphanSweep ? { orphanSweep: provided.orphanSweep } : {}), hyperdrive, ...(traceExporter ? { traceExporter } : {}) } };
  } catch (e) {
    // Logged once: every later request answers 503 silently.
    operational.error(e instanceof BootError ? e.message : "invalid relay configuration");
    operational.error("relay misconfigured: answering 503 to every request until a deploy fixes it");
    state = { ok: false };
  }
  return state;
}

/** Tests only: forget the cached boot outcome so the next request re-reads its env. */
export function resetBootStateForTests(): void {
  state = undefined;
}

function misconfigured(): Response {
  return new Response(MISCONFIGURED_BODY, {
    status: 503,
    headers: { "content-type": "application/json", "cache-control": "no-store", "retry-after": "60" },
  });
}

/** One Postgres client for this request or sweep. Opened lazily by the driver on first query. */
function openStore(booted: Booted) {
  return postgresStore(postgresClient(booted.hyperdrive.connectionString, { hyperdrive: true }), booted.config);
}

export default {
  async fetch(request: Request, env: WorkerEnv, ctx: WorkerExecutionContext): Promise<Response> {
    const s = boot(env);
    if (!s.ok) return misconfigured();
    const { config, metrics, gateway, orphanSweep, traceExporter } = s.booted;
    const path = new URL(request.url).pathname;
    // `/v1/ai/form_decide` (#1144) belongs to the Batch relay app on the company TypeSafe key.
    if (path.startsWith("/v1/ai/") && path !== "/v1/ai/form_decide") {
      const settings = envRecord(env);
      if (settings.LIVE_AI_ENABLED !== "1") return new Response('{"error":"live_not_enabled"}', { status: 503, headers: { "content-type": "application/json", "cache-control": "no-store", "retry-after": "60" } });
      let live;
      try { live = liveConfig(settings); } catch { return misconfigured(); }
      const db = postgresClient(s.booted.hyperdrive.connectionString, { hyperdrive: true });
      try {
        const app = createLiveApp({ config: live, store: new SqlLiveStore(db), publicKey: config.licensePublicKey, log: operational.info, ...(config.denySubjects ? { denySubjects: config.denySubjects } : {}) });
        const response = await app.fetch(request);
        if (!response.body) { ctx.waitUntil(db.close()); return response; }
        const reader = response.body.getReader();
        // The connection belongs to this invocation through stream settlement; closing it
        // when the headers return would race the usage update at the end of SSE.
        const body = new ReadableStream<Uint8Array>({
          async pull(out) {
            try { const next = await reader.read(); if (next.done) { out.close(); ctx.waitUntil(db.close()); } else out.enqueue(next.value); }
            catch { out.error(new Error("live stream closed")); ctx.waitUntil(db.close()); }
          },
          async cancel() { try { await reader.cancel(); } finally { ctx.waitUntil(db.close()); } },
        });
        return new Response(body, response);
      } catch { ctx.waitUntil(db.close()); return misconfigured(); }
    }

    // Per request: the client (see the module comment) and therefore the store and the app around
    // it. Composition is a handful of object literals; the metrics window and the gateway persist.
    const relay = createRelay(config, openStore(s.booted), { metrics, gateway, ...(orphanSweep ? { orphanSweep } : {}), ...(traceExporter ? { traceExporter } : {}) });
    return relay.app.fetch(request, env, ctx);
  },

  /** The periodic sweep (boot.ts `sweepOnce`: retention maintenance, then the settlement pass of
   * audit M9), from the Cron Trigger in wrangler.jsonc. Bounded (`SETTLE_MAX_PER_TICK`) and
   * idempotent, so an overlapping or repeated tick books nothing twice. Counts only in the log.
   * `SETTLE_INTERVAL_SECONDS=0` turns the whole tick off without a redeploy. */
  async scheduled(_controller: WorkerScheduledController, env: WorkerEnv, ctx: WorkerExecutionContext): Promise<void> {
    const s = boot(env);
    if (!s.ok) return;
    if (s.booted.config.settleEverySec !== 0) ctx.waitUntil(sweep(s.booted));
    // E37: spans still buffered from a quiet period go out with the cron tick.
    if (s.booted.traceExporter && s.booted.traceExporter.pendingCount > 0) ctx.waitUntil(s.booted.traceExporter.flush());
    if (envRecord(env).LIVE_AI_ENABLED === "1") {
      ctx.waitUntil((async () => {
        const db = postgresClient(s.booted.hyperdrive.connectionString, { hyperdrive: true });
        try { await new SqlLiveStore(db).maintenance(); }
        catch { operational.warn("live_retention_unavailable"); }
        finally { await db.close().catch(() => undefined); }
      })());
    }
    await Promise.resolve();
  },
};

async function sweep(booted: Booted): Promise<void> {
  const store = openStore(booted);
  try {
    // A missing table is otherwise one 503 per request with nothing in the log; the sweep is the
    // place to say so (table names only).
    try {
      await store.usage.checkSchema();
      if (booted.config.agent) await store.agent.checkSchema();
      if (booted.config.agent?.oauthSecret) await store.oauth.checkSchema();
      if (booted.config.agent?.payouts) await store.payouts.checkSchema();
      if (booted.config.agent?.x402) await store.x402.checkSchema();
      if (booted.config.agent?.enterprise) await store.enterprise.checkSchema();
    } catch (e) {
      operational.error(e instanceof Error ? e.message : "shared store table missing or unreadable");
      return;
    }
    const relay = createRelay(booted.config, store, { metrics: booted.metrics, gateway: booted.gateway, ...(booted.orphanSweep ? { orphanSweep: booted.orphanSweep } : {}) });
    const counts = await relay.sweep();
    operational.info(`settlement_sweep ${JSON.stringify(counts)}`);
  } catch {
    operational.error("settlement_sweep_failure");
  } finally {
    await store.close().catch(() => undefined);
  }
}
