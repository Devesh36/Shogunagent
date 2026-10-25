/**
 * The open public catalog (E29): every listed endpoint with its price, live health, p50/p95,
 * categories, first-party label, data-handling statement and the published score formula —
 * `GET /v1/catalog`, `GET /v1/catalog/{tool_id}` (JSON) and `GET /catalog` (a server-rendered
 * page with no JavaScript). No key, no account.
 *
 * Generated from the compiled catalog (invariant 12): nothing here is hand-written per tool.
 * What a provider declared privately in `provider.json` — legal name, contact, payout reference —
 * is not in the compiled catalog at all (src/catalog.ts publishes only `data_handling`), so it
 * cannot reach this surface; test/open-catalog.test.ts asserts it against every manifest.
 */
import type { Category } from "../connectors/categories.js";
import type { JsonObject, OpClass } from "../connectors/types.js";
import type { CatalogEntry, CompiledCatalog, PublicUsage } from "./catalog.js";
import { RANKING_FORMULA, RANKING_WEIGHTS } from "./discover.js";
import type { HealthStore } from "./health.js";
import { CLASS_MEANING, LEVEL_MEANING } from "./inspect.js";
import type { PublicDataHandling } from "./listing/manifest.js";
import { PERSONAL_COMPONENTS, PERSONAL_FORMULA, PERSONAL_MIN_RUNS } from "./personal.js";
import type { Level } from "./scope.js";

export const OPEN_CATALOG_FORMAT = "shogunagent-open-catalog/1";

export interface OpenCatalogEntry {
  tool_id: string;
  provider_id: string;
  provider_name: string;
  first_party: boolean;
  /** Shown, never boosted (invariant 13). */
  label: "first_party" | "third_party";
  summary: string;
  categories: Category[];
  permission: { level: Level; class: OpClass; meaning: string };
  price: PublicUsage & { quote_micro_usd: number; quote_usd: string; failures_cost: 0; empty_results_cost: 0 };
  health: { p50_ms: number; p95_ms: number; success_rate: number; source: "observed" | "declared" };
  data_handling: PublicDataHandling | null;
  docs_url: string | null;
  entry_hash: string;
  links: { detail: string; inspect: string; run: string };
}

export interface OpenCatalogDocument {
  format: typeof OPEN_CATALOG_FORMAT;
  catalog_hash: string;
  entry_count: number;
  provider_count: number;
  ranking: {
    formula: string;
    weights: typeof RANKING_WEIGHTS;
    personal: { formula: string; components: typeof PERSONAL_COMPONENTS; min_runs: number };
    policy: string;
  };
  pricing: { currency: "USD"; amount_unit: "micro_usd"; note: string };
  entries: OpenCatalogEntry[];
}

export const NO_SELF_PREFERENCING =
  "First-party tools are ranked by the same published formula as third-party tools, labeled first_party, and carry their price inside the score breakdown. There is no boost, no pinned slot and no hidden term.";

export const PRICING_NOTE =
  "quote_micro_usd is what one typical run is quoted (1 USD = 1,000,000 micro-USD). Failures and empty results cost 0. Third-party runs settle at the provider's price plus a 5% take; first-party runs at their listed price. Reads of the user's own state are free.";

function usd(micro: number): string {
  if (micro === 0) return "$0";
  return `$${(micro / 1_000_000).toFixed(6).replace(/0+$/, "").replace(/\.$/, "")}`;
}

export function openEntry(entry: CatalogEntry, health: HealthStore): OpenCatalogEntry {
  const live = health.get(entry.tool_id, entry.health);
  const id = encodeURIComponent(entry.tool_id);
  return {
    tool_id: entry.tool_id,
    provider_id: entry.provider_id,
    provider_name: entry.provider_name,
    first_party: entry.first_party,
    label: entry.first_party ? "first_party" : "third_party",
    summary: entry.summary,
    categories: entry.categories,
    permission: { level: entry.scope.level, class: entry.scope.class, meaning: LEVEL_MEANING[entry.scope.level] },
    price: { ...entry.usage, quote_micro_usd: entry.quote_micro_usd, quote_usd: usd(entry.quote_micro_usd), failures_cost: 0, empty_results_cost: 0 },
    health: { p50_ms: live.p50_ms, p95_ms: live.p95_ms, success_rate: live.success_rate, source: live === entry.health ? "declared" : "observed" },
    data_handling: entry.data_handling,
    docs_url: entry.docs_url,
    entry_hash: entry.hash,
    links: { detail: `/v1/catalog/${id}`, inspect: "/v1/router/inspect", run: "/v1/router/run" },
  };
}

export interface OpenCatalogFilter {
  category?: string;
  provider?: string;
  first_party?: boolean;
}

