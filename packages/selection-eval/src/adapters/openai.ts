/**
 * OpenAI Responses API with function tools (`tool_choice: auto`). The first `function_call`
 * output item is the pick; an output with only a `message` item is "none".
 */
import type { ModelAdapter, PickRequest, PickResult } from "../types.js";
import { asArray, asRecord, postJson, tryParseJson, type HttpOptions } from "./http.js";

export interface OpenAIOptions extends HttpOptions {
  apiKey: string;
  model: string;
  baseUrl?: string;
  /** For reasoning models; omitted when undefined. */
  reasoningEffort?: "minimal" | "low" | "medium" | "high";
}

export function openaiBody(req: PickRequest, reasoningEffort?: OpenAIOptions["reasoningEffort"]): Record<string, unknown> {
  const body: Record<string, unknown> = {
    model: req.model,
    instructions: req.system,
    input: req.prompt,
    tools: req.tools.map((t) => ({ type: "function", name: t.name, description: t.description, parameters: t.parameters, strict: false })),
    tool_choice: "auto",
    parallel_tool_calls: false,
    store: false,
  };
  if (reasoningEffort) body["reasoning"] = { effort: reasoningEffort };
  return body;
}

export function parseOpenAI(json: unknown): PickResult {
  const output = asArray(asRecord(json)?.["output"]);
  let text = "";
  for (const item of output) {
    const it = asRecord(item);
    if (!it) continue;
    if (it["type"] === "function_call" && typeof it["name"] === "string") {
      return { tool: it["name"], arguments: tryParseJson(it["arguments"]) };
    }
    if (it["type"] === "message") {
      for (const part of asArray(it["content"])) {
        const p = asRecord(part);
        if (p && typeof p["text"] === "string") text += p["text"];
      }
    }
  }
  return { tool: null, text };
}

export function openaiAdapter(opts: OpenAIOptions): ModelAdapter {
  const base = (opts.baseUrl ?? "https://api.openai.com").replace(/\/$/, "");
  return {
    id: `openai:${opts.model}`,
    provider: "openai",
    model: opts.model,
    async pick(req) {
      const json = await postJson(
        `${base}/v1/responses`,
        { authorization: `Bearer ${opts.apiKey}` },
        openaiBody({ ...req, model: opts.model }, opts.reasoningEffort),
        opts,
      );
      return parseOpenAI(json);
    },
  };
}
