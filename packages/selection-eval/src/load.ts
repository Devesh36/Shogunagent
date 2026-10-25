/**
 * Loading tasks, surfaces and SKILL.md. Surfaces may pull a candidate's description from
 * SKILL.md at load time (`description_from: "SKILL.md:description"`) so the Claude Code plugin
 * surface tracks the real skill text: an edit to SKILL.md moves the number, which is the point.
 */
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { AdoptionTask, Candidate, Surface, Task } from "./types.js";

export const PACKAGE_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const REPO_ROOT = resolve(PACKAGE_ROOT, "..", "..");

const CATEGORIES = new Set(["state", "memory", "web", "approval", "none"]);

function readJson(path: string): unknown {
  return JSON.parse(readFileSync(path, "utf8")) as unknown;
}

function fail(path: string, msg: string): never {
  throw new Error(`${path}: ${msg}`);
}

export function loadTasks(path: string = join(PACKAGE_ROOT, "tasks", "tasks.en.json")): Task[] {
  const raw = readJson(path);
  if (!Array.isArray(raw)) fail(path, "expected an array of tasks");
  const ids = new Set<string>();
  return raw.map((t, i) => {
    const r = t as Record<string, unknown>;
    const id = r["id"];
    const prompt = r["prompt"];
    const cat = r["expected_category"];
    const should = r["should_pick_shogunagent"];
    if (typeof id !== "string" || !id) fail(path, `task ${i}: id must be a non-empty string`);
    if (ids.has(id)) fail(path, `duplicate task id ${id}`);
    ids.add(id);
    if (typeof prompt !== "string" || !prompt.trim()) fail(path, `task ${id}: prompt must be a non-empty string`);
    if (typeof cat !== "string" || !CATEGORIES.has(cat)) fail(path, `task ${id}: expected_category must be one of ${[...CATEGORIES].join("|")}`);
    if (typeof should !== "boolean") fail(path, `task ${id}: should_pick_shogunagent must be boolean`);
    return { id, prompt, expected_category: cat as Task["expected_category"], should_pick_shogunagent: should };
  });
}

export function loadAdoptionTasks(path: string = join(PACKAGE_ROOT, "tasks", "adoption.en.json")): AdoptionTask[] {
  const raw = readJson(path);
  if (!Array.isArray(raw)) fail(path, "expected an array of adoption tasks");
  const ids = new Set<string>();
  return raw.map((t, i) => {
    const r = t as Record<string, unknown>;
    const id = r["id"];
    const scenario = r["scenario"];
    const should = r["should_propose"];
    if (typeof id !== "string" || !id) fail(path, `task ${i}: id must be a non-empty string`);
    if (ids.has(id)) fail(path, `duplicate task id ${id}`);
    ids.add(id);
    if (typeof scenario !== "string" || !scenario.trim()) fail(path, `task ${id}: scenario must be a non-empty string`);
    if (typeof should !== "boolean") fail(path, `task ${id}: should_propose must be boolean`);
    return { id, scenario, should_propose: should };
  });
}

/**
 * SKILL.md: SELECTION_EVAL_SKILL_PATH, then the repo root, then the snapshot in fixtures/ (a
 * verbatim copy kept so the package runs in a checkout where the root skill has not landed yet;
 * the run output names which file was used).
 */
export function skillPath(env: NodeJS.ProcessEnv = process.env): string {
  const candidates = [env["SELECTION_EVAL_SKILL_PATH"], join(REPO_ROOT, "SKILL.md"), join(PACKAGE_ROOT, "fixtures", "SKILL.snapshot.md")].filter(
    (p): p is string => !!p,
  );
  for (const p of candidates) if (existsSync(p)) return p;
  throw new Error(
    `SKILL.md not found (looked at ${candidates.join(", ")}). Set SELECTION_EVAL_SKILL_PATH to the skill file; the adoption eval and the claude-code-plugin surface read it.`,
  );
}

export interface Skill {
  path: string;
  text: string;
  frontmatter: Record<string, string>;
  /** Body of "## When to use this skill" up to "Do not use it for:". */
  triggerText: string;
  /** The "Do not use it for:" bullets. */
  nonTriggerText: string;
}

