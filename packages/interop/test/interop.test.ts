import { createHash } from "node:crypto";

import Ajv2020 from "ajv/dist/2020.js";
import { describe, expect, it } from "vitest";

import {
  detectFormat,
  exportV1ToV2,
  exportV2ToV1,
  flatToV2,
  memoriesTreeToV2,
  parseExportV2,
  parseUri,
  serializeExport,
  slug,
  v2ToFlat,
  v2ToMemoriesTree,
  type EntityV2,
  type ExportV1,
  type ExportV2,
  type FactV2,
  type FlatMemoryList,
} from "../src/index.js";
import { EVENTS, fixture, NS, schema } from "./fixture.js";

// ajv is CommonJS; under NodeNext its default import is the constructor at run time.
type Validate = ((v: unknown) => boolean) & { errors?: unknown };
const AjvCtor = Ajv2020 as unknown as new (o: object) => { compile(s: object): Validate };

function validator(name: string) {
  const validate = new AjvCtor({ strict: false, allErrors: true }).compile(schema(name));
  return (records: unknown[]) => {
    for (const record of records) {
      if (!validate(record)) throw new Error(`${name}: ${JSON.stringify(record)}\n${JSON.stringify(validate.errors)}`);
    }
  };
}
const validV2 = (e: ExportV2) => validator("export-v2.schema.json")([e.manifest, ...e.records]);
const validV1 = (e: ExportV1) => validator("export-v1.schema.json")([e.manifest, ...e.records]);

const typed = (e: ExportV2) => e.records.filter((r): r is EntityV2 | FactV2 => r.type === "entity" || r.type === "fact");

describe("export v2", () => {
  it("the fixture is a valid v2 export whose pointers verify against the source bodies", () => {
    const exp = fixture();
    validV2(exp);
    for (const r of typed(exp)) {
      for (const p of r.provenance) {
        const body = EVENTS[p.event_id] ?? "";
        expect(p.content_hash).toBe(`sha256:${createHash("sha256").update(body).digest("hex")}`);
        if (p.quote !== null) expect(body.includes(p.quote)).toBe(true);
      }
    }
    const text = serializeExport(exp);
    expect(detectFormat(text)).toBe("shogunagent-export/v2");
    expect(parseExportV2(text)).toEqual(exp);
    expect(parseUri(`urn:shogunagent:${NS}:open_loop:3`)).toEqual({ ns: NS, kind: "open_loop", id: 3 });
    expect(parseUri("urn:other:1")).toBeNull();
  });

  it("a bad record fails the published schema", () => {
    const exp = fixture();
    const bad = { ...exp.records[3], counterparty: "person 1" };
    expect(() => validator("export-v2.schema.json")([bad])).toThrow();
  });
});

describe("v1 <-> v2", () => {
  it("v2 -> v1 keeps every v1 field and reports relations and supersession as dropped", () => {
    const { export: v1, report } = exportV2ToV1(fixture());
    validV1(v1);
    expect(v1.records).toHaveLength(9);
    expect(report.dropped).toMatchObject({ relation: 1, superseded_by: 1, recorded_at: 1 });
    const back = exportV1ToV2(v1, NS);
    validV2(back.export);
    expect(back.export.manifest.capabilities.relations).toBe(false);
    expect(back.export.manifest.capabilities.provenance.content_hash).toBeNull();
    // The v1 view of the round trip is the v1 export.
    expect(exportV2ToV1(back.export).export.records).toEqual(v1.records);
    expect(back.report.approximated.recorded_at).toBe(8);
  });
});

