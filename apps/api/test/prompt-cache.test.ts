/** #667: a shared classification instruction rides once as `system`, is forwarded as the same
 * `system` block on every request (the prompt-cache prefix), and gets a `cache_control`
 * breakpoint only when it reaches the class model's cacheable minimum — a relay decision, never
 * the device's. Result lines pass the provider's token counts (incl. cache) through. */
import { createHash } from "node:crypto";

import { describe, expect, it } from "vitest";

import { parseResultLine, transformResultLine } from "../src/app.js";
import {
  MIN_CACHEABLE_TOKENS_BY_CLASS,
  MODEL_BY_CLASS,
  estimateTokens,
  systemBlocksFor,
} from "../src/models.js";
import { DEFAULT_BATCH_PRICES, priceFor, usageMicros } from "../src/spend.js";
import { MAX_SYSTEM_BYTES, parseRelayBatchRequest, relayPayloadHash, type RelayUsage } from "../src/types.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";

const SHORT_SYSTEM = "Return ONLY a JSON object. ".repeat(100); // ~2.7 KB ≈ 675 tokens, like today's prompt
const LONG_SYSTEM = "Shared classification rules. ".repeat(600); // ~17 KB ≈ 4.4k tokens
const TINY_SYSTEM = "Return ONLY a JSON object. ".repeat(40); // ~1.1 KB ≈ 270 tokens, under Haiku 5.5's 512

const sharedBody = (system: string) => ({
  purpose: "consolidation",
  model_class: "classify",
  system,
  items: [
    { custom_id: "1234", data: "<<<CONTEXT>>>\nEVENT-ALPHA\n<<<END CONTEXT>>>" },
    { custom_id: "5678", data: "<<<CONTEXT>>>\nEVENT-BRAVO\n<<<END CONTEXT>>>" },
  ],
});