export function parseSkill(text: string, path = "SKILL.md"): Skill {
  const fm: Record<string, string> = {};
  const m = /^---\n([\s\S]*?)\n---\n/.exec(text);
  if (m && m[1] !== undefined) {
    const lines = m[1].split("\n");
    let key: string | null = null;
    for (const line of lines) {
      const kv = /^([A-Za-z_][\w-]*):\s*(.*)$/.exec(line);
      if (kv && kv[1] !== undefined) {
        key = kv[1];
        const v = (kv[2] ?? "").trim();
        fm[key] = v === ">-" || v === ">" || v === "|" ? "" : v.replace(/^["']|["']$/g, "");
      } else if (key && /^\s+/.test(line)) {
        fm[key] = `${fm[key] ?? ""}${fm[key] ? " " : ""}${line.trim()}`;
      }
    }
  }
  const whenIdx = text.indexOf("## When to use this skill");
  const notIdx = text.indexOf("Do not use it for:", whenIdx);
  const nextH2 = whenIdx === -1 ? -1 : text.indexOf("\n## ", whenIdx + 1);
  const end = nextH2 === -1 ? text.length : nextH2;
  const triggerText = whenIdx === -1 ? "" : text.slice(whenIdx, notIdx === -1 ? end : notIdx);
  const nonTriggerText = notIdx === -1 ? "" : text.slice(notIdx, end);
  if (!fm["description"]) throw new Error(`${path}: frontmatter has no description`);
  return { path, text, frontmatter: fm, triggerText, nonTriggerText };
}

export function loadSkill(env: NodeJS.ProcessEnv = process.env): Skill {
  const p = skillPath(env);
  return parseSkill(readFileSync(p, "utf8"), p);
}

interface RawCandidate extends Partial<Candidate> {
  description_from?: string;
}

function resolveDescription(c: RawCandidate, skill: Skill | null, where: string): string {
  if (c.description_from) {
    const [file, field] = c.description_from.split(":");
    if (file !== "SKILL.md" || !field) fail(where, `unsupported description_from "${c.description_from}" (only SKILL.md:<frontmatter-field>)`);
    if (!skill) fail(where, `description_from needs SKILL.md but it was not loaded`);
    const v = skill.frontmatter[field];
    if (!v) fail(where, `SKILL.md frontmatter has no "${field}"`);
    return v;
  }
  if (typeof c.description !== "string" || !c.description.trim()) fail(where, "description must be a non-empty string");
  return c.description;
}

export function parseSurface(raw: unknown, path: string, skill: Skill | null): Surface {
  const r = raw as Record<string, unknown>;
  const id = r["id"];
  const title = r["title"];
  const framing = r["framing"];
  const cands = r["candidates"];
  if (typeof id !== "string" || !id) fail(path, "surface id must be a non-empty string");
  if (typeof title !== "string" || !title) fail(path, "surface title must be a non-empty string");
  if (typeof framing !== "string" || !framing) fail(path, "surface framing must be a non-empty string");
  if (!Array.isArray(cands) || cands.length === 0) fail(path, "candidates must be a non-empty array");
  const names = new Set<string>();
  let hasShogun = false;
  const candidates: Candidate[] = cands.map((c, i) => {
    const rc = c as RawCandidate;
    const where = `${path} candidates[${i}]`;
    if (typeof rc.vendor !== "string" || !rc.vendor) fail(where, "vendor must be a non-empty string");
    if (typeof rc.tool_name !== "string" || !rc.tool_name) fail(where, "tool_name must be a non-empty string");
    const key = `${rc.vendor}__${rc.tool_name}`;
    if (names.has(key)) fail(where, `duplicate candidate ${key}`);
    names.add(key);
    if (rc.vendor === "shogunagent") hasShogun = true;
    const out: Candidate = { vendor: rc.vendor, tool_name: rc.tool_name, description: resolveDescription(rc, skill, where) };
    if (rc.annotations) out.annotations = rc.annotations;
    if (rc.source) out.source = rc.source;
    if (typeof rc.verbatim === "boolean") out.verbatim = rc.verbatim;
    return out;
  });
  if (!hasShogun) fail(path, "surface lists no shogunagent candidate");
  return { id, title, framing, candidates };
}

export function loadSurfaces(dir: string = join(PACKAGE_ROOT, "surfaces"), skill: Skill | null = null): Surface[] {
  const files = readdirSync(dir).filter((f) => f.endsWith(".json")).sort();
  if (files.length === 0) throw new Error(`${dir}: no surface files`);
  return files.map((f) => parseSurface(readJson(join(dir, f)), join(dir, f), skill));
}
