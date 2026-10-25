import { defineEndpoint, utils } from "../../../../engine/mod.ts";

import { inputs } from "./schema/inputs.ts";

/** `shogunagent/state-open-loops-list` — wire name `state.open_loops.list` (frozen, CLAUDE.md invariant 11). */
export const stateOpenLoopsList = defineEndpoint({
  id: "state-open-loops-list",
  meta: {
    summary: "What is pending with a person or project: unanswered questions, blockers, things others owe the user.",
    description:
      "Lists the user's open loops — threads waiting on someone else, unanswered questions, blockers and follow-ups, each with the person or project it belongs to, when last seen and the quoted evidence. " +
      "Use for: what is pending with this person, what is the user waiting on, what is blocked, what should the user follow up on, what did someone promise the user. " +
      "Free read; scoped to the caller's own ShogunAgent key.",
    notes: [
      "Not for the user's own promises (use shogunagent/state-commitments-list), calendar events, or full message history.",
      "Items carry confidence and provenance; the state layer lists only rows at or above 0.84 as facts.",
      "Pass `person` or `project` to narrow; omit both for the whole open list, newest first.",
      "Requires the caller's ShogunAgent agent key; answers 401 without one.",
    ],
    docsUrl: "https://shogunai.si/docs/router/shogunagent",
  },
  request: { method: "GET", path: "/v1/state/open_loops", timeoutMs: 10_000 },
  input: {
    schema: inputs,
    toRequest: ({ input }) => ({ method: "GET", path: "/v1/state/open_loops", query: utils.query.fromObject(input) }),
  },
  usage: {
    consolidate: ({ output }) => ({ usage: { units: 1, cost: utils.money.fromDollars(0) }, output }),
  },
  output: {
    fromResponse: ({ output }) => output,
  },
});
