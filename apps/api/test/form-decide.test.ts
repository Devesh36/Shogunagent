/** Issue #1144 (#1101 D12): Jev form decisions on the company TypeSafe key, `POST /v1/ai/form_decide`. */
import { afterEach, describe, expect, it, vi } from "vitest";

import { formInFlightKey, formRateKey } from "../src/app.js";
import { isExpiredTrialToken } from "../src/auth.js";
import { relayConfigFromEnv, formLaneLine } from "../src/boot.js";
import {
  FORM_DECIDE_MAX_BODY_BYTES, FORM_DECIDE_PER_HOUR, FORM_DECIDE_TIMEOUT_MS, JEV_MODEL, TYPESAFE_ENDPOINT,
  formDecideReserveTokens, parseFormDecideRequest, typesafeDecider, type FormDecider, type FormDecideUpstream,
} from "../src/form-decide.js";
import { SqlInFlightLimiter, SqlWindowLimiter } from "../src/pg-store.js";
import { InMemoryInFlight, WindowLimiter } from "../src/ratelimit.js";
import { UpstreamError } from "../src/gateway.js";
import { DEFAULT_BATCH_PRICES, priceFor } from "../src/spend.js";
import { utcDate } from "../src/usage.js";
import { makeRig, signLicense, type Rig } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";
import releaseKey from "../../../packages/config/license-public-key.json";

afterEach(() => vi.unstubAllGlobals());

const SECRET_LABEL = "SECRET-FIELD-LABEL-7731";
const SECRET_FACT = "SECRET-CANDIDATE-FACT-4402";
const SECRET_ANSWER = "SECRET-ANSWER-TEXT-9910";

/** The body `forms/jev.rs` builds for a decision, minus `model`. */
const decisionBody = () => ({
  state: {
    fields: [{ id: "f1", label: SECRET_LABEL, kind: "text" }],
    candidates: [{ id: "c1", value: SECRET_FACT, label: "name", source_literal: "x", evidence_ids: ["e1"] }],
    intended_subject: "",
    supporting_evidence: { e1: "evidence" },
  },
  questions: {
    f1: { type: "choice", instructions: { task: "t", field: { id: "f1" } }, criteria: { c1: null, none: "no match" } },
    "form-purpose": { type: "choice", instructions: { task: "p" }, criteria: { registration: "Registration", unknown: "UNKNOWN" } },
  },
});

/** The body `jev.rs` `intent_request` builds, minus `model`. */
const intentBody = () => ({
  state: { user_instruction: "fill this form", destination: { focused_field_label: "", window_title: "", focused_field_has_text: false } },
  questions: { intent: { type: "choice", instructions: "pick", criteria: { fill_form: "a", edit_field: "b", clarify: "c" } } },
});

const okAnswer = (inputTokens = 1000): FormDecideUpstream => ({
  kind: "ok",
  text: JSON.stringify({ model: JEV_MODEL, answers: { note: SECRET_ANSWER }, usage: { input_tokens: inputTokens, output_tokens: 5 } }),
  inputTokens,
});

function post(rig: Rig, body: unknown, token: string | null = rig.token): Promise<Response> {
  return Promise.resolve(rig.app.request("/v1/ai/form_decide", {
    method: "POST",
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), "content-type": "application/json" },
    body: typeof body === "string" ? body : JSON.stringify(body),
  }));
}

describe("request contract", () => {
  it("accepts both shapes the Mac builds", () => {
    expect(typeof parseFormDecideRequest(decisionBody())).toBe("object");
    expect(typeof parseFormDecideRequest(intentBody())).toBe("object");
  });

  it("rejects a model, unknown fields and malformed questions", () => {
    expect(parseFormDecideRequest({ ...decisionBody(), model: "jev-latest" })).toMatch(/exactly state and questions/);
    expect(parseFormDecideRequest({ state: decisionBody().state })).toMatch(/exactly/);
    expect(parseFormDecideRequest({ ...decisionBody(), state: { ...decisionBody().state, extra: 1 } })).toMatch(/unknown shape/);
    const q = decisionBody();
    (q.questions.f1 as Record<string, unknown>).type = "text";
    expect(parseFormDecideRequest(q)).toMatch(/choice/);
    const extra = decisionBody();
    (extra.questions.f1 as Record<string, unknown>).model = "x";
    expect(parseFormDecideRequest(extra)).toMatch(/exactly type/);
    const one = decisionBody();
    one.questions.f1.criteria = { c1: null } as never;
    expect(parseFormDecideRequest(one)).toMatch(/2 to/);
    expect(parseFormDecideRequest({ state: decisionBody().state, questions: {} })).toMatch(/1 to/);
    expect(parseFormDecideRequest([])).toMatch(/object/);
  });
});

