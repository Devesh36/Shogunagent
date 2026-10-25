/**
 * E36b: `withShogunAgent(node)` inside a real LangGraph.js graph (`@langchain/langgraph`
 * `StateGraph(MessagesAnnotation)` with a `MemorySaver` checkpointer), the node calling LangChain's
 * own `FakeChatModel` (`@langchain/core/utils/testing`), which answers with the text of the
 * messages it was given, so the test sees exactly what reached the model. No network.
 */
import { AIMessage, HumanMessage, SystemMessage, type BaseMessage } from "@langchain/core/messages";
import { FakeChatModel } from "@langchain/core/utils/testing";
import { END, MemorySaver, MessagesAnnotation, START, StateGraph } from "@langchain/langgraph";
import { describe, expect, it } from "vitest";

import { withShogunAgent } from "../../src/adapters/langgraph.js";
import { CONTEXT_OPEN, PaymentRequiredError } from "../../src/index.js";
import { FakeFetch, PAYMENT_402, memoryFake, recallPayload, toolText } from "../fake-fetch.js";
import { blockCount, installNetworkGuard } from "./net-guard.js";

installNetworkGuard();

/** A graph whose one node calls the fake model; `seen` collects what the model received. */
/** LangChain's FakeChatModel, recording the messages as the model layer received them (after
 * LangChain coerced the adapter's plain `{ role: "system" }` message into a `SystemMessage`). */
class RecordingChatModel extends FakeChatModel {
  constructor(private readonly seen: string[][]) {
    super({});
  }

  override async _generate(...args: Parameters<FakeChatModel["_generate"]>): ReturnType<FakeChatModel["_generate"]> {
    this.seen.push(args[0].map((m) => `${m.getType()}:${String(m.content)}`));
    return super._generate(...args);
  }
}

function graph(options: Parameters<typeof withShogunAgent>[1], seen: string[][]) {
  const model = new RecordingChatModel(seen);
  let n = 0;
  const callModel = async (state: typeof MessagesAnnotation.State) => {
    const answer = await model.invoke(state.messages);
    n += 1;
    return { messages: [new AIMessage({ content: `reply ${n}: ${String(answer.content).length} chars`, id: `ai-${n}` })] };
  };
  return new StateGraph(MessagesAnnotation)
    .addNode("agent", withShogunAgent(callModel as never, options) as never)
    .addEdge(START, "agent")
    .addEdge("agent", END)
    .compile({ checkpointer: new MemorySaver() });
}

describe("LangGraph.js (@langchain/langgraph) integration", () => {
  it("invoke: the model sees the block in the system message, the graph state never stores it, the turn is ingested under thread_id", async () => {
    const fake = memoryFake();
    const seen: string[][] = [];
    const app = graph({ fetch: fake.fetch }, seen);
    const config = { configurable: { thread_id: "thread-7" } };
    const out = await app.invoke({ messages: [new SystemMessage("You are helpful."), new HumanMessage("What do I owe Oren?")] }, config);

    const modelSaw = seen[0]!;
    expect(modelSaw[0]!.startsWith("system:You are helpful.")).toBe(true);
    expect(blockCount(modelSaw[0]!)).toBe(1);
    expect(modelSaw[0]).toContain("send the rollout plan by Friday");
    expect(modelSaw.join("\n")).not.toContain("SECRET-LOW-ITEM");
    // The checkpointed state keeps the caller's system message, not the block.
    const state = await app.getState(config);
    const stored = (state.values.messages as BaseMessage[]).map((m) => String(m.content)).join("\n");
    expect(stored).not.toContain(CONTEXT_OPEN);
    expect((out.messages as BaseMessage[]).at(-1)!.getType()).toBe("ai");

    expect(fake.calls.map((c) => c.headers["mcp-name"])).toEqual(["memory.recall", "memory.ingest"]);
    const ingest = fake.toolCalls("memory.ingest")[0]!.body.params.arguments;
    expect(ingest.thread_key).toBe("thread-7");
    expect(ingest.turns[0]).toEqual({ role: "user", content: "What do I owe Oren?" });
    expect(ingest.turns[1].role).toBe("assistant");
    expect(ingest.idempotency_key).toMatch(/^turn:[0-9a-f]{40}$/);
  });

  it("a second turn on the same thread replaces the block instead of stacking it, with a new idempotency key", async () => {
    const fake = memoryFake();
    const seen: string[][] = [];
    const app = graph({ fetch: fake.fetch }, seen);
    const config = { configurable: { thread_id: "thread-8" } };
    await app.invoke({ messages: [new SystemMessage("sys"), new HumanMessage("first")] }, config);
    await app.invoke({ messages: [new HumanMessage("second")] }, config);
    expect(blockCount(seen[1]!.join("\n"))).toBe(1);
    expect(fake.toolCalls("memory.recall").map((c) => c.body.params.arguments.query)).toEqual(["first", "second"]);
    const keys = fake.toolCalls("memory.ingest").map((c) => c.body.params.arguments.idempotency_key as string);
    expect(keys).toHaveLength(2);
    expect(keys[1]).not.toBe(keys[0]);
  });

  it("a 402 on ingest is surfaced once and the graph run completes", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload())).tool("memory.ingest", PAYMENT_402);
    const errors: Array<[unknown, string]> = [];
    const app = graph({ fetch: fake.fetch, onError: (e, p) => errors.push([e, p]) }, []);
    const out = await app.invoke({ messages: [new HumanMessage("q")] }, { configurable: { thread_id: "t" } });
    expect((out.messages as BaseMessage[]).at(-1)!.getType()).toBe("ai");
    expect(fake.toolCalls("memory.ingest")).toHaveLength(1);
    expect(errors).toHaveLength(1);
    expect(errors[0]![0]).toBeInstanceOf(PaymentRequiredError);
  });
});
