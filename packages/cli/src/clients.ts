/**
 * Writing the MCP entry into the agent apps found on this machine — a port of the rules in
 * crates/shogun-mcp/src/client_install.rs (ShogunAI's one-click "Add to …"):
 *
 * - A read-modify-write that touches only the `shogunagent` entry. Every other byte of the file
 *   is kept; the result is parsed again and compared with the original, entry by entry, before
 *   it may be written; a file that does not parse is refused, never "repaired".
 * - Idempotent: an entry that is already exactly this is `unchanged` and nothing is written.
 * - Never a secret: the entry runs `mcp` through npx or an explicit local Node/CLI launcher
 *   (the read-only stdio face, src/readonly-proxy.ts), with only data/binary directory paths. The token
 *   and the store key stay in the data directory (0600) and are read by that subcommand.
 * - A plan is built first and shown; applying refuses when the file changed since the plan
 *   (`baseDigest`), writes a backup next to the file, then writes atomically.
 * - Claude Code: the user-scope `~/.claude.json` (top-level `mcpServers`), edited the same way.
 *   ShogunAI drove the vendor CLI instead; here the file edit keeps the CLI dependency-free and
 *   the entry is still exactly what `claude mcp add --scope user` would write.
 */
import { createHash } from "node:crypto";
import { dirname, isAbsolute, resolve } from "node:path";

import { locateBinaries } from "./binaries.js";
import { ALL_CLIENTS, BIN_DIR_ENV, CLIENT_LABEL, DATA_DIR_ENV, clientConfigPath, clientPresenceMarkers, dataPaths, type ClientId } from "./paths.js";
import type { Deps } from "./types.js";

export const ENTRY_NAME = "shogunagent";
export const NPM_PACKAGE = "shogunagent";

export interface Entry {
  command: string;
  args: string[];
  env?: Record<string, string>;
}

/** The default package entry. `skillMinor` is the SKILL.md / package minor (`0.1`). */
export function entryFor(skillMinor: string, dataDir?: string): Entry {
  return {
    command: "npx",
    args: ["-y", `${NPM_PACKAGE}@${skillMinor}`, "mcp"],
    ...(dataDir ? { env: { [DATA_DIR_ENV]: dataDir } } : {}),
  };
}

/** Launch this installed/built CLI directly, using only non-secret path settings. */
export async function localEntryFor(deps: Deps): Promise<Entry> {
  const launcher = deps.platform.localLauncher;
  const paths = dataPaths(deps.platform);
  const absolute = (path: string): boolean => isAbsolute(path) && !/[\u0000-\u001f\u007f]/u.test(path);
  if (!launcher || !absolute(launcher.node) || !absolute(launcher.script)) {
    throw new Error("Local launcher unavailable. Run the built bin/shogunagent.mjs with an absolute Node executable.");
  }
  if (!absolute(paths.root)) throw new Error("Local connection requires an absolute SHOGUNAGENT_DATA_DIR.");
  const configuredBin = deps.platform.env[BIN_DIR_ENV]?.trim();
  if (configuredBin && !absolute(configuredBin)) throw new Error("Local connection requires an absolute SHOGUNAGENT_BIN_DIR.");
  const isFile = async (path: string): Promise<boolean> => (await deps.fs.exists(path)) && !(await deps.fs.isDirectory(path));
  if (!(await isFile(launcher.node)) || !(await isFile(launcher.script)) ||
      !(await isFile(resolve(dirname(launcher.script), "../dist/bin.js")))) {
    throw new Error("Local launcher files are missing. Build the CLI, then run its bin/shogunagent.mjs again.");
  }
  const binaries = await locateBinaries(deps, paths);
  if (!binaries || !absolute(binaries.dir) || !(await isFile(binaries.api)) || !(await isFile(binaries.mcp))) {
    throw new Error("Local connection needs shogun-api and shogun-mcp. Set SHOGUNAGENT_BIN_DIR to their absolute directory or select an installed release.");
  }
  return {
    command: launcher.node,
    args: [launcher.script, "mcp"],
    env: { [DATA_DIR_ENV]: paths.root, [BIN_DIR_ENV]: binaries.dir },
  };
}

export type Change = "add" | "replace" | "unchanged" | "remove" | "absent";
export type Refusal = "unparseable" | "unexpected_shape" | "ambiguous_entry" | "unsupported_entry_form" | "not_verified";

export interface FilePlan {
  client: ClientId;
  path: string;
  change: Change;
  /** SHA-256 of the bytes the plan was made from, or `absent`. */
  baseDigest: string;
  /** The whole new file, or `undefined` when nothing is to be written. */
  newContents?: string;
  /** Other servers in the file, left exactly as they were. */
  otherEntries: number;
  /** Our entry before (masked) and after, for the preview. Never the rest of the file. */
  before?: string | undefined;
  after?: string | undefined;
}

