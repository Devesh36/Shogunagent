/** Issue #29: the spend breaker (FR-SVC-08), cost accounting, and ops metrics (FR-SVC-06). */
import { describe, expect, it } from "vitest";

import { RelayMetrics } from "../src/metrics.js";
import { MAX_TOKENS_BY_CLASS, MODEL_BY_CLASS, RESERVE_BYTES_PER_TOKEN, systemBlocksFor } from "../src/models.js";
import {
  DEFAULT_BATCH_PRICES,
  LOW_RATE_CARD_MAX_PROMPT_TOKENS,
  estimateSubmissionMicros,
  priceFor,
  priceTableFromJson,
  priceTableWithOverride,
  UNKNOWN_MODEL_FLOOR,
  rateCardFor,
  spendCapMicrosFromEnv,
  usageMicros,
} from "../src/spend.js";
import { MAX_CHUNK_BYTES, MAX_SYSTEM_BYTES, type AnthropicBatchRequestItem, type RelayUsage } from "../src/types.js";
import { utcDate } from "../src/usage.js";
import { makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";

const breaker = (capUsd: number) => ({ capMicros: Math.round(capUsd * 1_000_000), prices: DEFAULT_BATCH_PRICES });

function resultLine(customId: string, usage: Record<string, number>, model = MODEL_BY_CLASS.classify): string {
  return JSON.stringify({
    custom_id: customId,
    result: { type: "succeeded", message: { model, content: [{ type: "text", text: "{}" }], usage } },
  });
}

describe("cost accounting (FR-SVC-08)", () => {
  it("prices provider usage at Batch rates, micro-USD per token = USD per MTok", () => {
    // Haiku 4.5 stays priced: an older desktop's batch may still settle under this id.
    const haiku = priceFor(DEFAULT_BATCH_PRICES, "claude-haiku-4-5-20251001");
    expect(usageMicros({ input_tokens: 1_000_000, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }, haiku)).toBe(500_000);
    expect(usageMicros({ input_tokens: 0, output_tokens: 1_000, cache_read_input_tokens: 1_000, cache_creation_input_tokens: 1_000 }, haiku))
      .toBeCloseTo(2_500 + 50 + 1_000);
    // Haiku 5.5, the ≤100K-prompt-token card: $0.05/$0.25, cache read 0.1× input, 1-hour write 2× input.
    const haiku55 = priceFor(DEFAULT_BATCH_PRICES, MODEL_BY_CLASS.classify);
    expect(haiku55).toMatchObject({ input: 0.05, output: 0.25, cacheRead: 0.005, cacheWrite: 0.1 });
    // (A 1M-token prompt would be on the long card; 100K is the last token of the base one.)
    expect(usageMicros({ input_tokens: 100_000, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }, haiku55)).toBe(5_000);
    expect(usageMicros({ input_tokens: 0, output_tokens: 1_000, cache_read_input_tokens: 1_000, cache_creation_input_tokens: 1_000 }, haiku55))
      .toBeCloseTo(250 + 5 + 100);
    // Every class's model has its own row; none falls through to the worst case.
    for (const model of Object.values(MODEL_BY_CLASS)) expect(DEFAULT_BATCH_PRICES[model]).toBeDefined();
  });

  it("prices an unknown model at the most expensive card, never as free", () => {
    const worst = priceFor(DEFAULT_BATCH_PRICES, "claude-unknown");
    expect(worst).toEqual(DEFAULT_BATCH_PRICES["claude-sonnet-5"]);
    for (const row of Object.values(DEFAULT_BATCH_PRICES)) {
      for (const card of [row, ...(row.longPrompt ? [row.longPrompt] : [])]) {
        for (const k of ["input", "output", "cacheRead", "cacheWrite"] as const) expect(worst[k]).toBeGreaterThanOrEqual(card[k]);
      }
    }
    // A table whose only long card is the most expensive thing in it still prices the unknown at it.
    const table = priceTableFromJson('{"m":{"input":1,"output":1,"cacheRead":1,"cacheWrite":1,"longPrompt":{"input":9,"output":9,"cacheRead":9,"cacheWrite":9}}}');
    expect(priceFor(table, "other")).toEqual({ input: 9, output: 9, cacheRead: 9, cacheWrite: 9 });
  });

  describe("Haiku 5.5's two rate cards (second-pass review of PR #1022)", () => {
    const haiku55 = DEFAULT_BATCH_PRICES["claude-haiku-5-5"]!;
    const usage = (input_tokens: number, extra: Partial<RelayUsage> = {}): RelayUsage =>
      ({ input_tokens, output_tokens: 1_000, cache_read_input_tokens: 0, cache_creation_input_tokens: 0, ...extra });

    it("selects the long-prompt card by the provider's prompt count: input + cache reads + cache writes", () => {
      expect(LOW_RATE_CARD_MAX_PROMPT_TOKENS).toBe(100_000);
      expect(haiku55.longPrompt).toEqual({ input: 0.5, output: 2.5, cacheRead: 0.05, cacheWrite: 1 });
      expect(rateCardFor(haiku55, 100_000)).toBe(haiku55);
      expect(rateCardFor(haiku55, 100_001)).toBe(haiku55.longPrompt);
      // Exactly at the boundary: base card. 100,000 × 0.05 + 1,000 × 0.25.
      expect(usageMicros(usage(100_000), haiku55)).toBe(5_000 + 250);
      // One token over: everything on the long card, output included. 100,001 × 0.5 + 1,000 × 2.5.
      expect(usageMicros(usage(100_001), haiku55)).toBe(50_000.5 + 2_500);
      // Cache reads and writes count toward the prompt length even though input_tokens alone is short.
      expect(usageMicros(usage(10, { cache_read_input_tokens: 60_000, cache_creation_input_tokens: 40_000 }), haiku55))
        .toBeCloseTo(10 * 0.5 + 1_000 * 2.5 + 60_000 * 0.05 + 40_000 * 1);
      expect(usageMicros(usage(10, { cache_read_input_tokens: 60_000, cache_creation_input_tokens: 39_990 }), haiku55))
        .toBeCloseTo(10 * 0.05 + 1_000 * 0.25 + 60_000 * 0.005 + 39_990 * 0.1);
      // A single-card model is unaffected by prompt length.
      const sonnet = priceFor(DEFAULT_BATCH_PRICES, MODEL_BY_CLASS.brief);
      expect(rateCardFor(sonnet, 500_000)).toBe(sonnet);
    });

    it("reserves a worst-case CJK transcript on the long card, and a short item next to it on the base card", () => {
      // The largest chunk the relay accepts, in CJK: 192 KiB at ~2 bytes/token ≈ 98K tokens of
      // user turn, plus the shared system — over 100K, so the reservation must use the 10× card.
      const cjk = "会".repeat(MAX_CHUNK_BYTES / 3); // 3 bytes per character in UTF-8
      expect(Buffer.byteLength(cjk)).toBe(MAX_CHUNK_BYTES);
      const system = "s".repeat(MAX_SYSTEM_BYTES);
      const long: AnthropicBatchRequestItem = {
        custom_id: "long",
        params: { model: MODEL_BY_CLASS.summarize, max_tokens: MAX_TOKENS_BY_CLASS.summarize, system: systemBlocksFor("summarize", system), messages: [{ role: "user", content: cjk }], output_config: { effort: "medium" } },
      };
      const chunkTokens = MAX_CHUNK_BYTES / RESERVE_BYTES_PER_TOKEN; // 98,304
      const systemTokens = MAX_SYSTEM_BYTES / RESERVE_BYTES_PER_TOKEN; // 32,768
      expect(chunkTokens + systemTokens).toBeGreaterThan(LOW_RATE_CARD_MAX_PROMPT_TOKENS);
      const card = haiku55.longPrompt!;
      expect(estimateSubmissionMicros("summarize", [long], DEFAULT_BATCH_PRICES))
        .toBe(Math.ceil(systemTokens * card.cacheWrite + chunkTokens * card.input + MAX_TOKENS_BY_CLASS.summarize * card.output));
      // The same submission with a short item: that item is priced on the base card on its own.
      const short: AnthropicBatchRequestItem = { ...long, custom_id: "short", params: { ...long.params, messages: [{ role: "user", content: "x".repeat(4_000) }] } };
      const shortOnly = Math.ceil(systemTokens * haiku55.cacheWrite + 2_000 * haiku55.input + MAX_TOKENS_BY_CLASS.summarize * haiku55.output);
      expect(estimateSubmissionMicros("summarize", [short], DEFAULT_BATCH_PRICES)).toBe(shortOnly);
      expect(estimateSubmissionMicros("summarize", [long, short], DEFAULT_BATCH_PRICES))
        .toBe(Math.ceil(systemTokens * card.cacheWrite + chunkTokens * card.input + MAX_TOKENS_BY_CLASS.summarize * card.output + systemTokens * haiku55.cacheWrite + 2_000 * haiku55.input + MAX_TOKENS_BY_CLASS.summarize * haiku55.output));
      // Over-reserving is the safe direction: the long item's reservation is never below the long card's actual bill.
      expect(estimateSubmissionMicros("summarize", [long], DEFAULT_BATCH_PRICES))
        .toBeGreaterThanOrEqual(usageMicros({ input_tokens: chunkTokens, output_tokens: MAX_TOKENS_BY_CLASS.summarize, cache_read_input_tokens: 0, cache_creation_input_tokens: systemTokens }, haiku55));
    });

    it("parses a price table with and without the long card (SPEND_PRICE_TABLE_JSON stays backward compatible)", () => {
      const single = priceTableFromJson('{"claude-haiku-5-5":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":2}}')["claude-haiku-5-5"]!;
      expect(single.longPrompt).toBeUndefined();
      expect(usageMicros(usage(500_000), single)).toBe(500_000 + 2_000);
      const two = priceTableFromJson(JSON.stringify(DEFAULT_BATCH_PRICES));
      expect(two).toEqual(DEFAULT_BATCH_PRICES);
      expect(() => priceTableFromJson('{"m":{"input":1,"output":1,"cacheRead":1,"cacheWrite":1,"longPrompt":{"input":1}}}')).toThrow(/longPrompt/);
      expect(() => priceTableFromJson('{"m":{"input":1,"output":1,"cacheRead":1,"cacheWrite":1,"longPrompt":{"input":-1,"output":1,"cacheRead":1,"cacheWrite":1}}}')).toThrow();
    });
  });

  it("estimates an upper bound: input from bytes plus the full max_tokens of output", () => {
    const req: AnthropicBatchRequestItem = {
      custom_id: "1",
      params: {
        model: MODEL_BY_CLASS.classify, max_tokens: MAX_TOKENS_BY_CLASS.classify,
        messages: [{ role: "user", content: "x".repeat(4000) }],
        output_config: { effort: "low", format: { type: "json_schema", schema: {} } },
      },
    };
    // Reserved at 2 bytes/token (audit M9) at Haiku 5.5 rates: 2000 input tokens × 0.05 + "{}" (1 token) × 0.05
    // + the full 2048-token cap (thinking is output too) × 0.25
    expect(MAX_TOKENS_BY_CLASS.classify).toBe(2048);
    expect(estimateSubmissionMicros("classify", [req], DEFAULT_BATCH_PRICES)).toBe(Math.ceil(100 + 0.05 + 512));
    // An effort-only output_config (meeting recap) has no schema to reserve for.
    const recap: AnthropicBatchRequestItem = {
      custom_id: "2",
      params: { model: MODEL_BY_CLASS.summarize, max_tokens: 8192, messages: [{ role: "user", content: "x".repeat(4000) }], output_config: { effort: "medium" } },
    };
    expect(estimateSubmissionMicros("summarize", [recap], DEFAULT_BATCH_PRICES)).toBe(Math.ceil(100 + 8192 * 0.25));
  });

  it("reserves a cache-marked system prefix at the 1-hour write rate (2× input), the worst case for the first request", () => {
    const system = "r".repeat(2_900); // today's consolidation instruction, now above Haiku 5.5's 512-token minimum
    const blocks = systemBlocksFor("classify", system);
    expect(blocks[0]?.cache_control).toEqual({ type: "ephemeral", ttl: "1h" });
    const req: AnthropicBatchRequestItem = {
      custom_id: "1",
      params: { model: MODEL_BY_CLASS.classify, max_tokens: 2048, system: blocks, messages: [{ role: "user", content: "x".repeat(400) }], output_config: { effort: "low" } },
    };
    // 1450 reserved prefix tokens × cacheWrite 0.10 (not input 0.05) + 200 × 0.05 + 2048 × 0.25
    expect(estimateSubmissionMicros("classify", [req], DEFAULT_BATCH_PRICES)).toBe(Math.ceil(145 + 10 + 512));
    const plain: AnthropicBatchRequestItem = { ...req, params: { ...req.params, system: [{ type: "text", text: system }] } };
    expect(estimateSubmissionMicros("classify", [plain], DEFAULT_BATCH_PRICES)).toBe(Math.ceil(72.5 + 10 + 512));
  });

  it("is disabled unless the owner sets a cap, and refuses malformed configuration", () => {
    expect(spendCapMicrosFromEnv(undefined)).toBeUndefined();
    expect(spendCapMicrosFromEnv("  ")).toBeUndefined();
    expect(spendCapMicrosFromEnv("12.5")).toBe(12_500_000);
    expect(() => spendCapMicrosFromEnv("lots")).toThrow();
    expect(() => spendCapMicrosFromEnv("-1")).toThrow();
    expect(() => priceTableFromJson("{}")).toThrow();
    expect(() => priceTableFromJson('{"m":{"input":1,"output":1,"cacheRead":1}}')).toThrow();
    expect(priceTableFromJson('{"m":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":2}}').m?.output).toBe(2);
  });
});

describe("global spend breaker (FR-SVC-08)", () => {
  it("answers the cap-reached 429 with reason spend_cap, no Retry-After, and no provider call", async () => {
    // sampleBody: 2 classify items ≈ 2 × 2048 × 0.25 = 1,024 output micros alone — over a $0.001 cap.
    const rig = await makeRig({ spendBreaker: breaker(0.001) });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).toBeNull();
    expect(await res.json()).toMatchObject({ reason: "spend_cap", error: "daily spend cap reached" });
    expect(rig.gateway.createCalls).toHaveLength(0);
    // The chunk budget was not held either (same transaction).
    expect(await rig.usage.usedOn("lic_test_1", new Date().toISOString().slice(0, 10))).toBe(0);
  });

  it("is off when not configured: the same submission goes through", async () => {
    const rig = await makeRig();
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
  });

  it("settles a batch on the card each result's prompt length selects", async () => {
    const rig = await makeRig({ spendBreaker: breaker(100) });
    const today = new Date().toISOString().slice(0, 10);
    const res = await postBatch(rig, { ...sampleBody, purpose: "meeting_recap", model_class: "summarize" });
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    rig.gateway.status = { ...rig.gateway.status, processing_status: "ended", request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [
      // A three-hour CJK transcript: 120K prompt tokens, billed on the long card.
      resultLine("1234", { input_tokens: 120_000, output_tokens: 2_000, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }, MODEL_BY_CLASS.summarize),
      // A short meeting in the same batch stays on the base card.
      resultLine("5678", { input_tokens: 3_000, output_tokens: 500, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }, MODEL_BY_CLASS.summarize),
    ];
    const r = await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } });
    expect(r.status).toBe(200);
    await r.text();
    // 120,000 × 0.5 + 2,000 × 2.5 = 65,000; 3,000 × 0.05 + 500 × 0.25 = 275. Not 6,500 + 275.
    expect(await rig.usage.spentOn(today)).toBe(65_000 + 275);
  });

  it("books the estimate at submission and settles to actual usage once results are read", async () => {
    const rig = await makeRig({ spendBreaker: breaker(1) });
    const today = new Date().toISOString().slice(0, 10);
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    const estimate = await rig.usage.spentOn(today);
    // 2 items × (2048-token cap × 0.25 + schema and chunk input at 0.05) ≈ 1,074 micro-USD on Haiku 5.5.
    expect(estimate).toBeGreaterThan(1_000);

    rig.gateway.status = { ...rig.gateway.status, processing_status: "ended",
      request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [
      resultLine("1234", { input_tokens: 100, output_tokens: 10 }),
      resultLine("5678", { input_tokens: 100, output_tokens: 10 }),
    ];
    const read = async () => {
      const r = await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } });
      expect(r.status).toBe(200);
      await r.text();
    };
    await read();
    // 200 input × 0.05 + 20 output × 0.25 = 15 micro-USD (Haiku 5.5).
    expect(await rig.usage.spentOn(today)).toBe(15);
    await read(); // a re-read books nothing twice
    expect(await rig.usage.spentOn(today)).toBe(15);
  });

  it("refunds the estimate when the provider definitely rejects the create", async () => {
    const rig = await makeRig({ spendBreaker: breaker(1) });
    const { UpstreamError } = await import("../src/gateway.js");
    rig.gateway.createBatch = () => Promise.reject(new UpstreamError("create", 400));
    expect((await postBatch(rig, sampleBody)).status).toBe(502);
    expect(await rig.usage.spentOn(new Date().toISOString().slice(0, 10))).toBe(0);
  });
});

