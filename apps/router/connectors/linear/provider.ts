/**
 * First-party Linear connector (E22): issue creation, an external send (L3). Ranked by the same
 * score as any other provider and labeled `first_party` (invariant 13).
 *
 * Auth: the calling principal's own Linear OAuth access token (slot `linear-tokenset`, Bearer),
 * fetched from apps/api at run time and injected by the transport only (invariant 7; E48,
 * docs/api/connector-tokens.md). `LINEAR_OAUTH_TOKEN` is read in development only, behind
 * `ROUTER_DEV_SHARED_CREDENTIALS=1`.
 */
import { defineProvider } from "../define.js";
import { linearIssuesCreate } from "./endpoints/issues.create.js";
import manifest from "./provider.json" with { type: "json" };

export const linearProvider = defineProvider({
  id: "linear",
  name: "Linear (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "Create (L3) issues in Linear teams through Linear's GraphQL API.",
    docsUrl: "https://linear.app/developers",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://api.linear.app" },
  auth: { kind: "caller_connector", slot: "linear-tokenset", credential_env: "LINEAR_OAUTH_TOKEN" },
  manifest,
  categories: ["issue_tracking"],
  health: { p50_ms: 300, p95_ms: 1100, success_rate: 0.995 },
  endpoints: [linearIssuesCreate],
});