export class PlanRefusal extends Error {
  constructor(readonly refusal: Refusal, readonly client: ClientId, readonly path: string) {
    super(`${CLIENT_LABEL[client]} (${path}): ${REFUSAL_TEXT[refusal]}`);
  }
}

export const REFUSAL_TEXT: Readonly<Record<Refusal, string>> = {
  unparseable: "the file is not valid JSON / TOML; not repairing another app's file. Fix it or remove it, then retry.",
  unexpected_shape: "the file parses but `mcpServers` (JSON) / `mcp_servers` (TOML) is not an object. Not editing it.",
  ambiguous_entry: "the shogunagent entry appears more than once. Remove the duplicates by hand, then retry.",
  unsupported_entry_form: "the shogunagent entry is written as a TOML dotted key or inline table, which this editor does not rewrite. Remove it by hand, then retry.",
  not_verified: "the edited file did not read back as the original with only the shogunagent entry changed. Nothing was written.",
};

export const ABSENT_DIGEST = "absent";

export function digest(existing: string | undefined): string {
  return existing === undefined ? ABSENT_DIGEST : createHash("sha256").update(existing).digest("hex");
}

export const MASK = "••••••";

/** A value that looks like a credential: long, one token, no spaces. Masked in previews. */
export function looksLikeToken(s: string): boolean {
  return /^[A-Za-z0-9_.:+/=-]{20,}$/.test(s) && !s.startsWith("/") && !/^[A-Za-z]:[\\/]/.test(s) && !s.includes("@");
}

function maskEntry(entry: unknown): unknown {
  if (typeof entry !== "object" || entry === null) return entry;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(entry as Record<string, unknown>)) {
    if (k === "env" && typeof v === "object" && v !== null) {
      out[k] = Object.fromEntries(Object.entries(v as Record<string, unknown>).map(([ek, ev]) => [ek, typeof ev === "string" && looksLikeToken(ev) ? MASK : ev]));
    } else if (typeof v === "string" && looksLikeToken(v)) out[k] = MASK;
    else out[k] = v;
  }
  return out;
}

function deepEqual(a: unknown, b: unknown): boolean {
  return JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));
}

function sortKeys(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(sortKeys);
  if (typeof v === "object" && v !== null) return Object.fromEntries(Object.keys(v as object).sort().map((k) => [k, sortKeys((v as Record<string, unknown>)[k])]));
  return v;
}

// --- JSON (Claude Code, Cursor, Claude Desktop): { "mcpServers": { "<name>": { … } } } ---

function detectIndent(text: string): string {
  const m = /\n([ \t]+)"/.exec(text);
  return m?.[1] ?? "  ";
}

export function planJson(client: ClientId, path: string, existing: string | undefined, entry: Entry | null): FilePlan {
  const base = digest(existing);
  let root: Record<string, unknown>;
  if (existing === undefined || existing.trim() === "") root = {};
  else {
    let parsed: unknown;
    try {
      parsed = JSON.parse(existing);
    } catch {
      throw new PlanRefusal("unparseable", client, path);
    }
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new PlanRefusal("unexpected_shape", client, path);
    root = parsed as Record<string, unknown>;
  }
  const serversRaw = root.mcpServers;
  if (serversRaw !== undefined && (typeof serversRaw !== "object" || serversRaw === null || Array.isArray(serversRaw))) throw new PlanRefusal("unexpected_shape", client, path);
  const servers = { ...((serversRaw as Record<string, unknown> | undefined) ?? {}) };
  const current = servers[ENTRY_NAME];
  const otherEntries = Object.keys(servers).filter((k) => k !== ENTRY_NAME).length;
  const before = current === undefined ? undefined : JSON.stringify(maskEntry(current), null, 2);
  if (entry === null) {
    if (current === undefined) return { client, path, change: "absent", baseDigest: base, otherEntries };
    delete servers[ENTRY_NAME];
  } else {
    if (current !== undefined && deepEqual(current, entry)) return { client, path, change: "unchanged", baseDigest: base, otherEntries, before, after: before };
    servers[ENTRY_NAME] = entry;
  }
  const next = { ...root, mcpServers: servers };
  const indent = existing ? detectIndent(existing) : "  ";
  const newContents = JSON.stringify(next, null, indent) + "\n";
  // Verify: the rest of the file reads back unchanged, entry by entry.
  const reread = JSON.parse(newContents) as Record<string, unknown>;
  const rereadServers = (reread.mcpServers ?? {}) as Record<string, unknown>;
  for (const k of Object.keys(root)) if (k !== "mcpServers" && !deepEqual(root[k], reread[k])) throw new PlanRefusal("not_verified", client, path);
  for (const k of Object.keys(servers)) if (k !== ENTRY_NAME && !deepEqual(servers[k], rereadServers[k])) throw new PlanRefusal("not_verified", client, path);
  if (entry !== null && !deepEqual(rereadServers[ENTRY_NAME], entry)) throw new PlanRefusal("not_verified", client, path);
  if (entry === null && ENTRY_NAME in rereadServers) throw new PlanRefusal("not_verified", client, path);
  return {
    client, path, baseDigest: base, otherEntries, newContents,
    change: entry === null ? "remove" : current === undefined ? "add" : "replace",
    ...(before !== undefined ? { before } : {}),
    ...(entry !== null ? { after: JSON.stringify(entry, null, 2) } : {}),
  };
}

