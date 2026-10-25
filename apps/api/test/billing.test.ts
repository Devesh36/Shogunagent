/** E19 (and E14-T3): monthly statements, annual commits with drawdown and discounted receipts,
 * Stripe hardening (refunds, expired sessions, overage usage records), operator adjustments,
 * dispute resolution, the ShogunAI Pro bundle and the billing notifications — on the in-memory
 * store and the SQL store over the SQLite double running the real sql/agent-keys.sql. */
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import { SqlAgentStore } from "../src/agent-pg-store.js";
import { InMemoryAgentStore, type AgentStore, type ReserveInput } from "../src/agent-store.js";
import { grantProBundle, reportCommitOverage } from "../src/billing/jobs.js";
import {
  PRO_BUNDLE_MICROS, buildStatement, chargeRefundEntryId, discounted, monthClosed, overagePart, proGrantId, statementCsv, statementHash,
  type CommitRecord,
} from "../src/billing/ledger.js";
import { InMemoryNotifier, crossedLowBalance } from "../src/billing/notify.js";
import type { StripeFetch } from "../src/billing/stripe.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS } from "../src/pricing.js";
import { BASE, TEST_KEY_ITERATIONS, authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";
import { makeKeys, signLicense } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

const AGENT_SQL = readFileSync(new URL("../sql/agent-keys.sql", import.meta.url), "utf8");

const backends: Array<[string, () => AgentStore]> = [
  ["in-memory", () => new InMemoryAgentStore()],
  ["sql (sqlite double)", () => new SqlAgentStore(sqliteDouble(AGENT_SQL))],
];

const TS = Date.parse("2026-10-25T12:00:00Z");
const BIG_CAPS = { key: { day: 1e12, month: 1e12 }, principal: { day: 1e12, month: 1e12 } };

function commit(overrides: Partial<CommitRecord> = {}): CommitRecord {
  return {
    commitId: "cmt_1", subjectKind: "principal", subjectId: "prn_a", amountMicros: 10_000, discountBps: 2_000,
    startsAt: Date.parse("2026-01-01T00:00:00Z"), endsAt: Date.parse("2027-01-01T00:00:00Z"),
    drawnMicros: 0, overageMicros: 0, overageReportedMicros: 0, createdAt: TS, ...overrides,
  };
}

function reserveInput(overrides: Partial<ReserveInput> = {}): ReserveInput {
  return {
    receiptId: `rcpt_${Math.random().toString(36).slice(2)}`, principalId: "prn_a", keyId: "ak_a", method: "tools/call", name: "memory.ingest",
    unit: "ingest", quantity: 1, micros: 500, ts: TS, day: "2026-10-25", month: "2026-10", caps: BIG_CAPS, ...overrides,
  };
}

describe("ledger rules", () => {
  it("discounts down, never charging more than list × (1 − bps)", () => {
    expect(discounted(1_000, 2_000)).toBe(800);
    expect(discounted(999, 1_000)).toBe(900); // 999 − floor(99.9)
    expect(discounted(1, 2_500)).toBe(1);
    expect(discounted(0, 2_500)).toBe(0);
  });

  it("splits a drawdown into its overage part", () => {
    expect(overagePart(10_000, 0, 4_000)).toBe(0);
    expect(overagePart(10_000, 8_000, 4_000)).toBe(2_000);
    expect(overagePart(10_000, 12_000, 4_000)).toBe(4_000);
  });

  it("closes a month an hour after it ends", () => {
    expect(monthClosed("2026-09", Date.parse("2026-10-01T00:30:00Z"))).toBe(false);
    expect(monthClosed("2026-09", Date.parse("2026-10-01T01:00:00Z"))).toBe(true);
    expect(monthClosed("2026-13", TS)).toBe(false);
  });

  it("hashes a statement over everything but its hash, and renders CSV from the same body", () => {
    const body = buildStatement("prn_a", "2026-09", {
      usage: [
        { unit: "ingest", calls: 2, quantity: 12, listMicros: 6_000, chargedMicros: 4_800, commitFundedMicros: 4_800 },
        { unit: "read", calls: 5, quantity: 5, listMicros: 0, chargedMicros: 0, commitFundedMicros: 0 },
      ],
      unsettled: 0, credits: [{ topupId: "evt_1", principalId: "prn_a", micros: 5_000_000, source: "stripe_checkout", ts: TS - 30 * 86_400_000 }],
      adjustments: [], disputes: [],
    }, "closed", TS);
    expect(body.hash).toBe(statementHash(body));
    expect(String(body.hash)).toMatch(/^sha256:[0-9a-f]{64}$/);
    expect(body.totals).toEqual(expect.objectContaining({ calls: 7, free_calls: 5, list_micro_usd: 6_000, charged_micro_usd: 4_800, discount_micro_usd: 1_200, commit_funded_micro_usd: 4_800 }));
    const usage = body.usage as Array<Record<string, unknown>>;
    expect(usage.find((u) => u.unit === "read")).toEqual(expect.objectContaining({ free: true, charged_micro_usd: 0, calls: 5 }));
    const tampered = { ...body, totals: { ...(body.totals as object), charged_micro_usd: 1 } };
    expect(statementHash(tampered)).not.toBe(body.hash);
    const csv = statementCsv(body);
    expect(csv.split("\r\n")[0]).toBe("section,key,calls,quantity,list_micro_usd,amount_micro_usd,discount_micro_usd,status,ref,ts");
    expect(csv).toContain("usage,read,5,5,0,0,0,free,,");
    expect(csv).toContain(`hash,${String(body.hash)}`);
  });

  it("emits low balance only on the crossing", () => {
    expect(crossedLowBalance(900_000, 200_000, 1_000_000)).toBe(true);
    expect(crossedLowBalance(800_000, 100_000, 1_000_000)).toBe(false);
    expect(crossedLowBalance(1_000_000, 100_000, 1_000_000)).toBe(false);
  });
});

describe.each(backends)("billing ledger on %s", (_name, open) => {
  it("draws a commit down at the discounted price, books overage past the amount, and leaves the wallet alone", async () => {
    const store = open();
    if (store instanceof SqlAgentStore) await store.checkSchema();
    await store.createPrincipal({ principalId: "prn_a", tenantId: "ten_x", createdAt: TS });
    expect(await store.createCommit(commit())).toBe("created");
    expect(await store.createCommit(commit())).toBe("duplicate");
    expect(await store.createCommit(commit({ commitId: "cmt_2", startsAt: Date.parse("2026-12-01T00:00:00Z"), endsAt: Date.parse("2027-06-01T00:00:00Z") }))).toBe("overlap");
    expect((await store.activeCommit("prn_a", "ten_x", TS))?.commitId).toBe("cmt_1");
    expect(await store.activeCommit("prn_a", "ten_x", Date.parse("2027-02-01T00:00:00Z"))).toBeUndefined();
    const terms = { commitId: "cmt_1", discountBps: 2_000, funding: "commit" as const };
    // No wallet balance: a commit-funded call still reserves.
    expect(await store.reserve(reserveInput({ receiptId: "c1", micros: 8_000, listMicros: 10_000, commit: terms }))).toEqual({ kind: "ok" });
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 0, reservedMicros: 0 });
    await store.settle("c1", 8_000, 10_000);
    await store.settle("c1", 8_000, 10_000);
    expect(await store.receipt("c1")).toEqual(expect.objectContaining({ costMicros: 8_000, listCostMicros: 10_000, funding: "commit", commitId: "cmt_1", settled: true }));
    expect(await store.commit("cmt_1")).toEqual(expect.objectContaining({ drawnMicros: 8_000, overageMicros: 0 }));
    await store.reserve(reserveInput({ receiptId: "c2", micros: 4_000, listMicros: 5_000, commit: terms }));
    await store.settle("c2", 4_000, 5_000);
    expect(await store.commit("cmt_1")).toEqual(expect.objectContaining({ drawnMicros: 12_000, overageMicros: 2_000 }));
    // A tenant commit applies to the tenant's other principals; the principal's own wins.
    expect(await store.createCommit(commit({ commitId: "cmt_t", subjectKind: "tenant", subjectId: "ten_x", discountBps: 1_000 }))).toBe("created");
    expect((await store.activeCommit("prn_b", "ten_x", TS))?.commitId).toBe("cmt_t");
    expect((await store.activeCommit("prn_a", "ten_x", TS))?.commitId).toBe("cmt_1");
  });

  it("claims overage reports in whole cents with deterministic ids, once", async () => {
    const store = open();
    await store.createCommit(commit({ amountMicros: 0 }));
    expect(await store.setCommitSubscriptionItem("cmt_1", "cus_1", "si_1")).toBe(true);
    const terms = { commitId: "cmt_1", discountBps: 2_000, funding: "commit" as const };
    await store.reserve(reserveInput({ receiptId: "o1", micros: 25_000, commit: terms }));
    await store.settle("o1", 25_000, 31_250);
    const first = await store.claimOverageReports(TS, 10);
    expect(first).toEqual([expect.objectContaining({ reportId: "ovr_cmt_1_0", cents: 2, subscriptionItemId: "si_1" })]);
    // A second claim before sending returns the same unsent report and creates nothing new.
    expect((await store.claimOverageReports(TS + 1, 10)).map((r) => r.reportId)).toEqual(["ovr_cmt_1_0"]);
    await store.markOverageReportSent("ovr_cmt_1_0", TS + 2);
    expect(await store.claimOverageReports(TS + 3, 10)).toEqual([]);
    expect(await store.commit("cmt_1")).toEqual(expect.objectContaining({ overageMicros: 25_000, overageReportedMicros: 20_000 }));
    await store.reserve(reserveInput({ receiptId: "o2", micros: 5_000, commit: terms }));
    await store.settle("o2", 5_000);
    expect((await store.claimOverageReports(TS + 4, 10)).map((r) => [r.reportId, r.cents])).toEqual([["ovr_cmt_1_20000", 1]]);
  });

  it("applies adjustments once, stops a debit at zero, and converges charge refunds on a high-water mark", async () => {
    const store = open();
    await store.credit("prn_a", "evt_1", 10_000_000, "stripe_checkout", TS);
    const credit = await store.adjust({ entryId: "adj:1", principalId: "prn_a", micros: 1_000_000, kind: "credit", reason: "goodwill", ts: TS });
    expect(credit).toEqual({ outcome: "applied", adjustment: expect.objectContaining({ appliedMicros: 1_000_000 }) });
    expect((await store.adjust({ entryId: "adj:1", principalId: "prn_a", micros: 1_000_000, kind: "credit", reason: "goodwill", ts: TS })).outcome).toBe("duplicate");
    expect((await store.wallet("prn_a")).balanceMicros).toBe(11_000_000);
    // A refund of $4 via the admin route, then the webhook carrying the same cumulative total: one debit.
    expect(await store.recordChargeRefund("prn_a", "ch_1", 4_000_000, TS)).toEqual({ debitedMicros: 4_000_000 });
    expect(await store.recordChargeRefund("prn_a", "ch_1", 4_000_000, TS + 1)).toEqual({ debitedMicros: 0 });
    // A later partial refund raises the mark by the difference only.
    expect(await store.recordChargeRefund("prn_a", "ch_1", 5_000_000, TS + 2)).toEqual({ debitedMicros: 1_000_000 });
    expect((await store.wallet("prn_a")).balanceMicros).toBe(6_000_000);
    // A debit larger than the balance stops at zero and records what it applied.
    const over = await store.adjust({ entryId: "adj:2", principalId: "prn_a", micros: -9_000_000, kind: "refund", reason: "refund", ts: TS });
    expect(over.adjustment.appliedMicros).toBe(-6_000_000);
    expect((await store.wallet("prn_a")).balanceMicros).toBe(0);
    const inputs = await store.statementInputs("prn_a", "2026-10", TS);
    expect(inputs.adjustments.map((a) => a.entryId).sort()).toEqual(["adj:1", "adj:2", chargeRefundEntryId("ch_1", 4_000_000), chargeRefundEntryId("ch_1", 5_000_000)].sort());
    expect(inputs.credits.map((c) => c.topupId)).toEqual(["evt_1"]);
  });

  it("tracks Checkout sessions: completed never expires, expiry is once", async () => {
    const store = open();
    await store.recordCheckoutSession("cs_1", "prn_a", 5_000_000, TS);
    await store.recordCheckoutSession("cs_2", "prn_a", 5_000_000, TS);
    expect(await store.setCheckoutStatus("cs_1", "completed", TS)).toBe(true);
    expect(await store.setCheckoutStatus("cs_1", "expired", TS)).toBe(false);
    expect(await store.setCheckoutStatus("cs_2", "expired", TS)).toBe(true);
    expect(await store.setCheckoutStatus("cs_2", "expired", TS)).toBe(false);
    expect(await store.setCheckoutStatus("cs_none", "expired", TS)).toBe(false);
    expect((await store.checkoutSession("cs_1"))?.status).toBe("completed");
    await store.recordPayment("pi_1", "prn_a", "cs_1", 5_000_000, TS);
    expect(await store.paymentPrincipal("pi_1")).toBe("prn_a");
    expect(await store.paymentPrincipal("pi_x")).toBeUndefined();
  });

  it("resolves a dispute once and builds statement inputs per unit with free usage", async () => {
    const store = open();
    await store.createPrincipal({ principalId: "prn_a", tenantId: "prn_a", createdAt: TS });
    await store.credit("prn_a", "grant", 1_000_000, "signup_grant", TS);
    await store.reserve(reserveInput({ receiptId: "r1", micros: 500 }));
    await store.settle("r1", 500);
    await store.reserve(reserveInput({ receiptId: "r2", micros: 0, unit: "read", name: "state.commitments.list" }));
    await store.reserve(reserveInput({ receiptId: "r3", micros: 500 }));
    const opened = await store.dispute("r1", "prn_a", "counted twice", "dsp_1", TS + 5);
    expect(opened?.created).toBe(true);
    const resolved = await store.resolveDispute("dsp_1", "resolved", 500, TS + 10);
    expect(resolved).toEqual(expect.objectContaining({ status: "resolved", creditMicros: 500, resolvedAt: TS + 10 }));
    expect(await store.resolveDispute("dsp_1", "rejected", 0, TS + 20)).toEqual(expect.objectContaining({ status: "resolved", creditMicros: 500 }));
    const inputs = await store.statementInputs("prn_a", "2026-10", TS);
    expect(inputs.unsettled).toBe(1);
    expect(inputs.usage.find((u) => u.unit === "ingest")).toEqual(expect.objectContaining({ calls: 1, quantity: 1, listMicros: 500, chargedMicros: 500 }));
    expect(inputs.usage.find((u) => u.unit === "read")).toEqual(expect.objectContaining({ calls: 1, chargedMicros: 0 }));
    expect(inputs.disputes).toEqual([expect.objectContaining({ disputeId: "dsp_1", status: "resolved", creditMicros: 500 })]);
    // A stored statement is written once.
    const first = await store.saveStatement({ principalId: "prn_a", month: "2026-09", body: "{\"a\":1}", hash: "sha256:a", closedAt: TS });
    const second = await store.saveStatement({ principalId: "prn_a", month: "2026-09", body: "{\"a\":2}", hash: "sha256:b", closedAt: TS + 1 });
    expect(second).toEqual(first);
  });

  it("links licences and grants the Pro bundle once per subject per month", async () => {
    const store = open();
    await store.linkLicence({ licenceSubject: "lic_pro", principalId: "prn_a", plan: "pro", verifiedAt: TS });
    await store.linkLicence({ licenceSubject: "lic_std", principalId: "prn_b", plan: "standard", verifiedAt: TS });
    await store.linkLicence({ licenceSubject: "lic_old", principalId: "prn_c", plan: "pro", verifiedAt: TS - 60 * 86_400_000 });
    expect(await grantProBundle(store, TS)).toEqual({ checked: 1, granted: 1 });
    expect(await grantProBundle(store, TS + 1_000)).toEqual({ checked: 1, granted: 0 });
    expect((await store.wallet("prn_a")).balanceMicros).toBe(PRO_BUNDLE_MICROS);
    // Relinking the subject to another principal in the same month does not grant again.
    await store.linkLicence({ licenceSubject: "lic_pro", principalId: "prn_d", plan: "pro", verifiedAt: TS + 2_000 });
    expect((await grantProBundle(store, TS + 3_000)).granted).toBe(0);
    // Next month: a new grant.
    const nov = Date.parse("2026-11-02T00:00:00Z");
    expect((await grantProBundle(store, nov)).granted).toBe(1);
    expect((await store.wallet("prn_d")).balanceMicros).toBe(PRO_BUNDLE_MICROS);
    expect((await store.statementInputs("prn_d", "2026-11", nov)).credits.map((c) => c.topupId)).toEqual([proGrantId("lic_pro", "2026-11")]);
  });
});

