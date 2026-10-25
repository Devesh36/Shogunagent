import { readFileSync } from "node:fs";
import { describe, expect, it, vi } from "vitest";
import { SqlAsrStore } from "../src/asr-store.js";
import { asrApp } from "../src/asr-app.js";
import { sqliteDouble } from "./sql-double.js";
import { makeKeys, signLicense } from "./helpers.js";
async function rig() {
  const db = sqliteDouble();
  const sql = readFileSync(new URL("../sql/asr-store.sql", import.meta.url), "utf8");
  for (const statement of sql.replace(/--[^\n]*/g, "").split(";").filter(s => s.trim())) await db.query(statement);
  let now = Date.UTC(2026, 9, 17);
  const store = new SqlAsrStore(db, () => now, 100_000_000);
  return { db, store, advance: (ms: number) => { now += ms; } };
}
describe("speech admission", () => {
  it("serializes concurrent grants and applies device pacing", async () => {
    const { store } = await rig();
    const results = await Promise.allSettled(Array.from({ length: 8 }, () => store.reserve("lic", "dev")));
    expect(results.filter(r => r.status === "fulfilled")).toHaveLength(1);
  });
  it("limits simultaneous sessions across devices", async () => {
    const { store } = await rig();
    await store.reserve("lic", "a"); await store.reserve("lic", "b");
    await expect(store.reserve("lic", "c")).rejects.toThrow("concurrency_limited");
  });
  it("preserves reservations after crashes and stops at 20 hours", async () => {
    const { store, advance } = await rig();
    for (let i = 0; i < 20; i++) { await store.reserve("lic", "a"); advance(3_661_000); }
    await expect(store.reserve("lic", "a")).rejects.toThrow("monthly_cap");
  });
  it("finishes once using server time and refuses another device's refund", async () => {
    const { store, db, advance } = await rig();
    const ticket = await store.reserve("lic", "a"); advance(90_000);
    await store.finish("lic", "b", ticket.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(3600);
    await store.finish("lic", "a", ticket.id); await store.finish("lic", "a", ticket.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(90);
  });
  it("never refunds below the last grant's window: an immediate finish keeps 60 s per stream", async () => {
    const { store, db, advance } = await rig();
    const ticket = await store.reserve("lic", "a", 2);
    const reservedMicros = Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros);
    advance(1_000);
    await store.finish("lic", "a", ticket.id);
    // The JWT could still open sockets for 59 more seconds: billed 60 s x 2 streams, not 1 s.
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(120);
    const spent = Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros);
    expect(spent).toBe(reservedMicros - Math.floor(reservedMicros * (7200 - 120) / 7200));
    // The concurrency slot is still freed: a supported Mac's next session is not blocked.
    advance(20_000);
    await expect(store.reserve("lic", "b", 2)).resolves.toBeDefined();
  });
  it("counts the window from the latest refresh, and bills elapsed time once it has passed", async () => {
    const { store, db, advance } = await rig();
    const ticket = await store.reserve("lic", "a");
    advance(600_000);
    await store.refresh("lic", "a", ticket.id); // a fresh JWT at 600 s
    advance(5_000);
    await store.finish("lic", "a", ticket.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(660);
    const other = await store.reserve("lic", "b");
    advance(1_800_000);
    await store.finish("lic", "b", other.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(660 + 1800);
  });
  it("refuses blocked accounts and ledger failures", async () => {
    const { store, db } = await rig();
    db.failNext(1); await expect(store.reserve("lic", "a")).rejects.toThrow();
    await db.query("INSERT INTO relay_asr_subjects VALUES('lic',1)");
    await expect(store.reserve("lic", "a")).rejects.toThrow("blocked");
  });
  it("shares the relay global breaker and rolls back denied admission", async () => {
    const { db } = await rig();
    const store = new SqlAsrStore(db, () => Date.UTC(2026, 9, 17), 500_000);
    await expect(store.reserve("lic", "a")).rejects.toThrow("spend_cap");
    expect(await db.query("SELECT * FROM relay_asr_months")).toHaveLength(0);
  });
  it("accounts both meeting channels with one grant and frees both slots on finish", async () => {
    const { db, store, advance } = await rig();
    const ticket = await store.reserve("lic", "a", 2);
    await expect(store.reserve("lic", "b")).rejects.toThrow("concurrency_limited");
    advance(90_000);
    await store.finish("lic", "a", ticket.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(180);
    expect((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros).toBeGreaterThan(0);
    await expect(store.reserve("lic", "b")).resolves.toBeDefined();
  });
  it("refreshes only the owning active reservation without extending quota or deadline", async () => {
    const { store, db, advance } = await rig();
    const ticket = await store.reserve("lic", "a", 2);
    await expect(store.refresh("lic", "a", ticket.id)).rejects.toThrow("rate_limited");
    advance(60_000);
    await expect(store.refresh("lic", "b", ticket.id)).rejects.toThrow("session_expired");
    await store.refresh("lic", "a", ticket.id);
    expect((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds).toBe(7200);
    await expect(store.refresh("lic", "a", ticket.id)).rejects.toThrow("rate_limited");
    advance(3_600_000);
    await expect(store.refresh("lic", "a", ticket.id)).rejects.toThrow("session_expired");
  });
  it("keeps an operator rollout closed to other valid licences", async () => {
    const { store } = await rig(); const keys = await makeKeys();
    const grant = vi.fn(); const reserve = vi.spyOn(store, "reserve");
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key", []);
    const authorization = `Bearer ${await signLicense(keys.privateKey)}`;
    for (const route of ["grant", "refresh", "finish"]) {
      const response = await app.request(`/${route}`, { method: "POST", headers: { authorization } });
      expect(response.status).toBe(403);
      expect(response.headers.get("cache-control")).toBe("no-store");
    }
    expect(reserve).not.toHaveBeenCalled(); expect(grant).not.toHaveBeenCalled();
  });
  it("authenticates before any paid mint and returns an uncacheable grant", async () => {
    const { store } = await rig(); const keys = await makeKeys();
    const grant = vi.fn().mockResolvedValue({ access_token: "test.jwt.signature", expires_in: 60 });
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key");
    expect((await app.request("/grant", { method: "POST" })).status).toBe(401);
    expect(grant).not.toHaveBeenCalled();
    const response = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await signLicense(keys.privateKey)}` } });
    expect(response.status).toBe(200); expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await response.json()).toMatchObject({ expires_in: 60, max_session_seconds: 3600 });
  });
});

describe("company speech for trials (owner decision 2026-10-22)", () => {
  const trialToken = (keys: Awaited<ReturnType<typeof makeKeys>>, device: string, acc?: string) =>
    signLicense(keys.privateKey, { sub: `trial_${device}`, plan: "trial", status: "trialing", device, ...(acc ? { acc } : {}) });
  it("admits paid licences and trials without an allowlist, metering a trial per account", async () => {
    const { store } = await rig(); const keys = await makeKeys();
    const grant = vi.fn().mockResolvedValue({ access_token: "test.jwt.signature", expires_in: 60 });
    const reserve = vi.spyOn(store, "reserve");
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key");
    const trial = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await trialToken(keys, "d1", "acc_1")}` } });
    expect(trial.status).toBe(200);
    expect(reserve).toHaveBeenLastCalledWith("account:acc_1", "d1", 1, undefined);
    const paid = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await signLicense(keys.privateKey)}` } });
    expect(paid.status).toBe(200);
  });
  it("an empty allowlist admits nobody, before any reservation or mint", async () => {
    const { store } = await rig(); const keys = await makeKeys();
    const grant = vi.fn(); const reserve = vi.spyOn(store, "reserve");
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key", []);
    for (const token of [await trialToken(keys, "d1", "acc_1"), await signLicense(keys.privateKey)]) {
      expect((await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${token}` } })).status).toBe(403);
    }
    expect(reserve).not.toHaveBeenCalled(); expect(grant).not.toHaveBeenCalled();
  });
  it("admits a trial the allowlist names, by licence id or by account, and meters it per account", async () => {
    const { store } = await rig(); const keys = await makeKeys();
    const grant = vi.fn().mockResolvedValue({ access_token: "test.jwt.signature", expires_in: 60 });
    const reserve = vi.spyOn(store, "reserve");
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key", ["account:acc_1", "trial_d9"]);
    const ok = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await trialToken(keys, "d1", "acc_1")}` } });
    expect(ok.status).toBe(200);
    expect(reserve).toHaveBeenLastCalledWith("account:acc_1", "d1", 1, undefined);
    expect((await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await trialToken(keys, "d9")}` } })).status).toBe(200);
    expect((await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await trialToken(keys, "d2", "acc_2")}` } })).status).toBe(403);
  });
  it("reserves a trial's speech only against its share of the global cap", async () => {
    const { db } = await rig(); const keys = await makeKeys();
    // Global cap $1.50, trial share 0.25 = $0.375: one one-hour stream ($1 reserved) does not fit.
    const store = new SqlAsrStore(db, () => Date.UTC(2026, 9, 17), 1_500_000);
    const grant = vi.fn().mockResolvedValue({ access_token: "test.jwt.signature", expires_in: 60 });
    const { planSpendCapMicros } = await import("../src/app.js");
    const app = asrApp(keys.publicKey, store, { grant }, "test-only-tag-key", ["account:acc_1"], undefined,
      { spendCapFor: license => planSpendCapMicros(license, 1_500_000, 0.25) });
    const trial = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await trialToken(keys, "d1", "acc_1")}` } });
    expect(trial.status).toBe(429);
    expect(await trial.json()).toEqual({ error: "spend_cap" });
    expect(grant).not.toHaveBeenCalled();
    // The store never lets a caller's cap exceed its own.
    await expect(store.reserve("lic", "a", 1, 50_000_000)).resolves.toBeDefined();
    await expect(store.reserve("lic2", "b", 1, 50_000_000)).rejects.toThrow("spend_cap");
  });
});