describe("flat memories list", () => {
  it("v2 -> flat -> v2 keeps typed fields, the first quote and the times; relations, tombstones and extra pointers are reported", () => {
    const source = fixture();
    const { list, report } = v2ToFlat(source);
    expect(list.results).toHaveLength(8);
    expect(list.results.every((m) => typeof m.memory === "string" && typeof m.created_at === "string")).toBe(true);
    expect(report.dropped).toMatchObject({ relation: 1, tombstone: 1, "provenance (pointers after the first)": 1 });

    const { export: rebuilt, report: back } = flatToV2(JSON.parse(JSON.stringify(list)) as FlatMemoryList, { now: 20_000 });
    validV2(rebuilt);
    expect(back.unmapped).toBe(0);
    const key = (r: EntityV2 | FactV2) =>
      JSON.stringify([
        r.uri,
        r.type,
        r.kind,
        r.type === "fact" ? [r.text, r.subtype, r.status, r.due_at, r.counterparty, r.project] : [r.name, r.summary, r.aliases, r.status],
        r.confidence,
        r.valid_from,
        r.valid_to,
        r.recorded_at,
        r.superseded_by,
        r.provenance[0]?.quote ?? null,
        r.provenance[0]?.content_hash ?? null,
      ]);
    expect(typed(rebuilt).map(key).sort()).toEqual(typed(source).map(key).sort());
  });

  it("free-text memories from another product are counted as unmapped, not invented into facts", () => {
    const foreign: FlatMemoryList = {
      results: [
        { id: "m-1", memory: "Prefers dark mode", user_id: "u1", agent_id: null, metadata: {}, created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z" },
      ],
    };
    const { export: exp, report } = flatToV2(foreign, { now: 1 });
    expect(exp.records).toHaveLength(0);
    expect(report.unmapped).toBe(1);
    validV2(exp);
  });
});

describe("/memories tree", () => {
  it("projects only current, world, non-low records, in the layout the engine serves", () => {
    const { tree, report } = v2ToMemoriesTree(fixture());
    expect(Object.keys(tree).sort()).toEqual([
      "/memories/INDEX.md",
      "/memories/commitments.md",
      "/memories/open_loops.md",
      "/memories/people/oren-vale-1.md",
      "/memories/people/tamsin-ekberg-2.md",
      "/memories/projects/heron-rollout-1.md",
    ]);
    expect(tree["/memories/commitments.md"]).toContain("- send the rollout plan by Friday (id 7, high, due 2026-10-18)");
    expect(tree["/memories/commitments.md"]).toContain("  > I'll send the rollout plan by Friday. (event 481)");
    expect(tree["/memories/open_loops.md"]).toContain("- possibly: cost estimate from Tamsin (waiting_on_them, id 3, medium)");
    expect(report.dropped).toMatchObject({ relation: 1, tombstone: 1, "low-confidence record": 1, "closed fact": 1, "superseded record": 1 });
    const index = tree["/memories/INDEX.md"] ?? "";
    expect(index.split("\n").length - 1).toBeLessThanOrEqual(200);
    expect(index).toContain("[Oren Vale](people/oren-vale-1.md)");
  });

  it("tree -> v2 recovers the projected records with quotes; confidence and times are approximated and reported", () => {
    const source = fixture();
    const { tree } = v2ToMemoriesTree(source);
    tree["/memories/notes/todo.md"] = "free text the agent wrote";
    const { export: rebuilt, report } = memoriesTreeToV2(tree, { ns: NS, now: 50_000 });
    validV2(rebuilt);
    expect(report.unmapped).toBe(1);
    expect(report.approximated["confidence (band floor)"]).toBe(2);
    const facts = rebuilt.records.filter((r): r is FactV2 => r.type === "fact");
    expect(facts.map((f) => [f.uri, f.text, f.band, f.due_at, f.provenance[0]?.quote])).toEqual([
      [`urn:shogunagent:${NS}:commitment:7`, "send the rollout plan by Friday", "high", 1_792_281_600_000, "I'll send the rollout plan by Friday."],
      [`urn:shogunagent:${NS}:open_loop:3`, "cost estimate from Tamsin", "medium", null, "Still waiting on Tamsin for the cost estimate."],
    ]);
    const people = rebuilt.records.filter((r): r is EntityV2 => r.type === "entity" && r.kind === "person");
    const oren = people.find((p) => p.id === 1);
    expect([oren?.name, oren?.aliases, oren?.summary, oren?.confidence]).toEqual(["Oren Vale", ["Oren"], "Leads the Heron rollout.", 0.9]);
    const tamsin = people.find((p) => p.id === 2);
    expect([tamsin?.summary, tamsin?.provenance[0]?.quote]).toEqual([null, "Still waiting on Tamsin"]);
  });

  it("slugs match the engine's", () => {
    expect(slug("Oren Vale", 1, "person")).toBe("oren-vale-1");
    expect(slug("  Q3 / Heron!! ", 12, "project")).toBe("q3-heron-12");
    expect(slug("!!!", 5, "person")).toBe("person-5");
  });
});
