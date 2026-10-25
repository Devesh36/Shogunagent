// Export v2 <-> a flat "memories list": the shape memory APIs that store one text string per
// memory return from their list / get-all endpoints (Mem0-style `results[]` with `memory`,
// `metadata`, `created_at`, `updated_at`; supermemory-style document lists are the same idea).
// The shape below is defined here, precisely, so the converter has a fixed target; it is modelled
// on those public descriptions and is not claimed to be any vendor's exact wire format. Map field
// names in a thin adapter when feeding a specific product.

import { FORMAT_V2, URN_PREFIX, type EntityV2, type ExportV2, type FactV2, type LossinessReport, type PointerV2, type RecordV2 } from "./types.js";
import { band, bump, countsOf, emptyReport, fullCapabilities, InteropError, parseUri, uri, validNamespace } from "./export.js";

export const FLAT_FORMAT = "flat-memories-list";

export type FlatScalar = string | number | boolean | null;

/** One memory: a text string plus flat scalar metadata. */
export interface FlatMemory {
  id: string;
  memory: string;
  user_id: string | null;
  agent_id: string | null;
  metadata: Record<string, FlatScalar>;
  /** ISO 8601, UTC. */
  created_at: string;
  /** ISO 8601, UTC. */
  updated_at: string;
}

export interface FlatMemoryList {
  results: FlatMemory[];
}

/** Metadata keys this converter writes (all prefixed, so they never collide with a product's own). */
export const META = {
  type: "shogun_type",
  kind: "shogun_kind",
  subtype: "shogun_subtype",
  status: "shogun_status",
  dueAt: "shogun_due_at",
  confidence: "shogun_confidence",
  band: "shogun_band",
  validFrom: "shogun_valid_from",
  validTo: "shogun_valid_to",
  recordedAt: "shogun_recorded_at",
  supersededBy: "shogun_superseded_by",
  namespace: "shogun_namespace",
  actor: "shogun_actor",
  name: "shogun_name",
  summary: "shogun_summary",
  aliases: "shogun_aliases",
  counterparty: "shogun_counterparty",
  project: "shogun_project",
  quote: "shogun_quote",
  eventUri: "shogun_event_uri",
  observedAt: "shogun_observed_at",
  source: "shogun_source",
  contentHash: "shogun_content_hash",
} as const;

const iso = (ms: number) => new Date(ms).toISOString();

function memoryText(r: EntityV2 | FactV2): string {
  if (r.type === "fact") return r.text;
  const label = r.kind === "person" ? "Person" : "Project";
  return r.summary ? `${label}: ${r.name}. ${r.summary}` : `${label}: ${r.name}`;
}

export interface ToFlatOptions {
  /** `user_id` on every memory; default the export's namespace URN. */
  userId?: string;
}

/**
 * Export v2 -> a flat memories list. Every entity and fact becomes one memory whose `id` is its
 * URI and whose metadata carries the typed fields and the first provenance pointer, so
 * {@link flatToV2} can rebuild it. Relations, tombstones and pointers after the first have no
 * place in the shape and are dropped (counted in the report).
 */
