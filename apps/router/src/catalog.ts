/**
 * The catalog compiler (E8-T2). Validates every connector definition and produces the
 * content-hashed catalog that `discover` ranks, `inspect` describes and `run` executes.
 *
 * Validation (a definition failing any of these does not compile):
 *   - provider and endpoint ids are URL-safe and unique; tool ids are `<provider>.<endpoint>`;
 *   - categories come from the closed list and the endpoint/provider union is non-empty;
 *   - the endpoint declares a scope class (src/scope.ts) and has a health baseline (own or provider's);
 *   - usage costs are non-negative integers (micro-USD);
 *   - every fixture's input passes the endpoint's input schema.
 *
 * The compiled entry carries no functions, so it serialises to the catalog JSON that
 * `scripts/build-catalog.ts` writes; hooks stay on the definition, reachable through `lookup`.
 */
import { CATEGORIES, isCategory, type Category } from "../connectors/categories.js";
import type {
  EndpointDefinition,
  HealthBaseline,
  Json,
  JsonObject,
  OpClass,
  ProviderDefinition,
  UsageModel,
} from "../connectors/types.js";
import { Bm25Index } from "./bm25.js";
import { contentHash, sha256Hex } from "./hash.js";
import { buildScopeTable, levelFor, type Level, type ScopeTable } from "./scope.js";

export type PublicUsage =
  | { model: "per_call"; unit: "call"; cost_micro_usd: number }
  | { model: "per_result"; unit: "result"; cost_micro_usd_per_result: number; base_micro_usd: number; typical_results: number }
  | { model: "per_unit"; unit: string; cost_micro_usd_per_unit: number; typical_units: number };

export interface CatalogEntry {
  tool_id: string;
  provider_id: string;
  provider_name: string;
  first_party: boolean;
  summary: string;
  description: string;
  /** Shown by inspect, never indexed by discover. */
  exclusions: string | null;
  notes: string[];
  categories: Category[];
  input_schema: JsonObject;
  output_schema: JsonObject | null;
  usage: PublicUsage;
  /** The typical cost of one run, in micro-USD — what `discover` ranks on and `dryRun` quotes. */
  quote_micro_usd: number;
  health: HealthBaseline;
  scope: { class: OpClass; level: Level };
  examples: { input: Json; output: Json }[];
  docs_url: string | null;
  /** SHA-256 of this entry's canonical JSON (all fields above). */
  hash: string;
}

export interface CatalogManifest {
  format: "shogunagent-router-catalog/1";
  /** SHA-256 over the ordered entry hashes. Changes iff any entry changes. */
  hash: string;
  entry_count: number;
  provider_count: number;
  entries: CatalogEntry[];
}

export interface CompiledCatalog {
  manifest: CatalogManifest;
  scope: ScopeTable;
  entries(): CatalogEntry[];
  get(toolId: string): CatalogEntry | undefined;
  /** The definition behind an entry — hooks live here, never in the manifest. */
  lookup(toolId: string): { provider: ProviderDefinition; endpoint: EndpointDefinition } | undefined;
  index: Bm25Index;
}

export class CatalogCompileError extends Error {
  constructor(readonly problems: string[]) {
    super(`catalog: ${problems.length} problem(s):\n  ${problems.join("\n  ")}`);
  }
}

const ID_RE = /^[a-z0-9][a-z0-9_.-]*$/;

function nonNegativeInt(n: number): boolean {
  return Number.isInteger(n) && n >= 0;
}

export function publicUsage(u: UsageModel): PublicUsage {
  switch (u.model) {
    case "per_call":
      return { model: "per_call", unit: "call", cost_micro_usd: u.cost_micro_usd };
    case "per_result":
      return {
        model: "per_result",
        unit: "result",
        cost_micro_usd_per_result: u.cost_micro_usd_per_result,
        base_micro_usd: u.base_micro_usd ?? 0,
        typical_results: u.typical_results,
      };
    case "per_unit":
      return { model: "per_unit", unit: u.unit, cost_micro_usd_per_unit: u.cost_micro_usd_per_unit, typical_units: u.typical_units };
  }
}

/** The typical cost of one run under a usage model. */
export function quoteMicroUsd(u: PublicUsage): number {
  switch (u.model) {
    case "per_call":
      return u.cost_micro_usd;
    case "per_result":
      return u.base_micro_usd + u.cost_micro_usd_per_result * u.typical_results;
    case "per_unit":
      return Math.round(u.cost_micro_usd_per_unit * u.typical_units);
  }
}

function validateUsage(u: UsageModel, where: string, problems: string[]): void {
  switch (u.model) {
    case "per_call":
      if (!nonNegativeInt(u.cost_micro_usd)) problems.push(`${where}: usage.cost_micro_usd must be a non-negative integer`);
      break;
    case "per_result":
      if (!nonNegativeInt(u.cost_micro_usd_per_result)) problems.push(`${where}: usage.cost_micro_usd_per_result must be a non-negative integer`);
      if (u.base_micro_usd !== undefined && !nonNegativeInt(u.base_micro_usd)) problems.push(`${where}: usage.base_micro_usd must be a non-negative integer`);
      if (!nonNegativeInt(u.typical_results)) problems.push(`${where}: usage.typical_results must be a non-negative integer`);
      break;
    case "per_unit":
      if (!(u.cost_micro_usd_per_unit >= 0)) problems.push(`${where}: usage.cost_micro_usd_per_unit must be >= 0`);
      if (!(u.typical_units >= 0)) problems.push(`${where}: usage.typical_units must be >= 0`);
      if (!u.unit) problems.push(`${where}: usage.unit is required`);
      break;
  }
}

