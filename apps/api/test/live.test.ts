import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it, vi } from "vitest";
import { meterSubject, verifyLicense } from "../src/auth.js";
import { createLiveApp, liveStream, upstreamBody } from "../src/live-app.js";
import { LIVE_LANES, liveConfig, type LiveConfig, type LiveLane } from "../src/live-config.js";
import { SqlLiveStore, LIVE_MAX_MS, type LiveAdmission } from "../src/live-store.js";
import { postgresClient, type SqlClient } from "../src/sql.js";
import { makeKeys, signLicense } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

export const configEnv = () => ({
  OPENAI_LIVE_API_KEY: "test-openai-not-real", GROQ_LIVE_API_KEY: "test-groq-not-real", LIVE_HMAC_KEY: "test-hmac-not-real-32-characters-long", LIVE_GLOBAL_DAILY_CAP_USD: "100",
  LIVE_LANE_CONFIG_JSON: JSON.stringify(Object.fromEntries(LIVE_LANES.map(lane => [lane, { model: lane === "dictation_cleanup" ? "test-groq-model" : "gpt-6-luna", effort: "low", maxOutputTokens: 100, inputMicrosPerToken: 1, outputMicrosPerToken: 2 }]))),
});
const schema = readFileSync(new URL("../sql/live-store.sql", import.meta.url), "utf8");
const openDbs: SqlClient[] = [];
afterEach(async () => { vi.useRealTimers(); await Promise.all(openDbs.splice(0).map(db => db.close())); });
function local() { const db = sqliteDouble(schema); openDbs.push(db); return db; }
const base: LiveAdmission = { subject: "lic_test", device: "dev-a", provider: "openai", lane: "chat", reserve: 100, daily: 1000, monthly: 2000, global: 10000 };
const tables = "relay_live_admin_actions,relay_live_requests,relay_live_counters,relay_live_subjects";
const backends: Array<[string, () => Promise<SqlClient>]> = [["SQLite SQL double", async () => local()]];
if (process.env.RELAY_LIVE_TEST_DATABASE_URL) backends.push(["real Postgres", async () => { const db = postgresClient(process.env.RELAY_LIVE_TEST_DATABASE_URL!); openDbs.push(db); await db.query(`TRUNCATE ${tables}`); return db; }]);