// --- HTTP ---

const WEBHOOK_SECRET = "whsec_test";

function fakeStripe() {
  const calls: Array<{ url: string; idempotencyKey?: string; form: URLSearchParams }> = [];
  let refunded = 0;
  const fetch: StripeFetch = (url, init) => {
    const form = new URLSearchParams(init.body);
    calls.push({ url, ...(init.headers["idempotency-key"] ? { idempotencyKey: init.headers["idempotency-key"] } : {}), form });
    if (url.endsWith("/v1/subscriptions")) return Promise.resolve({ status: 200, json: () => Promise.resolve({ id: "sub_1", items: { data: [{ id: "si_1" }] } }) });
    if (url.endsWith("/v1/refunds")) {
      refunded += Number(form.get("amount"));
      return Promise.resolve({ status: 200, json: () => Promise.resolve({ id: `re_${calls.length}`, charge: { id: form.get("charge"), amount_refunded: refunded } }) });
    }
    if (url.includes("/usage_records")) return Promise.resolve({ status: 200, json: () => Promise.resolve({ id: "mbur_1" }) });
    return Promise.resolve({ status: 200, json: () => Promise.resolve({ id: `cs_test_${calls.length}`, url: "https://checkout.stripe.test/c/pay/x" }) });
  };
  return { calls, fetch };
}

