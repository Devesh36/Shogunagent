// E47: export v2 records read as JSON-LD under `docs/api/export-v2.context.jsonld`.
//
// The package has no runtime dependencies and the repository does not pull a JSON-LD processor,
// so this test carries a small expander that implements the JSON-LD 1.1 Expansion Algorithm for
// exactly the context features the export v2 context uses (keyword aliases for `@id` / `@type`,
// `@vocab`, compact IRIs, `@type: @id` / `@json` / a datatype IRI, `@container: @set`, nested
// node objects, `null` dropped). A guard test fails if the context starts using any other
// feature, so the expander never silently disagrees with a full processor. Checking the same
// fixture with a full processor (jsonld.js, pyld) is listed in docs/standards/export-v2-cg-draft.md
// as a test the CG suite should add.
import { describe, expect, it } from "vitest";

import type { FactV2 } from "../src/index.js";
import { fixture, schema } from "./fixture.js";

const VOCAB = "https://shogunagent.dev/ns/export/v2#";
const XSD = "http://www.w3.org/2001/XMLSchema#";

type TermDef = { id: string; type?: string; container?: string };
type Ctx = { vocab: string; terms: Map<string, TermDef> };

const ABSOLUTE = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;

function context(): Ctx {
  const raw = schema("export-v2.context.jsonld")["@context"] as Record<string, unknown>;
  const terms = new Map<string, TermDef>();
  for (const [key, value] of Object.entries(raw)) {
    if (key.startsWith("@")) continue;
    if (typeof value === "string") {
      terms.set(key, { id: value });
      continue;
    }
    const def = value as Record<string, string | undefined>;
    terms.set(key, {
      id: def["@id"] ?? "",
      ...(def["@type"] !== undefined ? { type: def["@type"] } : {}),
      ...(def["@container"] !== undefined ? { container: def["@container"] } : {}),
    });
  }
  return { vocab: raw["@vocab"] as string, terms };
}

/** IRI expansion (JSON-LD 1.1 §5.2): keywords, terms (vocab-relative only), compact IRIs with a
 * defined prefix, absolute IRIs, then `@vocab`. */
function expandIri(ctx: Ctx, value: string, vocab: boolean): string {
  if (value.startsWith("@")) return value;
  const term = vocab ? ctx.terms.get(value) : undefined;
  if (term) return expandIri(ctx, term.id, true);
  const colon = value.indexOf(":");
  if (colon > 0) {
    const prefix = ctx.terms.get(value.slice(0, colon));
    if (prefix && !value.slice(colon + 1).startsWith("//")) return prefix.id + value.slice(colon + 1);
    if (ABSOLUTE.test(value)) return value;
  }
  if (vocab) return ctx.vocab + value;
  throw new Error(`relative IRI ${value} with no base`);
}

type Expanded = Record<string, unknown>;

function expandValue(ctx: Ctx, def: TermDef | undefined, value: unknown): unknown {
  if (def?.type === "@json") return { "@value": value, "@type": "@json" };
  if (value !== null && typeof value === "object" && !Array.isArray(value)) return expandNode(ctx, value as Record<string, unknown>);
  if (def?.type === "@id") return { "@id": expandIri(ctx, value as string, false) };
  if (def?.type) return { "@value": value, "@type": expandIri(ctx, def.type, true) };
  return { "@value": value };
}

function expandNode(ctx: Ctx, node: Record<string, unknown>): Expanded {
  const out: Expanded = {};
  for (const [key, value] of Object.entries(node)) {
    if (value === null) continue;
    const def = ctx.terms.get(key);
    const property = expandIri(ctx, key, true);
    if (property === "@id") {
      out["@id"] = expandIri(ctx, value as string, false);
    } else if (property === "@type") {
      out["@type"] = (Array.isArray(value) ? value : [value]).map((t) => expandIri(ctx, t as string, true));
    } else {
      if (!ABSOLUTE.test(property)) throw new Error(`${key} expands to a non-IRI ${property}`);
      const values = Array.isArray(value) && def?.type !== "@json" ? value : [value];
      out[property] = values.filter((v) => v !== null).map((v) => expandValue(ctx, def, v));
    }
  }
  return out;
}