export function v2ToFlat(exp: ExportV2, options: ToFlatOptions = {}): { list: FlatMemoryList; report: LossinessReport } {
  const report = emptyReport(FORMAT_V2, FLAT_FORMAT, exp.records.length);
  const userId = options.userId ?? exp.manifest.namespace;
  const results: FlatMemory[] = [];
  for (const r of exp.records) {
    if (r.type === "relation") {
      bump(report.dropped, "relation");
      continue;
    }
    if (r.type === "tombstone") {
      bump(report.dropped, "tombstone");
      continue;
    }
    const first = r.provenance[0];
    bump(report.dropped, "provenance (pointers after the first)", Math.max(0, r.provenance.length - 1));
    const metadata: Record<string, FlatScalar> = {
      [META.type]: r.type,
      [META.kind]: r.kind,
      [META.status]: r.status,
      [META.confidence]: r.confidence,
      [META.band]: r.band,
      [META.validFrom]: r.valid_from,
      [META.validTo]: r.valid_to,
      [META.recordedAt]: r.recorded_at,
      [META.supersededBy]: r.superseded_by,
      [META.namespace]: r.namespace,
      [META.actor]: r.actor,
      [META.quote]: first?.quote ?? null,
      [META.eventUri]: first?.event_uri ?? null,
      [META.observedAt]: first?.observed_at ?? null,
      [META.source]: first?.source ?? null,
      [META.contentHash]: first?.content_hash ?? null,
    };
    if (r.type === "entity") {
      metadata[META.name] = r.name;
      metadata[META.summary] = r.summary;
      metadata[META.aliases] = JSON.stringify(r.aliases);
    } else {
      metadata[META.subtype] = r.subtype;
      metadata[META.dueAt] = r.due_at;
      metadata[META.counterparty] = r.counterparty;
      metadata[META.project] = r.project;
    }
    const scratch = r.namespace.startsWith("agent_scratch:") ? r.namespace.slice("agent_scratch:".length) : null;
    results.push({
      id: r.uri,
      memory: memoryText(r),
      user_id: userId,
      agent_id: scratch,
      metadata,
      created_at: iso(r.recorded_at),
      updated_at: iso(r.updated_at),
    });
  }
  report.records_out = results.length;
  report.notes.push("typed fields travel as shogun_* metadata scalars; a product that drops metadata keeps only the text");
  return { list: { results }, report };
}

export interface FromFlatOptions {
  /** Namespace id of the resulting export; default the one in the memories' URIs, else `imported`. */
  ns?: string;
  /** Manifest time and the stand-in time for missing timestamps (unix ms). */
  now: number;
}

function num(v: FlatScalar | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: FlatScalar | undefined): string | null {
  return typeof v === "string" ? v : null;
}

/**
 * A flat memories list -> export v2. Memories this converter wrote (with `shogun_type`) are
 * rebuilt as typed entities and facts. A memory without it is a free-text note that has no
 * place in a typed world model: it is counted as `unmapped` and left out (feed such lists to
 * `memory.import` with the `shogun-memory-import/v1` package, which reviews them).
 */
