import { z } from "../../../../../engine/mod.ts";

// Identical to the router's first-party definition (apps/router/connectors/shogunagent/endpoints/state.commitments.list.ts).
// Only constraints z.toJSONSchema can express are used, as Monid requires.
export const inputs = z
  .object({
    status: z.enum(["open", "done", "overdue", "all"]).optional().describe("Default: open."),
    person: z.string().min(1).max(200).optional().describe("Name or id of the person the commitment was made to."),
    due_before: z.string().date().optional().describe("ISO date; only commitments due on or before it."),
    limit: z.number().int().min(1).max(100).optional().describe("Default 20."),
    cursor: z.string().optional(),
  })
  .strict();
