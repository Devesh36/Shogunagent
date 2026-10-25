import { z } from "../../../../../engine/mod.ts";

// Identical to the router's first-party definition (apps/router/connectors/shogunagent/endpoints/memory.recall.ts).
export const inputs = z
  .object({
    query: z.string().min(1).max(500),
    since: z.string().optional().describe("ISO timestamp."),
    until: z.string().optional().describe("ISO timestamp."),
    max_chars: z.number().int().min(200).max(20_000).optional().describe("Default 4000."),
  })
  .strict();