describe("ops metrics (FR-SVC-06)", () => {
  it("is off (404) without METRICS_TOKEN, and 401 for the wrong bearer", async () => {
    const off = await makeRig();
    expect((await off.app.request("/metrics")).status).toBe(404);
    const on = await makeRig({ metricsToken: "ops-secret" });
    expect((await on.app.request("/metrics")).status).toBe(401);
    expect((await on.app.request("/metrics", { headers: { authorization: "Bearer nope" } })).status).toBe(401);
  });

  it("reports per-route rate, p95, 429 and 5xx, reservations and cap consumption — no ids", async () => {
    const rig = await makeRig({ metricsToken: "ops-secret", dailyChunkCap: 2, spendBreaker: breaker(5) });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    expect((await postBatch(rig, sampleBody)).status).toBe(429);
    await rig.app.request("/v1/batch/rb_nope", { headers: { authorization: `Bearer ${rig.token}` } });
    const res = await rig.app.request("/metrics", { headers: { authorization: "Bearer ops-secret" } });
    expect(res.status).toBe(200);
    const text = await res.text();
    const snap = JSON.parse(text) as Record<string, any>;
    expect(snap.routes["/v1/batch"].window_requests).toBe(2);
    expect(snap.routes["/v1/batch"].rate_429).toBe(0.5);
    expect(snap.routes["/v1/batch"].p95_ms).toBeGreaterThan(0);
    expect(snap.routes["/v1/batch/:id"].total_requests).toBe(1);
    expect(snap.batch.plans.standard).toMatchObject({
      reservations: { ok: 1, capped: 1 }, active_licences: 1, chunks_reserved: 2, cap_consumption: 1, licences_over_80pct: 1,
    });
    expect(snap.spend.cap_micros).toBe(5_000_000);
    expect(snap.spend.spent_micros).toBeGreaterThan(0);
    // Nothing identifying: no licence id, no device id, no batch id, no content.
    for (const needle of ["lic_test_1", "dev_test_device", "rb_nope", "TOP-SECRET"]) expect(text).not.toContain(needle);
  });

  it("keeps part of the daily budget for paying users, whatever trials do (issue #763)", async () => {
    const trialToken = (rig: Awaited<ReturnType<typeof makeRig>>) =>
      signLicense(rig.keys.privateKey, { sub: "trial_dev_a", plan: "trial", status: "trialing" });

    // A share of 0: trial submissions are refused on spend, paid ones go through.
    const none = await makeRig({ spendBreaker: breaker(1), trialDailyChunkCap: 50, trialSpendShare: 0 });
    const refused = await postBatch(none, sampleBody, await trialToken(none));
    expect(refused.status).toBe(429);
    expect(((await refused.json()) as { reason: string }).reason).toBe("spend_cap");
    expect((await postBatch(none, sampleBody)).status).toBe(202);
    expect(none.gateway.createCalls).toHaveLength(1);

    // The default share leaves room for a trial submission on an unspent day.
    const half = await makeRig({ spendBreaker: breaker(1), trialDailyChunkCap: 50 });
    expect((await postBatch(half, sampleBody, await trialToken(half))).status).toBe(202);

    // Paid spend counts against the trial's ceiling too. Measure what one submission reserves,
    // then give trials 1.5 submissions' worth of a 10-submission budget.
    const probe = await makeRig({ spendBreaker: breaker(1) });
    const before = utcDate(new Date());
    expect((await postBatch(probe, sampleBody)).status).toBe(202);
    const one = Math.max(await probe.usage.spentOn(before), await probe.usage.spentOn(utcDate(new Date())));
    expect(one).toBeGreaterThan(0);
    const tight = await makeRig({
      spendBreaker: { capMicros: one * 10, prices: DEFAULT_BATCH_PRICES },
      trialDailyChunkCap: 50,
      trialSpendShare: 0.15,
    });
    const trial = await trialToken(tight);
    expect((await postBatch(tight, sampleBody, trial)).status).toBe(202); // total 1 of the trial's 1.5
    expect((await postBatch(tight, sampleBody)).status).toBe(202); // paid: total 2 of 10
    expect((await postBatch(tight, sampleBody, trial)).status).toBe(429); // 3 would pass 1.5
    expect((await postBatch(tight, sampleBody)).status).toBe(202); // paid carries on

    // Garbage is no share at all, never the whole budget.
    const nan = await makeRig({ spendBreaker: breaker(1), trialDailyChunkCap: 50, trialSpendShare: Number.NaN });
    expect((await postBatch(nan, sampleBody, await trialToken(nan))).status).toBe(429);
  });

  it("measures a plan with its own cap against that cap", () => {
    // Trial Macs have a lower cap (issue #763): against the paid cap their consumption would
    // read low and the 0.8 alert would never fire.
    const m = new RelayMetrics(10, { trial: 2 });
    m.reservation("2026-10-01", "trial", "trial_dev_a", "ok", 2);
    m.reservation("2026-10-01", "standard", "lic_1", "ok", 2);
    const plans = m.snapshot(0).batch.plans;
    expect(plans.trial).toMatchObject({ cap_consumption: 1, licences_over_80pct: 1 });
    expect(plans.standard).toMatchObject({ cap_consumption: 0.2, licences_over_80pct: 0 });
  });

  it("computes the rolling window and p95 from the histogram", () => {
    const m = new RelayMetrics(10);
    for (let i = 0; i < 95; i++) m.observe("/v1/batch", 202, 40, 0);
    for (let i = 0; i < 5; i++) m.observe("/v1/batch", 503, 3000, 0);
    const now = m.snapshot(0).routes["/v1/batch"]!;
    expect(now.p95_ms).toBe(50);
    expect(now.rate_5xx).toBe(0.05);
    // Five minutes later the window is empty but the lifetime totals remain.
    const later = m.snapshot(5 * 60_000).routes["/v1/batch"]!;
    expect(later.window_requests).toBe(0);
    expect(later.p95_ms).toBeNull();
    expect(later.total_5xx).toBe(5);
  });
});