describe.each(backends)("live admission on %s", (_name, open) => {
  it("atomically admits only three user streams across devices", async () => {
    const store = new SqlLiveStore(await open());
    const outcomes = await Promise.allSettled(Array.from({ length: 10 }, (_, i) => store.reserve({ ...base, device: `dev-${i}` })));
    expect(outcomes.filter(o => o.status === "fulfilled")).toHaveLength(3);
    expect((await store.usage(base.subject)).daily).toBe(300);
  });
  it("shares a global provider cap across unrelated subjects", async () => {
    const store = new SqlLiveStore(await open());
    const outcomes = await Promise.allSettled(Array.from({ length: 8 }, (_, i) => store.reserve({ ...base, subject: `lic_other_${i}`, global: 200 })));
    expect(outcomes.filter(o => o.status === "fulfilled")).toHaveLength(2);
    expect(outcomes.filter(o => o.status === "rejected").every(o => o.status === "rejected" && o.reason.reason === "provider_budget_exceeded")).toBe(true);
  });
  it("never oversubscribes the user's daily budget with racing requests", async () => {
    const store = new SqlLiveStore(await open());
    const outcomes = await Promise.allSettled(Array.from({ length: 8 }, (_, i) => store.reserve({ ...base, device: `d${i}`, daily: 100 })));
    expect(outcomes.filter(o => o.status === "fulfilled")).toHaveLength(1);
    expect((await store.usage(base.subject)).daily).toBe(100);
  });
  it("refuses the third stream on a device, keeps budgets unchanged", async () => {
    const store = new SqlLiveStore(await open()); await store.reserve(base); await store.reserve(base);
    await expect(store.reserve(base)).rejects.toMatchObject({ reason: "concurrency_limited" });
    expect((await store.usage(base.subject)).monthly).toBe(200);
  });
  it("rolls back every bucket when monthly or global budget fails", async () => {
    const store = new SqlLiveStore(await open());
    await expect(store.reserve({ ...base, monthly: 99 })).rejects.toMatchObject({ reason: "budget_exceeded" });
    await expect(store.reserve({ ...base, global: 99 })).rejects.toMatchObject({ reason: "provider_budget_exceeded" });
    expect(await store.usage(base.subject)).toEqual({ daily: 0, monthly: 0, blocked: false });
  });
  it("settles once, retaining unknown usage and recording over-estimate costs", async () => {
    const store = new SqlLiveStore(await open()); const a = await store.reserve(base);
    await Promise.all([store.settle(a, 30), store.settle(a, 30)]);
    expect((await store.usage(base.subject)).daily).toBe(30);
    const b = await store.reserve(base); await store.settle(b);
    const c = await store.reserve(base); await store.settle(c, 120);
    expect((await store.usage(base.subject)).daily).toBe(250);
  });
  it("keeps monthly charges across UTC days, and expires abandoned concurrency only", async () => {
    let now = Date.parse("2026-10-14T23:58:00Z"); const store = new SqlLiveStore(await open(), () => now);
    await store.reserve(base); await store.reserve(base);
    now += LIVE_MAX_MS+60_001;
    await store.reserve(base);
    expect(await store.usage(base.subject)).toEqual({ daily: 100, monthly: 300, blocked: false });
  });
  it("limits rapid sequential calls even after settlement", async () => {
    const store = new SqlLiveStore(await open());
    for (let i = 0; i < 12; i++) await store.settle(await store.reserve(base), 0);
    await expect(store.reserve(base)).rejects.toMatchObject({ reason: "rate_limited" });
  });
  it("lets live meeting translation send a line at a time beyond the press-lane limit", async () => {
    const store = new SqlLiveStore(await open());
    const translate = { ...base, lane: "translate_live" as const };
    for (let i = 0; i < 40; i++) await store.settle(await store.reserve(translate), 0);
    await expect(store.reserve(translate)).rejects.toMatchObject({ reason: "rate_limited" });
  });
  it("removes expired content-free records without changing current month balances", async () => {
    let now = Date.parse("2025-01-01T00:00:00Z"); const store = new SqlLiveStore(await open(), () => now);
    await store.reserve(base);
    now = Date.parse("2026-10-14T00:00:00Z"); await store.reserve(base);
    await store.maintenance();
    expect(await store.usage(base.subject)).toEqual({ daily: 100, monthly: 100, blocked: false });
  });
  it("blocks the next request in a different store instance and audits unblock", async () => {
    const db = await open(); const a = new SqlLiveStore(db); const b = new SqlLiveStore(db);
    await a.block(base.subject, true, "test-admin");
    await expect(b.reserve(base)).rejects.toMatchObject({ reason: "blocked" });
    await a.block(base.subject, false, "test-admin"); await b.reserve(base);
    expect((await db.query("SELECT * FROM relay_live_admin_actions"))).toHaveLength(2);
  });
});