describe("POST /v1/ai/form_decide", () => {
  const rigWith = (decider: FormDecider, extra: Parameters<typeof makeRig>[0] = {}) => makeRig({ formDecider: decider, ...extra });

  it("needs a licence; a plan without the lane and an ended trial answer plan_required", async () => {
    const decider = vi.fn(async () => okAnswer());
    const rig = await rigWith(decider, { spendBreaker: { capMicros: 1_000_000, prices: DEFAULT_BATCH_PRICES }, trialDailyChunkCap: 10 });
    expect((await post(rig, decisionBody(), null)).status).toBe(401);
    const free = await signLicense(rig.keys.privateKey, { plan: "free" });
    const refused = await post(rig, decisionBody(), free);
    expect(refused.status).toBe(402);
    expect(await refused.json()).toEqual({ error: "plan_required" });
    const now = Math.floor(Date.now() / 1000);
    const ended = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60, periodEnd: now - 60 });
    const endedRes = await post(rig, decisionBody(), ended);
    expect(endedRes.status).toBe(402);
    expect(await endedRes.json()).toEqual({ error: "plan_required" });
    // A trial token that only ran out mid-trial (its trial ends later) is a stale credential: 401.
    const lapsed = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60, periodEnd: now + 86_400 });
    expect((await post(rig, decisionBody(), lapsed)).status).toBe(401);
    // An expired paid token is a stale credential, not a plan question: 401 as everywhere else.
    const stale = await signLicense(rig.keys.privateKey, { iat: now - 7200, exp: now - 60 });
    expect((await post(rig, decisionBody(), stale)).status).toBe(401);
    // A live trial is served.
    const trial = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing" });
    expect((await post(rig, decisionBody(), trial)).status).toBe(200);
    expect(decider).toHaveBeenCalledTimes(1);
  });

  it("forged trial tokens are not mistaken for ended trials", async () => {
    const rig = await rigWith(async () => okAnswer());
    const other = await makeRig();
    const now = Math.floor(Date.now() / 1000);
    const forged = await signLicense(other.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60, periodEnd: now - 60 });
    expect(isExpiredTrialToken(`Bearer ${forged}`, rig.keys.publicKey)).toBe(false);
    expect((await post(rig, decisionBody(), forged)).status).toBe(401);
  });

  it("adds the pinned model, returns TypeSafe's JSON unchanged", async () => {
    const sent: string[] = [];
    const answer = okAnswer();
    const rig = await rigWith(async (body) => { sent.push(body); return answer; });
    const res = await post(rig, decisionBody());
    expect(res.status).toBe(200);
    expect(await res.text()).toBe((answer as { text: string }).text);
    const forwarded = JSON.parse(sent[0]!) as Record<string, unknown>;
    expect(Object.keys(forwarded)).toEqual(["model", "state", "questions"]);
    expect(forwarded.model).toBe(JEV_MODEL);
    expect(forwarded.state).toEqual(decisionBody().state);
    expect((await post(rig, intentBody())).status).toBe(200);
  });

  it("refuses a body with a model or an unknown field, and anything over 128 KiB", async () => {
    const decider = vi.fn(async () => okAnswer());
    const rig = await rigWith(decider);
    expect((await post(rig, { ...decisionBody(), model: "jev-latest" })).status).toBe(400);
    expect((await post(rig, { ...decisionBody(), tools: [] })).status).toBe(400);
    expect((await post(rig, "not json")).status).toBe(400);
    const big = decisionBody();
    big.state.supporting_evidence = { e1: "x".repeat(FORM_DECIDE_MAX_BODY_BYTES) };
    expect((await post(rig, big)).status).toBe(413);
    expect(decider).not.toHaveBeenCalled();
  });

  it("maps TypeSafe's statuses: 401→503 with an alert, 429, 529→503, 422, other→502", async () => {
    let next: FormDecideUpstream = { kind: "status", status: 401 };
    const rig = await rigWith(async () => next);
    const rejected = await post(rig, decisionBody());
    expect(rejected.status).toBe(503);
    expect(await rejected.json()).toEqual({ error: "form_decide_unavailable" });
    expect(rig.logLines.some((l) => l.startsWith("ALERT typesafe_key_rejected status=401"))).toBe(true);

    next = { kind: "status", status: 429, retryAfterSecs: 17 };
    const limited = await post(rig, decisionBody());
    expect(limited.status).toBe(429);
    expect(limited.headers.get("retry-after")).toBe("17");
    expect(await limited.json()).toMatchObject({ reason: "rate_limited" });
    next = { kind: "status", status: 429 };
    expect((await post(rig, decisionBody())).headers.get("retry-after")).toBe("30");

    next = { kind: "status", status: 529 };
    const busy = await post(rig, decisionBody());
    expect(busy.status).toBe(503);
    expect(await busy.json()).toEqual({ error: "overloaded", reason: "overloaded" });
    expect(busy.headers.get("retry-after")).toBe("30");

    next = { kind: "status", status: 422 };
    const invalid = await post(rig, decisionBody());
    expect(invalid.status).toBe(422);
    expect(await invalid.json()).toEqual({ error: "form_request_invalid" });

    next = { kind: "status", status: 500 };
    expect((await post(rig, decisionBody())).status).toBe(502);
  });

  it("without TYPESAFE_API_KEY the route answers 503 and calls nothing", async () => {
    const rig = await makeRig();
    const res = await post(rig, decisionBody());
    expect(res.status).toBe(503);
    expect(await res.json()).toEqual({ error: "form_decide_unavailable" });
  });

  it("allows 30 requests an hour per licence; the 31st is 429 with Retry-After", async () => {
    let clock = new Date("2026-10-15T10:00:00Z");
    const rig = await rigWith(async () => okAnswer(), { now: () => clock });
    for (let i = 0; i < FORM_DECIDE_PER_HOUR; i += 1) expect((await post(rig, decisionBody())).status).toBe(200);
    const over = await post(rig, decisionBody());
    expect(over.status).toBe(429);
    expect(over.headers.get("retry-after")).toBe("3600");
    // The licence, not the device: another Mac on the same licence shares the hour.
    const otherMac = await signLicense(rig.keys.privateKey, { device: "dev_other", iat: Math.floor(clock.getTime() / 1000) });
    expect((await post(rig, decisionBody(), otherMac)).status).toBe(429);
    clock = new Date("2026-10-15T11:00:00Z");
    expect((await post(rig, decisionBody())).status).toBe(200);
  });

  it("holds at most two decisions in flight per device", async () => {
    const waiting: Array<() => void> = [];
    const rig = await rigWith(() => new Promise((resolve) => { waiting.push(() => resolve(okAnswer())); }));
    const first = post(rig, decisionBody());
    const second = post(rig, decisionBody());
    await vi.waitFor(() => expect(waiting).toHaveLength(2));
    const third = await post(rig, decisionBody());
    expect(third.status).toBe(429);
    expect(third.headers.get("retry-after")).toBe("2");
    // Another device of the licence has its own two.
    const otherMac = await signLicense(rig.keys.privateKey, { device: "dev_other" });
    const other = post(rig, decisionBody(), otherMac);
    await vi.waitFor(() => expect(waiting).toHaveLength(3));
    for (const go of waiting.splice(0)) go();
    expect((await first).status).toBe(200);
    expect((await second).status).toBe(200);
    expect((await other).status).toBe(200);
    // Slots come back when the calls end, failures included.
    const again = post(rig, decisionBody());
    await vi.waitFor(() => expect(waiting).toHaveLength(1));
    waiting.splice(0)[0]!();
    expect((await again).status).toBe(200);
  });

  it("reserves bytes ÷ 4 at $0.042/M input, then settles from usage.input_tokens", async () => {
    let release: (() => void) | undefined;
    const rig = await rigWith(() => new Promise((resolve) => { release = () => resolve(okAnswer(10_000)); }),
      { spendBreaker: { capMicros: 1_000_000, prices: DEFAULT_BATCH_PRICES }, licenseSpendCapMicros: 500_000 });
    const date = utcDate(new Date());
    const body = decisionBody();
    const pending = post(rig, body);
    await vi.waitFor(() => expect(release).toBeDefined());
    const forwardedBytes = Buffer.byteLength(JSON.stringify({ model: JEV_MODEL, ...body }), "utf8");
    const reserved = Math.ceil(formDecideReserveTokens(forwardedBytes) * 0.042);
    expect(formDecideReserveTokens(forwardedBytes)).toBe(Math.ceil(forwardedBytes / 4));
    expect(await rig.usage.spentOn(date)).toBe(reserved);
    expect(await rig.usage.licenseSpentOn("lic_test_1", date)).toBe(reserved);
    release!();
    expect((await pending).status).toBe(200);
    // 10,000 input tokens × $0.042/M = 420 micro-USD; output is free.
    expect(await rig.usage.spentOn(date)).toBe(420);
    // The Batch lane's chunk counter is untouched once the call ends.
    expect(await rig.usage.usedOn("lic_test_1", date)).toBe(0);
    expect(priceFor(DEFAULT_BATCH_PRICES, JEV_MODEL)).toMatchObject({ input: 0.042, output: 0 });
  });

  it("the operator's denylist refuses the form route like every other (audit 2026-10-20 L3)", async () => {
    let called = false;
    const rig = await rigWith(async () => { called = true; return okAnswer(); }, { denySubjects: ["lic_test_1"] });
    expect((await post(rig, decisionBody())).status).toBe(401);
    expect(called).toBe(false);
  });

  it("a definite rejection books nothing, an unknown bill keeps the estimate; a cap answers the cap-reached 429", async () => {
    const breaker = { spendBreaker: { capMicros: 1_000_000, prices: DEFAULT_BATCH_PRICES } };
    for (const status of [401, 422, 429]) {
      const rejected = await rigWith(async () => ({ kind: "status", status }), breaker);
      await post(rejected, decisionBody());
      expect(await rejected.usage.spentOn(utcDate(new Date()))).toBe(0);
    }
    // 529 (or any 5xx): TypeSafe may have done the work, so the bill is the estimate, never 0
    // (audit 2026-10-20 C1, as on /v1/review).
    const rig = await rigWith(async () => ({ kind: "status", status: 529 }), breaker);
    expect((await post(rig, decisionBody())).status).toBe(503);
    expect(await rig.usage.spentOn(utcDate(new Date()))).toBeGreaterThan(0);
    expect(rig.logLines.join("\n")).toContain("form_bill_unknown (reservation kept)");
    const dropped = await rigWith(async () => { throw new UpstreamError("form_decide", 0); }, breaker);
    expect((await post(dropped, decisionBody())).status).toBe(502);
    expect(await dropped.usage.spentOn(utcDate(new Date()))).toBeGreaterThan(0);
    const capped = await rigWith(async () => okAnswer(), { spendBreaker: { capMicros: 1, prices: DEFAULT_BATCH_PRICES } });
    const res = await post(capped, decisionBody());
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).toBeNull();
  });

  it("logs no body: not the request's, not TypeSafe's answer, not its errors", async () => {
    const outcomes: FormDecideUpstream[] = [okAnswer(), { kind: "status", status: 401 }, { kind: "status", status: 422 }];
    const rig = await rigWith(async () => outcomes.shift()!);
    for (let i = 0; i < 3; i += 1) await post(rig, decisionBody());
    await post(rig, { ...decisionBody(), model: SECRET_LABEL });
    const all = rig.logLines.join("\n");
    for (const secret of [SECRET_LABEL, SECRET_FACT, SECRET_ANSWER, rig.token]) expect(all).not.toContain(secret);
    expect(all).toContain("POST /v1/ai/form_decide 200");
    expect(all).toContain("provider=typesafe model=jev-1.13.0");
  });
});