describe("gpt-6-luna pricing (Batch lane)", () => {
  it("is priced, not worst-case; an unknown model still is", () => {
    const luna = priceFor(DEFAULT_BATCH_PRICES, "gpt-6-luna");
    expect(luna).toMatchObject({ input: 0.05, output: 0.25 });
    expect(luna).not.toEqual(priceFor(DEFAULT_BATCH_PRICES, "unknown-model"));
    expect(priceFor(DEFAULT_BATCH_PRICES, "unknown-model")).toEqual(DEFAULT_BATCH_PRICES["claude-sonnet-5"]);
  });

  it("a 1,000-item luna reservation is ~20x below worst-case pricing and a 500-item hour fits the 0.50 USD licence day", () => {
    const item = (i: number): AnthropicBatchRequestItem => ({
      custom_id: String(i),
      params: { model: "gpt-6-luna", max_tokens: MAX_TOKENS_BY_CLASS.classify, system: [{ type: "text", text: "s".repeat(2000) }], messages: [{ role: "user", content: "c".repeat(2000) }], output_config: { effort: "low" } },
    });
    const items = Array.from({ length: 1000 }, (_, i) => item(i));
    const micros = estimateSubmissionMicros("classify", items, DEFAULT_BATCH_PRICES);
    const worst = estimateSubmissionMicros("classify", items, { x: DEFAULT_BATCH_PRICES["claude-sonnet-5"]! });
    expect(micros).toBeLessThan(worst / 10);
    // ~612 micro-USD per item (2,048 reserved output tokens dominate): a full 1,000-item batch
    // reserves ~0.61 USD, over the 0.50 default; up to ~800 items fit. Documented in the PR.
    expect(micros / 1000).toBeLessThan(700);
    expect(micros / 2).toBeLessThan(500_000);
  });
});

