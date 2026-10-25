import { defineEndpoint, z, zodInput } from "../../define.js";

export const memoryRecall = defineEndpoint({
  id: "memory.recall",
  meta: {
    summary: "Recall what the user saw, said or did about a topic: a ranked, cited context pack from their own memory.",
    description:
      "Searches the user's event log and derived memory for a topic and returns a compact context pack: the most relevant excerpts with timestamps, sources and the people and projects involved. " +
      "Use for: what do I know about X, what happened with X last week, remind me what was decided about X, give me context before replying. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for public web facts (use a search tool), storing new memory (use memory.ingest), or verbatim chat transcripts.",
    categories: ["memory", "state"],
    notes: ["Excerpts are capped by `max_chars` (default 4000); raise it for long context, lower it for a quick check.", "`since` / `until` are ISO timestamps."],
    examples: [
      {
        input: { query: "venue for the offsite", max_chars: 1200 },
        output: { excerpts: [{ ts: "2026-10-24T09:12:00Z", source: "slack", text: "Mika: I'll confirm the venue tomorrow", people: ["Mika"], projects: ["Offsite"] }], people: ["Mika"], projects: ["Offsite"], chars: 60 },
      },
    ],
  },
  request: { method: "POST", path: "/v1/memory/recall" },
  input: {
    schema: zodInput(
      z
        .object({
          query: z.string().min(1).max(500),
          since: z.string().optional(),
          until: z.string().optional(),
          max_chars: z.number().int().min(200).max(20_000).optional(),
        })
        .strict(),
    ),
  },
  output: {
    isEmpty: (output) => {
      const ex = output && typeof output === "object" && !Array.isArray(output) ? output["excerpts"] : null;
      return !Array.isArray(ex) || ex.length === 0;
    },
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "topic-found",
      input: { query: "venue for the offsite", max_chars: 1200 },
      response: { status: 200, body: { excerpts: [{ ts: "2026-10-24T09:12:00Z", source: "slack", text: "Mika: I'll confirm the venue tomorrow", people: ["Mika"], projects: ["Offsite"] }], people: ["Mika"], projects: ["Offsite"], chars: 60 } },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "nothing-recalled",
      input: { query: "zebra accounting" },
      response: { status: 200, body: { excerpts: [], people: [], projects: [], chars: 0 } },
      expect: { usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-state-unavailable",
      input: { query: "venue for the offsite", max_chars: 1200 },
      response: { status: 503, body: { error: "unavailable" } },
      expect: { error: true },
    },
  ],
});