export function flatToV2(list: FlatMemoryList, options: FromFlatOptions): { export: ExportV2; report: LossinessReport } {
  const report = emptyReport(FLAT_FORMAT, FORMAT_V2, list.results.length);
  const typed = list.results.filter((m) => m.metadata[META.type] === "entity" || m.metadata[META.type] === "fact");
  report.unmapped = list.results.length - typed.length;
  if (report.unmapped > 0) report.notes.push(`${report.unmapped} untyped memories left out: free text is not a typed record`);
  const ns = options.ns ?? typed.map((m) => parseUri(m.id)?.ns).find((n) => n !== undefined) ?? "imported";
  if (!validNamespace(ns)) throw new InteropError("bad_namespace", "the namespace id is [0-9a-z-]{1,64}");

  // Ids: keep the URI's id when it is in the target namespace and free; otherwise a new one.
  const used = new Set<string>();
  const remap = new Map<string, string>();
  let next = 1 + Math.max(0, ...typed.map((m) => parseUri(m.id)?.id ?? 0));
  const kindOf = (m: FlatMemory) => str(m.metadata[META.kind]) ?? "";
  for (const m of typed) {
    const parsed = parseUri(m.id);
    const kind = kindOf(m);
    let id = parsed && parsed.ns === ns && parsed.kind === kind && !used.has(`${kind}:${parsed.id}`) ? parsed.id : next++;
    if (used.has(`${kind}:${id}`)) id = next++;
    used.add(`${kind}:${id}`);
    remap.set(m.id, uri(ns, kind, id));
  }
  const ref = (v: FlatScalar | undefined): string | null => {
    const s = str(v);
    if (s === null) return null;
    return remap.get(s) ?? (parseUri(s)?.ns === ns ? s : null);
  };

  const records: RecordV2[] = [];
  for (const m of typed) {
    const md = m.metadata;
    const newUri = remap.get(m.id) ?? m.id;
    const id = parseUri(newUri)?.id ?? 0;
    let confidence = num(md[META.confidence]);
    if (confidence === null) {
      confidence = 0.5;
      bump(report.approximated, "confidence");
    }
    const created = Date.parse(m.created_at);
    const updated = Date.parse(m.updated_at);
    const recordedAt = num(md[META.recordedAt]) ?? (Number.isNaN(created) ? options.now : created);
    let validFrom = num(md[META.validFrom]);
    if (validFrom === null) {
      validFrom = recordedAt;
      bump(report.approximated, "valid_from");
    }
    const pointer: PointerV2[] = [];
    const eventUri = str(md[META.eventUri]);
    const event = parseUri(eventUri);
    if (eventUri !== null && event) {
      pointer.push({
        event_id: event.id,
        event_uri: eventUri,
        quote: str(md[META.quote]),
        observed_at: num(md[META.observedAt]),
        source: str(md[META.source]),
        content_hash: str(md[META.contentHash]),
      });
    } else {
      bump(report.dropped, "provenance");
    }
    const shared = {
      id,
      uri: newUri,
      confidence,
      band: band(confidence),
      valid_from: validFrom,
      valid_to: num(md[META.validTo]),
      recorded_at: recordedAt,
      updated_at: Number.isNaN(updated) ? recordedAt : updated,
      superseded_by: ref(md[META.supersededBy]),
      actor: str(md[META.actor]),
      namespace: str(md[META.namespace]) ?? "world",
      provenance: pointer,
    };
    if (md[META.type] === "entity") {
      const kind = kindOf(m) === "project" ? "project" : "person";
      let aliases: string[] = [];
      try {
        const parsed = JSON.parse(str(md[META.aliases]) ?? "[]") as unknown;
        if (Array.isArray(parsed)) aliases = parsed.filter((a): a is string => typeof a === "string");
      } catch {
        bump(report.dropped, "aliases");
      }
      const status = str(md[META.status]);
      records.push({
        type: "entity",
        kind,
        name: str(md[META.name]) ?? m.memory,
        aliases,
        summary: str(md[META.summary]),
        status: kind === "project" && (status === "active" || status === "waiting" || status === "paused" || status === "done") ? status : null,
        ...shared,
      } satisfies EntityV2);
    } else {
      const kind = kindOf(m) === "open_loop" ? "open_loop" : "commitment";
      const subtype = (str(md[META.subtype]) ?? (kind === "commitment" ? "mine" : "other")) as FactV2["subtype"];
      const status = (str(md[META.status]) ?? "open") as FactV2["status"];
      const counterparty = ref(md[META.counterparty]);
      const project = ref(md[META.project]);
      records.push({
        type: "fact",
        kind,
        subtype,
        text: m.memory,
        status,
        due_at: num(md[META.dueAt]),
        counterparty_id: parseUri(counterparty)?.id ?? null,
        project_id: parseUri(project)?.id ?? null,
        counterparty,
        project,
        ...shared,
      } satisfies FactV2);
    }
  }
  // Entities before facts, as the format orders them.
  records.sort((a, b) => (a.type === b.type ? 0 : a.type === "entity" ? -1 : 1));
  report.records_out = records.length;
  return {
    export: {
      manifest: {
        type: "manifest",
        format: FORMAT_V2,
        schema_version: 1,
        generated_at: options.now,
        since_cursor: null,
        cursor: options.now,
        namespace: `${URN_PREFIX}${ns}`,
        capabilities: { ...fullCapabilities(), record_types: ["entity", "fact"], relations: false, tombstones: false, differential: false },
        counts: countsOf(records),
      },
      records,
    },
    report,
  };
}
