/**
 * ShogunAgent as a Monid provider (E31). DRAFT for a pull request to github.com/monid-ai/monid —
 * not submitted (see ../../README.md and ../../CHECKLIST.md).
 *
 * Shape: Monid's connector format as described in its DEVELOPMENT.md (provider.ts + one
 * endpoints/<name>/endpoint.ts per endpoint, zod input schemas in schema/inputs.ts, recorded
 * fixtures, offline endpoint tests). The import path and helper names below follow that
 * description; before opening the PR they must be aligned with Monid's reference connector
 * (`connectors/exa/`) — CHECKLIST item 1. Nothing here is compiled in the ShogunAgent repository.
 *
 * Auth is the one deliberate departure from a typical Monid provider: ShogunAgent holds each
 * user's own world model, so every call must carry the CALLER's ShogunAgent agent key (scoped to
 * that user's principal), never a vendor-level key held by Monid. The key is read from the run's
 * auth params and injected as a bearer header; it is never part of `input` (Monid stores run
 * inputs and outputs). Whether Monid can pass a per-caller credential this way is open question 1
 * in the README.
 */
import { defineProvider } from "../../engine/mod.ts";

import { memoryRecall } from "./endpoints/memory-recall/endpoint.ts";
import { stateCommitmentsList } from "./endpoints/state-commitments-list/endpoint.ts";
import { stateOpenLoopsList } from "./endpoints/state-open-loops-list/endpoint.ts";

export default defineProvider({
  id: "shogunagent",
  name: "ShogunAgent",
  meta: {
    description:
      "The state layer for AI agents: a typed, provenance-backed model of one person's work — commitments, open loops, people, projects — with recall over their own event log. Every answer is scoped to the caller's own ShogunAgent key; reads are free.",
    docsUrl: "https://shogunai.si/docs/router/shogunagent",
    homepage: "https://shogunai.si",
  },
  // `memory` is not in Monid's closed category list today (open question 2). The PR proposes
  // adding it to connectors/categories.ts; until then `data` is the closest existing value.
  categories: ["memory"],
  request: {
    baseUrl: "https://api.shogunaios.com",
    timeoutMs: 10_000,
  },
  auth: {
    // A caller-provided credential, not Monid's own: the user's ShogunAgent agent key
    // (`sa_live_…`, registered at POST /v1/agents or issued by the user's operator).
    credentials: { shogunagentKey: { kind: "caller_provided", description: "The calling user's ShogunAgent agent key (sa_live_…). Scopes every read to that user's principal." } },
    inject: ({ request, params }) => ({
      ...request,
      headers: { ...(request.headers ?? {}), authorization: `Bearer ${params.shogunagentKey}` },
    }),
  },
});
