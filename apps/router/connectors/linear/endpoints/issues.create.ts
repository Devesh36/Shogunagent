import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json, RunResponse } from "../../types.js";

interface CreateInput {
  team_id: string;
  title: string;
  description?: string;
  priority?: number;
  assignee_id?: string;
  label_ids?: string[];
  project_id?: string;
}

export const ISSUE_CREATE_MUTATION =
  "mutation IssueCreate($input: IssueCreateInput!) { issueCreate(input: $input) { success issue { id identifier title url priority state { name } assignee { name } } } }";

type Row = Record<string, Json>;

function payload(body: Json): Row | undefined {
  const data = ((body ?? {}) as Row)["data"];
  return ((data ?? {}) as Row)["issueCreate"] as Row | undefined;
}

/** Linear answers GraphQL errors with HTTP 200: a failure is an `errors` array or `success: false`. */
export function linearFailed(response: RunResponse): boolean {
  if (response.status >= 400) return true;
  const b = (response.body ?? {}) as Row;
  if (Array.isArray(b["errors"]) && b["errors"].length > 0) return true;
  return payload(response.body)?.["success"] !== true;
}

/** A Linear issue is visible to the team and notifies its subscribers: `external_send`, L3. */
export const linearIssuesCreate = defineEndpoint({
  id: "issues.create",
  meta: {
    summary: "Create an issue in a Linear team, after a human approves it.",
    description:
      "Creates a Linear issue in one team with title, Markdown description, priority, assignee, labels and project; the team and assignee are notified. " +
      "Use for: create a Linear ticket for this, track this bug in Linear, add a task to the team backlog, assign the follow-up to X in Linear. " +
      "Always L3: the call returns an approval URL and nothing is created until a human approves. $0.002 per created issue.",
    exclusions: "Not for searching or updating existing issues, GitHub issues (use github.issues.create), or a private to-do for the user.",
    categories: ["issue_tracking", "approval"],
    notes: ["team_id, assignee_id, label_ids and project_id are Linear UUIDs.", "priority: 0 none, 1 urgent, 2 high, 3 medium, 4 low."],
    examples: [
      {
        input: { team_id: "9cfb482a-81e3-4154-b5b9-2c805e70a02d", title: "Timeout on /v1/run", priority: 2 },
        output: { resultType: "approval_required", approval: { url: "https://api.shogunaios.com/approve/…" } },
      },
    ],
    docsUrl: "https://linear.app/developers/graphql",
  },
  request: { method: "POST", path: "/graphql", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          team_id: z.string().uuid(),
          title: z.string().min(1).max(512),
          description: z.string().max(100_000).optional().describe("Markdown."),
          priority: z.number().int().min(0).max(4).optional(),
          assignee_id: z.string().uuid().optional(),
          label_ids: z.array(z.string().uuid()).max(20).optional(),
          project_id: z.string().uuid().optional(),
        })
        .strict(),
    ),
    toRequest: ({ input, request }) => {
      const i = input as unknown as CreateInput;
      const vars: Row = { teamId: i.team_id, title: i.title };
      if (i.description) vars["description"] = i.description;
      if (i.priority !== undefined) vars["priority"] = i.priority;
      if (i.assignee_id) vars["assigneeId"] = i.assignee_id;
      if (i.label_ids?.length) vars["labelIds"] = i.label_ids;
      if (i.project_id) vars["projectId"] = i.project_id;
      return { method: request.method, path: request.path, body: { query: ISSUE_CREATE_MUTATION, variables: { input: vars } } };
    },
  },
  output: {
    isFailure: linearFailed,
    fromResponse: ({ response }) => {
      const issue = (payload(response.body)?.["issue"] ?? {}) as Row;
      return {
        id: issue["id"] ?? null,
        identifier: issue["identifier"] ?? null,
        title: issue["title"] ?? null,
        url: issue["url"] ?? null,
        priority: issue["priority"] ?? null,
        state: ((issue["state"] ?? {}) as Row)["name"] ?? null,
        assignee: ((issue["assignee"] ?? {}) as Row)["name"] ?? null,
      };
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "external_send" },
  fixtures: [
    {
      scenario: "synthetic-created",
      input: { team_id: "9cfb482a-81e3-4154-b5b9-2c805e70a02d", title: "Timeout on /v1/run", priority: 2 },
      response: {
        status: 200,
        body: { data: { issueCreate: { success: true, issue: { id: "a1b2c3d4-0000-4000-8000-000000000001", identifier: "ENG-123", title: "Timeout on /v1/run", url: "https://linear.app/acme/issue/ENG-123", priority: 2, state: { name: "Todo" }, assignee: null } } } },
      },
      expect: {
        output: { id: "a1b2c3d4-0000-4000-8000-000000000001", identifier: "ENG-123", title: "Timeout on /v1/run", url: "https://linear.app/acme/issue/ENG-123", priority: 2, state: "Todo", assignee: null },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-graphql-error-in-200",
      input: { team_id: "9cfb482a-81e3-4154-b5b9-2c805e70a02d", title: "x" },
      response: { status: 200, body: { errors: [{ message: "Entity not found: Team", extensions: { code: "INVALID_INPUT" } }], data: null } },
      expect: { error: true },
    },
  ],
});