describe("provider speech usage reconciliation", () => {
  it("never refunds provider usage or debits a repeated/stale snapshot twice", async () => {
    const { store, db, advance } = await rig();
    const usage = { day: { seconds: 120, unsupported: false }, month: { seconds: 120, unsupported: false } };
    const ticket = await store.reserve("lic", "dev"); advance(30_000);
    await Promise.all([store.reconcile("lic", usage), store.reconcile("lic", usage)]);
    await store.finish("lic", "dev", ticket.id);
    const spent = Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros);
    expect(spent).toBeGreaterThanOrEqual(150 * 1_000_000 / 3600);
    await store.reconcile("lic", { day: { seconds: 10, unsupported: false }, month: { seconds: 10, unsupported: false } });
    expect(Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros)).toBe(spent);
  });
  it("refuses monthly provider excess even after client finish refunds", async () => {
    const { store } = await rig();
    await store.reconcile("lic", { day: { seconds: 1, unsupported: false }, month: { seconds: 72000, unsupported: false } });
    await expect(store.reserve("lic", "dev")).rejects.toThrow("monthly_cap");
  });
  it("adds reservations on top of the provider floor across devices", async () => {
    const { store, db } = await rig();
    await store.reconcile("lic", { day: { seconds: 1, unsupported: false }, month: { seconds: 71900, unsupported: false } });
    const ticket = await store.reserve("lic", "a");
    expect(ticket).toBeDefined();
    expect(Number((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds)).toBe(72000);
    await expect(store.reserve("lic", "b")).rejects.toThrow("monthly_cap");
  });
  it("alerts on unattributed project-wide non-speech usage without switching speech off for everyone (H3)", async () => {
    const { db } = await rig();
    let now = Date.UTC(2026, 9, 17);
    const alerts: string[] = [];
    const store = new SqlAsrStore(db, () => now, 100_000_000, 1_000_000, line => alerts.push(line));
    const ticket = await store.reserve("lic", "dev"); now += 60_000;
    await store.reconcile("lic", { day: { seconds: 1, unsupported: true }, month: { seconds: 1, unsupported: false } });
    expect(alerts).toEqual([expect.stringMatching(/^ALERT speech_project_non_speech_usage/)]);
    expect(alerts.join()).not.toContain("lic");
    await expect(store.reserve("other", "dev")).resolves.toBeDefined();
    await expect(store.refresh("lic", "dev", ticket.id)).resolves.toBeUndefined();
  });
  it("blocks only the subject whose own tagged usage is non-speech, with an alert", async () => {
    const { db } = await rig();
    const alerts: string[] = [];
    const store = new SqlAsrStore(db, () => Date.UTC(2026, 9, 17), 100_000_000, 1_000_000, line => alerts.push(line));
    await store.reconcile("lic", { day: { seconds: 1, unsupported: true }, month: { seconds: 1, unsupported: true } });
    await expect(store.reserve("lic", "dev")).rejects.toThrow("blocked");
    await expect(store.reserve("other", "dev")).resolves.toBeDefined();
    expect(alerts).toEqual(["ALERT speech_subject_blocked reason=tagged_non_speech_usage"]);
  });
  it("keeps the operator stop: a ('blocked','global') row refuses every grant and refresh", async () => {
    const { store, db, advance } = await rig();
    const ticket = await store.reserve("lic", "dev"); advance(60_000);
    await db.query("INSERT INTO relay_asr_provider_usage(scope,period,seconds) VALUES('blocked','global',1)");
    await expect(store.reserve("other", "dev")).rejects.toThrow("blocked");
    await expect(store.refresh("lic", "dev", ticket.id)).rejects.toThrow("blocked");
    await expect(store.finish("lic", "dev", ticket.id)).resolves.toBeUndefined();
  });
  it("paces grants and refreshes before the provider usage reads (no Deepgram call when refused)", async () => {
    const { store, advance } = await rig(); const keys = await makeKeys();
    const grant = vi.fn().mockResolvedValue({ access_token: "test.jwt.signature", expires_in: 60 });
    const beforeMint = vi.fn(async () => undefined);
    const app = asrApp(keys.publicKey, store, { grant }, "test-key", undefined, beforeMint);
    const authorization = `Bearer ${await signLicense(keys.privateKey)}`;
    const first = await app.request("/grant", { method: "POST", headers: { authorization }, body: "{}" });
    expect(first.status).toBe(200);
    const { session_id: id } = await first.json() as { session_id: string };
    expect(beforeMint).toHaveBeenCalledTimes(1);
    // Within 20 s of the mint: refused with 429 before any usage read or ledger write.
    const again = await app.request("/grant", { method: "POST", headers: { authorization }, body: "{}" });
    expect(again.status).toBe(429);
    expect(await again.json()).toEqual({ error: "rate_limited" });
    const refresh = await app.request("/refresh", { method: "POST", headers: { authorization }, body: JSON.stringify({ session_id: id }) });
    expect(refresh.status).toBe(429);
    expect(beforeMint).toHaveBeenCalledTimes(1);
    expect(grant).toHaveBeenCalledTimes(1);
    // After the pacing window the refresh reads usage and mints.
    advance(21_000);
    const later = await app.request("/refresh", { method: "POST", headers: { authorization }, body: JSON.stringify({ session_id: id }) });
    expect(later.status).toBe(200);
    expect(beforeMint).toHaveBeenCalledTimes(2);
  });
  it("answers an ended trial with 402 plan_required and a lapsed trial token with 401", async () => {
    const { store } = await rig(); const keys = await makeKeys(); const grant = vi.fn();
    const app = asrApp(keys.publicKey, store, { grant }, "test-key", ["trial_d1"]);
    const now = Math.floor(Date.now() / 1000);
    const ended = await signLicense(keys.privateKey, { sub: "trial_d1", plan: "trial", status: "trialing", device: "d1", iat: now - 7200, exp: now - 60, periodEnd: now - 60 });
    const lapsed = await signLicense(keys.privateKey, { sub: "trial_d1", plan: "trial", status: "trialing", device: "d1", iat: now - 7200, exp: now - 60, periodEnd: now + 86_400 });
    const res = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${ended}` }, body: "{}" });
    expect(res.status).toBe(402);
    expect(await res.json()).toEqual({ error: "plan_required" });
    expect((await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${lapsed}` }, body: "{}" })).status).toBe(401);
    expect(grant).not.toHaveBeenCalled();
  });
  it("does not mint when provider accounting is unavailable", async () => {
    const { store } = await rig(); const keys = await makeKeys(); const grant = vi.fn();
    const app = asrApp(keys.publicKey, store, { grant }, "test-key", undefined, async () => { throw new Error("provider unavailable"); });
    const response = await app.request("/grant", { method: "POST", headers: { authorization: `Bearer ${await signLicense(keys.privateKey)}` }, body: '{}' });
    expect(response.status).toBe(503); expect(grant).not.toHaveBeenCalled();
  });
});
