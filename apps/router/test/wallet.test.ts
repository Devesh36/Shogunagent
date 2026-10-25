/**
 * E32: the router charges nothing itself. `ApiWalletClient` sends apps/api the provider's price
 * (never a caller price), records a durable draft before money is held, and settles through the
 * outbox. The take lives in one place (packages/config/pricing.json) and is applied once.
 */
import { describe, expect, it } from "vitest";
import sharedPricing from "../../../packages/config/pricing.json";
import type { Json } from "../connectors/types.js";
import type { LogEvent } from "../src/logging.js";
import { InMemorySettlementOutbox, SettlementDelivery } from "../src/outbox.js";
import { run } from "../src/run.js";
import type { Secret } from "../src/secret.js";
import type { RunSettlement } from "../src/settlement.js";
import { callerPrice, RUN_MARKUP_BPS, takeOf } from "../src/take.js";
import { ApiWalletClient, type WalletReserveRequest } from "../src/wallet.js";
import { AGENT_KEY, bearer, fullCatalog, makeDeps } from "./helpers.js";

const SEARCH = "monid.tinyfish.web-search";
const okResponse = { status: 200, body: { status: "succeeded", output: { results: [{ url: "https://e.x/1" }] } } };

function rig(reserveAnswer: (body: WalletReserveRequest) => { status: number; body: Json } | Promise<never> = () => ({ status: 200, body: { reservation_id: "x" } }), settleStatus = 200) {
  const reserves: { body: WalletReserveRequest; caller: string }[] = [];
  const posted: RunSettlement[][] = [];
  const events: LogEvent[] = [];
  const logger = { log: (e: LogEvent) => events.push(e) };
  const outbox = new InMemorySettlementOutbox();
  const delivery = new SettlementDelivery(outbox, async (batch) => { posted.push(batch); return { status: settleStatus }; }, logger);
  const wallet = new ApiWalletClient({
    wallet: { reserve: async (body: WalletReserveRequest, caller: Secret) => { reserves.push({ body, caller: caller.expose() }); return reserveAnswer(body); } },
    outbox, delivery, logger,
  });
  const deps = makeDeps({ catalog: fullCatalog(), responder: () => okResponse });
  (deps as { metering: unknown }).metering = wallet;
  let n = 0;
  deps.randomId = () => `run${++n}`;
  return { deps, reserves, posted, outbox, delivery, events };
}

describe("the take, in one place", () => {
  it("is packages/config/pricing.json's run_markup_bps, applied once and never on first-party", () => {
    expect(RUN_MARKUP_BPS).toBe(sharedPricing.run_markup_bps);
    expect(callerPrice(1300, false)).toBe(1300 + Math.ceil((1300 * RUN_MARKUP_BPS) / 10_000));
    expect(callerPrice(1001, false)).toBe(1001 + takeOf(1001, false));
    expect(callerPrice(1300, true)).toBe(1300);
    expect(callerPrice(0, false)).toBe(0);
  });

  it("the Monid passthrough carries Monid's own price; the caller sees it plus one take (no double markup)", () => {
    const entry = fullCatalog().get(SEARCH);
    expect(entry?.usage).toMatchObject({ model: "per_call", cost_micro_usd: 1300 });
    expect(entry?.provider_quote_micro_usd).toBe(1300);
    expect(entry?.quote_micro_usd).toBe(1365);
    expect(entry?.description).toContain("plus the router's take, applied once");
  });
});

