/**
 * E32 + E33 end to end: the real router engine (apps/router) charging through the real wallet
 * routes of this gateway (router-wallet.ts). The router's wallet transport and settlement poster
 * are the production ones; their `fetch` is this app's in-process `request`, so nothing leaves the
 * test. What is proved:
 *
 * - a router run is charged exactly once, by the wallet, at provider price + one take; the Monid
 *   passthrough is not marked up twice (Monid's 1300 µ$ → 1365 µ$, never 1434);
 * - the provider accrual records the same split (provider 1300, take 65);
 * - a redelivered settlement, or a retried reserve, changes nothing;
 * - failures and empty results cost nothing; a short wallet is a 402 from the wallet itself;
 * - a run abandoned after its hold is released at $0 by the outbox retry job;
 * - no key and no body reach a log line.
 */
import { describe, expect, it } from "vitest";

import { allProviders } from "../../router/connectors/index.js";
import { compileCatalog } from "../../router/src/catalog.js";
import { InMemoryHealthStore } from "../../router/src/health.js";
import { InMemoryIdempotencyStore } from "../../router/src/idempotency.js";
import type { LogEvent } from "../../router/src/logging.js";
import { ABANDON_AFTER_MS, InMemorySettlementOutbox, SettlementDelivery } from "../../router/src/outbox.js";
import { run, type RunDeps } from "../../router/src/run.js";
import { InMemoryRunLogSink } from "../../router/src/runlog.js";
import { Secret } from "../../router/src/secret.js";
import { buildSettlement } from "../../router/src/settlement.js";
import { createReplayTransport, createSettlementPoster, createWalletTransport, type TransportResult } from "../../router/src/transport.js";
import { ApiWalletClient } from "../../router/src/wallet.js";
import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import { routerReceiptId, InMemoryPayoutStore } from "../src/payouts.js";
import { RUN_MARKUP_BPS } from "../src/pricing.js";
import { routerRunQuote } from "../src/router-wallet.js";
import { TEST_KEY_ITERATIONS, makeAgentRig, type AgentRig } from "./agent-helpers.js";

const TOKEN = "rst_router_wallet_token_0123456789abcdef";
const SEARCH = "monid.tinyfish.web-search";
const MONID_OK: TransportResult = { status: 200, body: { status: "succeeded", output: { results: [{ url: "https://e.x/SENTINEL-body-91" }] } } };

