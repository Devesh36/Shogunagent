/**
 * E29: every settled run produces one content-free settlement for the provider ledger, failures
 * and empty results at $0, and the metering client forwards it to the sink apps/api implements.
 */
import { describe, expect, it } from "vitest";
import type { ProviderDefinition } from "../connectors/types.js";
import { compileCatalog } from "../src/catalog.js";
import { InMemoryMeteringClient } from "../src/metering.js";
import { run } from "../src/run.js";
import { InMemorySettlementSink, RUN_SETTLEMENT_KEYS, SETTLEMENT_FORMAT, type RunSettlement } from "../src/settlement.js";
import { createSettlementPoster, type TransportCall, type TransportResult } from "../src/transport.js";
import { bearer, fullCatalog, makeDeps, synthEndpoint, synthProvider } from "./helpers.js";

const SENTINEL = "SENTINEL-settlement-body-7f3a";

function listed(): ProviderDefinition {
  const p = synthProvider("acme", [synthEndpoint("lookup", { summary: "Look up an address for a phone number", cost: 1000 })]);
  return {
    ...p,
    manifest: {
      format: "shogunagent-provider/1",
      provider_id: "acme",
      legal_name: "Acme Data Ltd",
      contact: { email: "listings@acme.example" },
      payout_account_ref: "acct_1AcmeExample",
      pricing: [{ endpoint: "lookup", model: "per_call", cost_micro_usd: 1000 }],
      data_handling: { retention: "none", retention_days: 0, zero_retention: true, regions: ["eu"], statement: "Requests are processed in memory and not stored." },
    },
  };
}

function rig(responder: (call: TransportCall) => TransportResult | Promise<TransportResult>, catalog = compileCatalog([listed()])) {
  const sink = new InMemorySettlementSink();
  const metering = new InMemoryMeteringClient({ settlements: sink });
  let n = 0;
  const deps = makeDeps({ catalog, metering, responder });
  deps.randomId = () => `run-${++n}`;
  return { deps, sink, metering };
}