async function rig(fetcher?: typeof fetch) {
  const keys = await makeKeys(); const db = local(); const store = new SqlLiveStore(db); const config = liveConfig(configEnv()); const logs: string[] = [];
  const upstream = vi.fn(fetcher ?? (async () => new Response('data: {"type":"response.output_text.delta","delta":"answer"}\n\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":10}}}\n\n', { headers: { "content-type": "text/event-stream" } })));
  const app = createLiveApp({ config, store, publicKey: keys.publicKey, log: line => logs.push(line), fetch: upstream });
  const token = await signLicense(keys.privateKey);
  const request = async (lane: string, body: unknown, headers: Record<string, string> = {}) => app.request(`/v1/ai/${lane}`, { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json", ...headers }, body: JSON.stringify(body) });
  return { app, request, token, keys, db, store, config, logs, upstream };
}
describe("hosted live route", () => {
  it("ignores client model/tier/store injection; reserves and settles rewrite", async () => {
    const r = await rig(); const response = await r.request("rewrite", { input: "private user prompt", model: "expensive-model", max_output_tokens: 999999, store: true, service_tier: "default" });
    expect(response.status).toBe(200); expect(await response.text()).toContain('"delta":"answer"');
    const sent = JSON.parse(String(r.upstream.mock.calls[0]?.[1]?.body));
    expect(sent).toMatchObject({ model: "gpt-6-luna", store: false, max_output_tokens: 100, service_tier: "priority", reasoning: { effort: "low" } });
    expect(sent.safety_identifier).toMatch(/^[a-f0-9]{64}$/);
    expect((await r.store.usage("lic_test_1")).daily).toBe(25);
    expect(r.logs.join()).not.toMatch(/private user prompt|lic_test_1|test-openai|dev_test/);
  });
  it("authenticates before parsing; refuses browser requests and hosted tools", async () => {
    const r = await rig();
    expect((await r.app.request("/v1/ai/chat", { method: "POST", body: "not json" })).status).toBe(401);
    expect((await r.request("chat", { input: "hi" }, { origin: "https://evil.invalid" })).status).toBe(403);
    expect((await r.request("chat", { input: "hi", tools: [{ type: "web_search" }] })).status).toBe(400);
    expect((await r.request("rewrite", { input: "x".repeat(65537) })).status).toBe(413);
    expect((await r.request("form_decide", { input: "hi" })).status).toBe(404);
    expect(r.upstream).not.toHaveBeenCalled();
  });
  it("fails closed on ledger outages and blocked subjects", async () => {
    const r = await rig(); r.db.failNext(1);
    expect((await r.request("chat", { input: "hi" })).status).toBe(503);
    await r.store.block("lic_test_1", true, "test-admin");
    expect((await r.request("chat", { input: "hi" })).status).toBe(403);
    expect(r.upstream).not.toHaveBeenCalled();
  });
  it("redacts provider errors and retains ambiguous reservation", async () => {
    const r = await rig(async () => new Response("secret-upstream-body", { status: 500 }));
    const response = await r.request("chat", { input: "private" }); expect(response.status).toBe(502);
    expect(await response.text()).not.toContain("secret-upstream-body");
    expect((await r.store.usage("lic_test_1")).daily).toBeGreaterThan(0);
  });
  it("returns the authenticated subject's usage, not a query-supplied identity", async () => {
    const r = await rig(); await r.request("chat", { input: "hi" }).then(res => res.text());
    const res = await r.app.request("/v1/ai/usage?subject=someone-else", { headers: { authorization: `Bearer ${r.token}` } });
    expect(await res.json()).toMatchObject({ daily: 25, caps: { daily: 3000000, monthly: 40000000 }, unit: "micro_usd" });
  });
  it("meters a paid licence by its licence id even when the token carries an account", async () => {
    const r = await rig();
    const paid = await signLicense(r.keys.privateKey, { sub: "lic_paid", plan: "pro", acc: "acc_shared" });
    const res = await r.app.request("/v1/ai/chat", { method: "POST", headers: { authorization: `Bearer ${paid}`, "content-type": "application/json" }, body: JSON.stringify({ input: "hi" }) });
    expect(res.status).toBe(200); await res.text();
    expect((await r.store.usage("lic_paid")).daily).toBe(25);
    expect((await r.store.usage("account:acc_shared")).daily).toBe(0);
    const usage = await r.app.request("/v1/ai/usage", { headers: { authorization: `Bearer ${paid}` } });
    expect(await usage.json()).toMatchObject({ daily: 25, caps: { daily: 6000000, monthly: 80000000 } });
  });
  it("meters a trial by its account across devices, and a device-only trial by its trial licence", async () => {
    const r = await rig();
    const send = async (token: string) => { const res = await r.app.request("/v1/ai/chat", { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify({ input: "hi" }) }); expect(res.status).toBe(200); await res.text(); };
    const trial = (device: string, acc?: string) => signLicense(r.keys.privateKey, { sub: `trial_${device}`, plan: "trial", status: "trialing", device, periodEnd: Math.floor(Date.now()/1000) + 86_400, ...(acc ? { acc } : {}) });
    await send(await trial("mac-a", "acc_1")); await send(await trial("mac-b", "acc_1"));
    expect((await r.store.usage("account:acc_1")).daily).toBe(50);
    expect((await r.store.usage("trial_mac-a")).daily).toBe(0);
    await send(await trial("mac-c"));
    expect((await r.store.usage("trial_mac-c")).daily).toBe(25);
    const usage = await r.app.request("/v1/ai/usage", { headers: { authorization: `Bearer ${await trial("mac-b", "acc_1")}` } });
    expect(await usage.json()).toMatchObject({ daily: 50, caps: { daily: 1000000, monthly: 7000000 } });
  });
  it("routes dictation to Groq and parses its final usage", async () => {
    const r = await rig(async () => new Response('data: {"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":4}}\n\ndata: [DONE]\n\n', { headers: { "content-type": "text/event-stream" } }));
    expect((await r.request("dictation_cleanup", { input: "hi" }).then(res => res.text()))).toContain("[DONE]");
    expect(r.upstream.mock.calls[0]?.[0]).toBe("https://api.groq.com/openai/v1/chat/completions");
    const sent = JSON.parse(String((r.upstream.mock.calls[0]?.[1] as RequestInit).body));
    expect(sent).toMatchObject({ reasoning_effort: "low", include_reasoning: false, messages: [{ role: "user", content: "hi" }] });
    expect((await r.store.usage("lic_test_1")).daily).toBe(16);
  });
});

describe("stream lifecycle and config", () => {
  it("redacts in-stream error payloads and keeps reservation on truncation", async () => {
    const policy = liveConfig(configEnv()).policies.chat; const settle = vi.fn(async () => {});
    const input = new Response('data: {"type":"response.failed","error":{"message":"secret error"}}\n\n').body!;
    const output = await new Response(liveStream(input, policy, new AbortController(), settle)).text();
    expect(output).toContain("upstream_stream_failed"); expect(output).toContain('"type":"error"'); expect(output).not.toContain("secret error"); expect(settle).toHaveBeenCalledWith(undefined);
  });
  it("handles split CRLF boundaries and multiple events", async () => {
    const policy = liveConfig(configEnv()).policies.chat; const settle = vi.fn(async (_cost?: number) => {});
    const chunks = ['data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}\r','\n\r','\n'];
    const input = new ReadableStream<Uint8Array>({ pull(c) { const text = chunks.shift(); if (text === undefined) c.close(); else c.enqueue(new TextEncoder().encode(text)); } });
    const output = await new Response(liveStream(input, policy, new AbortController(), settle)).text();
    expect(output).not.toContain("upstream_stream_failed"); expect(settle).toHaveBeenCalledWith(5);
  });
  it("cancels upstream and conservatively settles on client cancellation", async () => {
    const controller = new AbortController(); const settle = vi.fn(async () => {}); const cancel = vi.fn();
    const input = new ReadableStream<Uint8Array>({ cancel });
    const stream = liveStream(input, liveConfig(configEnv()).policies.chat, controller, settle);
    await stream.cancel(); expect(controller.signal.aborted).toBe(true); expect(cancel).toHaveBeenCalled(); expect(settle).toHaveBeenCalledOnce();
  });
  it("requires separate live secrets and explicit nonzero pricing", () => {
    expect(() => liveConfig({ ...configEnv(), OPENAI_LIVE_API_KEY: undefined, OPENAI_API_KEY: "batch-key" })).toThrow();
    const env = configEnv(); const policies = JSON.parse(env.LIVE_LANE_CONFIG_JSON); policies.chat.inputMicrosPerToken = 0;
    expect(() => liveConfig({ ...env, LIVE_LANE_CONFIG_JSON: JSON.stringify(policies) })).toThrow();
    expect(liveConfig({ ...env, LIVE_GLOBAL_DAILY_CAP_USD: "0" }).globalDailyMicros).toBe(0);
  });
  it("trusts an account claim only in a valid signed token and rejects malformed claims", async () => {
    const keys = await makeKeys();
    const token = await signLicense(keys.privateKey, { account: "account-123" });
    expect(verifyLicense(`Bearer ${token}`, keys.publicKey, Math.floor(Date.now()/1000)).accountId).toBe("account-123");
    for (const account of [null, "", 123, "bad whitespace", "a".repeat(129)]) {
      const bad = await signLicense(keys.privateKey, { account });
      expect(() => verifyLicense(`Bearer ${bad}`, keys.publicKey, Math.floor(Date.now()/1000))).toThrow();
    }
  });
  it("answers an ended trial 402 and the operator denylist 401, before any provider call", async () => {
    const keys = await makeKeys(); const upstream = vi.fn(fetch);
    const app = createLiveApp({ config: liveConfig(configEnv()), store: new SqlLiveStore(local()), publicKey: keys.publicKey, log: () => undefined, fetch: upstream, denySubjects: ["lic_denied"] });
    const ask = (token: string) => app.request("/v1/ai/chat", { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify({ input: "hi" }) });
    const past = Math.floor(Date.now()/1000) - 60;
    const ended = await signLicense(keys.privateKey, { plan: "trial", status: "trialing", iat: past - 3600, exp: past, periodEnd: past });
    expect((await ask(ended)).status).toBe(402);
    expect((await ask(await signLicense(keys.privateKey, { sub: "lic_denied" }))).status).toBe(401);
    expect(upstream).not.toHaveBeenCalled();
  });
  it("meters live budgets on the relay's payer subject (owner decision 2026-10-21)", () => {
    expect(meterSubject({ licenseId: "trial_d", deviceId: "d", plan: "trial", accountId: "a" })).toBe("account:a");
    expect(meterSubject({ licenseId: "trial_d", deviceId: "d", plan: "trial" })).toBe("trial_d");
    expect(meterSubject({ licenseId: "lic_paid", deviceId: "d", plan: "pro", accountId: "a" })).toBe("lic_paid");
  });
});

it("replays stateless function calls and encrypted reasoning without widening tools", () => {
  const config = liveConfig(configEnv());
  const input = [
    { role: "user", content: "check my calendar" },
    { type: "reasoning", id: "rs_1", encrypted_content: "opaque", summary: [{ type: "summary_text", text: "discarded" }] },
    { type: "message", role: "assistant", content: [{ type: "output_text", text: "Checking", annotations: [] }] },
    { type: "function_call", call_id: "call_1", name: "list_calendar_events", arguments: "{}" },
    { type: "function_call_output", call_id: "call_1", output: "[]" },
  ];
  const body = upstreamBody({ input, tools: [{ type: "function", name: "list_calendar_events", parameters: { type: "object" }, strict: true }], store: true }, "chat", config.policies.chat, "license:fixture", config.hmacKey)!;
  expect(body.store).toBe(false);
  expect(upstreamBody({ input: [{ type: "reasoning", encrypted_content: "opaque" }] }, "chat", config.policies.chat, "x", config.hmacKey)).toBeUndefined();
  expect(body.include).toEqual(["reasoning.encrypted_content"]);
  expect(body.tools).toEqual([{ type: "function", name: "list_calendar_events", parameters: { type: "object" }, strict: false }]);
  expect((body.input as unknown[])[1]).toEqual({ type: "reasoning", id: "rs_1", encrypted_content: "opaque", summary: [] });
  expect((body.input as unknown[])[2]).toEqual({ role: "assistant", content: "Checking" });
  expect(upstreamBody({ input: [{ type: "message", role: "assistant", content: [{ type: "input_image", image_url: "https://example.com" }] }] }, "chat", config.policies.chat, "x", config.hmacKey)).toBeUndefined();
});
