import { defineEndpoint, z, zodInput } from "../../define.js";

export const stateCommitmentsList = defineEndpoint({
  id: "state.commitments.list",
  meta: {
    summary: "Find what the user committed to: promises, deliverables and deadlines they made to others.",
    description:
      "Lists the user's commitments — things the user said they would do, with the person they owe it to, the due date when known, the status (open, done, overdue) and the quoted evidence each one rests on. " +
      "Use for: what did the user commit to, what does the user owe someone, what is due this week, which promises are overdue, what has the user promised to deliver. " +
      "Free read (L1); results are the user's own state, scoped to your agent key.",
    exclusions: "Not for tasks other people owe the user (use state.open_loops.list), calendar events, or raw notes.",
    categories: ["state", "memory"],
    notes: [
      "Each item carries confidence and provenance (the event and quote it was extracted from); treat confidence below 0.8 as a lead, not a fact.",
      "Pass `person` to narrow to commitments made to one person; pass `due_before` (ISO date) for a deadline window.",
    ],
    examples: [
      {
        input: { status: "open", limit: 5 },
        output: {
          items: [
            { id: "c_1", text: "send the revised deck to Mika", to_person: "Mika", due: "2026-10-27", status: "open", confidence: 0.91, provenance: { event_id: "e_8812", quote: "I'll send the revised deck by Monday" } },
          ],
          next_cursor: null,
        },
      },
    ],
  },
  request: { method: "GET", path: "/v1/state/commitments" },
  input: {
    schema: zodInput(
      z
        .object({
          status: z.enum(["open", "done", "overdue", "all"]).optional().describe("Default: open."),
          person: z.string().min(1).max(200).optional().describe("Name or id of the person the commitment was made to."),
          due_before: z.string().date().optional().describe("ISO date; only commitments due on or before it."),
          limit: z.number().int().min(1).max(100).optional().describe("Default 20."),
          cursor: z.string().optional(),
        })
        .strict(),
    ),
  },
  output: {
    fromResponse: ({ response }) => response.body,
  },
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "open-commitments",
      input: { status: "open", limit: 2 },
      response: {
        status: 200,
        body: {
          items: [
            { id: "c_1", text: "send the revised deck to Mika", to_person: "Mika", due: "2026-10-27", status: "open", confidence: 0.91, provenance: { event_id: "e_8812", quote: "I'll send the revised deck by Monday" } },
            { id: "c_2", text: "review the contract draft", to_person: "Jun", due: null, status: "open", confidence: 0.86, provenance: { event_id: "e_8901", quote: "I will review the contract draft tonight" } },
          ],
          next_cursor: null,
        },
      },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "nothing-open",
      input: { status: "open" },
      response: { status: 200, body: { items: [], next_cursor: null } },
      expect: { usage: { quantity: 0, cost_micro_usd: 0 } },
    },
  ],
});