describe("run settlement (E29)", () => {
  it("a successful run settles once with the provider's price and payout reference; the run id matches the result", async () => {
    const { deps, sink } = rig(() => ({ status: 200, body: { items: [{ address: SENTINEL }] } }));
    const out = await run(deps, { input: { tool_id: "acme.lookup", input: { q: SENTINEL }, idempotency_key: "s1" }, bearer: bearer() });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "result") throw new Error("expected a result");
    expect(sink.settlements).toHaveLength(1);
    const s = sink.settlements[0] as RunSettlement;
    expect(s).toMatchObject({
      format: SETTLEMENT_FORMAT, run_id: out.body.run_id, receipt_id: null, tool_id: "acme.lookup", provider_id: "acme", first_party: false,
      payout_account_ref: "acct_1AcmeExample", marketplace: null, outcome: "ok", quantity: 1, provider_micro_usd: 1000,
    });
    // Content-free: exactly the published key set, and no input or output reaches it.
    expect(Object.keys(s).sort()).toEqual([...RUN_SETTLEMENT_KEYS].sort());
    expect(JSON.stringify(sink.settlements)).not.toContain(SENTINEL);
  });

  it("failures and empty results settle at $0 with their outcome; a replay does not settle again", async () => {
    const failing = rig(() => ({ status: 500, body: null }));
    await run(failing.deps, { input: { tool_id: "acme.lookup", input: { q: "x" }, idempotency_key: "f1" }, bearer: bearer() });
    expect(failing.sink.settlements.map((s) => [s.outcome, s.provider_micro_usd, s.quantity])).toEqual([["provider_error", 0, 0]]);

    const empty = rig(() => ({ status: 200, body: { items: [] } }));
    await run(empty.deps, { input: { tool_id: "acme.lookup", input: { q: "x" }, idempotency_key: "e1" }, bearer: bearer() });
    expect(empty.sink.settlements.map((s) => [s.outcome, s.provider_micro_usd])).toEqual([["empty", 0]]);

    const ok = rig(() => ({ status: 200, body: { items: [1] } }));
    await run(ok.deps, { input: { tool_id: "acme.lookup", input: { q: "x" }, idempotency_key: "same" }, bearer: bearer() });
    await run(ok.deps, { input: { tool_id: "acme.lookup", input: { q: "x" }, idempotency_key: "same" }, bearer: bearer() });
    expect(ok.sink.settlements).toHaveLength(1);
  });

  it("an unreachable provider settles as transport_error at $0", async () => {
    const { deps, sink } = rig(() => { throw new Error("down"); });
    const out = await run(deps, { input: { tool_id: "acme.lookup", input: { q: "x" }, idempotency_key: "t1" }, bearer: bearer() });
    expect(out.status).toBe(502);
    expect(sink.settlements.map((s) => s.outcome)).toEqual(["transport_error"]);
  });

  it("runs that never reserve (dryRun, L2 ask, L3 approval, 402) settle nothing", async () => {
    const sink = new InMemorySettlementSink();
    const deps = makeDeps({ metering: new InMemoryMeteringClient({ settlements: sink, defaultBalanceMicroUsd: 0 }) });
    await run(deps, { input: { tool_id: "gmail.messages.send", input: { to: ["a@b.c"], subject: "s", body: "b" }, idempotency_key: "l3" }, bearer: bearer() });
    await run(deps, { input: { tool_id: "monid.tinyfish.web-search", input: { query: "x" }, idempotency_key: "dry", dryRun: true }, bearer: bearer() });
    const short = await run(deps, { input: { tool_id: "monid.tinyfish.web-search", input: { query: "x" }, idempotency_key: "402" }, bearer: bearer() });
    expect(short.status).toBe(402);
    expect(sink.settlements).toHaveLength(0);
  });

  it("first-party and marketplace runs carry their kind: first_party true, or the marketplace tag", async () => {
    const sink = new InMemorySettlementSink();
    const deps = makeDeps({ catalog: fullCatalog(), metering: new InMemoryMeteringClient({ settlements: sink }), responder: () => ({ status: 200, body: { status: "succeeded", output: { results: [{ url: "u" }] } } }) });
    await run(deps, { input: { tool_id: "monid.tinyfish.web-search", input: { query: "x" }, idempotency_key: "m1" }, bearer: bearer() });
    const fp = makeDeps({ catalog: fullCatalog(), metering: new InMemoryMeteringClient({ settlements: sink }), responder: () => ({ status: 200, body: { items: [{ id: "c1" }] } }) });
    await run(fp, { input: { tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "fp1" }, bearer: bearer() });
    const [monid, first] = sink.settlements;
    // Monid's own price (no markup baked in): the take is the wallet's, applied once.
    expect(monid).toMatchObject({ provider_id: "monid", first_party: false, payout_account_ref: null, marketplace: "monid", provider_micro_usd: 1300 });
    expect(first).toMatchObject({ provider_id: "shogunagent", first_party: true, payout_account_ref: "first-party:select", marketplace: null, provider_micro_usd: 0 });
  });
});

describe("the settlement poster", () => {
  it("posts { settlements } with the token from the env map only, and reports the status", async () => {
    const seen: { url: string; auth: string | null; body: string }[] = [];
    const poster = createSettlementPoster({
      env: { ROUTER_SETTLEMENT_TOKEN: "rst_secret_value" },
      url: "https://api.test/v1/internal/router/settlements",
      fetch: (async (url: string, init: RequestInit) => {
        seen.push({ url, auth: new Headers(init.headers).get("authorization"), body: String(init.body) });
        return new Response("{}", { status: 503 });
      }) as unknown as typeof fetch,
    });
    const s: RunSettlement = { format: SETTLEMENT_FORMAT, run_id: "r1", receipt_id: "rcpt_run_r1", ts: 1, tool_id: "acme.lookup", provider_id: "acme", first_party: false, payout_account_ref: "acct_1", marketplace: null, outcome: "ok", quantity: 1, provider_micro_usd: 1000 };
    expect(await poster([s])).toEqual({ status: 503 });
    expect(seen[0]?.auth).toBe("Bearer rst_secret_value");
    expect(JSON.parse(seen[0]?.body ?? "{}")).toEqual({ settlements: [s] });
  });

  it("throws (and the outbox keeps the row) when the token is missing", async () => {
    const poster = createSettlementPoster({ env: {}, url: "https://api.test/x", fetch: (async () => new Response("{}")) as unknown as typeof fetch });
    await expect(poster([])).rejects.toThrow(/ROUTER_SETTLEMENT_TOKEN/);
  });
});
