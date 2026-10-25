/**
 * Request shaping and response parsing for every adapter, through an injected fetch. No network.
 */
import { describe, expect, it } from "vitest";
import { anthropicAdapter, parseAnthropic } from "../src/adapters/anthropic.js";
import { geminiAdapter, parseGemini } from "../src/adapters/gemini.js";
import type { FetchLike } from "../src/adapters/http.js";
import { resolveAdapters } from "../src/adapters/index.js";
import { localAdapter, parseLocal } from "../src/adapters/local.js";
import { openaiAdapter, parseOpenAI } from "../src/adapters/openai.js";
import type { PickRequest } from "../src/types.js";

const req: PickRequest = {
  model: "ignored-by-adapter",
  system: "sys",
  prompt: "what did I promise",
  tools: [{ name: "shogunagent__x", description: "d", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] } }],
};

interface Captured {
  url: string;
  headers: Record<string, string>;
  body: Record<string, unknown>;
}

function fakeFetch(response: unknown, status = 200): { fetch: FetchLike; captured: Captured[] } {
  const captured: Captured[] = [];
  const fetch: FetchLike = async (url, init) => {
    captured.push({ url, headers: Object.fromEntries(Object.entries(init.headers as Record<string, string>)), body: JSON.parse(String(init.body)) as Record<string, unknown> });
    return new Response(JSON.stringify(response), { status, headers: { "content-type": "application/json" } });
  };
  return { fetch, captured };
}

describe("anthropic", () => {
  it("shapes a Messages request with tools and reads the first tool_use", async () => {
    const f = fakeFetch({ content: [{ type: "text", text: "Let me check." }, { type: "tool_use", name: "shogunagent__x", input: { query: "q" } }] });
    const a = anthropicAdapter({ apiKey: "sk-ant-test", model: "claude-sonnet-5-5", fetch: f.fetch });
    const r = await a.pick(req);
    expect(r).toEqual({ tool: "shogunagent__x", arguments: { query: "q" } });
    const c = f.captured[0]!;
    expect(c.url).toBe("https://api.anthropic.com/v1/messages");
    expect(c.headers["x-api-key"]).toBe("sk-ant-test");
    expect(c.headers["anthropic-version"]).toBe("2023-06-01");
    expect(c.body["model"]).toBe("claude-sonnet-5-5");
    expect(c.body["system"]).toBe("sys");
    expect(c.body["tool_choice"]).toEqual({ type: "auto", disable_parallel_tool_use: true });
    expect((c.body["tools"] as unknown[])[0]).toEqual({ name: "shogunagent__x", description: "d", input_schema: req.tools[0]!.parameters });
    expect(a.id).toBe("anthropic:claude-sonnet-5-5");
  });
  it("returns none for a text-only reply", () => {
    expect(parseAnthropic({ content: [{ type: "text", text: "No tool fits." }] })).toEqual({ tool: null, text: "No tool fits." });
  });
});

describe("openai", () => {
  it("shapes a Responses request and reads the first function_call", async () => {
    const f = fakeFetch({ output: [{ type: "function_call", name: "shogunagent__x", arguments: "{\"query\":\"q\"}" }] });
    const a = openaiAdapter({ apiKey: "sk-test", model: "gpt-5-mini", fetch: f.fetch, reasoningEffort: "low" });
    const r = await a.pick(req);
    expect(r).toEqual({ tool: "shogunagent__x", arguments: { query: "q" } });
    const c = f.captured[0]!;
    expect(c.url).toBe("https://api.openai.com/v1/responses");
    expect(c.headers["authorization"]).toBe("Bearer sk-test");
    expect(c.body["instructions"]).toBe("sys");
    expect(c.body["input"]).toBe("what did I promise");
    expect(c.body["tool_choice"]).toBe("auto");
    expect(c.body["store"]).toBe(false);
    expect(c.body["reasoning"]).toEqual({ effort: "low" });
    expect((c.body["tools"] as unknown[])[0]).toMatchObject({ type: "function", name: "shogunagent__x", description: "d" });
  });
  it("returns none for a message-only output", () => {
    expect(parseOpenAI({ output: [{ type: "message", content: [{ type: "output_text", text: "Nope" }] }] })).toEqual({ tool: null, text: "Nope" });
  });
});

