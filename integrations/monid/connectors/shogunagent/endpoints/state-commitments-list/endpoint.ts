import { defineEndpoint, utils } from "../../../../engine/mod.ts";

import { inputs } from "./schema/inputs.ts";

/** `shogunagent/state-commitments-list` — wire name `state.commitments.list` (frozen, CLAUDE.md invariant 11). */
export const stateCommitmentsList = defineEndpoint({
  id: "state-commitments-list",
  meta: {
    summary: "Find what the user committed to: promises, deliverables and deadlines they made to others.",
    description:
      "Lists the user's commitments — things the user said they would do, with the person they owe it to, the due date when known, the status (open, done, overdue) and the quoted evidence behind each. " +
      "Use for: what did the user commit to, what does the user owe someone, what is due this week, which promises are overdue, what has the user promised to deliver. " +
      "Free read; scoped to the caller's own ShogunAgent key.",
    notes: [
      "Not for tasks other people owe the user (use shogunagent/state-open-loops-list), calendar events, or raw notes.",
      "Each item carries confidence and provenance (the event and quote it was extracted from); treat confidence below 0.8 as a lead, not a fact.",
      "Pass `person` to narrow to commitments made to one person; pass `due_before` (ISO date) for a deadline window.",
      "Requires the caller's ShogunAgent agent key; answers 401 without one.",
    ],
    docsUrl: "https://shogunai.si/docs/router/shogunagent",
  },
  request: { method: "GET", path: "/v1/state/commitments", timeoutMs: 10_000 },
  input: {
    schema: inputs,
    // GET: the validated input becomes the query string.
    toRequest: ({ input }) => ({ method: "GET", path: "/v1/state/commitments", query: utils.query.fromObject(input) }),
  },
  usage: {
    // Per call, $0: reads of the user's own state are free on every ShogunAgent surface
    // (invariant 10). An empty list is a result, not a failure, and is equally free.
    consolidate: ({ output }) => ({ usage: { units: 1, cost: utils.money.fromDollars(0) }, output }),
  },
  output: {
    fromResponse: ({ output }) => output,
  },
});
