import { defineEndpoint, z, zodInput } from "../../define.js";
import type { Json } from "../../types.js";
import { blocksText, cursor, MAX_TEXT_CHARS, NOTION_HEADERS, pageId } from "../shared.js";

interface TextInput {
  page_id: string;
  cursor?: string;
}

/**
 * E51: one page of the user's own Notion workspace as plain text (L1 by the scope table): its
 * top-level blocks, 100 at a time. Images, files and embeds are counted, never fetched.
 */
export const notionPagesText = defineEndpoint({
  id: "pages.text",
  meta: {
    summary: "Read one page from the user's Notion workspace as plain text: headings, paragraphs, lists and to-dos.",
    description:
      "Reads one page of the user's own Notion workspace by id as plain text: headings, paragraphs, lists, to-dos, quotes and code (cut at 20,000 characters), 100 blocks per call with a cursor. " +
      "Use for: what does this Notion doc say, summarize the onboarding page in Notion, read the to-dos written on a page. " +
      "L1 read. $0.002 per call.",
    exclusions: "Not for finding pages (use notion.search), images, files or databases, editing pages, or the user's commitments (use state.commitments.list).",
    categories: ["files"],
    notes: ["page_id comes from notion.search.", "Nested blocks are not expanded; images, files and embeds are counted in skipped_blocks, never fetched."],
    examples: [
      {
        input: { page_id: "1a2b3c4d-0000-1111-2222-333344445555" },
        output: { page_id: "1a2b3c4d-0000-1111-2222-333344445555", text: "# Goals\nShip the hosted connectors.\n[ ] Notion sign-in", truncated: false, skipped_blocks: 0, next_cursor: null },
      },
    ],
    docsUrl: "https://developers.notion.com/reference/get-block-children",
  },
  request: { method: "GET", path: "/v1/blocks/{page_id}/children", timeout_ms: 15_000 },
  input: {
    schema: zodInput(
      z
        .object({
          page_id: pageId.describe("The Notion page id."),
          cursor: cursor.optional().describe("next_cursor from a previous call, for the next 100 blocks."),
        })
        .strict(),
    ),
    toRequest: ({ input }) => {
      const i = input as unknown as TextInput;
      const query: Record<string, string> = { page_size: "100" };
      if (i.cursor) query["start_cursor"] = i.cursor;
      return { method: "GET", path: `/v1/blocks/${i.page_id}/children`, query, headers: { ...NOTION_HEADERS } };
    },
  },
  output: {
    fromResponse: ({ input, response }) => {
      const b = (response.body ?? {}) as Record<string, Json>;
      const rows = Array.isArray(b["results"]) ? (b["results"] as Record<string, Json>[]) : [];
      const { text, skipped } = blocksText(rows);
      const truncated = text.length > MAX_TEXT_CHARS;
      return {
        page_id: (input as unknown as TextInput).page_id,
        text: truncated ? text.slice(0, MAX_TEXT_CHARS) : text,
        truncated,
        skipped_blocks: skipped,
        next_cursor: b["has_more"] === true && typeof b["next_cursor"] === "string" ? b["next_cursor"] : null,
      };
    },
    isEmpty: (output) => {
      const o = (output ?? {}) as Record<string, Json>;
      return o["text"] === "" && o["next_cursor"] === null;
    },
  },
  usage: { model: "per_call", cost_micro_usd: 2000 },
  scope: { class: "read" },
  fixtures: [
    {
      scenario: "synthetic-text-blocks-and-an-image",
      input: { page_id: "1a2b3c4d-0000-1111-2222-333344445555" },
      response: {
        status: 200,
        body: {
          object: "list",
          results: [
            { object: "block", type: "heading_1", heading_1: { rich_text: [{ plain_text: "Goals" }] } },
            { object: "block", type: "paragraph", paragraph: { rich_text: [{ plain_text: "Ship the hosted " }, { plain_text: "connectors." }] } },
            { object: "block", type: "image", image: { type: "file", file: { url: "https://files.example/secret.png" } } },
            { object: "block", type: "to_do", to_do: { rich_text: [{ plain_text: "Notion sign-in" }], checked: false } },
          ],
          has_more: false,
          next_cursor: null,
        },
      },
      expect: {
        output: { page_id: "1a2b3c4d-0000-1111-2222-333344445555", text: "# Goals\nShip the hosted connectors.\n[ ] Notion sign-in", truncated: false, skipped_blocks: 1, next_cursor: null },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-more-blocks-follow",
      input: { page_id: "1a2b3c4d000011112222333344445555", cursor: "c1" },
      response: { status: 200, body: { object: "list", results: [{ object: "block", type: "bulleted_list_item", bulleted_list_item: { rich_text: [{ plain_text: "Item" }] } }], has_more: true, next_cursor: "c2" } },
      expect: {
        output: { page_id: "1a2b3c4d000011112222333344445555", text: "- Item", truncated: false, skipped_blocks: 0, next_cursor: "c2" },
        usage: { quantity: 1, cost_micro_usd: 2000 },
      },
    },
    {
      scenario: "synthetic-empty-page",
      input: { page_id: "1a2b3c4d-0000-1111-2222-333344445556" },
      response: { status: 200, body: { object: "list", results: [], has_more: false, next_cursor: null } },
      expect: { output: { page_id: "1a2b3c4d-0000-1111-2222-333344445556", text: "", truncated: false, skipped_blocks: 0, next_cursor: null }, usage: { quantity: 0, cost_micro_usd: 0 } },
    },
    {
      scenario: "synthetic-page-not-shared",
      input: { page_id: "1a2b3c4d-0000-1111-2222-333344445557" },
      response: { status: 404, body: { object: "error", status: 404, code: "object_not_found", message: "Could not find block." } },
      expect: { error: true },
    },
  ],
});
