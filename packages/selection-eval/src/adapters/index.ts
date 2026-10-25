/**
 * Which models run. Keys come from the environment only; a key that is absent simply leaves that
 * provider out. With no key at all the run is `mode: "mock"`. Default model ids are config
 * (SELECTION_EVAL_*_MODEL) because they go stale; the README says so.
 */
import type { ModelAdapter } from "../types.js";
import { mockAdapter } from "../mock.js";
import { anthropicAdapter } from "./anthropic.js";
import { geminiAdapter } from "./gemini.js";
import { localAdapter } from "./local.js";
import { openaiAdapter } from "./openai.js";

export const DEFAULT_MODELS = {
  anthropic: "claude-sonnet-5-5",
  openai: "gpt-5-mini",
  gemini: "gemini-2.5-flash",
  local: "llama3.1",
} as const;

export interface ResolveOptions {
  /** Force mock even when keys are present. */
  forceMock?: boolean;
  /** Restrict to these providers (ids from DEFAULT_MODELS keys). */
  only?: string[];
}

export function resolveAdapters(env: NodeJS.ProcessEnv, opts: ResolveOptions = {}): { mode: "mock" | "live"; adapters: ModelAdapter[] } {
  if (opts.forceMock || env["SELECTION_EVAL_MODE"] === "mock") return { mode: "mock", adapters: [mockAdapter()] };

  const want = (p: string) => !opts.only || opts.only.includes(p);
  const adapters: ModelAdapter[] = [];

  const anthropicKey = env["ANTHROPIC_API_KEY"];
  if (anthropicKey && want("anthropic")) {
    adapters.push(anthropicAdapter({ apiKey: anthropicKey, model: env["SELECTION_EVAL_ANTHROPIC_MODEL"] ?? DEFAULT_MODELS.anthropic }));
  }
  const openaiKey = env["OPENAI_API_KEY"];
  if (openaiKey && want("openai")) {
    const effort = env["SELECTION_EVAL_OPENAI_REASONING"];
    adapters.push(
      openaiAdapter({
        apiKey: openaiKey,
        model: env["SELECTION_EVAL_OPENAI_MODEL"] ?? DEFAULT_MODELS.openai,
        ...(effort === "minimal" || effort === "low" || effort === "medium" || effort === "high" ? { reasoningEffort: effort } : {}),
      }),
    );
  }
  const geminiKey = env["GEMINI_API_KEY"];
  if (geminiKey && want("gemini")) {
    adapters.push(geminiAdapter({ apiKey: geminiKey, model: env["SELECTION_EVAL_GEMINI_MODEL"] ?? DEFAULT_MODELS.gemini }));
  }
  const localBase = env["LOCAL_OPENAI_BASE_URL"];
  if (localBase && want("local")) {
    const apiKey = env["LOCAL_OPENAI_API_KEY"];
    adapters.push(localAdapter({ baseUrl: localBase, model: env["SELECTION_EVAL_LOCAL_MODEL"] ?? DEFAULT_MODELS.local, ...(apiKey ? { apiKey } : {}) }));
  }

  if (adapters.length === 0) return { mode: "mock", adapters: [mockAdapter()] };
  return { mode: "live", adapters };
}
