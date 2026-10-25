/**
 * Input schemas of the three verbs, shared by the REST face (src/app.ts) and the MCP face
 * (src/mcp.ts) so both validate the same way (invariant 4) and the MCP `tools/list` input
 * schemas are generated from code (invariant 12).
 */
import { z } from "zod";
import { CATEGORIES } from "../connectors/categories.js";
import { MAX_LIMIT } from "./discover.js";

export const DiscoverInputSchema = z
  .object({
    task: z.string().min(1).max(500).describe("What the agent is trying to do, in plain words, e.g. \"what did the user commit to this week\"."),
    budget_micro_usd: z.number().int().min(0).optional().describe("Exclude tools whose typical run costs more than this (micro-USD)."),
    limit: z.number().int().min(1).max(MAX_LIMIT).optional().describe("Maximum candidates (default 10)."),
    categories: z.array(z.enum(CATEGORIES)).optional().describe("Restrict to these categories."),
  })
  .strict();

export const InspectInputSchema = z
  .object({
    tool_id: z.string().min(1).describe("A tool_id returned by discover."),
  })
  .strict();

export const RunInputSchema = z
  .object({
    tool_id: z.string().min(1).describe("A tool_id returned by discover."),
    input: z.record(z.string(), z.unknown()).describe("Arguments matching the tool's input_schema from inspect."),
    idempotency_key: z.string().min(1).max(200).describe("A key you generate per attempt; repeating it returns the stored result without re-running."),
    dryRun: z.boolean().optional().describe("Return the quote and permission level only; nothing runs and nothing is charged."),
    task_hint: z.string().max(500).optional().describe("Optional: the task phrase you discovered with. Used for better-fit hints and the run log, never stored with the input."),
    hint_followed: z.boolean().optional().describe("Optional: true if you chose this tool because of a hint."),
    inputResponses: z.record(z.string(), z.unknown()).optional().describe("Answers to an earlier input_required result (L2 confirmation)."),
  })
  .strict();

export type DiscoverVerbInput = z.infer<typeof DiscoverInputSchema>;
export type InspectVerbInput = z.infer<typeof InspectInputSchema>;
export type RunVerbInput = z.infer<typeof RunInputSchema>;

export const VERB_SCHEMAS = {
  "router.discover": DiscoverInputSchema,
  "router.inspect": InspectInputSchema,
  "router.run": RunInputSchema,
} as const;

export type VerbName = keyof typeof VERB_SCHEMAS;

export function parseVerbInput<N extends VerbName>(name: N, raw: unknown): { ok: true; value: z.infer<(typeof VERB_SCHEMAS)[N]> } | { ok: false; errors: string[] } {
  const parsed = VERB_SCHEMAS[name].safeParse(raw);
  if (parsed.success) return { ok: true, value: parsed.data as z.infer<(typeof VERB_SCHEMAS)[N]> };
  return { ok: false, errors: parsed.error.issues.map((i) => `${i.path.length ? i.path.join(".") : "$"}: ${i.message}`) };
}
