/**
 * Google Gemini API (`generateContent`) with function declarations, calling mode AUTO. The first
 * part carrying `functionCall` is the pick; text-only parts are "none".
 *
 * Gemini's schema dialect is an OpenAPI subset without `additionalProperties`, so the shared
 * parameter schema (src/harness.ts) stays within that subset for every provider.
 */
import type { ModelAdapter, PickRequest, PickResult } from "../types.js";
import { asArray, asRecord, postJson, type HttpOptions } from "./http.js";

export interface GeminiOptions extends HttpOptions {
  apiKey: string;
  model: string;
  baseUrl?: string;
}

export function geminiBody(req: PickRequest): Record<string, unknown> {
  return {
    systemInstruction: { parts: [{ text: req.system }] },
    contents: [{ role: "user", parts: [{ text: req.prompt }] }],
    tools: [{ functionDeclarations: req.tools.map((t) => ({ name: t.name, description: t.description, parameters: t.parameters })) }],
    toolConfig: { functionCallingConfig: { mode: "AUTO" } },
    generationConfig: { temperature: 0 },
  };
}

export function parseGemini(json: unknown): PickResult {
  const candidates = asArray(asRecord(json)?.["candidates"]);
  const first = asRecord(candidates[0]);
  const parts = asArray(asRecord(first?.["content"])?.["parts"]);
  let text = "";
  for (const part of parts) {
    const p = asRecord(part);
    if (!p) continue;
    const call = asRecord(p["functionCall"]);
    if (call && typeof call["name"] === "string") return { tool: call["name"], arguments: call["args"] };
    if (typeof p["text"] === "string") text += p["text"];
  }
  return { tool: null, text };
}

export function geminiAdapter(opts: GeminiOptions): ModelAdapter {
  const base = (opts.baseUrl ?? "https://generativelanguage.googleapis.com").replace(/\/$/, "");
  return {
    id: `gemini:${opts.model}`,
    provider: "gemini",
    model: opts.model,
    async pick(req) {
      const json = await postJson(
        `${base}/v1beta/models/${encodeURIComponent(opts.model)}:generateContent`,
        { "x-goog-api-key": opts.apiKey },
        geminiBody({ ...req, model: opts.model }),
        opts,
      );
      return parseGemini(json);
    },
  };
}
