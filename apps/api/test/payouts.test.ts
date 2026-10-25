/** E29: the provider settlement ledger (the router's seam), Stripe Connect Express onboarding,
 * the monthly payout (idempotent per provider-month, net of refunds and disputes, minimum
 * threshold, held while a dispute is open) and the provider statement — on the in-memory store
 * and the SQL store over the SQLite double running the real sql/provider-payouts.sql, with a
 * fake Stripe. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import type { ConnectFetch } from "../src/billing/connect.js";
import { payoutsConfigFromEnv } from "../src/boot.js";
import { signState, verifyState } from "../src/payouts-app.js";
import { SqlPayoutStore } from "../src/payouts-pg-store.js";
import {
  DEFAULT_MIN_PAYOUT_MICROS, InMemoryPayoutStore, SETTLEMENT_FORMAT, decidePayout, parseSettlement, payeeOf, runProviderPayouts, settlementRecord, takeOf,
  type PayoutStore, type RunSettlementInput, type TransferFn,
} from "../src/payouts.js";
import { BASE, TEST_KEY_ITERATIONS, authed, makeAgentRig, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";

const PAYOUT_SQL = readFileSync(new URL("../sql/provider-payouts.sql", import.meta.url), "utf8");
const TOKEN = "rst_test_settlement_token_0123456789abcdef";
const STATE_SECRET = "payout-state-secret-0123456789abcdef";
const STRIPE_KEY = "sk_test_connect";
const OCT_10 = Date.parse("2026-10-10T09:00:00Z");
const NOV_02 = Date.parse("2026-11-02T00:00:00Z");

const backends: Array<[string, () => PayoutStore]> = [
  ["in-memory", () => new InMemoryPayoutStore()],
  ["sql (sqlite double)", () => new SqlPayoutStore(sqliteDouble(PAYOUT_SQL))],
];

function settlement(over: Partial<RunSettlementInput> = {}): RunSettlementInput {
  return {
    format: SETTLEMENT_FORMAT, run_id: `run_${Math.random().toString(36).slice(2)}`, ts: OCT_10, tool_id: "acme.lookup", provider_id: "acme",
    first_party: false, payout_account_ref: "acct_1Acme", outcome: "ok", quantity: 1, provider_micro_usd: 1_000_000, ...over,
  };
}

/** A fake Stripe: records every call (path, method, idempotency key, form), answers like Stripe. */
function fakeStripe(opts: { payoutsEnabled?: boolean; failTransfers?: number } = {}) {
  const calls: Array<{ method: string; path: string; idem?: string; form: URLSearchParams; auth?: string }> = [];
  let failTransfers = opts.failTransfers ?? 0;
  const transfersByKey = new Map<string, string>();
  const state = { payoutsEnabled: opts.payoutsEnabled ?? true };
  const fetch: ConnectFetch = async (url, init) => {
    const path = new URL(url).pathname;
    const form = new URLSearchParams(init.body ?? "");
    calls.push({ method: init.method, path, ...(init.headers["idempotency-key"] ? { idem: init.headers["idempotency-key"] } : {}), form, ...(init.headers.authorization ? { auth: init.headers.authorization } : {}) });
    const json = (status: number, body: unknown) => ({ status, json: async () => body });
    if (path === "/v1/accounts" && init.method === "POST") return json(200, { id: "acct_1Acme" });
    if (path === "/v1/account_links") return json(200, { url: `https://connect.stripe.test/setup/${form.get("account")}`, expires_at: 1_800_000_000 });
    if (path.startsWith("/v1/accounts/") && init.method === "GET") return json(200, { id: "acct_1Acme", details_submitted: true, payouts_enabled: state.payoutsEnabled, requirements: { disabled_reason: null } });
    if (path === "/v1/transfers") {
      if (failTransfers > 0) { failTransfers -= 1; return json(500, { error: { message: "boom" } }); }
      const key = init.headers["idempotency-key"] ?? "";
      const id = transfersByKey.get(key) ?? `tr_${transfersByKey.size + 1}`;
      transfersByKey.set(key, id);
      return json(200, { id });
    }
    return json(404, {});
  };
  return { fetch, calls, state, transfers: () => calls.filter((c) => c.path === "/v1/transfers") };
}

