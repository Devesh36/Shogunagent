/**
 * Mock mode: a deterministic lexical picker so the harness, its scoring and the CI gate run with
 * no keys and no network. It is not a model; its numbers only mean "how the listing text ranks
 * lexically against the task text", the same signal a registry or router search uses.
 */
import { Bm25Index, tokenize } from "./bm25.js";
import type { ModelAdapter, PickRequest, PickResult } from "./types.js";

export const MOCK_MODEL_ID = "mock-bm25";

/** Below this normalised fit the mock calls no tool. Tuned once against tasks.en.json; changing it moves the baseline. */
export const MOCK_PICK_THRESHOLD = 0.03;

export function mockPick(req: PickRequest, threshold: number = MOCK_PICK_THRESHOLD): PickResult {
  const index = new Bm25Index(req.tools.map((t) => ({ id: t.name, text: `${t.name.replace(/[_.]/g, " ")} ${t.description}` })));
  let best: { name: string; fit: number } | null = null;
  // Ties resolve to the earlier tool; the listing order is part of the surface file and so of the baseline.
  for (const t of req.tools) {
    const fit = index.fit(t.name, req.prompt);
    if (best === null || fit > best.fit + 1e-12) best = { name: t.name, fit };
  }
  if (!best || best.fit < threshold) return { tool: null, text: "(mock) no tool fits" };
  return { tool: best.name, arguments: { query: req.prompt } };
}

export function mockAdapter(threshold: number = MOCK_PICK_THRESHOLD): ModelAdapter {
  return {
    id: MOCK_MODEL_ID,
    provider: "mock",
    model: MOCK_MODEL_ID,
    pick: async (req) => mockPick(req, threshold),
  };
}

/**
 * Adoption mock (E11-T3): does the scenario overlap the skill's trigger text more than its
 * non-trigger text? `triggerText` is the "When to use" section, `nonTriggerText` the "Do not use"
 * section, both read from SKILL.md at run time so a SKILL.md edit moves this number.
 */
export interface AdoptionMockOptions {
  /** Minimum net overlap (trigger-only tokens minus non-trigger-only tokens) to propose. */
  minNet?: number;
}

export function mockAdoptionDecision(
  scenario: string,
  triggerText: string,
  nonTriggerText: string,
  opts: AdoptionMockOptions = {},
): { proposed: boolean; evidence: string[] } {
  const minNet = opts.minNet ?? 1;
  const trigger = new Set(tokenize(triggerText));
  const nonTrigger = new Set(tokenize(nonTriggerText));
  const hits: string[] = [];
  const misses: string[] = [];
  for (const tok of new Set(tokenize(scenario))) {
    const inT = trigger.has(tok);
    const inN = nonTrigger.has(tok);
    if (inT && !inN) hits.push(tok);
    else if (inN && !inT) misses.push(tok);
  }
  const net = hits.length - misses.length;
  return {
    proposed: net >= minNet,
    evidence: [`trigger_tokens=${hits.join(",")}`, `non_trigger_tokens=${misses.join(",")}`, `net=${net}`],
  };
}
