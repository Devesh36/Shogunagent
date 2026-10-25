import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { GITHUB_HEADERS, mapIssue, repoPart } from "../shared.js";

interface ListInput {
  owner: string;
  repo: string;
  state?: "open" | "closed" | "all";
  labels?: string[];
  assignee?: string;
  limit?: number;
}

export const githubIssuesList = defineEndpoint({
  id: "issues.list",
  meta: {
    summary: "List issues in a GitHub repository, filtered by state, labels or assignee.",
    description:
      "Reads issues from one GitHub repository with number, title, state, labels, assignees, author, timestamps and URL; pull requests are left out. " +
      "Use for: what issues are open in this repo, which bugs are assigned to X, what is labeled release-blocker, list the backlog of a repository. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for pull requests or code search, creating issues (use github.issues.create, L3), or the user's own commitments (use state.commitments.list).",
    categories: ["issue_tracking"],
    notes: ["state defaults to open; limit defaults to 30 (max 100).", "Labels are matched all-of, as GitHub does."],
    examples: [
      {
        input: { owner: "octo-org", repo: "api", labels: ["bug"], limit: 5 },
        output: { items: [{ number: 42, title: "Timeout on /v1/run", state: "open", labels: ["bug"], assignees: ["mika"], author: "jun", created_at: "2026-10-20T09:00:00Z", updated_at: "2026-10-24T11:00:00Z", url: "https://github.com/octo-org/api/issues/42" }] },
      },
    ],
    docsUrl: "https://docs.github.com/en/rest/issues/issues#list-repository-issues",
  },
  request: { method: "GET", path: "/repos/{owner}/{repo}/issues", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          owner: repoPart.describe("Repository owner (user or organisation)."),
          repo: repoPart.describe("Repository name."),
          state: z.enum(["open", "closed", "all"]).optional(),
          labels: z.array(z.string().min(1).max(100)).max(20).optional(),
          assignee: z.string().min(1).max(100).optional().describe("A login, \"none\" or \"*\"."),
          limit: z.number().int().min(1).max(100).optional(),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const i = input as unknown as ListInput;
      const query: Record<string, string> = { state: i.state ?? "open", per_page: String(i.limit ?? 30) };
      if (i.labels?.length) query["labels"] = i.labels.join(",");
      if (i.assignee) query["assignee"] = i.assignee;
      return { method: "GET", path: `/repos/${i.owner}/${i.repo}/issues`, query, headers: { ...GITHUB_HEADERS } };
    },
  },
  output: {
    fromResponse: ({ response }) => {
      const rows = Array.isArray(response.body) ? (response.body as Record<string, Json>[]) : [];
      return { items: rows.filter((r) => r["pull_request"] === undefined).map(mapIssue) };
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "synthetic-open-bugs-pr-filtered",
      input: { owner: "octo-org", repo: "api", labels: ["bug"], limit: 5 },
      response: {
        status: 200,
        body: [
          { number: 42, title: "Timeout on /v1/run", state: "open", labels: [{ name: "bug" }], assignees: [{ login: "mika" }], user: { login: "jun" }, created_at: "2026-10-20T09:00:00Z", updated_at: "2026-10-24T11:00:00Z", html_url: "https://github.com/octo-org/api/issues/42" },
          { number: 43, title: "Fix timeout", state: "open", labels: [{ name: "bug" }], assignees: [], user: { login: "mika" }, created_at: "2026-10-24T12:00:00Z", updated_at: "2026-10-24T12:00:00Z", html_url: "https://github.com/octo-org/api/pull/43", pull_request: { url: "https://api.github.com/repos/octo-org/api/pulls/43" } },
        ],
      },
      expect: {
        output: { items: [{ number: 42, title: "Timeout on /v1/run", state: "open", labels: ["bug"], assignees: ["mika"], author: "jun", created_at: "2026-10-20T09:00:00Z", updated_at: "2026-10-24T11:00:00Z", url: "https://github.com/octo-org/api/issues/42" }] },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-no-issues",
      input: { owner: "octo-org", repo: "quiet" },
      response: { status: 200, body: [] },
      expect: { output: { items: [] }, usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-repo-not-found",
      input: { owner: "octo-org", repo: "missing" },
      response: { status: 404, body: { message: "Not Found" } },
      expect: { error: true },
    },
  ],
});