async function keyFor(rig: AgentRig, principalId: string, scopes: AgentScope[]): Promise<string> {
  const { key, prefix } = generateAgentKey("live");
  await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: rig.now().getTime() });
  await rig.store.insertKey({ keyId: `ak_${principalId}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: rig.now().getTime() });
  return key;
}

function payoutRig(store: PayoutStore = new InMemoryPayoutStore(), stripe = fakeStripe()) {
  const rig = makeAgentRig({ payouts: { store, settlementToken: TOKEN, stateSecret: STATE_SECRET, stripe: { secretKey: STRIPE_KEY, fetch: stripe.fetch } } });
  return { rig, store, stripe };
}

function postSettlements(rig: AgentRig, settlements: unknown[], token = TOKEN): Promise<Response> {
  return Promise.resolve(rig.app.request("/v1/internal/router/settlements", {
    method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify({ settlements }),
  }));
}

describe("configuration", () => {
  it("payouts are off without PAYOUTS_STATE_SECRET; short secrets and bad minimums stop boot", () => {
    expect(payoutsConfigFromEnv({})).toBeUndefined();
    expect(() => payoutsConfigFromEnv({ PAYOUTS_STATE_SECRET: "short" })).toThrow(/PAYOUTS_STATE_SECRET/);
    expect(() => payoutsConfigFromEnv({ PAYOUTS_STATE_SECRET: STATE_SECRET, ROUTER_SETTLEMENT_TOKEN: "short" })).toThrow(/ROUTER_SETTLEMENT_TOKEN/);
    expect(() => payoutsConfigFromEnv({ PAYOUTS_STATE_SECRET: STATE_SECRET, PAYOUT_MIN_USD: "0.5" })).toThrow(/PAYOUT_MIN_USD/);
    expect(payoutsConfigFromEnv({ PAYOUTS_STATE_SECRET: STATE_SECRET, ROUTER_SETTLEMENT_TOKEN: TOKEN, PAYOUT_MIN_USD: "25" }))
      .toEqual({ stateSecret: STATE_SECRET, settlementToken: TOKEN, minPayoutMicros: 25_000_000 });
  });
});

describe("rules", () => {
  it("takes 5% rounded up on third-party runs only; first-party and passthrough take nothing", () => {
    expect(takeOf(1_000)).toBe(50);
    expect(takeOf(1_001)).toBe(51);
    expect(payeeOf({ first_party: true, payout_account_ref: "first-party:select" })).toBe("first_party");
    expect(payeeOf({ first_party: false, payout_account_ref: null })).toBe("passthrough");
    expect(payeeOf({ first_party: false, payout_account_ref: "acct_1" })).toBe("connect");
    expect(settlementRecord(settlement({ first_party: true, payout_account_ref: "first-party:select", provider_micro_usd: 2000 })).takeMicros).toBe(0);
    expect(settlementRecord(settlement({ provider_micro_usd: 2000 })).takeMicros).toBe(100);
  });

  it("validates a settlement strictly: content-free fields only, a price only on ok runs", () => {
    expect(parseSettlement(settlement()).ok).toBe(true);
    expect(parseSettlement({ ...settlement(), input: "SENTINEL" }).ok).toBe(false);
    expect(parseSettlement(settlement({ outcome: "provider_error", provider_micro_usd: 5 })).ok).toBe(false);
    expect(parseSettlement(settlement({ tool_id: "other.lookup" })).ok).toBe(false);
    expect(parseSettlement(settlement({ payout_account_ref: "DE89 3704 0044" })).ok).toBe(false);
  });

  it("decides held / below_minimum / pending in that order", () => {
    const active = { providerId: "acme", ownerPrincipalId: "prn_p", stripeAccountId: "acct_1Acme", status: "active" as const, createdAt: 0, updatedAt: 0 };
    const inputs = { earnedMicros: 50_000_000, refundedMicros: 0, disputesLostMicros: 0, openDisputes: 0, previouslyPaidMicros: 0, declaredRef: "acct_1Acme" };
    expect(decidePayout("acme", "2026-10", inputs, undefined, DEFAULT_MIN_PAYOUT_MICROS, NOV_02).reason).toBe("account_not_active");
    expect(decidePayout("acme", "2026-10", { ...inputs, declaredRef: "acct_9Other" }, active, DEFAULT_MIN_PAYOUT_MICROS, NOV_02).reason).toBe("payout_ref_mismatch");
    expect(decidePayout("acme", "2026-10", { ...inputs, openDisputes: 1 }, active, DEFAULT_MIN_PAYOUT_MICROS, NOV_02).reason).toBe("dispute_open");
    expect(decidePayout("acme", "2026-10", { ...inputs, earnedMicros: 9_999_999 }, active, DEFAULT_MIN_PAYOUT_MICROS, NOV_02).status).toBe("below_minimum");
    const ok = decidePayout("acme", "2026-10", { ...inputs, earnedMicros: 12_345_678 }, active, DEFAULT_MIN_PAYOUT_MICROS, NOV_02);
    expect(ok).toMatchObject({ status: "pending", amountMicros: 12_340_000, destination: "acct_1Acme", idempotencyKey: "payout:acme:2026-10" });
  });

  it("signs onboarding state per provider and rejects tampering and expiry", () => {
    const s = signState(STATE_SECRET, "acme", OCT_10 + 1000);
    expect(verifyState(STATE_SECRET, s, OCT_10)).toBe("acme");
    expect(verifyState(STATE_SECRET, s, OCT_10 + 2000)).toBeUndefined();
    expect(verifyState("another-secret-0123456789abcdefgh", s, OCT_10)).toBeUndefined();
    const forged = s.replace(Buffer.from("acme").toString("base64url"), Buffer.from("evil").toString("base64url"));
    expect(verifyState(STATE_SECRET, forged, OCT_10)).toBeUndefined();
  });
});

describe.each(backends)("settlement ledger on %s", (_name, open) => {
  it("records each run once and accrues per provider per day; failures and empty runs count at $0", async () => {
    const store = open();
    const ok = settlement({ run_id: "run_1" });
    expect(await store.recordSettlement(settlementRecord(ok))).toBe("recorded");
    expect(await store.recordSettlement(settlementRecord(ok))).toBe("duplicate");
    await store.recordSettlement(settlementRecord(settlement({ run_id: "run_2", outcome: "provider_error", provider_micro_usd: 0, quantity: 0 })));
    await store.recordSettlement(settlementRecord(settlement({ run_id: "run_3", outcome: "empty", provider_micro_usd: 0, quantity: 0 })));
    await store.recordSettlement(settlementRecord(settlement({ run_id: "run_4", ts: OCT_10 + 86_400_000, provider_micro_usd: 500 })));
    const rows = await store.accruals("acme", "2026-10-01", "2026-11-01");
    expect(rows).toEqual([
      { providerId: "acme", day: "2026-10-10", payee: "connect", payoutAccountRef: "acct_1Acme", runs: 3, okRuns: 1, emptyRuns: 1, failedRuns: 1, providerMicros: 1_000_000, takeMicros: 50_000 },
      { providerId: "acme", day: "2026-10-11", payee: "connect", payoutAccountRef: "acct_1Acme", runs: 1, okRuns: 1, emptyRuns: 0, failedRuns: 0, providerMicros: 500, takeMicros: 25 },
    ]);
    expect(await store.connectProviders("2026-11-01")).toEqual(["acme"]);
  });

  it("pays a closed month once, net of refunds and lost disputes, holds while a dispute is open, and carries a balance below the minimum", async () => {
    const store = open();
    await store.upsertAccount("acme", "prn_p", OCT_10);
    await store.setStripeAccount("acme", "acct_1Acme", OCT_10);
    await store.setAccountStatus("acme", "active", OCT_10);
    for (let i = 0; i < 30; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `r${i}` })));
    await store.addAdjustment({ entryId: "adj_refund", providerId: "acme", kind: "refund", micros: 2_000_000, status: "applied", reason: "refund", ts: OCT_10 });
    await store.addAdjustment({ entryId: "adj_dispute", providerId: "acme", kind: "dispute", micros: 3_000_000, status: "open", reason: "chargeback", ts: OCT_10 });

    const sent: Array<{ amountCents: number; destination: string; idempotencyKey: string }> = [];
    const transfer: TransferFn = async (t) => { sent.push(t); return { transferId: `tr_${sent.length}` }; };

    expect(await runProviderPayouts(store, transfer, "2026-10", Date.parse("2026-10-31T23:00:00Z"))).toEqual({ error: "month_open" });
    const held = await runProviderPayouts(store, transfer, "2026-10", NOV_02);
    expect(held).toMatchObject({ providers: 1, held: 1, paid: 0 });
    expect((await store.payout("acme", "2026-10"))?.reason).toBe("dispute_open");
    expect(sent).toHaveLength(0);

    await store.resolveDispute("acme", "adj_dispute", "lost", NOV_02);
    const paid = await runProviderPayouts(store, transfer, "2026-10", NOV_02 + 1000);
    expect(paid).toMatchObject({ paid: 1 });
    expect(sent).toEqual([{ amountCents: 2_500, destination: "acct_1Acme", providerId: "acme", month: "2026-10", idempotencyKey: "payout:acme:2026-10" }]);
    expect(await store.payout("acme", "2026-10")).toMatchObject({ status: "paid", earnedMicros: 30_000_000, refundedMicros: 2_000_000, disputesLostMicros: 3_000_000, amountMicros: 25_000_000, transferId: "tr_1" });

    // Idempotent: a second run pays nothing more.
    expect(await runProviderPayouts(store, transfer, "2026-10", NOV_02 + 2000)).toMatchObject({ already_paid: 1, paid: 0 });
    expect(sent).toHaveLength(1);

    // November: $4 earned is below the $10 minimum and carries; December pays November's + December's.
    for (let i = 0; i < 4; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `n${i}`, ts: Date.parse("2026-11-15T00:00:00Z") })));
    expect(await runProviderPayouts(store, transfer, "2026-11", Date.parse("2026-12-02T00:00:00Z"))).toMatchObject({ below_minimum: 1 });
    for (let i = 0; i < 7; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `d${i}`, ts: Date.parse("2026-12-15T00:00:00Z") })));
    expect(await runProviderPayouts(store, transfer, "2026-12", Date.parse("2027-01-02T00:00:00Z"))).toMatchObject({ paid: 1 });
    expect(sent[1]).toMatchObject({ amountCents: 1_100, idempotencyKey: "payout:acme:2026-12" });
    // Re-deciding November later never pays its carried balance twice.
    expect(await runProviderPayouts(store, transfer, "2026-11", Date.parse("2027-01-03T00:00:00Z"))).toMatchObject({ below_minimum: 1, paid: 0 });
    expect(sent).toHaveLength(2);
  });

  it("leaves a payout pending when the transfer fails and resends the same amount with the same key", async () => {
    const store = open();
    await store.upsertAccount("acme", "prn_p", OCT_10);
    await store.setStripeAccount("acme", "acct_1Acme", OCT_10);
    await store.setAccountStatus("acme", "active", OCT_10);
    for (let i = 0; i < 20; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `r${i}` })));
    let fail = true;
    const keys: string[] = [];
    const transfer: TransferFn = async (t) => { keys.push(`${t.idempotencyKey}:${t.amountCents}`); if (fail) throw new Error("stripe down"); return { transferId: "tr_ok" }; };
    expect(await runProviderPayouts(store, transfer, "2026-10", NOV_02)).toMatchObject({ failed: 1 });
    expect((await store.payout("acme", "2026-10"))?.status).toBe("pending");
    // More earnings arrive late for October: the pending amount does not change.
    await store.recordSettlement(settlementRecord(settlement({ run_id: "late" })));
    fail = false;
    expect(await runProviderPayouts(store, transfer, "2026-10", NOV_02 + 1)).toMatchObject({ paid: 1 });
    expect(keys).toEqual(["payout:acme:2026-10:2000", "payout:acme:2026-10:2000"]);
  });

  it("never pays first-party or passthrough revenue", async () => {
    const store = open();
    for (let i = 0; i < 50; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `f${i}`, provider_id: "gmail", tool_id: "gmail.messages.send", first_party: true, payout_account_ref: "first-party:select" })));
    for (let i = 0; i < 50; i++) await store.recordSettlement(settlementRecord(settlement({ run_id: `m${i}`, provider_id: "monid", tool_id: "monid.exa.search", payout_account_ref: null })));
    expect(await store.connectProviders("2026-11-01")).toEqual([]);
    expect(await runProviderPayouts(store, async () => ({ transferId: "x" }), "2026-10", NOV_02)).toMatchObject({ providers: 0, paid: 0 });
  });
});

describe("settlement route", () => {
  it("accepts the router's token only, validates every entry before recording any, and is idempotent", async () => {
    const { rig, store } = payoutRig();
    expect((await postSettlements(rig, [settlement()], "wrong-token")).status).toBe(401);
    const bad = await postSettlements(rig, [settlement({ run_id: "a" }), { ...settlement({ run_id: "b" }), body: "SENTINEL" }]);
    expect(bad.status).toBe(400);
    expect(await store.accruals("acme", "2026-10-01", "2026-11-01")).toEqual([]);
    const s = settlement({ run_id: "same" });
    const first = await postSettlements(rig, [s]);
    expect(await first.json()).toEqual({ recorded: 1, duplicates: 0 });
    expect(await (await postSettlements(rig, [s])).json()).toEqual({ recorded: 0, duplicates: 1 });
    expect(rig.logLines.join("\n")).not.toContain(TOKEN);
  });

  it("answers 503 when no settlement token is configured", async () => {
    const rig = makeAgentRig({ payouts: { store: new InMemoryPayoutStore(), stateSecret: STATE_SECRET } });
    expect((await postSettlements(rig, [settlement()])).status).toBe(503);
  });
});

describe("Connect onboarding and the provider's own routes", () => {
  it("binds the provider to its owner, onboards with Express, binds payout_account_ref on return, and serves the statement", async () => {
    const { rig, store, stripe } = payoutRig();
    const admin = await keyFor(rig, "prn_admin", ["admin"]);
    const provider = await keyFor(rig, "prn_provider", ["read", "write"]);
    const stranger = await keyFor(rig, "prn_stranger", ["read", "write"]);

    expect((await authed(rig, provider, "/v1/providers/me")).status).toBe(404);
    const bind = await authed(rig, admin, "/v1/admin/providers", { method: "POST", body: { provider_id: "acme", owner_principal_id: "prn_provider" } });
    expect(bind.status).toBe(200);
    expect((await authed(rig, stranger, "/v1/providers/me")).status).toBe(404);

    const connect = await authed(rig, provider, "/v1/providers/me/connect", { method: "POST" });
    expect(connect.status).toBe(201);
    const link = (await connect.json()) as { url: string; next_action: string };
    expect(link).toMatchObject({ url: "https://connect.stripe.test/setup/acct_1Acme", next_action: "approve_at_url" });
    const created = stripe.calls.find((c) => c.path === "/v1/accounts");
    expect(created?.idem).toBe("connect:account:acme");
    expect(created?.form.get("type")).toBe("express");
    expect(created?.auth).toBe(`Bearer ${STRIPE_KEY}`);
    const accountLink = stripe.calls.find((c) => c.path === "/v1/account_links");
    const returnUrl = new URL(accountLink?.form.get("return_url") ?? "");
    expect(returnUrl.origin + returnUrl.pathname).toBe(`${BASE}/v1/providers/connect/return`);
    // A second connect reuses the account.
    await authed(rig, provider, "/v1/providers/me/connect", { method: "POST" });
    expect(stripe.calls.filter((c) => c.path === "/v1/accounts" && c.method === "POST")).toHaveLength(1);

    expect((await store.account("acme"))?.payoutAccountRef).toBeUndefined();
    const back = await rig.app.request(`${returnUrl.pathname}${returnUrl.search}`);
    expect(back.status).toBe(200);
    expect(back.headers.get("content-security-policy")).toContain("default-src 'none'");
    const html = await back.text();
    expect(html).toContain("Payouts enabled");
    expect(html).not.toMatch(/<script/i);
    expect(await store.account("acme")).toMatchObject({ status: "active", payoutAccountRef: "acct_1Acme" });
    expect((await rig.app.request("/v1/providers/connect/return?state=forged")).status).toBe(400);
    const refresh = await rig.app.request(`/v1/providers/connect/refresh${returnUrl.search}`);
    expect(refresh.status).toBe(302);
    expect(refresh.headers.get("location")).toBe("https://connect.stripe.test/setup/acct_1Acme");

    // Settlements arrive; the provider reads its statement; an admin runs the payout.
    rig.setNow(new Date("2026-10-20T00:00:00Z"));
    await postSettlements(rig, Array.from({ length: 12 }, (_, i) => settlement({ run_id: `s${i}` })));
    await postSettlements(rig, [settlement({ run_id: "fail", outcome: "transport_error", provider_micro_usd: 0, quantity: 0 })]);
    const me = (await (await authed(rig, provider, "/v1/providers/me")).json()) as { provider: Record<string, unknown>; this_month: Record<string, unknown> };
    expect(me.provider).toMatchObject({ provider_id: "acme", status: "active" });
    expect(me.this_month).toMatchObject({ month: "2026-10", runs: 13, provider_micro_usd: 12_000_000 });

    rig.setNow(new Date(NOV_02));
    expect((await authed(rig, provider, "/v1/admin/payouts/run", { method: "POST", body: { month: "2026-10" } })).status).toBe(403);
    const run = await authed(rig, admin, "/v1/admin/payouts/run", { method: "POST", body: { month: "2026-10" } });
    expect(await run.json()).toMatchObject({ month: "2026-10", paid: 1 });
    expect(stripe.transfers()).toHaveLength(1);
    expect(stripe.transfers()[0]?.idem).toBe("payout:acme:2026-10");
    expect(stripe.transfers()[0]?.form.get("amount")).toBe("1200");
    expect(stripe.transfers()[0]?.form.get("destination")).toBe("acct_1Acme");
    await authed(rig, admin, "/v1/admin/payouts/run", { method: "POST", body: { month: "2026-10" } });
    expect(stripe.transfers()).toHaveLength(1);

    const st = await authed(rig, provider, "/v1/providers/me/statements?month=2026-10");
    expect(st.status).toBe(200);
    const body = (await st.json()) as Record<string, unknown> & { totals: Record<string, number>; payout: Record<string, unknown>; hash: string };
    expect(body.schema).toBe("shogunagent/provider-statement/v1");
    expect(body.status).toBe("closed");
    expect(body.totals).toMatchObject({ runs: 13, ok_runs: 12, failed_runs: 1, provider_micro_usd: 12_000_000, take_micro_usd: 600_000, charged_micro_usd: 12_600_000 });
    expect(body.payout).toMatchObject({ status: "paid", amount_micro_usd: 12_000_000 });
    expect(st.headers.get("x-statement-hash")).toBe(body.hash);
    expect((await authed(rig, provider, "/v1/providers/me/statements?month=2026-13")).status).toBe(400);
    expect((await authed(rig, stranger, "/v1/providers/me/statements?month=2026-10")).status).toBe(404);
  });

  it("operator adjustments: a refund needs an Idempotency-Key and is recorded once; a dispute holds until resolved", async () => {
    const { rig, store } = payoutRig();
    const admin = await keyFor(rig, "prn_admin", ["admin"]);
    await store.upsertAccount("acme", "prn_p", OCT_10);
    const path = "/v1/admin/providers/acme/adjustments";
    expect((await authed(rig, admin, path, { method: "POST", body: { kind: "refund", amount_micro_usd: 100, reason: "refund" } })).status).toBe(400);
    const first = await authed(rig, admin, path, { method: "POST", body: { kind: "refund", amount_micro_usd: 100, reason: "refund" }, headers: { "idempotency-key": "r-1" } });
    expect(first.status).toBe(201);
    const again = await authed(rig, admin, path, { method: "POST", body: { kind: "refund", amount_micro_usd: 100, reason: "refund" }, headers: { "idempotency-key": "r-1" } });
    expect(again.status).toBe(200);
    const d = await authed(rig, admin, path, { method: "POST", body: { kind: "dispute", amount_micro_usd: 500, reason: "chargeback" }, headers: { "idempotency-key": "d-1" } });
    const entry = ((await d.json()) as { adjustment: { entry_id: string; status: string } }).adjustment;
    expect(entry.status).toBe("open");
    const resolved = await authed(rig, admin, `/v1/admin/providers/acme/disputes/${encodeURIComponent(entry.entry_id)}/resolve`, { method: "POST", body: { outcome: "provider_won" } });
    expect(((await resolved.json()) as { adjustment: { status: string } }).adjustment.status).toBe("won");
    expect((await store.adjustments("acme")).map((a) => `${a.kind}:${a.status}`).sort()).toEqual(["dispute:won", "refund:applied"]);
  });

  it("an OAuth-free key without a provider binding cannot onboard; Connect absent answers 503", async () => {
    const rig = makeAgentRig({ payouts: { store: new InMemoryPayoutStore(), settlementToken: TOKEN, stateSecret: STATE_SECRET } });
    const k = await keyFor(rig, "prn_x", ["read", "write"]);
    expect((await authed(rig, k, "/v1/providers/me/connect", { method: "POST" })).status).toBe(503);
  });
});