describe("gemini", () => {
  it("shapes a generateContent request and reads the first functionCall", async () => {
    const f = fakeFetch({ candidates: [{ content: { parts: [{ functionCall: { name: "shogunagent__x", args: { query: "q" } } }] } }] });
    const a = geminiAdapter({ apiKey: "g-test", model: "gemini-2.5-flash", fetch: f.fetch });
    const r = await a.pick(req);
    expect(r).toEqual({ tool: "shogunagent__x", arguments: { query: "q" } });
    const c = f.captured[0]!;
    expect(c.url).toBe("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent");
    expect(c.headers["x-goog-api-key"]).toBe("g-test");
    expect(c.url).not.toContain("g-test");
    expect(c.body["systemInstruction"]).toEqual({ parts: [{ text: "sys" }] });
    expect(c.body["toolConfig"]).toEqual({ functionCallingConfig: { mode: "AUTO" } });
    const decls = (c.body["tools"] as Array<{ functionDeclarations: unknown[] }>)[0]!.functionDeclarations;
    expect(decls[0]).toEqual({ name: "shogunagent__x", description: "d", parameters: req.tools[0]!.parameters });
    expect(JSON.stringify(decls)).not.toContain("additionalProperties");
  });
  it("returns none for a text-only candidate", () => {
    expect(parseGemini({ candidates: [{ content: { parts: [{ text: "No." }] } }] })).toEqual({ tool: null, text: "No." });
  });
});

describe("local openai-compatible", () => {
  it("shapes a chat.completions request and reads tool_calls[0]", async () => {
    const f = fakeFetch({ choices: [{ message: { content: null, tool_calls: [{ type: "function", function: { name: "shogunagent__x", arguments: "{\"query\":\"q\"}" } }] } }] });
    const a = localAdapter({ baseUrl: "http://localhost:11434/v1/", model: "llama3.1", fetch: f.fetch });
    const r = await a.pick(req);
    expect(r).toEqual({ tool: "shogunagent__x", arguments: { query: "q" } });
    const c = f.captured[0]!;
    expect(c.url).toBe("http://localhost:11434/v1/chat/completions");
    expect(c.headers["authorization"]).toBeUndefined();
    expect(c.body["messages"]).toEqual([{ role: "system", content: "sys" }, { role: "user", content: "what did I promise" }]);
    expect((c.body["tools"] as unknown[])[0]).toEqual({ type: "function", function: { name: "shogunagent__x", description: "d", parameters: req.tools[0]!.parameters } });
  });
  it("returns none for a plain assistant message", () => {
    expect(parseLocal({ choices: [{ message: { content: "none fits" } }] })).toEqual({ tool: null, text: "none fits" });
  });
});

describe("errors", () => {
  it("surfaces the status without the request (keys never appear)", async () => {
    const f = fakeFetch({ error: "bad" }, 401);
    const a = anthropicAdapter({ apiKey: "sk-ant-secret", model: "m", fetch: f.fetch });
    await expect(a.pick(req)).rejects.toThrow(/api.anthropic.com responded 401/);
    await expect(a.pick(req)).rejects.not.toThrow(/sk-ant-secret/);
  });
});

describe("resolveAdapters", () => {
  it("is mock with no keys, live with any key, and mock when forced", () => {
    expect(resolveAdapters({})).toMatchObject({ mode: "mock" });
    expect(resolveAdapters({}).adapters.map((a) => a.id)).toEqual(["mock-bm25"]);
    const live = resolveAdapters({ ANTHROPIC_API_KEY: "a", OPENAI_API_KEY: "b", GEMINI_API_KEY: "c", LOCAL_OPENAI_BASE_URL: "http://x/v1" });
    expect(live.mode).toBe("live");
    expect(live.adapters.map((a) => a.provider)).toEqual(["anthropic", "openai", "gemini", "local"]);
    expect(resolveAdapters({ ANTHROPIC_API_KEY: "a" }, { forceMock: true }).mode).toBe("mock");
    expect(resolveAdapters({ ANTHROPIC_API_KEY: "a", SELECTION_EVAL_MODE: "mock" }).mode).toBe("mock");
  });
  it("takes model ids from the environment", () => {
    const r = resolveAdapters({ OPENAI_API_KEY: "b", SELECTION_EVAL_OPENAI_MODEL: "gpt-6-luna" });
    expect(r.adapters[0]!.id).toBe("openai:gpt-6-luna");
  });
  it("restricts to --only providers", () => {
    const r = resolveAdapters({ ANTHROPIC_API_KEY: "a", OPENAI_API_KEY: "b" }, { only: ["openai"] });
    expect(r.adapters.map((a) => a.provider)).toEqual(["openai"]);
  });
});
