/**
 * E36b: `withShogunAgent` / `shogunAgentMiddleware` inside the real Vercel AI SDK (`ai`):
 * `generateText` and `streamText` over `wrapLanguageModel`, with the SDK's own
 * `MockLanguageModelV4` as the model. No provider, no network.
 */
import { generateText, streamText, wrapLanguageModel } from "ai";
import { MockLanguageModelV4, convertArrayToReadableStream } from "ai/test";
import { describe, expect, it } from "vitest";

import { shogunAgentMiddleware, withShogunAgent } from "../../src/adapters/ai-sdk.js";
import { PaymentRequiredError } from "../../src/index.js";
import { FakeFetch, PAYMENT_402, memoryFake, recallPayload, toolText } from "../fake-fetch.js";
import { blockCount, installNetworkGuard } from "./net-guard.js";

installNetworkGuard();

const usage = {
  inputTokens: { total: 3, noCache: 3, cacheRead: undefined, cacheWrite: undefined },
  outputTokens: { total: 5, text: 5, reasoning: undefined },
};

function mockModel(reply: string, responseId: string): MockLanguageModelV4 {
  return new MockLanguageModelV4({
    doGenerate: async () => ({
      content: [{ type: "text", text: reply }],
      finishReason: { unified: "stop", raw: "stop" },
      usage,
      warnings: [],
      response: { id: responseId, timestamp: new Date(0), modelId: "mock" },
    }),
    doStream: async () => ({
      stream: convertArrayToReadableStream([
        { type: "stream-start", warnings: [] },
        { type: "response-metadata", id: responseId, timestamp: new Date(0), modelId: "mock" },
        { type: "text-start", id: "t1" },
        { type: "text-delta", id: "t1", delta: reply.slice(0, 4) },
        { type: "text-delta", id: "t1", delta: reply.slice(4) },
        { type: "text-end", id: "t1" },
        { type: "finish", finishReason: { unified: "stop", raw: "stop" }, usage },
      ] as never),
    }),
  });
}

/** The system text the model actually received on its last call. */
function systemSeen(model: MockLanguageModelV4, streamed = false): string {
  const calls = streamed ? model.doStreamCalls : model.doGenerateCalls;
  const prompt = calls.at(-1)?.prompt ?? [];
  return prompt.filter((m) => m.role === "system").map((m) => String(m.content)).join("\n\n");
}

