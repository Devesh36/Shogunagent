import { defineEndpoint, utils } from "../../../../engine/mod.ts";

import { inputs } from "./schema/inputs.ts";

/** `shogunagent/memory-recall` — wire name `memory.recall` (frozen, CLAUDE.md invariant 11). */
export const memoryRecall = defineEndpoint({
  id: "memory-recall",
  meta: {
    summary: "Recall what the user saw, said or did about a topic: a ranked, cited context pack from their own memory.",
    description:
      "Searches the user's event log and derived memory for a topic and returns a compact context pack: the most relevant excerpts with timestamps, sources and the people and projects involved. " +
      "Use for: what do I know about X, what happened with X last week, remind me what was decided about X, give me context before replying. " +
      "Free read; scoped to the caller's own ShogunAgent key.",
    notes: [
      "Not for public web facts (use a search endpoint), storing new memory, or verbatim chat transcripts.",
      "Excerpts are capped by `max_chars` (default 4000); raise it for long context, lower it for a quick check.",
      "`since` / `until` are ISO timestamps.",
      "Requires the caller's ShogunAgent agent key; answers 401 without one.",
    ],
    docsUrl: "https://shogunai.si/docs/router/shogunagent",
  },
  request: { method: "POST", path: "/v1/memory/recall", timeoutMs: 10_000 },
  input: {
    schema: inputs,
    toRequest: ({ input }) => ({ method: "POST", path: "/v1/memory/recall", body: input }),
  },
  usage: {
    consolidate: ({ output }) => ({ usage: { units: 1, cost: utils.money.fromDollars(0) }, output }),
  },
  output: {
    fromResponse: ({ output }) => output,
  },
});
