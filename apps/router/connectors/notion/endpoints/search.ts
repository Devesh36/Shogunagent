import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { cursor, mapPage, NOTION_HEADERS } from "../shared.js";

interface SearchInput {
  query: string;
  limit?: number;
  cursor?: string;
}

/**
 * E51: find pages in the user's own Notion workspace (L1 by the scope table). Notion's search
 * matches page titles; `notion.pages.text` reads one page. Only pages the user shared with the
 * integration at sign-in are visible.
 */
export const notionSearch = defineEndpoint({
  id: "search",
  meta: {
    summary: "Search the user's Notion workspace for pages by title and list their ids, titles and links.",
    description:
      "Finds pages in the user's own Notion workspace whose title matches the text, most recently edited first, with page id, title, URL and last edited time. " +
      "Use for: find the Notion page about the Q3 plan, where are the onboarding docs in Notion, look up a page by its title. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for a page's text (use notion.pages.text), databases or comments, editing pages, or the user's commitments (use state.commitments.list).",
    categories: ["files"],
    notes: ["Only pages the user shared with ShogunAgent when connecting Notion are found.", "limit defaults to 20 (max 100); pass next_cursor to continue."],
    examples: [
      {
        input: { query: "Q3 plan", limit: 5 },
        output: { items: [{ page_id: "1a2b3c4d-0000-1111-2222-333344445555", title: "Q3 plan", url: "https://www.notion.so/Q3-plan-1a2b3c4d000011112222333344445555", last_edited_time: "2026-10-24T08:00:00.000Z" }], next_cursor: null },
      },
    ],
    docsUrl: "https://developers.notion.com/reference/post-search",
  },
  request: { method: "POST", path: "/v1/search", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          query: z.string().min(1).max(200).describe("Text to match in page titles."),
          limit: z.number().int().min(1).max(100).optional().describe("Default 20."),
          cursor: cursor.optional().describe("next_cursor from a previous call."),
        })
        .strict(),
    ),
    toRequest: ({ input, request }) => {
      const i = input as unknown as SearchInput;
      const body: Record<string, Json> = {
        query: i.query,
        filter: { property: "object", value: "page" },
        sort: { direction: "descending", timestamp: "last_edited_time" },
        page_size: i.limit ?? 20,
      };
      if (i.cursor) body["start_cursor"] = i.cursor;
      return { method: request.method, path: request.path, headers: { ...NOTION_HEADERS }, body };
    },
  },
  output: {
    fromResponse: ({ response }) => {
      const b = (response.body ?? {}) as Record<string, Json>;
      const rows = Array.isArray(b["results"]) ? (b["results"] as Record<string, Json>[]) : [];
      return {
        items: rows.filter((r) => r["object"] === "page" && r["archived"] !== true && r["in_trash"] !== true).map(mapPage),
        next_cursor: b["has_more"] === true && typeof b["next_cursor"] === "string" ? b["next_cursor"] : null,
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
      scenario: "synthetic-two-pages-one-archived",
      input: { query: "Q3 plan", limit: 5 },
      response: {
        status: 200,
        body: {
          object: "list",
          results: [
            {
              object: "page",
              id: "1a2b3c4d-0000-1111-2222-333344445555",
              url: "https://www.notion.so/Q3-plan-1a2b3c4d000011112222333344445555",
              last_edited_time: "2026-10-24T08:00:00.000Z",
              archived: false,
              properties: { title: { id: "title", type: "title", title: [{ plain_text: "Q3 plan" }] } },
            },
            {
              object: "page",
              id: "9f8e7d6c-0000-1111-2222-333344445555",
              url: "https://www.notion.so/Old-Q3-plan-9f8e7d6c000011112222333344445555",
              last_edited_time: "2026-01-02T08:00:00.000Z",
              archived: true,
              properties: { Name: { id: "title", type: "title", title: [{ plain_text: "Old Q3 plan" }] } },
            },
          ],
          has_more: false,
          next_cursor: null,
        },
      },
      expect: {
        output: { items: [{ page_id: "1a2b3c4d-0000-1111-2222-333344445555", title: "Q3 plan", url: "https://www.notion.so/Q3-plan-1a2b3c4d000011112222333344445555", last_edited_time: "2026-10-24T08:00:00.000Z" }], next_cursor: null },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-no-pages",
      input: { query: "nothing like this" },
      response: { status: 200, body: { object: "list", results: [], has_more: false, next_cursor: null } },
      expect: { output: { items: [], next_cursor: null }, usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-unauthorized",
      input: { query: "Q3 plan" },
      response: { status: 401, body: { object: "error", status: 401, code: "unauthorized", message: "API token is invalid." } },
      expect: { error: true },
    },
  ],
});
