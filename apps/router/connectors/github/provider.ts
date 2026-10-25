/**
 * First-party GitHub connector (E22). Listing issues is L1; opening one notifies others and is L3.
 * Ranked by the same score as any other provider and labeled `first_party` (invariant 13).
 *
 * Auth: a GitHub token from `GITHUB_TOKEN`, injected by the transport only (invariant 7).
 */
import { defineProvider } from "../define.js";
import { githubIssuesCreate } from "./endpoints/issues.create.js";
import { githubIssuesList } from "./endpoints/issues.list.js";
import manifest from "./provider.json" with { type: "json" };

export const githubProvider = defineProvider({
  id: "github",
  name: "GitHub (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "List (L1) and open (L3) issues in GitHub repositories through the GitHub REST API.",
    docsUrl: "https://docs.github.com/en/rest/issues",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://api.github.com" },
  auth: { kind: "bearer", credential_env: "GITHUB_TOKEN" },
  manifest,
  categories: ["issue_tracking"],
  health: { p50_ms: 300, p95_ms: 1000, success_rate: 0.996 },
  endpoints: [githubIssuesList, githubIssuesCreate],
});