describe("typesafeDecider", () => {
  it("posts to TypeSafe with the company key and a 45 s timeout, and reads usage", async () => {
    const calls: Array<[string, RequestInit]> = [];
    vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit) => {
      calls.push([url, init]);
      return Response.json({ model: JEV_MODEL, answers: {}, usage: { input_tokens: 12, output_tokens: 3 } });
    }));
    const timeout = vi.spyOn(AbortSignal, "timeout");
    const out = await typesafeDecider("ts-company-key")('{"model":"jev-1.13.0"}');
    expect(out).toMatchObject({ kind: "ok", inputTokens: 12 });
    expect(calls[0]![0]).toBe(TYPESAFE_ENDPOINT);
    expect((calls[0]![1].headers as Record<string, string>).authorization).toBe("Bearer ts-company-key");
    expect(timeout).toHaveBeenCalledWith(FORM_DECIDE_TIMEOUT_MS);
    timeout.mockRestore();
  });

  it("returns a status without reading the error body, and Retry-After in whole seconds", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(`{"error":"${SECRET_FACT}"}`, { status: 429, headers: { "retry-after": "9" } })));
    expect(await typesafeDecider("k")("{}")).toEqual({ kind: "status", status: 429, retryAfterSecs: 9 });
    vi.stubGlobal("fetch", vi.fn(async () => new Response("", { status: 529, headers: { "retry-after": "Wed, 21 Oct 2026 07:28:00 GMT" } })));
    expect(await typesafeDecider("k")("{}")).toEqual({ kind: "status", status: 529 });
  });

  it("a transport failure is an UpstreamError, never an empty decision", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => { throw new Error(`socket closed ${SECRET_FACT}`); }));
    await expect(typesafeDecider("k")("{}")).rejects.toMatchObject({ name: "UpstreamError", status: 0 });
  });
});

