/**
 * Helpers connector authors use (connectors/schema.md §2). `defineProvider` / `defineEndpoint`
 * are typed identities; `zodInput` turns a zod schema into the `InputSpec` the engine validates
 * with and the JSON Schema `inspect` shows. Only constraints `z.toJSONSchema` can express are
 * enforced (as in Monid: put cross-field rules in `meta.notes`).
 */
import { z } from "zod";
import type { EndpointDefinition, InputSpec, Json, JsonObject, ProviderDefinition } from "./types.js";

export function defineProvider(def: ProviderDefinition): ProviderDefinition {
  return def;
}

export function defineEndpoint(def: EndpointDefinition): EndpointDefinition {
  return def;
}

export function zodInput(schema: z.ZodType): InputSpec {
  const jsonSchema = z.toJSONSchema(schema, { unrepresentable: "any" }) as JsonObject;
  delete jsonSchema["$schema"];
  return {
    jsonSchema,
    validate(input: unknown) {
      const parsed = schema.safeParse(input);
      if (parsed.success) return { ok: true, value: parsed.data as Json };
      return {
        ok: false,
        errors: parsed.error.issues.map((i) => `${i.path.length ? i.path.join(".") : "$"}: ${i.message}`),
      };
    },
  };
}

/** Re-exported so a connector file imports one module. */
export { z };
