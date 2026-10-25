/**
 * E36b: `withShogunAgent(run)` around the real OpenAI Agents SDK (`@openai/agents`): a real
 * `Agent` and `run`, with the SDK's own `ScriptedModel` (`@openai/agents/testing`) as the model.
 * Tracing is disabled and the network guard proves nothing is exported.
 */
import { Agent, Usage, run, setTracingDisabled } from "@openai/agents";
import { ScriptedModel, assistantMessage } from "@openai/agents/testing";
import { beforeAll, describe, expect, it } from "vitest";

import { withShogunAgent } from "../../src/adapters/openai-agents.js";
import { PaymentRequiredError } from "../../src/index.js";
import { FakeFetch, PAYMENT_402, memoryFake, recallPayload, toolText } from "../fake-fetch.js";
import { blockCount, installNetworkGuard } from "./net-guard.js";

installNetworkGuard();
beforeAll(() => setTracingDisabled(true));


function scripted(reply: string, responseId: string): ScriptedModel {
  return new ScriptedModel([{ type: "response", response: { output: [assistantMessage(reply)], usage: new Usage({ requests: 1, inputTokens: 3, outputTokens: 5, totalTokens: 8 }), responseId } } as never]);
}

describe("OpenAI Agents SDK (@openai/agents) integration", () => {
  it("run: recall before the first model call (block in the instructions), ingest after, keyed by lastResponseId", async () => {
    const fake = memoryFake();
    const model = scripted("I will draft the plan.", "resp_1");
    const agent = new Agent({ name: "helper", instructions: "You are helpful.", model });
    const runWithMemory = withShogunAgent(run as never, { fetch: fake.fetch, threadKey: "task-1" }) as (a: unknown, i: string) => Promise<{ finalOutput: unknown; lastResponseId?: string }>;
    const result = await runWithMemory(agent, "What do I owe Oren?");

    expect(result.finalOutput).toBe("I will draft the plan.");
    expect(result.lastResponseId).toBe("resp_1");
    model.assertComplete();
    const instructions = model.firstCall!.request.systemInstructions ?? "";
    expect(instructions.startsWith("You are helpful.")).toBe(true);
    expect(blockCount(instructions)).toBe(1);
    expect(instructions).toContain("send the rollout plan by Friday");
    expect(instructions).not.toContain("SECRET-LOW-ITEM");
    expect(fake.calls.map((c) => c.headers["mcp-name"])).toEqual(["memory.recall", "memory.ingest"]);
    const ingest = fake.toolCalls("memory.ingest")[0]!.body.params.arguments;
    expect(ingest).toMatchObject({ thread_key: "task-1", turns: [{ role: "user", content: "What do I owe Oren?" }, { role: "assistant", content: "I will draft the plan." }] });
    expect(ingest.idempotency_key).toMatch(/^turn:[0-9a-f]{40}$/);
  });

  it("composes with the caller's callModelInputFilter and replays the same response id under the same key", async () => {
    const fake = memoryFake();
    const runWithMemory = withShogunAgent(run as never, { fetch: fake.fetch, threadKey: "task-1" }) as (a: unknown, i: string, o?: unknown) => Promise<unknown>;
    let callerFilterRan = 0;
    const filter = ({ modelData }: { modelData: { instructions?: string; input: unknown } }) => {
      callerFilterRan += 1;
      return { ...modelData, instructions: `${modelData.instructions ?? ""}\nCALLER-FILTER` };
    };
    const models = [scripted("a", "resp_1"), scripted("a", "resp_1"), scripted("b", "resp_2")];
    for (const model of models) {
      await runWithMemory(new Agent({ name: "helper", instructions: "sys", model }), "status?", { callModelInputFilter: filter });
    }
    expect(callerFilterRan).toBe(3);
    const instructions = models[0]!.firstCall!.request.systemInstructions ?? "";
    expect(instructions).toContain("CALLER-FILTER");
    expect(blockCount(instructions)).toBe(1);
    const keys = fake.toolCalls("memory.ingest").map((c) => c.body.params.arguments.idempotency_key as string);
    expect(keys[1]).toBe(keys[0]);
    expect(keys[2]).not.toBe(keys[0]);
  });

  it("a 402 on ingest is surfaced once and the run result is returned unchanged", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload())).tool("memory.ingest", PAYMENT_402);
    const errors: Array<[unknown, string]> = [];
    const runWithMemory = withShogunAgent(run as never, { fetch: fake.fetch, onError: (e, p) => errors.push([e, p]) }) as (a: unknown, i: string) => Promise<{ finalOutput: unknown }>;
    const result = await runWithMemory(new Agent({ name: "helper", instructions: "sys", model: scripted("fine", "r") }), "q");
    expect(result.finalOutput).toBe("fine");
    expect(fake.toolCalls("memory.ingest")).toHaveLength(1);
    expect(errors.map(([e, p]) => [(e as PaymentRequiredError).code, p])).toEqual([["payment_required", "ingest"]]);
  });

  it("recall off and ingest off: the agent runs and ShogunAgent is not called", async () => {
    const fake = memoryFake();
    const model = scripted("ok", "r");
    const runWithMemory = withShogunAgent(run as never, { fetch: fake.fetch, recall: false, ingest: false }) as (a: unknown, i: string) => Promise<unknown>;
    await runWithMemory(new Agent({ name: "helper", instructions: "sys", model }), "q");
    expect(fake.calls).toHaveLength(0);
    expect(model.firstCall!.request.systemInstructions).toBe("sys");
  });
});