async function adminKey(rig: AgentRig, principalId = "prn_admin", scopes: AgentScope[] = ["admin"]): Promise<string> {
  const { key, prefix } = generateAgentKey("live");
  await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: rig.now().getTime() });
  await rig.store.insertKey({ keyId: `ak_${principalId}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: rig.now().getTime() });
  return key;
}

function signed(rig: AgentRig, event: unknown): { body: string; signature: string } {
  const body = JSON.stringify(event);
  const at = Math.floor(rig.now().getTime() / 1000);
  return { body, signature: `t=${at},v1=${createHmac("sha256", WEBHOOK_SECRET).update(`${at}.${body}`).digest("hex")}` };
}

function webhook(rig: AgentRig, event: unknown): Promise<Response> {
  const s = signed(rig, event);
  return Promise.resolve(rig.app.request("/v1/stripe/webhook", { method: "POST", headers: { "content-type": "application/json", "stripe-signature": s.signature }, body: s.body }));
}

const balanceOf = async (rig: AgentRig, key: string): Promise<number> => ((await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number }).balance_micro_usd;

describe("GET /v1/statements", () => {
  it("aggregates the open month per unit, free units at $0, as JSON and CSV", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "3" })).status).toBe(200);
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "state.commitments.list" })).status).toBe(200);
    const res = await authed(rig, key, "/v1/statements?month=2026-10");
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown> & { usage: Array<Record<string, unknown>>; totals: Record<string, number>; credits: Array<Record<string, unknown>> };
    expect(body.status).toBe("open");
    expect(body.usage.find((u) => u.unit === "ingest")).toEqual(expect.objectContaining({ calls: 1, quantity: 3, charged_micro_usd: 3 * RATE_MICROS.ingest, free: false }));
    expect(body.usage.find((u) => u.unit === "read")).toEqual(expect.objectContaining({ calls: 1, charged_micro_usd: 0, free: true }));
    expect(body.credits).toEqual([expect.objectContaining({ source: "signup_grant", amount_micro_usd: FREE_ALLOWANCE_MICROS })]);
    expect(res.headers.get("x-statement-hash")).toBe(String(body.hash));
    const csv = await authed(rig, key, "/v1/statements?month=2026-10&format=csv");
    expect(csv.headers.get("content-type")).toContain("text/csv");
    expect(await csv.text()).toContain("usage,ingest,1,3,1500,1500,0,billed");
    expect((await authed(rig, key, "/v1/statements?month=2026-13")).status).toBe(400);
    expect((await authed(rig, key, "/v1/statements?month=2027-01")).status).toBe(400);
    expect((await authed(rig, key, "/v1/statements?format=xml")).status).toBe(400);
  });

  it("is immutable once the month closes: stored once, same hash after later ledger changes", async () => {
    const rig = makeAgentRig();
    const { key, principalId } = await registerAgent(rig);
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest" });
    rig.setNow(new Date("2026-11-01T02:00:00Z"));
    const first = (await (await authed(rig, key, "/v1/statements?month=2026-10")).json()) as Record<string, unknown>;
    expect(first.status).toBe("closed");
    expect(rig.logLines.some((l) => l.includes("event=statement_closed"))).toBe(true);
    // A late write dated inside October (as a settle of a straggler would be) changes nothing.
    await rig.store.credit(principalId, "late", 1, "signup_grant", Date.parse("2026-10-30T00:00:00Z"));
    rig.setNow(new Date("2026-12-15T00:00:00Z"));
    const again = (await (await authed(rig, key, "/v1/statements?month=2026-10")).json()) as Record<string, unknown>;
    expect(again).toEqual(first);
    expect(statementHash(again)).toBe(again.hash);
  });
});

describe("commits", () => {
  it("admin creates a commit idempotently; calls are discounted, commit-funded and shown with the list price", async () => {
    const stripe = fakeStripe();
    const rig = makeAgentRig({ stripe: { secretKey: "sk_test", webhookSecret: WEBHOOK_SECRET, fetch: stripe.fetch, overagePriceId: "price_over" } });
    const { key, principalId } = await registerAgent(rig);
    const admin = await adminKey(rig);
    const body = { subject: "principal", subject_id: principalId, amount_micro_usd: 1_000_000, discount_bps: 2_000, starts_at: "2026-10-01T00:00:00Z", ends_at: "2027-10-01T00:00:00Z", stripe_customer_id: "cus_1" };
    // Not an admin: 403. No Idempotency-Key: 400. Out-of-range discount: 400.
    expect((await authed(rig, key, "/v1/admin/commits", { method: "POST", body, headers: { "idempotency-key": "c-1" } })).status).toBe(403);
    expect((await authed(rig, admin, "/v1/admin/commits", { method: "POST", body })).status).toBe(400);
    expect((await authed(rig, admin, "/v1/admin/commits", { method: "POST", body: { ...body, discount_bps: 3_000 }, headers: { "idempotency-key": "c-x" } })).status).toBe(400);
    expect((await authed(rig, admin, "/v1/admin/commits", { method: "POST", body: { ...body, discount_bps: 900 }, headers: { "idempotency-key": "c-y" } })).status).toBe(400);
    const created = await authed(rig, admin, "/v1/admin/commits", { method: "POST", body, headers: { "idempotency-key": "c-1" } });
    expect(created.status).toBe(201);
    const c = ((await created.json()) as { commit: Record<string, unknown> }).commit;
    expect(c).toEqual(expect.objectContaining({ discount_bps: 2_000, amount_micro_usd: 1_000_000, remaining_micro_usd: 1_000_000, overage_billing: true, active: true }));
    const sub = stripe.calls.find((x) => x.url.endsWith("/v1/subscriptions"))!;
    expect(sub.form.get("items[0][price]")).toBe("price_over");
    expect(sub.idempotencyKey).toBe(`commit:${String(c.commit_id)}:subscription`);
    const replay = await authed(rig, admin, "/v1/admin/commits", { method: "POST", body, headers: { "idempotency-key": "c-1" } });
    expect(replay.status).toBe(200);
    const overlap = await authed(rig, admin, "/v1/admin/commits", { method: "POST", body: { ...body, stripe_customer_id: undefined, starts_at: "2027-01-01T00:00:00Z", ends_at: "2027-12-01T00:00:00Z" }, headers: { "idempotency-key": "c-2" } });
    expect(overlap.status).toBe(409);

    // The quote and the dry run show the discount and the list price; the real call settles at the dry run.
    const quote = (await (await authed(rig, key, "/v1/quote", { method: "POST", body: { name: "memory.ingest", quantity: 10 } })).json()) as { quote: Record<string, unknown> };
    expect(quote.quote).toEqual(expect.objectContaining({ cost_micro_usd: 4_000, list_cost_micro_usd: 5_000, discount_bps: 2_000 }));
    const headers = { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "10" };
    const dry = (await (await mcp(rig, key, { ...headers, "mcp-dry-run": "true" })).json()) as { quote: Record<string, unknown> };
    expect(dry.quote).toEqual(expect.objectContaining({ cost_micro_usd: 4_000, list_cost_micro_usd: 5_000 }));
    const before = await balanceOf(rig, key);
    const res = await mcp(rig, key, headers);
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("4000");
    expect(res.headers.get("x-usage-list-cost-micro-usd")).toBe("5000");
    // Commit-funded: the wallet is untouched, the commit is drawn down.
    expect(await balanceOf(rig, key)).toBe(before);
    const mine = ((await (await authed(rig, key, "/v1/commit")).json()) as { commit: Record<string, unknown> }).commit;
    expect(mine).toEqual(expect.objectContaining({ drawn_micro_usd: 4_000, remaining_micro_usd: 996_000 }));
    const statement = (await (await authed(rig, key, "/v1/statements")).json()) as { totals: Record<string, number>; commit: Record<string, unknown> };
    expect(statement.totals).toEqual(expect.objectContaining({ list_micro_usd: 5_000, charged_micro_usd: 4_000, discount_micro_usd: 1_000, commit_funded_micro_usd: 4_000 }));
    expect(statement.commit).toEqual(expect.objectContaining({ discount_bps: 2_000, drawn_micro_usd: 4_000 }));
  });

  it("reports overage to Stripe as usage records with the report id as the idempotency key", async () => {
    const stripe = fakeStripe();
    const rig = makeAgentRig({ stripe: { secretKey: "sk_test", webhookSecret: WEBHOOK_SECRET, fetch: stripe.fetch, overagePriceId: "price_over" } });
    const { key, principalId } = await registerAgent(rig);
    const admin = await adminKey(rig);
    await authed(rig, admin, "/v1/admin/commits", { method: "POST", headers: { "idempotency-key": "c-o" }, body: { subject_id: principalId, amount_micro_usd: 1, discount_bps: 1_000, starts_at: "2026-10-01T00:00:00Z", ends_at: "2027-10-01T00:00:00Z", stripe_customer_id: "cus_1" } });
    for (let i = 0; i < 5; i++) await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "10" });
    // 5 × (5000 × 0.9) = 22,500 µ$ drawn, 22,499 above the 1 µ$ commit → 2 whole cents.
    const out = await reportCommitOverage(rig.store, { secretKey: "sk_test", fetch: stripe.fetch }, rig.now().getTime());
    expect(out).toEqual({ claimed: 1, sent: 1, failed: 0 });
    const usage = stripe.calls.filter((x) => x.url.includes("/usage_records"));
    expect(usage).toHaveLength(1);
    expect(usage[0]!.url).toContain("/v1/subscription_items/si_1/usage_records");
    expect(usage[0]!.form.get("quantity")).toBe("2");
    expect(usage[0]!.idempotencyKey).toMatch(/^ovr_cmt_[0-9a-f]+_0$/);
    expect(await reportCommitOverage(rig.store, { secretKey: "sk_test", fetch: stripe.fetch }, rig.now().getTime())).toEqual({ claimed: 0, sent: 0, failed: 0 });
  });
});

describe("Stripe webhooks: charge.refunded and checkout.session.expired", () => {
  it("debits a refund once whichever path records it, and closes an expired session", async () => {
    const stripe = fakeStripe();
    const rig = makeAgentRig({ stripe: { secretKey: "sk_test", webhookSecret: WEBHOOK_SECRET, fetch: stripe.fetch } });
    const { key, principalId } = await registerAgent(rig);
    const admin = await adminKey(rig);
    const topup = (await (await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: 10_000_000 } })).json()) as { session_id: string };
    const completed = { id: "evt_c", type: "checkout.session.completed", data: { object: { id: topup.session_id, payment_status: "paid", amount_total: 1_000, currency: "usd", client_reference_id: principalId, payment_intent: "pi_1" } } };
    expect(await (await webhook(rig, completed)).json()).toEqual({ received: true, credited: true });
    expect(await balanceOf(rig, key)).toBe(FREE_ALLOWANCE_MICROS + 10_000_000);
    expect((await rig.store.checkoutSession(topup.session_id))?.status).toBe("completed");
    // Expiry after completion changes nothing.
    const expiredLate = await webhook(rig, { id: "evt_x", type: "checkout.session.expired", data: { object: { id: topup.session_id } } });
    expect(await expiredLate.json()).toEqual({ received: true, credited: false, expired: false });

    // Admin refunds $3 of the charge through Stripe: debited once.
    const refund = await authed(rig, admin, "/v1/admin/adjustments", { method: "POST", headers: { "idempotency-key": "r-1" }, body: { principal_id: principalId, kind: "refund", amount_micro_usd: 3_000_000, charge_id: "ch_1", reason: "refund" } });
    expect(refund.status).toBe(201);
    expect(await refund.json()).toEqual(expect.objectContaining({ debited_micro_usd: 3_000_000, charge_id: "ch_1" }));
    expect(stripe.calls.find((x) => x.url.endsWith("/v1/refunds"))?.form.get("amount")).toBe("300");
    // The charge.refunded event for the same total debits nothing more.
    const event = { id: "evt_r1", type: "charge.refunded", data: { object: { id: "ch_1", amount_refunded: 300, currency: "usd", payment_intent: "pi_1" } } };
    expect(await (await webhook(rig, event)).json()).toEqual(expect.objectContaining({ refunded: true, debited_micro_usd: 0 }));
    // A dashboard refund raises the total to $5: the webhook debits the $2 difference, once.
    const more = { id: "evt_r2", type: "charge.refunded", data: { object: { id: "ch_1", amount_refunded: 500, currency: "usd", payment_intent: "pi_1" } } };
    expect(await (await webhook(rig, more)).json()).toEqual(expect.objectContaining({ debited_micro_usd: 2_000_000 }));
    expect(await (await webhook(rig, more)).json()).toEqual(expect.objectContaining({ debited_micro_usd: 0 }));
    expect(await balanceOf(rig, key)).toBe(FREE_ALLOWANCE_MICROS + 5_000_000);

    const second = (await (await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: 5_000_000 } })).json()) as { session_id: string };
    const expired = await webhook(rig, { id: "evt_e", type: "checkout.session.expired", data: { object: { id: second.session_id } } });
    expect(await expired.json()).toEqual({ received: true, credited: false, expired: true });
    expect((await rig.store.checkoutSession(second.session_id))?.status).toBe("expired");
    expect(rig.logLines.join("\n")).not.toContain("ch_1");
    expect(rig.logLines.join("\n")).not.toContain(principalId);
  });
});

describe("admin adjustments and dispute resolution", () => {
  it("credits once per Idempotency-Key and resolves a dispute with a credit entry", async () => {
    const rig = makeAgentRig();
    const { key, principalId } = await registerAgent(rig);
    const admin = await adminKey(rig);
    const credit = { principal_id: principalId, kind: "credit", amount_micro_usd: 2_000_000, reason: "service_credit" };
    expect((await authed(rig, admin, "/v1/admin/adjustments", { method: "POST", body: credit })).status).toBe(400);
    expect((await authed(rig, admin, "/v1/admin/adjustments", { method: "POST", body: { ...credit, reason: "because" }, headers: { "idempotency-key": "a-0" } })).status).toBe(400);
    expect((await authed(rig, admin, "/v1/admin/adjustments", { method: "POST", body: credit, headers: { "idempotency-key": "a-1" } })).status).toBe(201);
    expect((await authed(rig, admin, "/v1/admin/adjustments", { method: "POST", body: credit, headers: { "idempotency-key": "a-1" } })).status).toBe(200);
    expect(await balanceOf(rig, key)).toBe(FREE_ALLOWANCE_MICROS + 2_000_000);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest" });
    const receiptId = res.headers.get("x-receipt-id")!;
    const dispute = (await (await authed(rig, key, `/v1/receipts/${receiptId}/dispute`, { method: "POST", body: { reason: "wrong count" } })).json()) as { dispute_id: string };
    expect((await authed(rig, key, `/v1/admin/disputes/${dispute.dispute_id}/resolve`, { method: "POST", body: { outcome: "credit" } })).status).toBe(403);
    const resolved = await authed(rig, admin, `/v1/admin/disputes/${dispute.dispute_id}/resolve`, { method: "POST", body: { outcome: "credit" } });
    expect(await resolved.json()).toEqual(expect.objectContaining({ status: "resolved", credit_micro_usd: RATE_MICROS.ingest }));
    // Replaying the resolution credits nothing more.
    await authed(rig, admin, `/v1/admin/disputes/${dispute.dispute_id}/resolve`, { method: "POST", body: { outcome: "credit" } });
    expect(await balanceOf(rig, key)).toBe(FREE_ALLOWANCE_MICROS + 2_000_000);
    const statement = (await (await authed(rig, key, "/v1/statements")).json()) as { disputes: Array<Record<string, unknown>>; adjustments: Array<Record<string, unknown>> };
    expect(statement.disputes).toEqual([expect.objectContaining({ dispute_id: dispute.dispute_id, status: "resolved", credit_micro_usd: RATE_MICROS.ingest })]);
    expect(statement.adjustments.map((a) => a.reason).sort()).toEqual(["dispute", "service_credit"]);
    // The free-text reason never reaches the statement.
    expect(JSON.stringify(statement)).not.toContain("wrong count");
  });
});

describe("ShogunAI Pro bundle (E14-T3)", () => {
  it("links a licence with its token and grants $20 for a Pro licence once a month", async () => {
    const keys = await makeKeys();
    const rig = makeAgentRig({ licensePublicKey: keys.publicKey });
    const { key, principalId } = await registerAgent(rig);
    const iat = Math.floor(rig.now().getTime() / 1000);
    const pro = await signLicense(keys.privateKey, { sub: "lic_pro_1", plan: "pro", iat });
    expect((await authed(rig, key, "/v1/agents/me/licence", { method: "POST" })).status).toBe(400);
    expect((await authed(rig, key, "/v1/agents/me/licence", { method: "POST", headers: { "x-shogun-licence": "v1.nope.nope" } })).status).toBe(401);
    const linked = await authed(rig, key, "/v1/agents/me/licence", { method: "POST", headers: { "x-shogun-licence": pro } });
    expect(linked.status).toBe(200);
    expect(await linked.json()).toEqual(expect.objectContaining({ linked: true, plan: "pro", pro_bundle: { month: "2026-10", amount_micro_usd: PRO_BUNDLE_MICROS, credited_now: true } }));
    const again = (await (await authed(rig, key, "/v1/agents/me/licence", { method: "POST", headers: { "x-shogun-licence": pro } })).json()) as { pro_bundle: { credited_now: boolean } };
    expect(again.pro_bundle.credited_now).toBe(false);
    expect(await balanceOf(rig, key)).toBe(FREE_ALLOWANCE_MICROS + PRO_BUNDLE_MICROS);
    expect((await grantProBundle(rig.store, rig.now().getTime())).granted).toBe(0);
    const standard = await signLicense(keys.privateKey, { sub: "lic_std", plan: "standard", iat });
    expect(((await (await authed(rig, key, "/v1/agents/me/licence", { method: "POST", headers: { "x-shogun-licence": standard } })).json()) as { pro_bundle: unknown }).pro_bundle).toBeNull();
    expect(rig.logLines.join("\n")).not.toContain("lic_pro_1");
    expect(principalId).toMatch(/^prn_/);
  });
});

describe("billing notifications", () => {
  it("emits cap.reached on a cap refusal and wallet.low_balance on the crossing, deduplicated, never logged", async () => {
    const notifier = new InMemoryNotifier();
    const rig = makeAgentRig({ notifier, caps: { key: { day: 4_500_000, month: 20_000_000 }, principal: { day: 50_000_000, month: 100_000_000 } } });
    const { key, principalId } = await registerAgent(rig);
    // $5 allowance; each call costs 1000 × 500 µ$ = $0.50. The ninth leaves $0.50: the crossing.
    const big = { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "1000" };
    for (let i = 0; i < 9; i++) expect((await mcp(rig, key, big)).status).toBe(200);
    // $4.50 spent: the key's $4.50/day cap refuses the next call (the wallet could still pay).
    expect((await mcp(rig, key, big)).status).toBe(402);
    expect((await mcp(rig, key, big)).status).toBe(402);
    const events = notifier.events(principalId);
    const caps = events.filter((e) => e.type === "cap.reached");
    expect(caps).toHaveLength(1);
    expect(caps[0]).toEqual(expect.objectContaining({ data: expect.objectContaining({ scope: "key", period: "day", period_key: "2026-10-25", limit_micro_usd: 4_500_000 }) }));
    const low = events.filter((e) => e.type === "wallet.low_balance");
    expect(low).toHaveLength(1);
    expect(low[0]).toEqual(expect.objectContaining({ data: { balance_micro_usd: 500_000, threshold_micro_usd: 1_000_000, topup_url: `${BASE}/v1/wallet/topup` } }));
    expect(rig.logLines.join("\n")).not.toContain("low_balance");
  });
});