describe("SPEND_PRICE_TABLE_JSON merges over the defaults; unknown models never price cheap (audit 2026-10-20 L2)", () => {
  it("an override row replaces only its own model; every other row keeps its shipped price", () => {
    const merged = priceTableWithOverride('{"gpt-6-luna":{"input":0.06,"output":0.3,"cacheRead":0.006,"cacheWrite":0.07}}');
    expect(merged["gpt-6-luna"]).toEqual({ input: 0.06, output: 0.3, cacheRead: 0.006, cacheWrite: 0.07 });
    for (const [model, row] of Object.entries(DEFAULT_BATCH_PRICES)) if (model !== "gpt-6-luna") expect(merged[model]).toEqual(row);
    expect(priceTableWithOverride(undefined)).toBe(DEFAULT_BATCH_PRICES);
    expect(priceTableWithOverride("  ")).toBe(DEFAULT_BATCH_PRICES);
    expect(() => priceTableWithOverride("{")).toThrow();
  });

  it("an unknown model is priced at no less than the hard-coded floor, even under a table of cheap rows", () => {
    const cheap = priceTableFromJson('{"m":{"input":0.001,"output":0.001,"cacheRead":0,"cacheWrite":0}}');
    expect(priceFor(cheap, "unknown")).toEqual(UNKNOWN_MODEL_FLOOR);
    expect(priceFor(cheap, undefined)).toEqual(UNKNOWN_MODEL_FLOOR);
    // The floor stays at or above every shipped card, so it is a real worst case.
    for (const row of Object.values(DEFAULT_BATCH_PRICES)) {
      for (const card of [row, ...(row.longPrompt ? [row.longPrompt] : [])]) {
        for (const k of ["input", "output", "cacheRead", "cacheWrite"] as const) expect(UNKNOWN_MODEL_FLOOR[k]).toBeGreaterThanOrEqual(card[k]);
      }
    }
  });

  it("a dated snapshot of a known model prices as the model; any other suffix stays unknown", () => {
    expect(priceFor(DEFAULT_BATCH_PRICES, "gpt-6-luna-2026-10-01")).toEqual(DEFAULT_BATCH_PRICES["gpt-6-luna"]);
    expect(priceFor(DEFAULT_BATCH_PRICES, "gpt-6-luna-20261001")).toEqual(DEFAULT_BATCH_PRICES["gpt-6-luna"]);
    expect(priceFor(DEFAULT_BATCH_PRICES, "gpt-6-luna-pro")).toEqual(UNKNOWN_MODEL_FLOOR);
    expect(priceFor(DEFAULT_BATCH_PRICES, "gpt-6-luna-pro-2026-10-01")).toEqual(UNKNOWN_MODEL_FLOOR);
  });
});
