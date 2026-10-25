/**
 * An OpenAI-compatible local endpoint (Ollama, vLLM, llama.cpp server, LM Studio) through
 * `/chat/completions` with `tools`. The first `tool_calls[]` entry is the pick.
 */
import type { ModelAdapter, PickRequest, PickResult } from "../types.js";
import { asArray, asRecord, postJson, tryParseJson, type HttpOptions } from "./http.js";

export interface LocalOptions extends HttpOptions {
  /** e.g. http://localhost:11434/v1 */
  baseUrl: string;
  model: string;
  apiKey?: string;
}

export function localBody(req: PickRequest): Record<string, unknown> {
  return {
    model: req.model,
    messages: [
      { role: "system", content: req.system },
      { role: "user", content: req.prompt },
    ],
    tools: req.tools.map((t) => ({ type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } })),
    tool_choice: "auto",
    temperature: 0,
  };
}

export function parseLocal(json: unknown): PickResult {
  const choices = asArray(asRecord(json)?.["choices"]);
  const message = asRecord(asRecord(choices[0])?.["message"]);
  const calls = asArray(message?.["tool_calls"]);
  const fn = asRecord(asRecord(calls[0])?.["function"]);
  if (fn && typeof fn["name"] === "string") return { tool: fn["name"], arguments: tryParseJson(fn["arguments"]) };
  const content = message?.["content"];
  return { tool: null, text: typeof content === "string" ? content : "" };
}

export function localAdapter(opts: LocalOptions): ModelAdapter {
  const base = opts.baseUrl.replace(/\/$/, "");
  const headers: Record<string, string> = opts.apiKey ? { authorization: `Bearer ${opts.apiKey}` } : {};
  return {
    id: `local:${opts.model}`,
    provider: "local",
    model: opts.model,
    async pick(req) {
      const json = await postJson(`${base}/chat/completions`, headers, localBody({ ...req, model: opts.model }), opts);
      return parseLocal(json);
    },
  };
}
