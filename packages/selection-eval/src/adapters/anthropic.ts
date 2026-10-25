/**
 * Anthropic Messages API with tools (`tool_choice: auto`). The first `tool_use` content block is
 * the pick; a reply with only text blocks is "none".
 */
import type { ModelAdapter, PickRequest, PickResult } from "../types.js";
import { asArray, asRecord, postJson, type HttpOptions } from "./http.js";

export interface AnthropicOptions extends HttpOptions {
  apiKey: string;
  model: string;
  baseUrl?: string;
  maxTokens?: number;
}

export function anthropicBody(req: PickRequest, maxTokens: number): Record<string, unknown> {
  return {
    model: req.model,
    max_tokens: maxTokens,
    system: req.system,
    messages: [{ role: "user", content: req.prompt }],
    tools: req.tools.map((t) => ({ name: t.name, description: t.description, input_schema: t.parameters })),
    tool_choice: { type: "auto", disable_parallel_tool_use: true },
  };
}

export function parseAnthropic(json: unknown): PickResult {
  const content = asArray(asRecord(json)?.["content"]);
  let text = "";
  for (const block of content) {
    const b = asRecord(block);
    if (!b) continue;
    if (b["type"] === "tool_use" && typeof b["name"] === "string") {
      return { tool: b["name"], arguments: b["input"] };
    }
    if (b["type"] === "text" && typeof b["text"] === "string") text += b["text"];
  }
  return { tool: null, text };
}

export function anthropicAdapter(opts: AnthropicOptions): ModelAdapter {
  const base = (opts.baseUrl ?? "https://api.anthropic.com").replace(/\/$/, "");
  const maxTokens = opts.maxTokens ?? 512;
  return {
    id: `anthropic:${opts.model}`,
    provider: "anthropic",
    model: opts.model,
    async pick(req) {
      const json = await postJson(
        `${base}/v1/messages`,
        { "x-api-key": opts.apiKey, "anthropic-version": "2023-06-01" },
        anthropicBody({ ...req, model: opts.model }, maxTokens),
        opts,
      );
      return parseAnthropic(json);
    },
  };
}
