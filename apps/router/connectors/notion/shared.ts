import { z } from "../define.js";
import type { Json } from "../types.js";

/** Notion's REST API requires a version header; it is pinned (the read-sync in the core speaks the
 * same one, `shogun_integrations::read_sync::NOTION_VERSION`). The transport adds auth only. */
export const NOTION_HEADERS = { "notion-version": "2022-06-28" } as const;

/** A Notion page id: 32 hex digits, with or without the dashes. Safe to place in a path segment. */
export const pageId = z
  .string()
  .regex(/^[0-9a-fA-F]{8}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{12}$/, "a Notion page id (32 hex digits, dashes optional)");

/** Notion's opaque pagination cursor. */
export const cursor = z.string().regex(/^[A-Za-z0-9_-]{1,128}$/);

/** Characters of page text returned (the rest is cut and `truncated` is true). */
export const MAX_TEXT_CHARS = 20_000;

type Row = Record<string, Json>;

/** The plain text of a Notion rich-text array. */
export function plainText(v: Json | undefined): string {
  if (!Array.isArray(v)) return "";
  return (v as Row[]).map((t) => (typeof t["plain_text"] === "string" ? t["plain_text"] : "")).join("");
}

/** A page's title: the one property of type `title`. */
export function pageTitle(page: Row): string {
  const props = (page["properties"] ?? {}) as Record<string, Row>;
  for (const p of Object.values(props)) {
    if (p && p["type"] === "title") return plainText(p["title"]);
  }
  return "";
}

/** Notion's page object → the fields an agent picks a page by. */
export function mapPage(page: Row): Json {
  return {
    page_id: page["id"] ?? null,
    title: pageTitle(page),
    url: page["url"] ?? null,
    last_edited_time: page["last_edited_time"] ?? null,
  };
}

/** Block types whose rich text is read, with the prefix that keeps their shape in plain text. */
const TEXT_BLOCKS: Record<string, string> = {
  paragraph: "",
  heading_1: "# ",
  heading_2: "## ",
  heading_3: "### ",
  bulleted_list_item: "- ",
  numbered_list_item: "1. ",
  toggle: "",
  quote: "> ",
  callout: "",
  code: "",
};

/** Children blocks → the page's text. Images, files, embeds and every other block that is not
 * text are counted in `skipped_blocks`, never fetched. Pure. */
export function blocksText(blocks: Row[]): { text: string; skipped: number } {
  const lines: string[] = [];
  let skipped = 0;
  for (const b of blocks) {
    const type = typeof b["type"] === "string" ? b["type"] : "";
    const content = (b[type] ?? {}) as Row;
    if (type === "to_do") {
      lines.push(`${content["checked"] === true ? "[x]" : "[ ]"} ${plainText(content["rich_text"])}`);
    } else if (type === "child_page") {
      lines.push(`[page] ${typeof content["title"] === "string" ? content["title"] : ""}`);
    } else if (type in TEXT_BLOCKS) {
      lines.push(`${TEXT_BLOCKS[type]}${plainText(content["rich_text"])}`);
    } else if (type === "divider") {
      lines.push("---");
    } else {
      skipped += 1;
    }
  }
  return { text: lines.join("\n"), skipped };
}
