/**
 * A small JSON Schema validator for endpoints imported from a catalog JSON (src/passthrough/monid.ts),
 * where the input schema arrives as data rather than as zod. Covers the subset `z.toJSONSchema`
 * emits for connector inputs: object / properties / required / additionalProperties, string
 * (minLength, maxLength, enum, pattern), number / integer (minimum, maximum), boolean, array
 * (items, minItems, maxItems), anyOf, const, nullable via type arrays. Anything else validates as
 * `true` and is reported in `notes` by the importer, never silently enforced.
 */
import type { InputSpec, Json, JsonObject } from "../connectors/types.js";

type Schema = JsonObject;

function typeOf(v: unknown): string {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  if (typeof v === "number") return Number.isInteger(v) ? "integer" : "number";
  return typeof v;
}

function matchesType(declared: Json | undefined, v: unknown): boolean {
  if (declared === undefined) return true;
  const types = Array.isArray(declared) ? declared : [declared];
  const actual = typeOf(v);
  return types.some((t) => t === actual || (t === "number" && actual === "integer"));
}

export function validateAgainst(schema: Schema, value: unknown, path: string, errors: string[]): void {
  if (!matchesType(schema["type"], value)) {
    errors.push(`${path}: expected ${JSON.stringify(schema["type"])}, got ${typeOf(value)}`);
    return;
  }
  if ("const" in schema && JSON.stringify(schema["const"]) !== JSON.stringify(value)) {
    errors.push(`${path}: must equal ${JSON.stringify(schema["const"])}`);
  }
  const enumValues = schema["enum"];
  if (Array.isArray(enumValues) && !enumValues.some((e) => JSON.stringify(e) === JSON.stringify(value))) {
    errors.push(`${path}: must be one of ${enumValues.map((e) => JSON.stringify(e)).join(", ")}`);
  }
  const anyOf = schema["anyOf"];
  if (Array.isArray(anyOf)) {
    const ok = anyOf.some((s) => {
      const sub: string[] = [];
      validateAgainst(s as Schema, value, path, sub);
      return sub.length === 0;
    });
    if (!ok) errors.push(`${path}: matches none of the allowed shapes`);
  }
  if (typeof value === "string") {
    const min = schema["minLength"];
    const max = schema["maxLength"];
    if (typeof min === "number" && value.length < min) errors.push(`${path}: shorter than ${min}`);
    if (typeof max === "number" && value.length > max) errors.push(`${path}: longer than ${max}`);
    const pattern = schema["pattern"];
    if (typeof pattern === "string" && !new RegExp(pattern).test(value)) errors.push(`${path}: does not match ${pattern}`);
  }
  if (typeof value === "number") {
    const min = schema["minimum"];
    const max = schema["maximum"];
    if (typeof min === "number" && value < min) errors.push(`${path}: below minimum ${min}`);
    if (typeof max === "number" && value > max) errors.push(`${path}: above maximum ${max}`);
  }
  if (Array.isArray(value)) {
    const min = schema["minItems"];
    const max = schema["maxItems"];
    if (typeof min === "number" && value.length < min) errors.push(`${path}: fewer than ${min} items`);
    if (typeof max === "number" && value.length > max) errors.push(`${path}: more than ${max} items`);
    const items = schema["items"];
    if (items && typeof items === "object" && !Array.isArray(items)) {
      value.forEach((item, i) => validateAgainst(items as Schema, item, `${path}[${i}]`, errors));
    }
  }
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    const obj = value as Record<string, unknown>;
    const props = (schema["properties"] ?? {}) as Record<string, Json>;
    const required = Array.isArray(schema["required"]) ? (schema["required"] as string[]) : [];
    for (const r of required) if (!(r in obj)) errors.push(`${path}.${r}: required`);
    for (const [k, v] of Object.entries(obj)) {
      const sub = props[k];
      if (sub && typeof sub === "object" && !Array.isArray(sub)) {
        validateAgainst(sub as Schema, v, `${path}.${k}`, errors);
      } else if (schema["additionalProperties"] === false) {
        errors.push(`${path}.${k}: unknown field`);
      }
    }
  }
}

export function jsonSchemaInput(schema: JsonObject): InputSpec {
  return {
    jsonSchema: schema,
    validate(input: unknown) {
      const errors: string[] = [];
      validateAgainst(schema, input, "$", errors);
      return errors.length === 0 ? { ok: true, value: input as Json } : { ok: false, errors };
    },
  };
}