describe("ApiWalletClient through run (E32)", () => {
  it("a paid run reserves the provider price with the caller's key, settles once through the outbox, and shows provider + one take", async () => {
    const { deps, reserves, posted, outbox } = rig();
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k1" }, bearer: bearer() });
    expect(out.status).toBe(200);
    if (out.body.resultType !== "result") throw new Error("expected result");
    expect(out.body.usage).toEqual({ unit: "call", quantity: 1, cost_micro_usd: 1365 });
    expect(reserves).toEqual([{ body: { run_id: "run1", tool_id: SEARCH, provider_micro_usd: 1300, first_party: false }, caller: AGENT_KEY }]);
    expect(posted).toHaveLength(1);
    expect(posted[0]).toEqual([expect.objectContaining({ run_id: "run1", receipt_id: "rcpt_run_run1", marketplace: "monid", outcome: "ok", provider_micro_usd: 1300 })]);
    expect(await outbox.stats()).toMatchObject({ open: 0, pending: 0, delivered: 1 });
    // A replay by idempotency key reserves and settles nothing.
    const again = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k1" }, bearer: bearer() });
    expect(again.status).toBe(200);
    expect(reserves).toHaveLength(1);
    expect(posted).toHaveLength(1);
  });

  it("a provider failure settles the hold at $0", async () => {
    const { deps, posted } = rig();
    deps.transport = { execute: async () => ({ status: 200, body: { status: "failed", error: { code: "x" } } }) };
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "f" }, bearer: bearer() });
    expect(out.status).toBe(502);
    expect(out.body.usage.cost_micro_usd).toBe(0);
    expect(posted[0]?.[0]).toMatchObject({ outcome: "provider_error", provider_micro_usd: 0, receipt_id: "rcpt_run_run1" });
  });

  it("a 402 from the wallet is passed to the agent, the draft is discarded and the provider is never called", async () => {
    const payment = { amount_micro_usd: 1365, unit: "run", accepts: [{ rail: "card", url: "https://api.test/v1/wallet/topup", method: "POST" }], retry_after: 0, balance_micro_usd: 10 };
    const { deps, outbox } = rig(() => ({ status: 402, body: { error: { code: "payment_required", message: "m", next_action: "top_up" }, payment } }));
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "p" }, bearer: bearer() });
    expect(out.status).toBe(402);
    if (out.body.resultType !== "error") throw new Error("expected error");
    expect(out.body.error).toMatchObject({ code: "payment_required", next_action: "top_up" });
    expect(out.body.payment).toEqual(payment);
    expect(deps.calls).toHaveLength(0);
    expect(await outbox.stats()).toMatchObject({ open: 0, pending: 0 });
  });

  it("a cap refusal says retry_after, not top_up", async () => {
    const payment = { amount_micro_usd: 1365, unit: "run", accepts: [], retry_after: 3600, cap: { scope: "key", period: "day", limit_micro_usd: 1, spent_micro_usd: 1 } };
    const { deps } = rig(() => ({ status: 402, body: { error: { code: "cap_exceeded", message: "m", next_action: "retry_after" }, payment } }));
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "c" }, bearer: bearer() });
    expect(out.status).toBe(402);
    if (out.body.resultType !== "error") throw new Error("expected error");
    expect(out.body.error.next_action).toBe("retry_after");
  });

  it("fails closed: an unreachable wallet or an unwritable outbox refuses the run before the provider is called", async () => {
    const down = rig(() => Promise.reject(new Error("network")));
    const a = await run(down.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "d" }, bearer: bearer() });
    expect(a.status).toBe(503);
    expect(down.deps.calls).toHaveLength(0);
    // Unknown whether apps/api holds money: the draft stays, and the retry job releases it at $0.
    expect(await down.outbox.stats()).toMatchObject({ open: 1 });

    const noDb = rig();
    noDb.outbox.failNext = 1;
    const b = await run(noDb.deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "o" }, bearer: bearer() });
    expect(b.status).toBe(503);
    expect(noDb.reserves).toHaveLength(0);
    expect(noDb.deps.calls).toHaveLength(0);
  });

  it("a $0 tool (a first-party state read) never calls the wallet and is delivered by the retry job, not inline", async () => {
    const { deps, reserves, posted, delivery } = rig();
    deps.transport = { execute: async () => ({ status: 200, body: { items: [{ id: "c1" }] } }) };
    const out = await run(deps, { input: { tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "free" }, bearer: bearer() });
    expect(out.status).toBe(200);
    expect(out.body.usage.cost_micro_usd).toBe(0);
    expect(reserves).toHaveLength(0);
    expect(posted).toHaveLength(0);
    await delivery.flush();
    expect(posted[0]?.[0]).toMatchObject({ tool_id: "shogunagent.state.commitments.list", receipt_id: null, first_party: true, provider_micro_usd: 0 });
  });

  it("an anonymous or malformed bearer cannot reserve; nothing about the key reaches the outbox or a log", async () => {
    const { deps, outbox, events, reserves } = rig();
    const anon = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "a" } });
    expect(anon.status).toBe(401);
    expect(reserves).toHaveLength(0);
    await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "b" }, bearer: bearer() });
    const everything = JSON.stringify([[...outbox.rows.values()], events, deps.logs]);
    expect(everything).not.toContain(AGENT_KEY);
  });
});
