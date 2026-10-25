import { z } from "../../../../../engine/mod.ts";

// Identical to the router's first-party definition (apps/router/connectors/shogunagent/endpoints/state.open_loops.list.ts).
export const inputs = z
  .object({
    person: z.string().min(1).max(200).optional(),
    project: z.string().min(1).max(200).optional(),
    kind: z.enum(["question", "blocker", "followup", "decision", "all"]).optional().describe("Default: all."),
    limit: z.number().int().min(1).max(100).optional(),
    cursor: z.string().optional(),
  })
  .strict();