async function keyFor(rig: AgentRig, principalId: string, scopes: AgentScope[], balance: number): Promise<string> {
  const { key, prefix } = generateAgentKey("live");
  await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: rig.now().getTime() });
  await rig.store.insertKey({ keyId: `ak_${principalId}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: rig.now().getTime() });
  if (balance > 0) await rig.store.credit(principalId, `seed_${principalId}`, balance, "test", rig.now().getTime());
  return key;
}

/** The router wired to this gateway: production transport and poster over the app's `request`. */
function routerFor(rig: AgentRig, responder: () => TransportResult = () => MONID_OK, idPrefix = "run") {
  const env = { ROUTER_SETTLEMENT_TOKEN: TOKEN };
  const inProcess = (async (url: string | URL, init?: RequestInit) => rig.app.request(new URL(String(url)).pathname, init)) as unknown as typeof fetch;
  const events: LogEvent[] = [];
  const logger = { log: (e: LogEvent) => events.push(e) };
  let clock = rig.now().getTime();
  const outbox = new InMemorySettlementOutbox();
  const poster = createSettlementPoster({ env, url: "https://api.test/v1/internal/router/settlements", fetch: inProcess });
  const delivery = new SettlementDelivery(outbox, poster, logger, { now: () => clock });
  const wallet = new ApiWalletClient({ wallet: createWalletTransport({ env, baseUrl: "https://api.test", fetch: inProcess }), outbox, delivery, logger, now: () => clock });
  let n = 0;
  const deps: RunDeps = {
    catalog: compileCatalog(allProviders()),
    transport: createReplayTransport(responder),
    metering: wallet,
    runLog: new InMemoryRunLogSink(),
    idempotency: new InMemoryIdempotencyStore(),
    health: new InMemoryHealthStore(),
    logger,
    approvalBaseUrl: "https://approve.test",
    now: () => clock,
    randomId: () => `${idPrefix}${++n}`,
  };
  return { deps, wallet, outbox, delivery, poster, events, advance: (ms: number) => { clock += ms; } };
}

function walletRig() {
  const payouts = new InMemoryPayoutStore();
  const rig = makeAgentRig({ routerWallet: { token: TOKEN }, payouts: { store: payouts, stateSecret: "payout-state-secret-0123456789abcdef" } });
  return { rig, payouts };
}

async function available(rig: AgentRig, principalId: string): Promise<{ balance: number; reserved: number }> {
  const w = await rig.store.wallet(principalId);
  return { balance: w.balanceMicros, reserved: w.reservedMicros };
}

describe("the router's wallet (E32): one charge, one take", () => {
  it("prices a third-party run as provider + RUN_MARKUP_BPS and a first-party run at its listed price", () => {
    expect(RUN_MARKUP_BPS).toBe(500);
    expect(routerRunQuote(1300, false).cost_micro_usd).toBe(1365);
    expect(routerRunQuote(1300, true).cost_micro_usd).toBe(1300);
  });

  it("a Monid run is charged exactly once at Monid's price + one take; the accrual records the same split", async () => {
    const { rig, payouts } = walletRig();
    const key = await keyFor(rig, "prn_a", ["read", "run"], 1_000_000);
    const router = routerFor(rig);
    const out = await run(router.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k1" }, bearer: new Secret(key) });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "result") throw new Error("expected a result");
    expect(out.body.usage.cost_micro_usd).toBe(1365);
    expect(await available(rig, "prn_a")).toEqual({ balance: 1_000_000 - 1365, reserved: 0 });
    const receipt = await rig.store.receipt(routerReceiptId(out.body.run_id));
    expect(receipt).toMatchObject({ settled: true, costMicros: 1365, listCostMicros: 1365, unit: "run", name: "router.run", principalId: "prn_a" });
    expect((await rig.store.receipts("prn_a", 0, 100)).length).toBe(1);
    const [day] = await payouts.accruals("monid", "2026-10-01", "2026-11-01");
    expect(day).toMatchObject({ payee: "passthrough", runs: 1, okRuns: 1, providerMicros: 1300, takeMicros: 65 });
    expect(await router.outbox.stats()).toMatchObject({ pending: 0, delivered: 1 });

    // Redelivery (the outbox is at-least-once) changes neither the wallet nor the accrual.
    const settled = [...router.outbox.rows.values()][0]?.settlement;
    if (!settled) throw new Error("no outbox row");
    const again = await rig.app.request("/v1/internal/router/settlements", { method: "POST", headers: { authorization: `Bearer ${TOKEN}`, "content-type": "application/json" }, body: JSON.stringify({ settlements: [settled] }) });
    expect(await again.json()).toEqual({ recorded: 0, duplicates: 1, wallet_settled: 0, accrual: "on" });
    expect(await available(rig, "prn_a")).toEqual({ balance: 1_000_000 - 1365, reserved: 0 });
    expect((await payouts.accruals("monid", "2026-10-01", "2026-11-01"))[0]).toMatchObject({ runs: 1, providerMicros: 1300, takeMicros: 65 });

    // Nothing from the key or the body reaches a log line on either side.
    const logs = JSON.stringify([rig.logLines, router.events]);
    expect(logs).not.toContain(key);
    expect(logs).not.toContain("SENTINEL-body-91");
  });

  it("failures and empty results cost nothing; the hold is released", async () => {
    const { rig } = walletRig();
    const key = await keyFor(rig, "prn_f", ["run"], 1_000_000);
    const failing = routerFor(rig, () => ({ status: 200, body: { status: "failed", error: { code: "x" } } }));
    expect((await run(failing.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "f" }, bearer: new Secret(key) })).status).toBe(502);
    const empty = routerFor(rig, () => ({ status: 200, body: { status: "succeeded", output: { results: [] } } }), "empty");
    expect((await run(empty.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "e" }, bearer: new Secret(key) })).status).toBe(200);
    expect(await available(rig, "prn_f")).toEqual({ balance: 1_000_000, reserved: 0 });
    expect((await rig.store.receipts("prn_f", 0, 10)).map((r) => [r.settled, r.costMicros])).toEqual([[true, 0], [true, 0]]);
  });

  it("a short wallet is the wallet's own 402, and the provider is never called", async () => {
    const { rig } = walletRig();
    const key = await keyFor(rig, "prn_poor", ["run"], 1000);
    let called = 0;
    const router = routerFor(rig, () => { called += 1; return MONID_OK; });
    const out = await run(router.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "p" }, bearer: new Secret(key) });
    expect(out.status).toBe(402);
    if (out.body.resultType !== "error") throw new Error("expected an error");
    expect(out.body.error.next_action).toBe("top_up");
    expect(out.body.payment).toMatchObject({ amount_micro_usd: 1365, unit: "run", balance_micro_usd: 1000 });
    expect(out.body.payment?.accepts[0]).toMatchObject({ rail: "card" });
    expect(called).toBe(0);
    expect(await router.outbox.stats()).toMatchObject({ open: 0, pending: 0 });
  });

  it("a key without the run scope, or a wrong service token, cannot reserve", async () => {
    const { rig } = walletRig();
    const readOnly = await keyFor(rig, "prn_r", ["read"], 1_000_000);
    const router = routerFor(rig);
    const out = await run(router.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "s" }, bearer: new Secret(readOnly) });
    expect(out.status).toBe(401);
    const wrong = await rig.app.request("/v1/internal/router/reserve", { method: "POST", headers: { authorization: "Bearer nope", "x-caller-authorization": `Bearer ${readOnly}`, "content-type": "application/json" }, body: JSON.stringify({ run_id: "r", tool_id: SEARCH, provider_micro_usd: 1, first_party: false }) });
    expect(wrong.status).toBe(401);
  });

  it("a retried reserve for the same run is one hold; a reused run id after settlement is refused", async () => {
    const { rig } = walletRig();
    const key = await keyFor(rig, "prn_twice", ["run"], 1_000_000);
    const body = JSON.stringify({ run_id: "same", tool_id: SEARCH, provider_micro_usd: 1300, first_party: false });
    const headers = { authorization: `Bearer ${TOKEN}`, "x-caller-authorization": `Bearer ${key}`, "content-type": "application/json" };
    const first = await rig.app.request("/v1/internal/router/reserve", { method: "POST", headers, body });
    const second = await rig.app.request("/v1/internal/router/reserve", { method: "POST", headers, body });
    expect(await first.json()).toMatchObject({ reservation_id: "rcpt_run_same", charged_micro_usd: 1365, replayed: false });
    expect(await second.json()).toMatchObject({ reservation_id: "rcpt_run_same", replayed: true });
    expect(await available(rig, "prn_twice")).toEqual({ balance: 1_000_000, reserved: 1365 });
    await rig.app.request("/v1/internal/router/settlements", { method: "POST", headers: { authorization: `Bearer ${TOKEN}`, "content-type": "application/json" }, body: JSON.stringify({ settlements: [
      { format: "shogunagent-run-settlement/2", run_id: "same", receipt_id: "rcpt_run_same", ts: rig.now().getTime(), tool_id: SEARCH, provider_id: "monid", first_party: false, payout_account_ref: null, marketplace: "monid", outcome: "ok", quantity: 1, provider_micro_usd: 1300 },
    ] }) });
    expect(await available(rig, "prn_twice")).toEqual({ balance: 1_000_000 - 1365, reserved: 0 });
    const reused = await rig.app.request("/v1/internal/router/reserve", { method: "POST", headers, body });
    expect(reused.status).toBe(409);
  });
});

describe("the durable outbox (E33) against the wallet", () => {
  it("a run that dies after its hold is released at $0 by the retry job; an outage only delays the charge", async () => {
    const { rig } = walletRig();
    const key = await keyFor(rig, "prn_crash", ["run"], 1_000_000);
    const router = routerFor(rig);
    const entry = router.deps.catalog.get(SEARCH);
    const def = router.deps.catalog.lookup(SEARCH);
    if (!entry || !def) throw new Error("catalog");
    // The router reserves, then "crashes": settle is never called.
    const draft = buildSettlement({ runId: "dead1", receiptId: null, ts: rig.now().getTime(), toolId: SEARCH, provider: def.provider, endpoint: def.endpoint, outcome: "transport_error", quantity: 0, costMicroUsd: 0 });
    const held = await router.wallet.reserve({ actor: "a", anonymous: false }, { tool_id: SEARCH, estimate_micro_usd: 1365, provider_micro_usd: 1300, first_party: false, unit: "call" }, { runId: "dead1", bearer: new Secret(key), draft });
    expect(held).toMatchObject({ ok: true, receipt_id: "rcpt_run_dead1" });
    expect(await available(rig, "prn_crash")).toEqual({ balance: 1_000_000, reserved: 1365 });
    await router.delivery.flush();
    expect(await available(rig, "prn_crash")).toEqual({ balance: 1_000_000, reserved: 1365 });
    router.advance(ABANDON_AFTER_MS);
    expect(await router.delivery.flush()).toMatchObject({ delivered: 1 });
    expect(await available(rig, "prn_crash")).toEqual({ balance: 1_000_000, reserved: 0 });
    expect(await rig.store.receipt("rcpt_run_dead1")).toMatchObject({ settled: true, costMicros: 0 });
  });

  it("apps/api down at settle time: the run's answer stands, the outbox keeps the settlement, the retry job charges it once", async () => {
    const payouts = new InMemoryPayoutStore();
    const rig = makeAgentRig({ routerWallet: { token: TOKEN }, payouts: { store: payouts, stateSecret: "payout-state-secret-0123456789abcdef" } });
    const key = await keyFor(rig, "prn_out", ["run"], 1_000_000);
    const router = routerFor(rig);
    // The settlement POST fails once (apps/api unreachable), then works.
    let failOnce = true;
    const realPoster = router.poster;
    const flaky = async (batch: Parameters<typeof realPoster>[0]) => {
      if (failOnce) { failOnce = false; throw new Error("network"); }
      return realPoster(batch);
    };
    const delivery = new SettlementDelivery(router.outbox, flaky, { log: () => undefined }, { now: () => rig.now().getTime() });
    const wallet = new ApiWalletClient({ wallet: createWalletTransport({ env: { ROUTER_SETTLEMENT_TOKEN: TOKEN }, baseUrl: "https://api.test", fetch: (async (url: string, init?: RequestInit) => rig.app.request(new URL(url).pathname, init)) as unknown as typeof fetch }), outbox: router.outbox, delivery, logger: { log: () => undefined }, now: () => rig.now().getTime() });
    const out = await run({ ...router.deps, metering: wallet }, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "o" }, bearer: new Secret(key) });
    expect(out.status).toBe(200);
    expect(await available(rig, "prn_out")).toEqual({ balance: 1_000_000, reserved: 1365 });
    expect(await router.outbox.stats()).toMatchObject({ pending: 1 });
    expect(await delivery.flush()).toMatchObject({ delivered: 1 });
    expect(await available(rig, "prn_out")).toEqual({ balance: 1_000_000 - 1365, reserved: 0 });
    expect(await delivery.flush()).toMatchObject({ delivered: 0 });
    expect(await available(rig, "prn_out")).toEqual({ balance: 1_000_000 - 1365, reserved: 0 });
  });
});