// --- TOML (Codex CLI / ChatGPT desktop): [mcp_servers.<name>] tables ---

const HEADER_RE = /^\s*\[\[?\s*([^\]]+?)\s*\]\]?\s*(#.*)?$/;

function headerKey(line: string): string | undefined {
  const m = HEADER_RE.exec(line);
  return m ? (m[1] ?? "").replace(/"/g, "").replace(/\s+/g, "") : undefined;
}

function tomlQuote(s: string): string {
  return JSON.stringify(s);
}

export function renderTomlEntry(entry: Entry): string {
  const lines = [`[mcp_servers.${ENTRY_NAME}]`, `command = ${tomlQuote(entry.command)}`, `args = [${entry.args.map(tomlQuote).join(", ")}]`];
  if (entry.env && Object.keys(entry.env).length > 0) {
    lines.push("", `[mcp_servers.${ENTRY_NAME}.env]`);
    for (const [k, v] of Object.entries(entry.env)) lines.push(`${k} = ${tomlQuote(v)}`);
  }
  return lines.join("\n") + "\n";
}

/** The line ranges of our tables, every other header's key, and whether the file looks like TOML. */
function scanToml(lines: string[]): { ours: Array<[number, number]>; ourHeaderCount: number; otherServers: Set<string>; inlineOurs: boolean; sane: boolean } {
  const ours: Array<[number, number]> = [];
  const otherServers = new Set<string>();
  let ourHeaderCount = 0;
  let inlineOurs = false;
  let sane = true;
  let current: string | undefined;
  let start = -1;
  const close = (end: number): void => {
    if (start >= 0) ours.push([start, end]);
    start = -1;
  };
  lines.forEach((line, i) => {
    const key = headerKey(line);
    if (key !== undefined) {
      close(i);
      current = key;
      if (key === `mcp_servers.${ENTRY_NAME}` || key.startsWith(`mcp_servers.${ENTRY_NAME}.`)) {
        if (key === `mcp_servers.${ENTRY_NAME}`) ourHeaderCount += 1;
        start = i;
      } else if (key.startsWith("mcp_servers.")) {
        otherServers.add(key.split(".")[1] ?? "");
      }
      return;
    }
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#")) return;
    const kv = /^([A-Za-z0-9_."'-]+)\s*=/.exec(trimmed);
    if (!kv) {
      // A continuation of a multi-line array/string is fine inside a table; anything at top level that is not key = value is not TOML we understand.
      if (current === undefined && !/^[\]\}\)",]/.test(trimmed)) sane = false;
      return;
    }
    const k = (kv[1] ?? "").replace(/"/g, "");
    if (current === "mcp_servers" && (k === ENTRY_NAME || k.startsWith(`${ENTRY_NAME}.`))) inlineOurs = true;
    if (current === undefined && k.startsWith(`mcp_servers.${ENTRY_NAME}`)) inlineOurs = true;
    if (current === "mcp_servers" && k !== ENTRY_NAME && !k.includes(".")) otherServers.add(k);
  });
  close(lines.length);
  return { ours, ourHeaderCount, otherServers, inlineOurs, sane };
}

function stripOurs(lines: string[], ours: Array<[number, number]>): string[] {
  const drop = new Set<number>();
  for (const [s, e] of ours) for (let i = s; i < e; i++) drop.add(i);
  return lines.filter((_, i) => !drop.has(i));
}

function normalizeBlank(lines: string[]): string {
  return lines.join("\n").replace(/\n{3,}/g, "\n\n").trim();
}

export function planToml(client: ClientId, path: string, existing: string | undefined, entry: Entry | null): FilePlan {
  const base = digest(existing);
  const text = existing ?? "";
  if (/\u0000/.test(text)) throw new PlanRefusal("unparseable", client, path);
  const lines = text.length === 0 ? [] : text.replace(/\r\n/g, "\n").split("\n");
  const scan = scanToml(lines);
  if (!scan.sane) throw new PlanRefusal("unparseable", client, path);
  if (scan.inlineOurs) throw new PlanRefusal("unsupported_entry_form", client, path);
  if (scan.ourHeaderCount > 1) throw new PlanRefusal("ambiguous_entry", client, path);
  const otherEntries = scan.otherServers.size;
  const currentText = scan.ours.length === 0 ? undefined : normalizeBlank(scan.ours.flatMap(([s, e]) => lines.slice(s, e)));
  const before = currentText === undefined ? undefined : currentText.split("\n").map((l) => {
    const m = /^(\s*[A-Za-z0-9_-]+\s*=\s*)"(.*)"\s*$/.exec(l);
    return m && looksLikeToken(m[2] ?? "") ? `${m[1]}"${MASK}"` : l;
  }).join("\n");
  if (entry === null) {
    if (currentText === undefined) return { client, path, change: "absent", baseDigest: base, otherEntries };
  } else {
    const rendered = renderTomlEntry(entry).trim();
    if (currentText !== undefined && currentText === rendered) return { client, path, change: "unchanged", baseDigest: base, otherEntries, before, after: before };
  }
  const rest = stripOurs(lines, scan.ours);
  let body = rest.join("\n").replace(/\n{3,}/g, "\n\n").replace(/\s+$/, "");
  if (entry !== null) body = (body.length > 0 ? body + "\n\n" : "") + renderTomlEntry(entry).trimEnd();
  const newContents = body.length > 0 ? body + "\n" : "";
  // Verify: strip our tables from both; what remains must be the same text.
  const rescan = scanToml(newContents.length === 0 ? [] : newContents.split("\n"));
  const restAfter = normalizeBlank(stripOurs(newContents.length === 0 ? [] : newContents.split("\n"), rescan.ours));
  if (restAfter !== normalizeBlank(rest)) throw new PlanRefusal("not_verified", client, path);
  if (entry !== null && rescan.ourHeaderCount !== 1) throw new PlanRefusal("not_verified", client, path);
  if (entry === null && rescan.ourHeaderCount !== 0) throw new PlanRefusal("not_verified", client, path);
  return {
    client, path, baseDigest: base, otherEntries, newContents,
    change: entry === null ? "remove" : currentText === undefined ? "add" : "replace",
    ...(before !== undefined ? { before } : {}),
    ...(entry !== null ? { after: renderTomlEntry(entry).trim() } : {}),
  };
}

// --- detection, planning, applying ---

export function formatOf(client: ClientId): "json" | "toml" {
  return client === "codex" ? "toml" : "json";
}

/** The clients present on this machine (a config file or directory exists). */
export async function detectClients(deps: Deps, only?: readonly ClientId[]): Promise<ClientId[]> {
  const out: ClientId[] = [];
  for (const client of only ?? ALL_CLIENTS) {
    for (const marker of clientPresenceMarkers(client, deps.platform)) {
      if (await deps.fs.exists(marker)) {
        out.push(client);
        break;
      }
    }
  }
  return out;
}

export async function planFor(deps: Deps, client: ClientId, entry: Entry | null): Promise<FilePlan> {
  const path = clientConfigPath(client, deps.platform);
  const existing = (await deps.fs.exists(path)) ? await deps.fs.readFile(path) : undefined;
  return formatOf(client) === "json" ? planJson(client, path, existing, entry) : planToml(client, path, existing, entry);
}

export function backupPath(path: string): string {
  return `${path}.bak-shogunagent`;
}

export type ApplyOutcome = "written" | "nothing_to_do" | "file_changed";

/** Write the plan: refuse when the file changed since the plan; back up; write atomically. */
export async function applyPlan(deps: Deps, plan: FilePlan): Promise<ApplyOutcome> {
  if (plan.newContents === undefined) return "nothing_to_do";
  const existing = (await deps.fs.exists(plan.path)) ? await deps.fs.readFile(plan.path) : undefined;
  if (digest(existing) !== plan.baseDigest) return "file_changed";
  if (existing !== undefined) await deps.fs.writeFile(backupPath(plan.path), existing, 0o600);
  await deps.fs.writeFile(plan.path, plan.newContents, 0o600);
  return "written";
}

export function describePlan(plan: FilePlan): string {
  const label = CLIENT_LABEL[plan.client];
  switch (plan.change) {
    case "add": return `${label}: add the shogunagent entry to ${plan.path} (${plan.otherEntries} other server${plan.otherEntries === 1 ? "" : "s"} untouched)`;
    case "replace": return `${label}: replace the shogunagent entry in ${plan.path} (${plan.otherEntries} other server${plan.otherEntries === 1 ? "" : "s"} untouched)`;
    case "unchanged": return `${label}: already connected (${plan.path})`;
    case "remove": return `${label}: remove the shogunagent entry from ${plan.path} (${plan.otherEntries} other server${plan.otherEntries === 1 ? "" : "s"} untouched)`;
    case "absent": return `${label}: no shogunagent entry in ${plan.path}`;
  }
}
