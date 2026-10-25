// Export v2 <-> a `/memories` directory tree: the file shape of Anthropic's memory tool
// (`memory_20250818`, client-side files under `/memories`) and of Claude Code's `MEMORY.md`. The
// tool defines operations, not a schema, so the layout here is the one ShogunAgent itself
// projects (crates/shogun-core/src/memfs/mod.rs, plan E6):
//
//   /memories/INDEX.md               what the tree holds (<= 200 lines, <= 25 KB)
//   /memories/commitments.md         open commitments, band and proving quote
//   /memories/open_loops.md          open loops, band and proving quote
//   /memories/people/<slug>.md       one per person
//   /memories/projects/<slug>.md     one per project
//
// Only what an agent should read as current is projected: world namespace, not Low, open facts.
// Everything else is counted as dropped. A tree is a set of files: `Record<path, content>`.

import { FORMAT_V2, URN_PREFIX, type Band, type EntityV2, type ExportV2, type FactV2, type LossinessReport, type RecordV2 } from "./types.js";
import { band, bump, countsOf, emptyReport, fullCapabilities, InteropError, uri, validNamespace } from "./export.js";

export const TREE_FORMAT = "memories-tree";
export const ROOT = "/memories";
export const INDEX_MAX_LINES = 200;
export const INDEX_MAX_BYTES = 25_000;
const POSSIBLY = "possibly: ";

export type MemoriesTree = Record<string, string>;

/** The stable file stem: lower-cased name, other characters folded to `-`, then `-<id>`. */
export function slug(name: string, id: number, kind: string): string {
  let stem = "";
  let dash = false;
  for (const c of name.toLowerCase()) {
    if (/[\p{L}\p{N}]/u.test(c)) {
      if (dash && stem !== "") stem += "-";
      dash = false;
      stem += c;
    } else {
      dash = true;
    }
    if ([...stem].length >= 60) break;
  }
  return `${stem === "" ? kind : stem}-${id}`;
}

