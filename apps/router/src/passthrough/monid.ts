/**
 * Monid passthrough (E8-T6; README §7 "borrow the cold start"). Imports a Monid-style catalog
 * JSON into one `ProviderDefinition` whose endpoints all execute through Monid's hosted
 * `POST /v1/run`, priced at Monid's own price: the figure Monid invoices us for the run.
 *
 * E32, one take: the take is NOT baked in here. The catalog carries provider prices only; the
 * caller pays Monid's price plus ONE take (`RUN_MARKUP_BPS`, src/take.ts), added by apps/api's
 * wallet at reserve and settle. Monid bills our account (the router's `MONID_API_KEY`), never the
 * caller, so whatever platform fee Monid includes in its price is part of the provider price and
 * gets exactly one take on top. Before E32 this importer added 5% and the wallet would have added
 * 5% again.
 *
 * Zero retention: the importer and the hooks below see inputs and vendor bodies only as values
 * passing through; nothing here logs, stores or formats them. The Monid credential is named
 * (`MONID_API_KEY`) and injected by src/transport.ts; this file never reads the environment.
 *
 * This importer is the one place a change in Monid's catalog shape is absorbed. The shape below
 * is our reading of their discover/inspect responses (research note KQ1/KQ5); the fixture in
 * connectors/monid/catalog.fixture.json documents it.
 */
import { CATEGORIES, isCategory, type Category } from "../../connectors/categories.js";
import type { EndpointDefinition, Fixture, Json, JsonObject, OpClass, ProviderDefinition, RunResponse, UsageModel } from "../../connectors/types.js";
import { jsonSchemaInput } from "../jsonschema.js";

export const MONID_BASE_URL = "https://api.monid.ai";
export const MONID_CREDENTIAL_ENV = "MONID_API_KEY";

/** Monid's category names (monid.ai/rankings filters) onto the closed list. */
export const MONID_CATEGORY_MAP: Record<string, Category> = {
  Search: "web_search",
  "Web scraping": "scrape",
  "Lead gen": "enrichment",
  Enrichment: "enrichment",
  Social: "social",
  "E-commerce": "ecommerce",
  "Gen AI": "gen_ai",
  "SEO & GEO": "seo",
  Listings: "listings",
  Communication: "communication",
  Telephony: "communication",
  Finance: "finance",
  Files: "files",
  Data: "data",
};

export interface MonidPricing {
  model: "PER_CALL" | "PER_RESULT" | "PER_UNIT";
  priceUsd: number;
  basePriceUsd?: number;
  unit?: string;
  typicalQuantity?: number;
}

export interface MonidEndpoint {
  id: string;
  provider: string;
  summary: string;
  description: string;
  categories: string[];
  inputSchema: JsonObject;
  pricing: MonidPricing;
  health: { p50Ms: number; p95Ms: number; successRate: number };
  verified?: boolean;
  /** Set by Monid for endpoints that act on the outside world (post, send, provision). */
  sideEffects?: boolean;
  /** Set for endpoints that provision a billable resource beyond the call price. */
  provisions?: boolean;
  notes?: string[];
  docsUrl?: string;
  fixtures?: { scenario: string; input: Json; response: MonidRunResponse; expectOutput?: Json; expectUsage?: { quantity: number; cost_micro_usd: number } }[];
}

export interface MonidCatalog {
  version: string;
  providers: { slug: string; name: string; docsUrl?: string }[];
  endpoints: MonidEndpoint[];
}

/** Monid's `POST /v1/run` answer, as the fixture records it. */
export interface MonidRunResponse {
  run_id?: string;
  status: "succeeded" | "failed" | "blocked";
  output?: Json;
  usage?: { quantity?: number; cost_usd?: number };
  error?: { code: string };
}

/** Monid's USD price in integer micro-USD, unchanged (no markup: the take is the wallet's). */
export function toMicroUsd(usd: number): number {
  return Math.round(usd * 1_000_000);
}

function usageFor(p: MonidPricing): UsageModel {
  switch (p.model) {
    case "PER_CALL":
      return { model: "per_call", cost_micro_usd: toMicroUsd(p.priceUsd) };
    case "PER_RESULT":
      return {
        model: "per_result",
        cost_micro_usd_per_result: toMicroUsd(p.priceUsd),
        base_micro_usd: toMicroUsd(p.basePriceUsd ?? 0),
        typical_results: p.typicalQuantity ?? 10,
        results: monidQuantity,
      };
    case "PER_UNIT":
      return {
        model: "per_unit",
        unit: p.unit ?? "unit",
        cost_micro_usd_per_unit: p.priceUsd * 1_000_000,
        typical_units: p.typicalQuantity ?? 1,
        units: ({ response }) => monidReportedQuantity(response),
      };
  }
}

