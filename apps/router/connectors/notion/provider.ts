/**
 * First-party Notion connector (E51): reads of the user's own Notion workspace, both L1 by the
 * scope table. Ranked by the same score as any other provider and labeled `first_party`
 * (invariant 13).
 *
 * Auth: the calling principal's own Notion access token (slot `notion-tokenset`, the grant the
 * hosted connect flow stores and the core's read-sync also reads), fetched from apps/api at run
 * time and injected by the transport only (invariant 7; docs/api/connector-tokens.md).
 * `NOTION_TOKEN` is read in development only, behind `ROUTER_DEV_SHARED_CREDENTIALS=1`.
 */
import { defineProvider } from "../define.js";
import { notionPagesText } from "./endpoints/pages.text.js";
import { notionSearch } from "./endpoints/search.js";
import manifest from "./provider.json" with { type: "json" };

export const notionProvider = defineProvider({
  id: "notion",
  name: "Notion (ShogunAgent connector)",
  first_party: true,
  meta: {
    description: "Search (L1) and read (L1) pages in the user's Notion workspace through Notion's REST API.",
    docsUrl: "https://developers.notion.com/reference/intro",
    homepage: "https://shogunai.si",
  },
  request: { baseUrl: "https://api.notion.com" },
  auth: { kind: "caller_connector", slot: "notion-tokenset", credential_env: "NOTION_TOKEN" },
  manifest,
  categories: ["files"],
  health: { p50_ms: 400, p95_ms: 1400, success_rate: 0.995 },
  endpoints: [notionSearch, notionPagesText],
});
