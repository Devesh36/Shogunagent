// Reading and writing export JSON Lines, URIs, bands, and the v1 <-> v2 conversion. Mirrors
// crates/shogun-memory/src/export/convert.rs; the Rust side is the reference.

import {
  FORMAT_V1,
  FORMAT_V2,
  URN_PREFIX,
  type Band,
  type Capabilities,
  type EntityV1,
  type EntityV2,
  type ExportV1,
  type ExportV2,
  type FactV1,
  type FactV2,
  type LossinessReport,
  type PointerV2,
  type RecordV1,
  type RecordV2,
  type TombstoneV2,
} from "./types.js";

export class InteropError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
    this.name = "InteropError";
  }
}

/** The confidence band, as every ShogunAgent face computes it. */
export function band(confidence: number): Band {
  if (confidence >= 0.8) return "high";
  if (confidence >= 0.5) return "medium";
  return "low";
}

/** `urn:shogunagent:<ns>:<kind>:<id>`. */
export function uri(ns: string, kind: string, id: number): string {
  return `${URN_PREFIX}${ns}:${kind}:${id}`;
}

/** `{ns, kind, id}` of a v2 URI, or null when it is not one. */
export function parseUri(raw: string | null | undefined): { ns: string; kind: string; id: number } | null {
  if (typeof raw !== "string" || !raw.startsWith(URN_PREFIX)) return null;
  const parts = raw.slice(URN_PREFIX.length).split(":");
  if (parts.length !== 3) return null;
  const [ns, kind, id] = parts as [string, string, string];
  const n = Number(id);
  if (!ns || !kind || !Number.isInteger(n)) return null;
  return { ns, kind, id: n };
}

export function validNamespace(ns: string): boolean {
  return /^[0-9a-z-]{1,64}$/.test(ns);
}

const TABLE_TO_KIND: Record<string, string> = {
  people: "person",
  projects: "project",
  commitments: "commitment",
  open_loops: "open_loop",
};

function lines(text: string): unknown[] {
  return text
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line, n) => {
      try {
        return JSON.parse(line) as unknown;
      } catch {
        throw new InteropError("invalid_jsonl", `line ${n + 1} is not JSON`);
      }
    });
}

function formatOf(first: unknown): string | null {
  if (first && typeof first === "object" && (first as { type?: unknown }).type === "manifest") {
    const format = (first as { format?: unknown }).format;
    return typeof format === "string" ? format : null;
  }
  return null;
}

/** The format named by the manifest line of an export text, or null. */
export function detectFormat(text: string): typeof FORMAT_V1 | typeof FORMAT_V2 | null {
  const first = text.split("\n").find((line) => line.trim() !== "");
  if (first === undefined) return null;
  try {
    const format = formatOf(JSON.parse(first));
    return format === FORMAT_V1 || format === FORMAT_V2 ? format : null;
  } catch {
    return null;
  }
}

export function parseExportV2(text: string): ExportV2 {
  const all = lines(text);
  if (formatOf(all[0]) !== FORMAT_V2) throw new InteropError("wrong_format", `the first line is not a ${FORMAT_V2} manifest`);
  return { manifest: all[0] as ExportV2["manifest"], records: all.slice(1) as RecordV2[] };
}

export function parseExportV1(text: string): ExportV1 {
  const all = lines(text);
  if (formatOf(all[0]) !== FORMAT_V1) throw new InteropError("wrong_format", `the first line is not a ${FORMAT_V1} manifest`);
  return { manifest: all[0] as ExportV1["manifest"], records: all.slice(1) as RecordV1[] };
}

/** JSON Lines text: the manifest, then every record, one compact object per line. */
export function serializeExport(exp: ExportV1 | ExportV2): string {
  return [exp.manifest, ...exp.records].map((r) => JSON.stringify(r)).join("\n") + "\n";
}

/** The namespace id (`<ns>`) of a v2 manifest. */
export function namespaceOf(exp: ExportV2): string {
  return exp.manifest.namespace.slice(URN_PREFIX.length);
}

export function emptyReport(from: string, to: string, recordsIn: number): LossinessReport {
  return { from, to, records_in: recordsIn, records_out: 0, dropped: {}, approximated: {}, unmapped: 0, notes: [] };
}

export function bump(map: Record<string, number>, key: string, by = 1): void {
  if (by > 0) map[key] = (map[key] ?? 0) + by;
}

/** The capabilities of a producer that models everything v2 can carry. */
export function fullCapabilities(): Capabilities {
  return {
    record_types: ["entity", "fact", "relation", "tombstone"],
    entity_kinds: ["person", "project"],
    fact_kinds: ["commitment", "open_loop"],
    global_ids: "urn",
    bitemporal: { valid_time: true, transaction_time: true },
    relations: true,
    supersession: true,
    provenance: { quotes: true, content_hash: "sha256", observed_at: true },
    confidence: { scale: "0..1", bands: { high: 0.8, medium: 0.5 } },
    namespaces: ["world", "agent_scratch:*"],
    tombstones: true,
    differential: true,
  };
}

