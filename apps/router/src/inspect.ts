/**
 * `inspect(tool_id)` (E8-T4): schema, price, live health, examples, permission meaning and
 * `hints`. Free, no auth.
 */
import { RUN_MARKUP_BPS } from "./take.js";
import type { Json, JsonObject, OpClass } from "../connectors/types.js";
import type { CatalogEntry, CompiledCatalog, PublicUsage } from "./catalog.js";
import type { HealthStore } from "./health.js";
import { cheaperAlternatives, type Hint } from "./hints.js";
import { errorBody, FREE_USAGE, type Envelope, type ErrorBody } from "./responses.js";
import type { Level } from "./scope.js";

export const LEVEL_MEANING: Record<Level, string> = {
  L1: "Runs immediately. Reads only; nothing leaves the user's data.",
  L2: "Writes the user's own data. The first call returns resultType \"input_required\"; confirm with inputResponses.confirm = true to proceed.",
  L3: "Sends, posts, creates or pays. Never auto-executed: the call returns an approval URL a human must open.",
};

export const CLASS_MEANING: Record<OpClass, string> = {
  read: "read",
  user_data_write: "write to the user's own data",
  service_state_change: "reversible change on the provider, delivers nothing",
  external_send: "external send (leaves the user's world)",
  payment: "spends money beyond the call price",
};

export interface InspectResultBody {
  tool: Omit<CatalogEntry, "hash"> & { catalog_entry_hash: string };
  /** `usage` is the provider's price; `quote_micro_usd` is what the caller pays for a typical run:
   * `provider_quote_micro_usd` plus one take (`take_bps`, 0 for first-party tools). */
  price: PublicUsage & { quote_micro_usd: number; provider_quote_micro_usd: number; take_bps: number; failures_cost: 0; empty_results_cost: 0 };
  permission: { class: OpClass; level: Level; meaning: string; class_meaning: string };
  health: { p50_ms: number; p95_ms: number; success_rate: number; source: "observed" | "declared" };
  run_example: { tool_id: string; input: Json; idempotency_key: string; dryRun: true };
}

export function inspect(catalog: CompiledCatalog, health: HealthStore, toolId: string, now: () => number = Date.now): { status: 200; body: Envelope<InspectResultBody> } | { status: 404; body: ErrorBody } {
  const start = now();
  const entry = catalog.get(toolId);
  if (!entry) {
    return { status: 404, body: errorBody("tool_not_found", `no tool "${toolId}" in the catalog; call discover with the task`, "discover", now() - start) };
  }
  const live = health.get(entry.tool_id, entry.health);
  const observed = live !== entry.health;
  const hints: Hint[] = cheaperAlternatives(catalog, entry);
  const { hash, ...tool } = entry;
  const exampleInput: Json = entry.examples[0]?.input ?? emptyExample(entry.input_schema);
  return {
    status: 200,
    body: {
      result: {
        tool: { ...tool, catalog_entry_hash: hash },
        price: { ...entry.usage, quote_micro_usd: entry.quote_micro_usd, provider_quote_micro_usd: entry.provider_quote_micro_usd, take_bps: entry.first_party ? 0 : RUN_MARKUP_BPS, failures_cost: 0, empty_results_cost: 0 },
        permission: { ...entry.scope, meaning: LEVEL_MEANING[entry.scope.level], class_meaning: CLASS_MEANING[entry.scope.class] },
        health: { ...live, source: observed ? "observed" : "declared" },
        run_example: { tool_id: entry.tool_id, input: exampleInput, idempotency_key: "<a uuid you generate per attempt>", dryRun: true },
      },
      usage: FREE_USAGE,
      latency_ms: now() - start,
      hints,
      next_action: null,
    },
  };
}

/** A placeholder input built from the schema's required string fields, for the run example. */
function emptyExample(schema: JsonObject): Json {
  const out: Record<string, Json> = {};
  const props = (schema["properties"] ?? {}) as Record<string, JsonObject>;
  const required = Array.isArray(schema["required"]) ? (schema["required"] as string[]) : [];
  for (const r of required) {
    const t = props[r]?.["type"];
    out[r] = t === "number" || t === "integer" ? 1 : t === "boolean" ? true : t === "array" ? [] : "...";
  }
  return out;
}