function monidQuantity(output: Json): number {
  if (Array.isArray(output)) return output.length;
  if (output && typeof output === "object") {
    const items = (output as Record<string, Json>)["results"] ?? (output as Record<string, Json>)["items"];
    if (Array.isArray(items)) return items.length;
  }
  return 1;
}

function monidReportedQuantity(response: RunResponse): number {
  const body = response.body as MonidRunResponse | null;
  const q = body?.usage?.quantity;
  return typeof q === "number" ? q : 0;
}

function scopeClassFor(e: MonidEndpoint): OpClass {
  if (e.provisions) return "payment";
  if (e.sideEffects) return "external_send";
  return "read";
}

function categoriesFor(e: MonidEndpoint, problems: string[]): Category[] {
  const cats = e.categories.map((c) => MONID_CATEGORY_MAP[c] ?? (isCategory(c) ? c : undefined)).filter((c): c is Category => c !== undefined);
  if (cats.length === 0) problems.push(`monid endpoint ${e.id}: no category maps onto ${CATEGORIES.join("/")}`);
  return [...new Set(cats)];
}

/** Monid endpoint ids look like `tinyfish/web-search`; our local ids are `tinyfish.web-search`. */
export function localId(monidId: string): string {
  return monidId.toLowerCase().replace(/^\/+/, "").replace(/[^a-z0-9._-]+/g, ".");
}

export function importMonidCatalog(catalog: MonidCatalog): ProviderDefinition {
  const problems: string[] = [];
  const providerNames = new Map(catalog.providers.map((p) => [p.slug, p]));

  const endpoints: EndpointDefinition[] = catalog.endpoints.map((e) => {
    const upstream = providerNames.get(e.provider);
    const categories = categoriesFor(e, problems);
    const fixtures: Fixture[] = (e.fixtures ?? []).map((f) => {
      const fx: Fixture = { scenario: f.scenario, input: f.input, response: { status: 200, body: f.response as unknown as Json } };
      const expect: NonNullable<Fixture["expect"]> = {};
      if (f.expectOutput !== undefined) expect.output = f.expectOutput;
      if (f.expectUsage) expect.usage = f.expectUsage;
      if (f.response.status !== "succeeded") expect.error = true;
      if (Object.keys(expect).length) fx.expect = expect;
      return fx;
    });
    const meta: EndpointDefinition["meta"] = {
      summary: e.summary,
      description: `${e.description} Via Monid (${upstream?.name ?? e.provider}); priced at Monid's price plus the router's take, applied once.`,
      exclusions: "Not for the user's own state or memory (use the shogunagent tools).",
      categories,
      notes: [
        `Upstream provider: ${upstream?.name ?? e.provider}${e.verified ? " (verified by Monid)" : ""}.`,
        "Executed through Monid's hosted run API; the Monid credential is held by the router, not by you.",
        ...(e.notes ?? []),
      ],
    };
    const docs = e.docsUrl ?? upstream?.docsUrl;
    if (docs) meta.docsUrl = docs;
    return {
      id: localId(e.id),
      meta,
      request: { method: "POST", path: "/v1/run", timeout_ms: 60_000 },
      input: {
        schema: jsonSchemaInput(e.inputSchema),
        toRequest: ({ input }) => ({ method: "POST", path: "/v1/run", body: { endpoint: e.id, input }, timeout_ms: 60_000 }),
      },
      output: {
        fromResponse: ({ response }) => ((response.body as MonidRunResponse | null)?.output ?? null),
        isFailure: (response) => response.status >= 400 || (response.body as MonidRunResponse | null)?.status !== "succeeded",
      },
      usage: usageFor(e.pricing),
      scope: { class: scopeClassFor(e) },
      health: { p50_ms: e.health.p50Ms, p95_ms: e.health.p95Ms, success_rate: e.health.successRate },
      fixtures,
    };
  });

  if (problems.length) throw new Error(`monid import: ${problems.join("; ")}`);

  return {
    id: "monid",
    name: "Monid (passthrough)",
    first_party: false,
    meta: {
      description: "Third-party tools reached through Monid's hosted catalog. Prices are Monid's plus the router's take, applied once; failures and empty results cost nothing.",
      docsUrl: "https://monid.ai/docs",
      homepage: "https://monid.ai",
    },
    request: { baseUrl: MONID_BASE_URL },
    auth: { kind: "bearer", credential_env: MONID_CREDENTIAL_ENV },
    endpoints,
  };
}