const records = () => {
  const exp = fixture();
  return [exp.manifest, ...exp.records] as unknown as Record<string, unknown>[];
};

describe("export v2 as JSON-LD", () => {
  it("the context uses only the features this expander implements", () => {
    const raw = schema("export-v2.context.jsonld");
    expect(Object.keys(raw)).toEqual(["@context"]);
    const ctx = raw["@context"] as Record<string, unknown>;
    expect(ctx["@version"]).toBe(1.1);
    for (const [key, value] of Object.entries(ctx)) {
      if (key.startsWith("@")) {
        expect(["@version", "@vocab"]).toContain(key);
        continue;
      }
      if (typeof value === "string") continue;
      const def = value as Record<string, unknown>;
      for (const k of Object.keys(def)) expect(["@id", "@type", "@container"]).toContain(k);
      if (def["@container"] !== undefined) expect(def["@container"]).toBe("@set");
      if (def["@type"] !== undefined) expect(["@id", "@json", "xsd:double"]).toContain(def["@type"]);
    }
  });

  it("every member the schema defines expands to an absolute IRI or a keyword", () => {
    const ctx = context();
    const defs = schema("export-v2.schema.json")["$defs"] as Record<string, { properties?: Record<string, unknown> }>;
    const members = new Set<string>();
    for (const def of Object.values(defs)) for (const name of Object.keys(def.properties ?? {})) members.add(name);
    expect(members.size).toBeGreaterThan(30);
    for (const name of members) {
      const iri = expandIri(ctx, name, true);
      expect(iri === "@id" || iri === "@type" || iri.startsWith(VOCAB), `${name} → ${iri}`).toBe(true);
    }
  });

  it("a v2 package expands without losing a member: ids, types, references, provenance and the manifest", () => {
    const ctx = context();
    for (const record of records()) {
      const node = expandNode(ctx, record);
      const kept = Object.entries(record).filter(([, v]) => v !== null).length;
      expect(Object.keys(node).length, JSON.stringify(record)).toBe(kept);
      expect(node["@type"]).toEqual([VOCAB + (record.type as string)]);
      if (record.type !== "manifest") expect(node["@id"]).toBe(record.uri);
    }

    const [manifest, ...rest] = records().map((r) => expandNode(ctx, r));
    expect(manifest?.[`${VOCAB}capabilities`]).toEqual([{ "@value": fixture().manifest.capabilities, "@type": "@json" }]);

    const fact = fixture().records.find((r): r is FactV2 => r.type === "fact");
    const commitment = rest.find((n) => n["@id"] === fact?.uri);
    expect(commitment?.[`${VOCAB}counterparty`]).toEqual([{ "@id": fact?.counterparty }]);
    expect(commitment?.[`${VOCAB}confidence`]).toEqual([{ "@value": 0.9, "@type": `${XSD}double` }]);
    const provenance = commitment?.[`${VOCAB}provenance`] as Expanded[];
    expect(provenance).toHaveLength(2);
    expect(provenance[0]?.[`${VOCAB}event`]).toEqual([{ "@id": expect.stringMatching(/^urn:shogunagent:[0-9a-z-]+:event:481$/) }]);
    expect(provenance[0]?.[`${VOCAB}quote`]).toEqual([{ "@value": "I'll send the rollout plan by Friday." }]);

    const superseded = rest.find((n) => Array.isArray(n[`${VOCAB}supersededBy`]));
    expect(superseded?.[`${VOCAB}supersededBy`]).toEqual([{ "@id": commitment?.["@id"] }]);

    const relation = rest.find((n) => (n["@type"] as string[])[0] === `${VOCAB}relation`);
    expect(relation?.[`${VOCAB}subject`]).toEqual([{ "@id": expect.stringContaining(":person:1") }]);
    expect(relation?.[`${VOCAB}object`]).toEqual([{ "@id": expect.stringContaining(":project:1") }]);
    // The predicate stays a literal: v2 does not fix a predicate vocabulary (open question 2).
    expect(relation?.[`${VOCAB}predicate`]).toEqual([{ "@value": "works_on" }]);
  });
});