export function countsOf(records: RecordV2[]): ExportV2["manifest"]["counts"] {
  const n = (t: string) => records.filter((r) => r.type === t).length;
  return { entities: n("entity"), facts: n("fact"), relations: n("relation"), tombstones: n("tombstone") };
}

/** Export v2 -> v1: every v1 field unchanged; relations, supersession, record time and pointer additions dropped. */
export function exportV2ToV1(exp: ExportV2): { export: ExportV1; report: LossinessReport } {
  const report = emptyReport(FORMAT_V2, FORMAT_V1, exp.records.length);
  const records: RecordV1[] = [];
  for (const r of exp.records) {
    if (r.type === "relation") {
      bump(report.dropped, "relation");
      continue;
    }
    if (r.type === "tombstone") {
      const { uri: _uri, ...rest } = r;
      records.push(rest);
      continue;
    }
    if (r.superseded_by !== null) bump(report.dropped, "superseded_by");
    if (r.recorded_at !== r.valid_from) bump(report.dropped, "recorded_at");
    bump(report.dropped, "provenance.content_hash", r.provenance.filter((p) => p.content_hash !== null).length);
    const provenance = r.provenance.map((p) => ({ event_id: p.event_id, quote: p.quote }));
    if (r.type === "entity") {
      const { uri: _u, recorded_at: _r, superseded_by: _s, ...rest } = r;
      records.push({ ...rest, provenance } satisfies EntityV1);
    } else {
      const { uri: _u, recorded_at: _r, superseded_by: _s, counterparty: _c, project: _p, ...rest } = r;
      records.push({ ...rest, provenance } satisfies FactV1);
    }
  }
  report.records_out = records.length;
  const n = (t: string) => records.filter((r) => r.type === t).length;
  const m = exp.manifest;
  return {
    export: {
      manifest: {
        type: "manifest",
        format: FORMAT_V1,
        schema_version: m.schema_version,
        generated_at: m.generated_at,
        since_cursor: m.since_cursor,
        cursor: m.cursor,
        counts: { entities: n("entity"), facts: n("fact"), tombstones: n("tombstone") },
      },
      records,
    },
    report,
  };
}

function pointerV2(ns: string, p: { event_id: number; quote: string | null }): PointerV2 {
  return { event_id: p.event_id, event_uri: uri(ns, "event", p.event_id), quote: p.quote, observed_at: null, source: null, content_hash: null };
}

/** Export v1 -> v2 under namespace id `ns`. Nothing is lost; what v1 cannot know is null and declared. */
export function exportV1ToV2(exp: ExportV1, ns: string): { export: ExportV2; report: LossinessReport } {
  if (!validNamespace(ns)) throw new InteropError("bad_namespace", "the namespace id is [0-9a-z-]{1,64}");
  const report = emptyReport(FORMAT_V1, FORMAT_V2, exp.records.length);
  const records: RecordV2[] = [];
  const ref = (kind: string, id: number | null) => (id === null ? null : uri(ns, kind, id));
  for (const r of exp.records) {
    if (r.type === "tombstone") {
      records.push({ ...r, uri: uri(ns, TABLE_TO_KIND[r.kind] ?? r.kind, r.id) } satisfies TombstoneV2);
      continue;
    }
    bump(report.approximated, "recorded_at");
    const extra = { uri: uri(ns, r.kind, r.id), recorded_at: r.valid_from, superseded_by: null, provenance: r.provenance.map((p) => pointerV2(ns, p)) };
    if (r.type === "entity") {
      records.push({ ...r, ...extra } satisfies EntityV2);
    } else {
      records.push({ ...r, ...extra, counterparty: ref("person", r.counterparty_id), project: ref("project", r.project_id) } satisfies FactV2);
    }
  }
  report.records_out = records.length;
  report.notes.push("v1 has no relations, supersession links or source hashes; the manifest declares them unsupported");
  const capabilities: Capabilities = {
    ...fullCapabilities(),
    record_types: ["entity", "fact", "tombstone"],
    bitemporal: { valid_time: true, transaction_time: false },
    relations: false,
    supersession: false,
    provenance: { quotes: true, content_hash: null, observed_at: false },
  };
  const m = exp.manifest;
  return {
    export: {
      manifest: {
        type: "manifest",
        format: FORMAT_V2,
        schema_version: m.schema_version,
        generated_at: m.generated_at,
        since_cursor: m.since_cursor,
        cursor: m.cursor,
        namespace: `${URN_PREFIX}${ns}`,
        capabilities,
        counts: countsOf(records),
      },
      records,
    },
    report,
  };
}
