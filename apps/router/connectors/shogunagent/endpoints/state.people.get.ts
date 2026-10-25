import { defineEndpoint, z, zodInput } from "../../define.js";

export const statePeopleGet = defineEndpoint({
  id: "state.people.get",
  meta: {
    summary: "Who is this person to the user: role, organisation, relationship, recent context and what is pending with them.",
    description:
      "Returns one person from the user's world model by id or name: role, organisation, how the user knows them, last contact, the open loops and commitments between them, and the evidence. " +
      "Use for: who is X, what do I know about X, prepare me for a meeting with X, what is my history with X. " +
      "Free read (L1); scoped to your agent key.",
    exclusions: "Not for searching the public web about a person (use an enrichment tool), listing everyone (use state.people.list), or contact details the user never recorded.",
    categories: ["state", "memory"],
    notes: ["A name that matches several people returns 300 with the candidates; pass the id to disambiguate."],
    examples: [
      { input: { person: "Mika" }, output: { id: "p_12", name: "Mika Tanaka", role: "Head of Design", org: "Acme", relationship: "collaborator", last_contact: "2026-10-24", open_loops: 1, commitments_to: 1, confidence: 0.95 } },
    ],
  },
  request: { method: "GET", path: "/v1/state/people/:person" },
  input: {
    schema: zodInput(
      z
        .object({
          person: z.string().min(1).max(200).describe("Person id (p_…) or a name."),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const person = (input as { person: string }).person;
      return { method: "GET", path: `/v1/state/people/${encodeURIComponent(person)}` };
    },
  },
  output: {},
  usage: { model: "per_call", cost_micro_usd: 0 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "by-name",
      input: { person: "Mika" },
      response: { status: 200, body: { id: "p_12", name: "Mika Tanaka", role: "Head of Design", org: "Acme", relationship: "collaborator", last_contact: "2026-10-24", open_loops: 1, commitments_to: 1, confidence: 0.95 } },
      expect: { usage: { quantity: 1, cost_micro_usd: 0 } },
    },
    {
      scenario: "unknown-person",
      input: { person: "p_does_not_exist" },
      response: { status: 404, body: { error: { code: "not_found" } } },
      expect: { error: true },
    },
  ],
});
