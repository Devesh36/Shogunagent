import { allProviders } from "../connectors/index.js";
import { defineEndpoint, defineProvider, z, zodInput } from "../connectors/define.js";
import type { EndpointDefinition, OpClass, ProviderDefinition } from "../connectors/types.js";
import { compileCatalog, type CompiledCatalog } from "../src/catalog.js";
import { InMemoryHealthStore } from "../src/health.js";
import { InMemoryIdempotencyStore } from "../src/idempotency.js";
import type { LogEvent, Logger } from "../src/logging.js";
import { InMemoryMeteringClient } from "../src/metering.js";
import type { RunDeps } from "../src/run.js";
import { InMemoryRunLogSink } from "../src/runlog.js";
import { Secret } from "../src/secret.js";
import { createReplayTransport, type TransportCall, type TransportResult } from "../src/transport.js";

export const AGENT_KEY = "sa_test_key_0123456789";
export const bearer = () => new Secret(AGENT_KEY);

export function fullCatalog(): CompiledCatalog {
  return compileCatalog(allProviders());
}

export interface TestDeps extends RunDeps {
  runLog: InMemoryRunLogSink;
  metering: InMemoryMeteringClient;
  logs: LogEvent[];
  calls: TransportCall[];
}

export function makeDeps(opts: { catalog?: CompiledCatalog; responder?: (call: TransportCall) => TransportResult | Promise<TransportResult>; metering?: InMemoryMeteringClient; now?: () => number } = {}): TestDeps {
  const logs: LogEvent[] = [];
  const calls: TransportCall[] = [];
  const logger: Logger = { log: (e) => logs.push(e) };
  const responder = opts.responder ?? (() => ({ status: 200, body: { items: [{ ok: true }] } }));
  const deps: TestDeps = {
    catalog: opts.catalog ?? fullCatalog(),
    transport: createReplayTransport((call) => {
      calls.push(call);
      return responder(call);
    }),
    metering: opts.metering ?? new InMemoryMeteringClient(),
    runLog: new InMemoryRunLogSink(),
    idempotency: new InMemoryIdempotencyStore(),
    health: new InMemoryHealthStore(),
    logger,
    approvalBaseUrl: "https://approve.test/approve",
    logs,
    calls,
  };
  if (opts.now) deps.now = opts.now;
  return deps;
}

/** A synthetic read endpoint with a controllable description and price, for ranking tests. */
export function synthEndpoint(id: string, opts: { summary: string; description?: string; cost?: number; cls?: OpClass; categories?: EndpointDefinition["meta"]["categories"]; p95?: number; success?: number }): EndpointDefinition {
  return defineEndpoint({
    id,
    meta: { summary: opts.summary, description: opts.description ?? opts.summary, categories: opts.categories ?? ["data"] },
    request: { method: "POST", path: `/${id}` },
    input: { schema: zodInput(z.object({ q: z.string().min(1) }).strict()) },
    output: {},
    usage: { model: "per_call", cost_micro_usd: opts.cost ?? 0 },
    scope: { class: opts.cls ?? "read" },
    health: { p50_ms: 100, p95_ms: opts.p95 ?? 300, success_rate: opts.success ?? 0.99 },
    fixtures: [{ scenario: "basic", input: { q: "x" }, response: { status: 200, body: { items: [1] } } }],
  });
}

export function synthProvider(id: string, endpoints: EndpointDefinition[], extra: Partial<ProviderDefinition> = {}): ProviderDefinition {
  return defineProvider({
    id,
    name: id,
    first_party: false,
    meta: { description: `${id} test provider` },
    request: { baseUrl: `https://${id}.test` },
    auth: { kind: "bearer", credential_env: `${id.toUpperCase()}_KEY` },
    endpoints,
    ...extra,
  });
}
