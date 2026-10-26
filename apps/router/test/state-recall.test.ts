import { describe, expect, it } from "vitest";
import { memoryRecall } from "../connectors/shogunagent/endpoints/memory.recall.js";

describe("first-party recall result accounting", () => {
  it("counts the core's grounded evidence even when no typed state has been inferred yet", () => {
    expect(memoryRecall.output.isEmpty!({ result: { items: [], facts: [], possibly: [], evidence: [{ event_id: 1, excerpt: "release review on Thursday" }] }, usage: { cost_micro_usd: 0 } })).toBe(false);
  });
  it("treats an empty core pack as empty even though it includes scalar budget fields", () => {
    expect(memoryRecall.output.isEmpty!({ result: { items: [], facts: [], possibly: [], evidence: [], budget_tokens: 2000, used_tokens: 0 } })).toBe(true);
  });
});
