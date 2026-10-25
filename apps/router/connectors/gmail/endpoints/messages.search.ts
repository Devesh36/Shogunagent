import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";

interface SearchInput {
  query: string;
  max_results?: number;
  page_token?: string;
  include_spam_trash?: boolean;
}

/**
 * E45: a read of the user's own mailbox (L1 by the scope table). Gmail's `messages.list` answers
 * ids only; `gmail.messages.get` reads one message. Same `gmail-tokenset` grant as the writes
 * (`gmail.modify` covers reading).
 */
export const gmailMessagesSearch = defineEndpoint({
  id: "messages.search",
  meta: {
    summary: "Search the user's Gmail with Gmail search syntax and list the matching message and thread ids.",
    description:
      "Finds messages in the user's own Gmail with Gmail's search operators (from:, to:, subject:, after:, is:unread, has:attachment, label:) and returns message and thread ids, newest first, with a page token. " +
      "Use for: find the email from Mika about the contract, which unread emails arrived today, find the thread with the invoice. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for the message text (use gmail.messages.get), sending or drafting (gmail.messages.send, gmail.drafts.create), or the user's commitments (use state.commitments.list).",
    categories: ["communication"],
    notes: ["query uses Gmail search syntax, e.g. from:mika@example.com after:2026/10/20.", "max_results defaults to 20 (max 100). Spam and trash are excluded unless include_spam_trash is true."],
    examples: [
      {
        input: { query: "from:mika@example.com subject:deck", max_results: 5 },
        output: { items: [{ message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2" }], next_page_token: null, result_size_estimate: 1 },
      },
    ],
    docsUrl: "https://developers.google.com/gmail/api/reference/rest/v1/users.messages/list",
  },
  request: { method: "GET", path: "/gmail/v1/users/me/messages", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          query: z.string().min(1).max(500).describe("Gmail search query."),
          max_results: z.number().int().min(1).max(100).optional().describe("Default 20."),
          page_token: z.string().regex(/^[A-Za-z0-9_-]{1,256}$/).optional().describe("next_page_token from a previous call."),
          include_spam_trash: z.boolean().optional().describe("Default false."),
        })
        .strict(),
    ),
    toRequest: ({ input, request }) => {
      const i = input as unknown as SearchInput;
      const query: Record<string, string> = { q: i.query, maxResults: String(i.max_results ?? 20) };
      if (i.page_token) query["pageToken"] = i.page_token;
      if (i.include_spam_trash) query["includeSpamTrash"] = "true";
      return { method: request.method, path: request.path, query };
    },
  },
  output: {
    fromResponse: ({ response }) => {
      const b = (response.body ?? {}) as Record<string, Json>;
      const rows = Array.isArray(b["messages"]) ? (b["messages"] as Record<string, Json>[]) : [];
      return {
        items: rows.map((m) => ({ message_id: m["id"] ?? null, thread_id: m["threadId"] ?? null })),
        next_page_token: b["nextPageToken"] ?? null,
        result_size_estimate: typeof b["resultSizeEstimate"] === "number" ? b["resultSizeEstimate"] : rows.length,
      };
    },
    isEmpty: (output) => {
      const items = (output as Record<string, Json> | null)?.["items"];
      return !Array.isArray(items) || items.length === 0;
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "synthetic-two-matches",
      input: { query: "from:mika@example.com subject:deck", max_results: 5 },
      response: { status: 200, body: { messages: [{ id: "18c2f0a1b2", threadId: "18c2f0a1b2" }, { id: "18c2f0a1c3", threadId: "18c2f0a1b2" }], resultSizeEstimate: 2 } },
      expect: {
        output: { items: [{ message_id: "18c2f0a1b2", thread_id: "18c2f0a1b2" }, { message_id: "18c2f0a1c3", thread_id: "18c2f0a1b2" }], next_page_token: null, result_size_estimate: 2 },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-no-match",
      input: { query: "subject:nothing-like-this" },
      response: { status: 200, body: { resultSizeEstimate: 0 } },
      expect: { output: { items: [], next_page_token: null, result_size_estimate: 0 }, usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-token-expired",
      input: { query: "is:unread" },
      response: { status: 401, body: { error: { code: 401, status: "UNAUTHENTICATED" } } },
      expect: { error: true },
    },
  ],
});
