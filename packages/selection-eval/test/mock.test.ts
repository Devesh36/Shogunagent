import { describe, expect, it } from "vitest";
import { Bm25Index, stem, tokenize } from "../src/bm25.js";
import { mockAdoptionDecision, mockPick } from "../src/mock.js";
import type { PickRequest, ToolSpec } from "../src/types.js";

const params = { type: "object", properties: { query: { type: "string" } }, required: ["query"] };
const tools: ToolSpec[] = [
  { name: "shogunagent__state_commitments_list", description: "Find what the user committed to: promises, deliverables and deadlines they owe others.", parameters: params },
  { name: "exa__search", description: "Neural web search that returns pages matching a described need.", parameters: params },
  { name: "mem0__add_memory", description: "Store memories, facts and preferences about a user for later sessions.", parameters: params },
];

function req(prompt: string): PickRequest {
  return { model: "mock-bm25", system: "", prompt, tools };
}

describe("bm25", () => {
  it("stems the way the router does", () => {
    expect(stem("commitments")).toBe("commit");
    expect(stem("committed")).toBe("commit");
    expect(tokenize("What did I commit to?")).toEqual(["commit"]);
  });
  it("ranks the lexically closest document first", () => {
    const idx = new Bm25Index(tools.map((t) => ({ id: t.name, text: t.description })));
    expect(idx.fit("shogunagent__state_commitments_list", "what did I commit to deliver")).toBeGreaterThan(idx.fit("exa__search", "what did I commit to deliver"));
  });
});

describe("mock picker", () => {
  it("is deterministic", () => {
    const a = mockPick(req("Which promises are overdue?"));
    const b = mockPick(req("Which promises are overdue?"));
    expect(a).toEqual(b);
    expect(a.tool).toBe("shogunagent__state_commitments_list");
  });
  it("picks the web tool for a web task and nothing for an unrelated one", () => {
    expect(mockPick(req("search the web for pages about EU regulation")).tool).toBe("exa__search");
    expect(mockPick(req("Compose a sonnet about rain.")).tool).toBeNull();
  });
  it("breaks ties by listing order", () => {
    const same: ToolSpec[] = [
      { name: "a__t", description: "alpha beta", parameters: params },
      { name: "b__t", description: "alpha beta", parameters: params },
    ];
    expect(mockPick({ model: "m", system: "", prompt: "alpha", tools: same }).tool).toBe("a__t");
  });
  it("honours the threshold", () => {
    expect(mockPick(req("Which promises are overdue?"), 0.99).tool).toBeNull();
  });
});

describe("mock adoption", () => {
  const trigger = "Use ShogunAgent when the task needs who the user works with, what the user committed to, what is pending, an external action that must be approved.";
  const nonTrigger = "Do not use it for general web search, scraping or enrichment, or storing a conversation verbatim.";
  it("proposes on trigger overlap and not on non-trigger overlap", () => {
    expect(mockAdoptionDecision("What did the user commit to and what is pending?", trigger, nonTrigger).proposed).toBe(true);
    expect(mockAdoptionDecision("Search the web and scrape the page.", trigger, nonTrigger).proposed).toBe(false);
    expect(mockAdoptionDecision("Rename files in a folder.", trigger, nonTrigger).proposed).toBe(false);
  });
  it("is deterministic and explains itself", () => {
    const d = mockAdoptionDecision("Approve an external action", trigger, nonTrigger);
    expect(d).toEqual(mockAdoptionDecision("Approve an external action", trigger, nonTrigger));
    expect(d.evidence.some((e) => e.startsWith("net="))).toBe(true);
  });
});
