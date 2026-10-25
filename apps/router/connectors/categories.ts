/**
 * The closed category list (connectors/schema.md §3). Validated at compile time, like Monid's
 * `connectors/categories.ts`: a definition naming a category outside this list does not compile.
 *
 * `memory`, `state` and `approval` are ShogunAgent additions (E8-T2); the rest cover what the
 * imported third-party catalog actually contains, so a Monid category maps onto exactly one of
 * them (src/passthrough/monid.ts `MONID_CATEGORY_MAP`).
 */
export const CATEGORIES = [
  /** The user's typed world model: people, projects, commitments, open loops. */
  "state",
  /** Recall over the user's event log and derived memory. */
  "memory",
  /** Writes that require a human approval before they take effect (L2 / L3). */
  "approval",
  "web_search",
  "scrape",
  "enrichment",
  "communication",
  "finance",
  "social",
  "ecommerce",
  "gen_ai",
  "seo",
  "listings",
  "files",
  /** Calendars and events (E22 first-party Google Calendar connector). */
  "calendar",
  /** Issue trackers: GitHub issues, Linear issues (E22). */
  "issue_tracking",
  /** Structured data lookups that fit no narrower category. */
  "data",
] as const;

export type Category = (typeof CATEGORIES)[number];

export function isCategory(value: unknown): value is Category {
  return typeof value === "string" && (CATEGORIES as readonly string[]).includes(value);
}