function validateHealth(h: HealthBaseline | undefined, where: string, problems: string[]): void {
  if (!h) {
    problems.push(`${where}: a health baseline is required (endpoint or provider)`);
    return;
  }
  if (!(h.p50_ms >= 0) || !(h.p95_ms >= h.p50_ms)) problems.push(`${where}: health p50_ms/p95_ms must be >= 0 and p95 >= p50`);
  if (!(h.success_rate >= 0 && h.success_rate <= 1)) problems.push(`${where}: health.success_rate must be in [0, 1]`);
}

/**
 * Drops exclusion sentences ("Not for …", "Not a crawler.") from a description before indexing,
 * so naming the task an endpoint refuses does not attract it. Imported catalogs put exclusions
 * inside the description; our own definitions put them in `meta.exclusions`, which is never indexed.
 */
export function stripExclusions(description: string): string {
  return description
    .split(/(?<=[.!?])\s+/)
    .filter((s) => !/^not\b/i.test(s.trim()))
    .join(" ");
}

/** The text BM25 ranks: summary and categories count twice so they outweigh prose of the same words. Exclusions are not ranked. */
export function rankingText(e: CatalogEntry): string {
  const cats = e.categories.join(" ");
  const id = e.tool_id.replace(/[._-]/g, " ");
  return [id, e.summary, e.summary, cats, cats, stripExclusions(e.description), e.notes.join(" ")].join("\n");
}

export function compileCatalog(providers: ProviderDefinition[]): CompiledCatalog {
  const problems: string[] = [];
  const entries: CatalogEntry[] = [];
  const defs = new Map<string, { provider: ProviderDefinition; endpoint: EndpointDefinition }>();
  const providerIds = new Set<string>();

  for (const provider of providers) {
    const pw = `provider ${provider.id}`;
    if (!ID_RE.test(provider.id)) problems.push(`${pw}: id must match ${ID_RE}`);
    if (providerIds.has(provider.id)) problems.push(`${pw}: duplicate provider id`);
    providerIds.add(provider.id);
    if (!provider.request.baseUrl) problems.push(`${pw}: request.baseUrl is required`);
    for (const c of provider.categories ?? []) if (!isCategory(c)) problems.push(`${pw}: unknown category ${String(c)} (allowed: ${CATEGORIES.join(", ")})`);
    if (provider.health) validateHealth(provider.health, pw, problems);

    for (const endpoint of provider.endpoints) {
      const toolId = `${provider.id}.${endpoint.id}`;
      const ew = `endpoint ${toolId}`;
      if (!ID_RE.test(endpoint.id)) problems.push(`${ew}: id must match ${ID_RE}`);
      if (defs.has(toolId)) {
        problems.push(`${ew}: duplicate tool id`);
        continue;
      }
      const categories = [...new Set([...(endpoint.meta.categories ?? []), ...(provider.categories ?? [])])];
      if (categories.length === 0) problems.push(`${ew}: at least one category is required`);
      for (const c of categories) if (!isCategory(c)) problems.push(`${ew}: unknown category ${String(c)}`);
      if (!endpoint.meta.summary?.trim()) problems.push(`${ew}: meta.summary is required`);
      if (!endpoint.meta.description?.trim()) problems.push(`${ew}: meta.description is required`);
      if (!endpoint.scope?.class) problems.push(`${ew}: scope.class is required (an endpoint without a class is not in the permission table)`);
      validateUsage(endpoint.usage, ew, problems);
      const health = endpoint.health ?? provider.health;
      validateHealth(health, ew, problems);
      for (const f of endpoint.fixtures) {
        const v = endpoint.input.schema.validate(f.input);
        if (!v.ok) problems.push(`${ew}: fixture "${f.scenario}" input fails the schema: ${v.errors.join("; ")}`);
      }
      if (!health || !endpoint.scope?.class) continue;

      const usage = publicUsage(endpoint.usage);
      const entryBody = {
        tool_id: toolId,
        provider_id: provider.id,
        provider_name: provider.name,
        first_party: provider.first_party,
        summary: endpoint.meta.summary,
        description: endpoint.meta.description,
        exclusions: endpoint.meta.exclusions ?? null,
        notes: endpoint.meta.notes ?? [],
        categories: categories.filter(isCategory),
        input_schema: endpoint.input.schema.jsonSchema,
        output_schema: endpoint.output.schema?.jsonSchema ?? null,
        usage,
        quote_micro_usd: quoteMicroUsd(usage),
        health,
        scope: { class: endpoint.scope.class, level: levelFor(endpoint.scope.class) },
        examples: endpoint.meta.examples ?? [],
        docs_url: endpoint.meta.docsUrl ?? provider.meta.docsUrl ?? null,
      };
      const entry: CatalogEntry = { ...entryBody, hash: contentHash(entryBody as unknown as Json) };
      entries.push(entry);
      defs.set(toolId, { provider, endpoint });
    }
  }

  if (problems.length) throw new CatalogCompileError(problems);

  entries.sort((a, b) => (a.tool_id < b.tool_id ? -1 : a.tool_id > b.tool_id ? 1 : 0));
  const manifest: CatalogManifest = {
    format: "shogunagent-router-catalog/1",
    hash: sha256Hex(entries.map((e) => e.hash).join("\n")),
    entry_count: entries.length,
    provider_count: providerIds.size,
    entries,
  };
  const byId = new Map(entries.map((e) => [e.tool_id, e]));
  return {
    manifest,
    scope: buildScopeTable(entries.map((e) => ({ tool_id: e.tool_id, class: e.scope.class }))),
    entries: () => entries,
    get: (id) => byId.get(id),
    lookup: (id) => defs.get(id),
    index: new Bm25Index(entries.map((e) => ({ id: e.tool_id, text: rankingText(e) }))),
  };
}