export function openCatalog(catalog: CompiledCatalog, health: HealthStore, filter: OpenCatalogFilter = {}): OpenCatalogDocument {
  let entries = catalog.entries();
  if (filter.category) entries = entries.filter((e) => (e.categories as string[]).includes(filter.category ?? ""));
  if (filter.provider) entries = entries.filter((e) => e.provider_id === filter.provider);
  if (filter.first_party !== undefined) entries = entries.filter((e) => e.first_party === filter.first_party);
  return {
    format: OPEN_CATALOG_FORMAT,
    catalog_hash: catalog.manifest.hash,
    entry_count: entries.length,
    provider_count: new Set(entries.map((e) => e.provider_id)).size,
    ranking: {
      formula: RANKING_FORMULA,
      weights: RANKING_WEIGHTS,
      personal: { formula: PERSONAL_FORMULA, components: PERSONAL_COMPONENTS, min_runs: PERSONAL_MIN_RUNS },
      policy: NO_SELF_PREFERENCING,
    },
    pricing: { currency: "USD", amount_unit: "micro_usd", note: PRICING_NOTE },
    entries: entries.map((e) => openEntry(e, health)),
  };
}

export interface OpenCatalogDetail extends OpenCatalogEntry {
  description: string;
  exclusions: string | null;
  notes: string[];
  input_schema: JsonObject;
  output_schema: JsonObject | null;
  class_meaning: string;
  ranking: { formula: string; weights: typeof RANKING_WEIGHTS };
}

export function openCatalogDetail(catalog: CompiledCatalog, health: HealthStore, toolId: string): OpenCatalogDetail | undefined {
  const entry = catalog.get(toolId);
  if (!entry) return undefined;
  return {
    ...openEntry(entry, health),
    description: entry.description,
    exclusions: entry.exclusions,
    notes: entry.notes,
    input_schema: entry.input_schema,
    output_schema: entry.output_schema,
    class_meaning: CLASS_MEANING[entry.scope.class],
    ranking: { formula: RANKING_FORMULA, weights: RANKING_WEIGHTS },
  };
}

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function priceCell(p: OpenCatalogEntry["price"]): string {
  switch (p.model) {
    case "per_call":
      return `${esc(p.quote_usd)} / call`;
    case "per_result":
      return `${esc(usd(p.cost_micro_usd_per_result))} / result${p.base_micro_usd ? ` + ${esc(usd(p.base_micro_usd))}` : ""} (typical ${esc(p.quote_usd)})`;
    case "per_unit":
      return `${esc(usd(p.cost_micro_usd_per_unit))} / ${esc(p.unit)} (typical ${esc(p.quote_usd)})`;
  }
}

/** The page: one table, no script, no external resource. Every value is escaped. */
export function openCatalogHtml(doc: OpenCatalogDocument): string {
  const rows = doc.entries.map((e) => `<tr>
<td><code>${esc(e.tool_id)}</code><br><small>${esc(e.summary)}</small></td>
<td>${esc(e.provider_name)}${e.first_party ? ' <span class="fp">first-party</span>' : ""}</td>
<td>${esc(e.categories.join(", "))}</td>
<td>${priceCell(e.price)}</td>
<td>${(e.health.success_rate * 100).toFixed(1)}% <small>(${esc(e.health.source)})</small></td>
<td>${e.health.p50_ms} / ${e.health.p95_ms} ms</td>
<td>${esc(e.permission.level)}</td>
<td><small>${e.data_handling ? `${esc(e.data_handling.retention)}${e.data_handling.zero_retention ? ", zero retention" : ""}; ${esc(e.data_handling.regions.join(", "))}. ${esc(e.data_handling.statement)}` : "Passthrough: handled under the upstream marketplace's terms."}</small></td>
</tr>`).join("\n");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ShogunAgent tool catalog</title>
<style>
body{font:15px/1.45 system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:16px;color:#1a1a1a;background:#fff}
table{border-collapse:collapse;width:100%}th,td{border-bottom:1px solid #ddd;padding:6px 8px;text-align:left;vertical-align:top}
th{background:#f4f4f4}code{font-size:13px}.fp{background:#eef;border:1px solid #99c;border-radius:3px;padding:0 4px;font-size:12px}
.wrap{overflow-x:auto}small{color:#555}
@media (prefers-color-scheme:dark){body{background:#111;color:#eee}th{background:#222}td,th{border-color:#333}small{color:#aaa}.fp{background:#223;border-color:#557}}
</style>
</head>
<body>
<h1>ShogunAgent tool catalog</h1>
<p>${doc.entry_count} endpoints from ${doc.provider_count} providers. Catalog hash <code>${esc(doc.catalog_hash.slice(0, 12))}</code>. Machine-readable: <a href="/v1/catalog">/v1/catalog</a>.</p>
<h2>How tools are ranked</h2>
<p><code>${esc(doc.ranking.formula)}</code></p>
<p>${esc(doc.ranking.policy)}</p>
<p>${esc(doc.pricing.note)}</p>
<div class="wrap"><table>
<thead><tr><th>Tool</th><th>Provider</th><th>Categories</th><th>Price</th><th>Health</th><th>p50 / p95</th><th>Level</th><th>Data handling</th></tr></thead>
<tbody>
${rows}
</tbody>
</table></div>
<p><small>L1 runs immediately; L2 asks the agent to confirm once; L3 needs a human approval URL. Prices and health are generated from the compiled catalog and the router's live health window.</small></p>
</body>
</html>
`;
}