describe("limits", () => {
  it("the in-process window and in-flight limits", async () => {
    const window = new WindowLimiter(2, 1000);
    expect(window.take("a", 0)).toBe(true);
    expect(window.take("a", 10)).toBe(true);
    expect(window.take("a", 20)).toBe(false);
    expect(window.retryAfterSecs("a", 20)).toBe(1);
    expect(window.take("a", 1000)).toBe(true);
    const inflight = new InMemoryInFlight(1);
    const r = await inflight.acquire("k");
    expect(await inflight.acquire("k")).toBeUndefined();
    await r!();
    await r!();
    expect(await inflight.acquire("k")).toBeDefined();
  });

  it("the shared-store window limiter counts per window and survives the idle eviction", async () => {
    const db = sqliteDouble();
    const limiter = new SqlWindowLimiter(db, 2, 3_600_000);
    expect(await limiter.take("lic", 0)).toBe(true);
    expect(await limiter.take("lic", 1)).toBe(true);
    expect(await limiter.take("lic", 2)).toBe(false);
    expect(await limiter.take("other", 2)).toBe(true);
    // `at_ms` is the window's end, so the ten-minute idle sweep cannot reset an open window.
    const rows = await db.query("SELECT at_ms FROM relay_rate_buckets");
    expect(rows.every((r) => Number(r.at_ms) === 3_600_000)).toBe(true);
    expect(limiter.retryAfterSecs("lic", 1_800_000)).toBe(1800);
    expect(await limiter.take("lic", 3_600_000)).toBe(true);
  });

  it("the shared-store in-flight limiter holds two slots, frees on release and after the lease", async () => {
    const db = sqliteDouble();
    const limiter = new SqlInFlightLimiter(db, 2, 60_000);
    const a = await limiter.acquire("dev", 0);
    const b = await limiter.acquire("dev", 1);
    expect(a && b).toBeTruthy();
    expect(await limiter.acquire("dev", 2)).toBeUndefined();
    expect(await limiter.acquire("dev2", 2)).toBeDefined();
    await a!();
    const c = await limiter.acquire("dev", 3);
    expect(c).toBeDefined();
    // A slot whose holder died frees itself once the lease passes.
    expect(await limiter.acquire("dev", 4)).toBeUndefined();
    expect(await limiter.acquire("dev", 60_002)).toBeDefined();
    // A stale release does not free the slot someone else now holds.
    await b!();
    expect(await limiter.acquire("dev", 60_003)).toBeUndefined();
  });

  it("route keys: payer for the hour, payer + device for in-flight", () => {
    const license = { licenseId: "lic", deviceId: "dev", plan: "pro" };
    expect(formRateKey(license)).not.toContain("dev");
    expect(formInFlightKey(license)).toContain("dev");
    // A trial issued through an account is metered on the account (audit 2026-10-20 H1): two
    // device ids of one account share one hour, never one each.
    const a = { licenseId: "trial_dev_a", deviceId: "dev_a", plan: "trial", accountId: "acc_1" };
    const b = { licenseId: "trial_dev_b", deviceId: "dev_b", plan: "trial", accountId: "acc_1" };
    expect(formRateKey(a)).toBe(formRateKey(b));
    expect(formRateKey(a)).toContain("account:acc_1");
    expect(formInFlightKey(a)).not.toBe(formInFlightKey(b));
  });
});

describe("boot", () => {
  const env = {
    ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key",
    LICENSE_PUBKEY_B64: (releaseKey as { ed25519: string }).ed25519,
    DAILY_SPEND_CAP_USD: "10",
    BATCH_PROVIDER: "anthropic",
    OPENAI_API_KEY: "sk-test-not-a-real-key",
  };
  it("TYPESAFE_API_KEY is optional; the boot line says whether the lane is open", () => {
    const closed = relayConfigFromEnv(env);
    expect(closed.typesafeApiKey).toBeUndefined();
    expect(formLaneLine(closed)).toBe("form_decide_lane closed (TYPESAFE_API_KEY unset)");
    const open = relayConfigFromEnv({ ...env, TYPESAFE_API_KEY: " ts-key " });
    expect(open.typesafeApiKey).toBe("ts-key");
    expect(formLaneLine(open)).toBe("form_decide_lane open");
    // The line names the variable, never its value.
    expect(formLaneLine(open)).not.toContain("ts-key");
  });
});