describe("shared system prefix (#667)", () => {
  it("forwards one byte-identical system block per request and the item data after it", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, sharedBody(LONG_SYSTEM));
    expect(res.status).toBe(202);
    const [a, b] = rig.gateway.createCalls[0] ?? [];
    expect(a?.params.system).toEqual([
      { type: "text", text: LONG_SYSTEM, cache_control: { type: "ephemeral", ttl: "1h" } },
    ]);
    // Everything up to and including the breakpoint is identical across the batch.
    expect(JSON.stringify(a?.params.system)).toBe(JSON.stringify(b?.params.system));
    // The per-item text is only in the user turn, after the prefix, with no breakpoint.
    expect(a?.params.messages).toEqual([
      { role: "user", content: "<<<CONTEXT>>>\nEVENT-ALPHA\n<<<END CONTEXT>>>" },
    ]);
    expect(b?.params.messages[0]?.content).toContain("EVENT-BRAVO");
    expect(JSON.stringify(a?.params.system)).not.toContain("EVENT-");
    expect(a?.params.model).toBe(MODEL_BY_CLASS.classify);
  });

  it("gives today's ~2.9 KB consolidation instruction the 1-hour breakpoint on Haiku 5.5 (512-token minimum)", async () => {
    // On Haiku 4.5 (4,096 minimum) this prefix was ~5.7× too short and the structure never engaged.
    expect(MIN_CACHEABLE_TOKENS_BY_CLASS.classify).toBe(512);
    expect(estimateTokens(SHORT_SYSTEM)).toBeGreaterThanOrEqual(MIN_CACHEABLE_TOKENS_BY_CLASS.classify);
    const rig = await makeRig();
    expect((await postBatch(rig, sharedBody(SHORT_SYSTEM))).status).toBe(202);
    const forwarded = rig.gateway.createCalls[0];
    expect(forwarded?.[0]?.params.system).toEqual([
      { type: "text", text: SHORT_SYSTEM, cache_control: { type: "ephemeral", ttl: "1h" } },
    ]);
    expect(forwarded?.[1]?.params.system).toEqual(forwarded?.[0]?.params.system);
  });

  it("sends no breakpoint when the prefix is below the class model's minimum", async () => {
    expect(estimateTokens(TINY_SYSTEM)).toBeLessThan(MIN_CACHEABLE_TOKENS_BY_CLASS.classify);
    const rig = await makeRig();
    expect((await postBatch(rig, sharedBody(TINY_SYSTEM))).status).toBe(202);
    const forwarded = rig.gateway.createCalls[0];
    expect(forwarded?.[0]?.params.system).toEqual([{ type: "text", text: TINY_SYSTEM }]);
    expect(JSON.stringify(forwarded)).not.toContain("cache_control");
  });

  it("keeps the original shape when no system is sent", async () => {
    const rig = await makeRig();
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const forwarded = rig.gateway.createCalls[0];
    expect(forwarded?.[0]?.params).not.toHaveProperty("system");
    expect(forwarded?.[0]?.params.messages[0]?.content).toBe("TOP-SECRET-CHUNK-ALPHA");
  });

  it("never forwards a device-sent cache_control — caching cost is the relay's call", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, {
      ...sharedBody(TINY_SYSTEM),
      cache_control: { type: "ephemeral", ttl: "1h" },
      items: [{ custom_id: "1", data: "d", cache_control: { type: "ephemeral" } }],
    });
    expect(res.status).toBe(202);
    expect(JSON.stringify(rig.gateway.createCalls[0])).not.toContain("cache_control");
  });

  it("rejects mixed or mismatched shapes with 400 before reserving or calling upstream", async () => {
    const bad: unknown[] = [
      // data without a system: forwarding it would bill instruction-less requests
      { ...sampleBody, items: [{ custom_id: "1", data: "d" }] },
      // system with chunk items: ambiguous — which half is the instruction?
      { ...sharedBody(SHORT_SYSTEM), items: [{ custom_id: "1", chunk: "c" }] },
      { ...sharedBody(SHORT_SYSTEM), items: [{ custom_id: "1", data: "d", chunk: "c" }] },
      { ...sharedBody(""), items: [{ custom_id: "1", data: "d" }] },
      { ...sharedBody(42 as unknown as string) },
      { ...sharedBody("x".repeat(MAX_SYSTEM_BYTES + 1)) },
    ];
    for (const body of bad) {
      const rig = await makeRig();
      const res = await postBatch(rig, body);
      expect(res.status).toBe(400);
      expect(rig.gateway.createCalls).toHaveLength(0);
      expect(rig.usage.records).toHaveLength(0);
    }
  });

  it("hashes a system-less body exactly as before and makes the system part of the identity", () => {
    const legacy = parseRelayBatchRequest({ ...sampleBody, client_request_id: "parse" });
    if (typeof legacy === "string") throw new Error(legacy);
    expect(legacy).not.toHaveProperty("system");
    // The pre-#667 canonical form, spelled out: bound ledger ids must keep replaying.
    const before = createHash("sha256")
      .update(
        JSON.stringify({
          purpose: "consolidation",
          model_class: "classify",
          items: [
            ["1234", "TOP-SECRET-CHUNK-ALPHA"],
            ["5678", "TOP-SECRET-CHUNK-BRAVO"],
          ],
        }),
      )
      .digest("hex");
    expect(relayPayloadHash(legacy)).toBe(before);

    const one = parseRelayBatchRequest({ ...sharedBody("rules v1"), client_request_id: "parse" });
    const two = parseRelayBatchRequest({ ...sharedBody("rules v2"), client_request_id: "parse" });
    if (typeof one === "string" || typeof two === "string") throw new Error("parse failed");
    expect(relayPayloadHash(one)).not.toBe(relayPayloadHash(two));
  });

  it("gates the breakpoint on each class model's minimum, inclusively", () => {
    for (const cls of ["classify", "summarize", "brief", "about_profile", "daily_activity_summary"] as const) {
      const at = "x".repeat(MIN_CACHEABLE_TOKENS_BY_CLASS[cls] * 4);
      const under = "x".repeat(MIN_CACHEABLE_TOKENS_BY_CLASS[cls] * 4 - 4);
      expect(systemBlocksFor(cls, at)[0]?.cache_control).toEqual({ type: "ephemeral", ttl: "1h" });
      expect(systemBlocksFor(cls, under)[0]).not.toHaveProperty("cache_control");
    }
    // The 5.5 generation caches from 512 tokens on every class model.
    expect(Object.values(MIN_CACHEABLE_TOKENS_BY_CLASS)).toEqual([512, 512, 512, 512, 512]);
  });
});

