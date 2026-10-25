import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { GITHUB_HEADERS, mapIssue, repoPart } from "../shared.js";

interface CreateInput {
  owner: string;
  repo: string;
  title: string;
  body?: string;
  labels?: string[];
  assignees?: string[];
}

/** An issue is visible to everyone with access to the repository and notifies watchers: `external_send`, L3. */
export const githubIssuesCreate = defineEndpoint({
  id: "issues.create",
  meta: {
    summary: "Open a new issue in a GitHub repository, after a human approves it.",
    description:
      "Creates an issue in one GitHub repository with title, Markdown body, labels and assignees; watchers and assignees are notified. " +
      "Use for: file a bug for this, open an issue to track the follow-up, report this problem on the repo, create a GitHub ticket and assign it to X. " +
      "Always L3: the call returns an approval URL and nothing is created until a human approves. $0.002 per created issue.",
    exclusions: "Not for listing or searching issues (use github.issues.list), commenting, pull requests, or a private to-do for the user.",
    categories: ["issue_tracking", "approval"],
    notes: ["Labels and assignees that do not exist in the repository make GitHub reject the call (422)."],
    examples: [
      {
        input: { owner: "octo-org", repo: "api", title: "Timeout on /v1/run", body: "Seen at 11:00 JST.", labels: ["bug"] },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
    docsUrl: "https://docs.github.com/en/rest/issues/issues#create-an-issue",
  },
  request: { method: "POST", path: "/repos/{owner}/{repo}/issues", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          owner: repoPart,
          repo: repoPart,
          title: z.string().min(1).max(256),
          body: z.string().max(65_536).optional().describe("Markdown."),
          labels: z.array(z.string().min(1).max(100)).max(20).optional(),
          assignees: z.array(z.string().min(1).max(100)).max(10).optional(),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const i = input as unknown as CreateInput;
      const body: Record<string, Json> = { title: i.title };
      if (i.body) body["body"] = i.body;
      if (i.labels?.length) body["labels"] = i.labels;
      if (i.assignees?.length) body["assignees"] = i.assignees;
      return { method: "POST", path: `/repos/${i.owner}/${i.repo}/issues`, headers: { ...GITHUB_HEADERS }, body };
    },
  },
  output: {
    fromResponse: ({ response }) => mapIssue((response.body ?? {}) as Record<string, Json>),
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "synthetic-created",
      input: { owner: "octo-org", repo: "api", title: "Timeout on /v1/run", body: "Seen at 11:00 JST.", labels: ["bug"] },
      response: {
        status: 201,
        body: { number: 44, title: "Timeout on /v1/run", state: "open", labels: [{ name: "bug" }], assignees: [], user: { login: "shogun-user" }, created_at: "2026-10-25T02:00:00Z", updated_at: "2026-10-25T02:00:00Z", html_url: "https://github.com/octo-org/api/issues/44" },
      },
      expect: {
        output: { number: 44, title: "Timeout on /v1/run", state: "open", labels: ["bug"], assignees: [], author: "shogun-user", created_at: "2026-10-25T02:00:00Z", updated_at: "2026-10-25T02:00:00Z", url: "https://github.com/octo-org/api/issues/44" },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-unknown-assignee",
      input: { owner: "octo-org", repo: "api", title: "x", assignees: ["nobody-here"] },
      response: { status: 422, body: { message: "Validation Failed" } },
      expect: { error: true },
    },
  ],
});
