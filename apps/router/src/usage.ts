/**
 * Usage settlement (E8-T5, README §7): what a completed run costs under its usage model.
 * Failures never reach this function (they settle at 0 in src/run.ts); empty results settle at 0
 * here whatever the model says.
 */
import type { EndpointDefinition, Json, RunResponse } from "../connectors/types.js";

export interface Usage {
  unit: string;
  quantity: number;
  cost_micro_usd: number;
}

export const ZERO_USAGE: Usage = { unit: "call", quantity: 0, cost_micro_usd: 0 };

/**
 * Default emptiness: null, "", [], {}, or an object with at least one array field whose every
 * value is null or an empty array (`{ items: [], next_cursor: null }`). Anything with a scalar
 * or a non-empty array is a result and is charged.
 */
export function defaultIsEmpty(output: Json): boolean {
  if (output === null || output === undefined) return true;
  if (Array.isArray(output)) return output.length === 0;
  if (typeof output === "object") {
    const values = Object.values(output);
    if (values.length === 0) return true;
    const hasArray = values.some(Array.isArray);
    return hasArray && values.every((v) => v === null || (Array.isArray(v) && v.length === 0));
  }
  return output === "";
}

function defaultResultCount(output: Json): number {
  if (Array.isArray(output)) return output.length;
  if (output && typeof output === "object") {
    const items = (output as Record<string, Json>)["items"];
    if (Array.isArray(items)) return items.length;
    const arrays = Object.values(output).filter(Array.isArray);
    if (arrays.length === 1) return arrays[0]?.length ?? 0;
  }
  return 1;
}

export function settleUsage(endpoint: EndpointDefinition, ctx: { input: Json; response: RunResponse; output: Json }): Usage {
  const isEmpty = endpoint.output.isEmpty ?? defaultIsEmpty;
  const u = endpoint.usage;
  if (isEmpty(ctx.output)) {
    return { unit: u.model === "per_call" ? "call" : u.model === "per_result" ? "result" : u.unit, quantity: 0, cost_micro_usd: 0 };
  }
  switch (u.model) {
    case "per_call":
      return { unit: "call", quantity: 1, cost_micro_usd: u.cost_micro_usd };
    case "per_result": {
      const n = (u.results ?? defaultResultCount)(ctx.output);
      if (n <= 0) return { unit: "result", quantity: 0, cost_micro_usd: 0 };
      return { unit: "result", quantity: n, cost_micro_usd: (u.base_micro_usd ?? 0) + n * u.cost_micro_usd_per_result };
    }
    case "per_unit": {
      const n = u.units({ input: ctx.input, response: ctx.response });
      if (!(n > 0)) return { unit: u.unit, quantity: 0, cost_micro_usd: 0 };
      return { unit: u.unit, quantity: n, cost_micro_usd: Math.round(n * u.cost_micro_usd_per_unit) };
    }
  }
}