describe("usage passthrough (#667)", () => {
  it("copies the four token counts and nothing else from a succeeded line", () => {
    const line = JSON.stringify({
      custom_id: "a",
      result: {
        type: "succeeded",
        message: {
          content: [{ type: "text", text: "label" }],
          usage: {
            input_tokens: 40,
            output_tokens: 7,
            cache_read_input_tokens: 700,
            cache_creation_input_tokens: 0,
            service_tier: "batch",
          },
        },
      },
    });
    expect(transformResultLine(line)).toEqual({
      custom_id: "a",
      text: "label",
      usage: {
        input_tokens: 40,
        output_tokens: 7,
        cache_read_input_tokens: 700,
        cache_creation_input_tokens: 0,
      },
    });
  });

  it("omits usage when the provider sent none", () => {
    const line = JSON.stringify({
      custom_id: "b",
      result: { type: "succeeded", message: { content: [{ type: "text", text: "x" }] } },
    });
    expect(transformResultLine(line)).toEqual({ custom_id: "b", text: "x" });
  });

  it("reads content by block type: a leading thinking block is skipped and its tokens stay in output_tokens", () => {
    // Haiku 5.5 thinks adaptively by default; the thinking text is omitted (empty) and only a
    // signature comes back. The answer is the text block, wherever it sits.
    const line = JSON.stringify({
      custom_id: "c",
      result: {
        type: "succeeded",
        message: {
          model: "claude-haiku-5-5",
          stop_reason: "end_turn",
          content: [
            { type: "thinking", thinking: "", signature: "sig-opaque" },
            { type: "thinking", thinking: "", signature: "sig-opaque-2" },
            { type: "text", text: '{"label":"x"}' },
          ],
          usage: { input_tokens: 900, output_tokens: 240, cache_read_input_tokens: 716, cache_creation_input_tokens: 0 },
        },
      },
    });
    const { result, model } = parseResultLine(line);
    expect(model).toBe("claude-haiku-5-5");
    expect(result).toEqual({
      custom_id: "c",
      text: '{"label":"x"}',
      usage: { input_tokens: 900, output_tokens: 240, cache_read_input_tokens: 716, cache_creation_input_tokens: 0 },
    });
    expect(JSON.stringify(result)).not.toContain("sig-opaque");
    // 900 × 0.05 + 240 × 0.25 (thinking included, nothing added for it) + 716 × 0.005 = 45 + 60 + 3.58
    expect(usageMicros(result.usage as RelayUsage, priceFor(DEFAULT_BATCH_PRICES, model))).toBeCloseTo(108.58);
  });

  it("turns a succeeded result with stop_reason refusal into an error entry that keeps its billed usage", () => {
    const line = JSON.stringify({
      custom_id: "d",
      result: {
        type: "succeeded",
        message: {
          model: "claude-haiku-5-5",
          stop_reason: "refusal",
          stop_details: { category: "general_harms" },
          content: [{ type: "thinking", thinking: "", signature: "s" }],
          usage: { input_tokens: 1000, output_tokens: 20, cache_read_input_tokens: 0, cache_creation_input_tokens: 716 },
        },
      },
    });
    const { result, model } = parseResultLine(line);
    expect(result).toEqual({
      custom_id: "d",
      error: "refusal",
      usage: { input_tokens: 1000, output_tokens: 20, cache_read_input_tokens: 0, cache_creation_input_tokens: 716 },
    });
    expect(result).not.toHaveProperty("text");
    expect(JSON.stringify(result)).not.toContain("general_harms");
    // 1000 × 0.05 + 20 × 0.25 + 716 × 0.1 = 50 + 5 + 71.6
    expect(usageMicros(result.usage as RelayUsage, priceFor(DEFAULT_BATCH_PRICES, model))).toBeCloseTo(126.6);
    // Without usage, still an error entry and nothing thrown.
    expect(transformResultLine(JSON.stringify({ custom_id: "e", result: { type: "succeeded", message: { stop_reason: "refusal", content: [] } } })))
      .toEqual({ custom_id: "e", error: "refusal" });
  });

  it("turns a succeeded result with no text block and a stop reason other than end_turn into an error entry with its usage", () => {
    // Adaptive thinking spent the whole max_tokens before any text: the device must see why,
    // not an empty verdict to fail parsing on. The usage stays — the request was billed in full.
    const truncated = JSON.stringify({
      custom_id: "f",
      result: {
        type: "succeeded",
        message: {
          model: "claude-sonnet-5-5",
          stop_reason: "max_tokens",
          content: [{ type: "thinking", thinking: "", signature: "s" }],
          usage: { input_tokens: 5_000, output_tokens: 8_192, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 },
        },
      },
    });
    const { result, model } = parseResultLine(truncated);
    expect(model).toBe("claude-sonnet-5-5");
    expect(result).toEqual({
      custom_id: "f",
      error: "max_tokens",
      usage: { input_tokens: 5_000, output_tokens: 8_192, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 },
    });
    expect(result).not.toHaveProperty("text");
    // No usage, no stop reason: still an error entry, never a silent "".
    expect(transformResultLine(JSON.stringify({ custom_id: "g", result: { type: "succeeded", message: { content: [{ type: "thinking", thinking: "", signature: "s" }] } } })))
      .toEqual({ custom_id: "g", error: "no text" });
    // A legitimately empty answer that ended normally is still an empty text (the device's call).
    expect(transformResultLine(JSON.stringify({ custom_id: "h", result: { type: "succeeded", message: { stop_reason: "end_turn", content: [] } } })))
      .toEqual({ custom_id: "h", text: "" });
    // Text that arrived before the cap is still text: the device decides whether it parses.
    expect(transformResultLine(JSON.stringify({ custom_id: "i", result: { type: "succeeded", message: { stop_reason: "max_tokens", content: [{ type: "text", text: "{\"partial" }] } } })))
      .toEqual({ custom_id: "i", text: "{\"partial" });
  });

  it("settles a batch whose results mix thinking-led answers and a refusal, at the Haiku 5.5 rates", async () => {
    const rig = await makeRig({ spendBreaker: { capMicros: 100_000_000, prices: DEFAULT_BATCH_PRICES } });
    const res = await postBatch(rig, sharedBody(SHORT_SYSTEM));
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    const message = (extra: Record<string, unknown>) => ({
      model: "claude-haiku-5-5",
      usage: { input_tokens: 100, output_tokens: 10, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 },
      ...extra,
    });
    rig.gateway.status = { ...rig.gateway.status, processing_status: "ended", request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [
      JSON.stringify({ custom_id: "1234", result: { type: "succeeded", message: message({ stop_reason: "end_turn", content: [{ type: "thinking", thinking: "", signature: "s" }, { type: "text", text: "{}" }] }) } }),
      JSON.stringify({ custom_id: "5678", result: { type: "succeeded", message: message({ stop_reason: "refusal", content: [] }) } }),
    ];
    const r = await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } });
    expect(r.status).toBe(200);
    const body = (await r.json()) as { status: string; results: unknown[] };
    expect(body.results).toEqual([
      { custom_id: "1234", text: "{}", usage: { input_tokens: 100, output_tokens: 10, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } },
      { custom_id: "5678", error: "refusal", usage: { input_tokens: 100, output_tokens: 10, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } },
    ]);
    // Both results billed: 2 × (100 × 0.05 + 10 × 0.25) = 15 micro-USD replaces the estimate.
    expect(await rig.usage.spentOn(new Date().toISOString().slice(0, 10))).toBe(15);
  });
});