describe("Vercel AI SDK (ai) integration", () => {
  it("generateText over wrapLanguageModel: recall before the model, ingest after, keyed by the response id", async () => {
    const fake = memoryFake();
    const model = mockModel("I will draft the plan.", "resp_1");
    const wrapped = wrapLanguageModel({ model, middleware: shogunAgentMiddleware({ fetch: fake.fetch, threadKey: "task-1" }) });
    const result = await generateText({ model: wrapped, system: "You are helpful.", prompt: "What do I owe Oren?" });

    expect(result.text).toBe("I will draft the plan.");
    // Order: the recall reached ShogunAgent before the model ran, the ingest after.
    const order = fake.calls.map((c) => c.headers["mcp-name"]);
    expect(order).toEqual(["memory.recall", "memory.ingest"]);
    expect(model.doGenerateCalls).toHaveLength(1);
    const seen = systemSeen(model);
    expect(blockCount(seen)).toBe(1);
    expect(seen).toContain("You are helpful.");
    expect(seen).toContain("send the rollout plan by Friday");
    expect(seen).not.toContain("SECRET-LOW-ITEM");
    expect(fake.toolCalls("memory.recall")[0]!.body.params.arguments.query).toBe("What do I owe Oren?");
    const ingest = fake.toolCalls("memory.ingest")[0]!;
    expect(ingest.body.params.arguments).toMatchObject({
      thread_key: "task-1",
      turns: [{ role: "user", content: "What do I owe Oren?" }, { role: "assistant", content: "I will draft the plan." }],
    });
    expect(ingest.body.params.arguments.idempotency_key).toMatch(/^turn:[0-9a-f]{40}$/);
    expect(ingest.headers["mcp-method"]).toBe("tools/call");
  });

  it("the same response id replays under the same idempotency key; a new one gets a new key", async () => {
    const fake = memoryFake();
    const mw = shogunAgentMiddleware({ fetch: fake.fetch, threadKey: "task-1" });
    for (const id of ["resp_1", "resp_1", "resp_2"]) {
      await generateText({ model: wrapLanguageModel({ model: mockModel("ok", id), middleware: mw }), prompt: "status?" });
    }
    const keys = fake.toolCalls("memory.ingest").map((c) => c.body.params.arguments.idempotency_key as string);
    expect(keys).toHaveLength(3);
    expect(keys[1]).toBe(keys[0]);
    expect(keys[2]).not.toBe(keys[0]);
  });

  it("streamText: the block reaches the stream call and the streamed text is ingested once the stream ends", async () => {
    const fake = memoryFake();
    const model = mockModel("Hello world", "resp_s");
    const wrapped = withShogunAgent(model, { fetch: fake.fetch, threadKey: "s" });
    const result = streamText({ model: wrapped, prompt: "hi" });
    let text = "";
    for await (const delta of result.textStream) text += delta;
    await result.consumeStream();
    expect(text).toBe("Hello world");
    expect(blockCount(systemSeen(model, true))).toBe(1);
    await expect.poll(() => fake.toolCalls("memory.ingest").length).toBe(1);
    expect(fake.toolCalls("memory.ingest")[0]!.body.params.arguments.turns).toEqual([
      { role: "user", content: "hi" },
      { role: "assistant", content: "Hello world" },
    ]);
  });

  it("a 402 on ingest is surfaced once with its payment payload, not retried, and generateText still answers", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload())).tool("memory.ingest", PAYMENT_402);
    const errors: Array<[unknown, string]> = [];
    const model = mockModel("fine", "resp_402");
    const wrapped = wrapLanguageModel({ model, middleware: shogunAgentMiddleware({ fetch: fake.fetch, onError: (e, p) => errors.push([e, p]) }) });
    const result = await generateText({ model: wrapped, prompt: "q" });
    expect(result.text).toBe("fine");
    expect(fake.toolCalls("memory.ingest")).toHaveLength(1);
    expect(errors).toHaveLength(1);
    expect(errors[0]![1]).toBe("ingest");
    const err = errors[0]![0] as PaymentRequiredError;
    expect(err).toBeInstanceOf(PaymentRequiredError);
    expect(err.code).toBe("payment_required");
    expect(err.nextAction).toBe("top_up");
    expect(err.topUpUrl).toBe("https://api.test/v1/wallet/topup");
  });

  it("a cap_exceeded 402 on recall: no block, the model still runs once, the error says retry_after", async () => {
    const cap = {
      status: 402,
      headers: { "retry-after": "30" },
      body: { error: { code: "cap_exceeded", message: "The daily cap is spent.", next_action: "retry_after" }, payment: { amount_micro_usd: 0, unit: "read", accepts: [], retry_after: 30 } },
    };
    const fake = new FakeFetch().tool("memory.recall", cap).tool("memory.ingest", toolText({ result: {} }));
    const errors: Array<[unknown, string]> = [];
    const model = mockModel("ok", "resp_cap");
    await generateText({ model: wrapLanguageModel({ model, middleware: shogunAgentMiddleware({ fetch: fake.fetch, onError: (e, p) => errors.push([e, p]) }) }), system: "sys", prompt: "q" });
    expect(model.doGenerateCalls).toHaveLength(1);
    expect(blockCount(systemSeen(model))).toBe(0);
    expect(fake.toolCalls("memory.recall")).toHaveLength(1);
    const [err, phase] = errors[0]!;
    expect(phase).toBe("recall");
    expect(err).toBeInstanceOf(PaymentRequiredError);
    expect((err as PaymentRequiredError).code).toBe("cap_exceeded");
    expect((err as PaymentRequiredError).nextAction).toBe("retry_after");
    expect((err as PaymentRequiredError).retryAfter).toBe(30);
    expect((err as PaymentRequiredError).topUpUrl).toBeNull();
  });
});
