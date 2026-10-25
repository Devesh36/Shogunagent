import { defineEndpoint, z, zodInput } from "../../define.js";

export const stateOpenLoopsList = defineEndpoint({
  id: "state.open_loops.list",
  meta: {
    summary: "What is pending with a person or project: unanswered questions, blockers, things others owe the user.",
    description:
      "Lists the user's open loops — threads waiting on someone else, unanswered questions, blockers and follow-ups, each with the person or project it belongs to, when it was last seen and the quoted evidence. " +
      "Use for: what is pending with this person, what is the user waiting on, what is blocked, what should the user follow up on, what did someone promise the user. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for the user's own promises (use state.commitments.list), calendar events, or full message history.",
    categories: ["state", "memory"],
    notes: [
      "Items carry confidence and provenance; the state layer lists only rows at or above 0.84 as facts.",
      "Pass `person` or `project` to narrow; omit both for the whole open list, newest first.",
    ],
    examples: [
      {
        input: { person: "Mika" },
        output: { items: [{ id: "l_7", text: "Mika to confirm the venue", kind: "followup", person: "Mika", last_seen: "2026-10-24T09:12:00Z", confidence: 0.88, provenance: { event_id: "e_9003", quote: "I'll confirm the venue tomorrow" } }], next_cursor: null },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/open_loops" },
  input: {
    schema: zodInput(
      z
        .object({
          person: z.string().min(1).max(200).optional(),
          project: z.string().min(1).max(200).optional(),
          kind: z.enum(["question", "blocker", "followup", "decision", "all"]).optional().describe("Default: all."),
          limit: z.number().int().min(1).max(100).optional(),
          cursor: z.string().optional(),
        })
        .strict(),
    ),
  },
  output: {},
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "pending-with-person",
      input: { person: "Mika" },
      response: { status: 200, body: { items: [{ id: "l_7", text: "Mika to confirm the venue", kind: "followup", person: "Mika", last_seen: "2026-10-24T09:12:00Z", confidence: 0.88, provenance: { event_id: "e_9003", quote: "I'll confirm the venue tomorrow" } }], next_cursor: null } },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
  ],
});