const oneLine = (text: string) => text.split(/\s+/).filter(Boolean).join(" ");
const prefix = (b: Band) => (b === "medium" ? POSSIBLY : "");
function day(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

function firstQuote(r: EntityV2 | FactV2): { quote: string; event: number } | null {
  const p = r.provenance.find((x) => x.quote !== null);
  return p && p.quote !== null ? { quote: oneLine(p.quote), event: p.event_id } : null;
}

/** Export v2 -> a `/memories` tree. */
export function v2ToMemoriesTree(exp: ExportV2): { tree: MemoriesTree; report: LossinessReport } {
  const report = emptyReport(FORMAT_V2, TREE_FORMAT, exp.records.length);
  const shown: Array<EntityV2 | FactV2> = [];
  for (const r of exp.records) {
    if (r.type === "relation" || r.type === "tombstone") {
      bump(report.dropped, r.type);
      continue;
    }
    if (r.namespace !== "world") {
      bump(report.dropped, "agent_scratch record");
      continue;
    }
    if (r.band === "low") {
      bump(report.dropped, "low-confidence record");
      continue;
    }
    if (r.superseded_by !== null) {
      bump(report.dropped, "superseded record");
      continue;
    }
    if (r.type === "fact" && !(r.status === "open" || r.status === "overdue")) {
      bump(report.dropped, "closed fact");
      continue;
    }
    shown.push(r);
    bump(report.dropped, "provenance (pointers without a quote, hashes, times)", r.provenance.length);
    bump(report.dropped, "timestamps");
    if (r.type === "fact" && (r.counterparty !== null || r.project !== null)) bump(report.dropped, "fact links");
  }
  const tree: MemoriesTree = {};
  const facts = (kind: "commitment" | "open_loop") => shown.filter((r): r is FactV2 => r.type === "fact" && r.kind === kind);

  let commitments = "# Commitments\n\nOpen commitments. Lines starting `possibly:` are Medium confidence: verify before acting.\n";
  const cs = facts("commitment");
  if (cs.length === 0) commitments += "\nNone open.\n";
  for (const c of cs) {
    commitments += `\n- ${prefix(c.band)}${oneLine(c.text)} (id ${c.id}, ${c.band}${c.due_at !== null ? `, due ${day(c.due_at)}` : ""})\n`;
    const q = firstQuote(c);
    if (q) commitments += `  > ${q.quote} (event ${q.event})\n`;
  }
  tree[`${ROOT}/commitments.md`] = commitments;

  let loops = "# Open loops\n\nWhat is waiting or unresolved. Lines starting `possibly:` are Medium confidence.\n";
  const ls = facts("open_loop");
  if (ls.length === 0) loops += "\nNone open.\n";
  for (const l of ls) {
    loops += `\n- ${prefix(l.band)}${oneLine(l.text)} (${l.subtype}, id ${l.id}, ${l.band})\n`;
    const q = firstQuote(l);
    if (q) loops += `  > ${q.quote} (event ${q.event})\n`;
  }
  tree[`${ROOT}/open_loops.md`] = loops;

  const entities = shown.filter((r): r is EntityV2 => r.type === "entity");
  const indexLines = [
    "# Memory index",
    "",
    "Generated from the world model; files outside scratch/ are read-only projections.",
    "",
    `- [Commitments](commitments.md): ${cs.length} open`,
    `- [Open loops](open_loops.md): ${ls.length} open`,
  ];
  for (const [kind, dir, title] of [["person", "people", "People"], ["project", "projects", "Projects"]] as const) {
    indexLines.push("", `## ${title}`);
    const list = entities.filter((e) => e.kind === kind);
    if (list.length === 0) indexLines.push("- none yet");
    for (const e of list) {
      const stem = slug(e.name, e.id, kind);
      indexLines.push(`- [${oneLine(e.name)}](${dir}/${stem}.md)${e.status ? ` ${e.status}` : ""}`);
      let md = `# ${oneLine(e.name)}\n\nConfidence: ${e.band} (${e.confidence.toFixed(2)}).`;
      if (e.status) md += ` Status: ${e.status}.`;
      md += "\n";
      if (e.aliases.length > 0) md += `Also called: ${e.aliases.join(", ")}.\n`;
      if (e.summary && e.summary.trim() !== "") md += `\n${prefix(e.band)}${e.summary.trim()}\n`;
      const quotes = e.provenance.filter((p) => p.quote !== null);
      if (quotes.length > 0) {
        md += "\n## Evidence\n\n";
        for (const p of quotes) md += `> ${oneLine(p.quote ?? "")} (event ${p.event_id})\n`;
      }
      tree[`${ROOT}/${dir}/${stem}.md`] = md;
    }
  }
  let index = indexLines.join("\n") + "\n";
  // The same limits Claude Code applies to MEMORY.md.
  while (index.split("\n").length - 1 > INDEX_MAX_LINES || new TextEncoder().encode(index).length > INDEX_MAX_BYTES) {
    const kept = index.split("\n").slice(0, -2);
    index = kept.join("\n") + "\n";
    bump(report.dropped, "index lines over the limit");
  }
  tree[`${ROOT}/INDEX.md`] = index;
  report.records_out = shown.length;
  report.notes.push("the tree is a read view: confidence becomes a band (and a `possibly:` prefix), times and links are not projected");
  return { tree, report };
}

export interface FromTreeOptions {
  /** Namespace id of the resulting export. */
  ns: string;
  /** Stand-in for every time the tree does not carry (unix ms). */
  now: number;
}

const BAND_FLOOR: Record<Band, number> = { high: 0.8, medium: 0.5, low: 0.0 };
const LOOP_KINDS = new Set(["reply_needed", "waiting_on_them", "review_pending", "decision_pending", "follow_up", "other"]);

function parseBand(raw: string | undefined): Band | null {
  return raw === "high" || raw === "medium" || raw === "low" ? raw : null;
}

/**
 * A `/memories` tree -> export v2. Reads the files this layout defines; any other file (an
 * agent's own notes, scratch files) is free text and counted as `unmapped`. Confidence comes
 * from an entity file's `(0.90)` or, for list lines, the band's floor (approximated); every
 * time is `now` (approximated); quotes come back with their event id.
 */
export function memoriesTreeToV2(tree: MemoriesTree, options: FromTreeOptions): { export: ExportV2; report: LossinessReport } {
  if (!validNamespace(options.ns)) throw new InteropError("bad_namespace", "the namespace id is [0-9a-z-]{1,64}");
  const ns = options.ns;
  const paths = Object.keys(tree).sort();
  const report = emptyReport(TREE_FORMAT, FORMAT_V2, paths.length);
  const records: RecordV2[] = [];
  const times = { valid_from: options.now, valid_to: null, recorded_at: options.now, updated_at: options.now };
  const pointer = (event: number, quote: string) => ({
    event_id: event,
    event_uri: uri(ns, "event", event),
    quote,
    observed_at: null,
    source: null,
    content_hash: null,
  });

  const listFile = (path: string, kind: "commitment" | "open_loop") => {
    const text = tree[path];
    if (text === undefined) return;
    const lines = text.split("\n");
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i] ?? "";
      const m =
        kind === "commitment"
          ? /^- (.*) \(id (-?\d+), (high|medium|low)(?:, due (\d{4}-\d{2}-\d{2}))?\)$/.exec(line)
          : /^- (.*) \(([a-z_]+), id (-?\d+), (high|medium|low)\)$/.exec(line);
      if (!m) continue;
      const rawText = m[1] ?? "";
      const id = Number(kind === "commitment" ? m[2] : m[3]);
      const b = parseBand(kind === "commitment" ? m[3] : m[4]) ?? "medium";
      const due = kind === "commitment" && m[4] ? Date.parse(`${m[4]}T00:00:00Z`) : null;
      const subtypeRaw = kind === "open_loop" ? (m[2] ?? "other") : "mine";
      const quoteLine = /^ {2}> (.*) \(event (-?\d+)\)$/.exec(lines[i + 1] ?? "");
      const textBody = rawText.startsWith(POSSIBLY) ? rawText.slice(POSSIBLY.length) : rawText;
      bump(report.approximated, "confidence (band floor)");
      bump(report.approximated, "times");
      if (kind === "commitment") bump(report.approximated, "subtype (mine/theirs unknown)");
      const confidence = BAND_FLOOR[b];
      records.push({
        type: "fact",
        kind,
        id,
        uri: uri(ns, kind, id),
        subtype: (kind === "open_loop" && LOOP_KINDS.has(subtypeRaw) ? subtypeRaw : kind === "open_loop" ? "other" : "mine") as FactV2["subtype"],
        text: textBody,
        status: "open",
        due_at: due,
        counterparty_id: null,
        project_id: null,
        counterparty: null,
        project: null,
        confidence,
        band: band(confidence),
        ...times,
        superseded_by: null,
        actor: null,
        namespace: "world",
        provenance: quoteLine ? [pointer(Number(quoteLine[2]), quoteLine[1] ?? "")] : [],
      });
    }
  };

  let mapped = 0;
  for (const path of paths) {
    const text = tree[path] ?? "";
    if (path === `${ROOT}/INDEX.md`) {
      mapped++;
      continue;
    }
    if (path === `${ROOT}/commitments.md`) {
      listFile(path, "commitment");
      mapped++;
      continue;
    }
    if (path === `${ROOT}/open_loops.md`) {
      listFile(path, "open_loop");
      mapped++;
      continue;
    }
    const entity = /^\/memories\/(people|projects)\/(.+)-(-?\d+)\.md$/.exec(path);
    const title = /^# (.+)$/m.exec(text);
    const conf = /^Confidence: (high|medium|low) \((\d(?:\.\d+)?)\)\.(?: Status: ([a-z]+)\.)?$/m.exec(text);
    if (!entity || !title || !conf) {
      report.unmapped++;
      continue;
    }
    mapped++;
    const kind = entity[1] === "people" ? "person" : "project";
    const id = Number(entity[3]);
    const confidence = Number(conf[2]);
    const aliases = /^Also called: (.*)\.$/m.exec(text)?.[1]?.split(", ").filter(Boolean) ?? [];
    // The summary is the first paragraph after the header block and before any `##` section.
    const body = text.split("\n## ")[0] ?? "";
    const paragraphs = body.split("\n\n").slice(2).map((p) => p.trim()).filter((p) => p !== "" && !p.startsWith("Also called:"));
    const summaryRaw = paragraphs[0] ?? null;
    const summary = summaryRaw && summaryRaw.startsWith(POSSIBLY) ? summaryRaw.slice(POSSIBLY.length) : summaryRaw;
    const evidence = [...text.matchAll(/^> (.*) \(event (-?\d+)\)$/gm)].map((m) => pointer(Number(m[2]), m[1] ?? ""));
    const status = conf[3];
    bump(report.approximated, "times");
    records.push({
      type: "entity",
      kind,
      id,
      uri: uri(ns, kind, id),
      name: title[1] ?? "",
      aliases,
      summary,
      status: kind === "project" && (status === "active" || status === "waiting" || status === "paused" || status === "done") ? status : null,
      confidence,
      band: band(confidence),
      ...times,
      superseded_by: null,
      actor: null,
      namespace: "world",
      provenance: evidence,
    } satisfies EntityV2);
  }
  if (report.unmapped > 0) report.notes.push(`${report.unmapped} files are not part of this layout (free-text notes) and were left out`);
  records.sort((a, b) => (a.type === b.type ? 0 : a.type === "entity" ? -1 : 1));
  report.records_out = records.length;
  report.notes.push(`${mapped} files read`);
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
        capabilities: {
          ...fullCapabilities(),
          record_types: ["entity", "fact"],
          bitemporal: { valid_time: false, transaction_time: false },
          relations: false,
          supersession: false,
          provenance: { quotes: true, content_hash: null, observed_at: false },
          tombstones: false,
          differential: false,
        },
        counts: countsOf(records),
      },
      records,
    },
    report,
  };
}
