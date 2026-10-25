/**
 * The tool table (docs/api/tool-table.json.md): the one input the agent-readable surfaces are
 * rendered from (CLAUDE.md invariant 12). Today it is `apps/api/fixtures/tool-table.json`, a
 * fixture pinned to the plan's §7.1 list; once E4 lands the Rust catalog exports the same shape
 * and the fixture is regenerated from it.
 *
 * This module validates the shape and derives the MCP annotations from the permission level by
 * one rule (`annotationsFor`). Nothing stores annotations.
 */
import { classifyCall, type MeterUnit } from "../pricing.js";

export const TOOL_TABLE_SCHEMA = "shogunagent/tool-table/v1";

export type ToolLevel = "read" | "L1" | "L2" | "L3";
export type ToolCostUnit = Exclude<MeterUnit, "storage" | "protocol">;

export interface ToolDeprecation {
  since: string;
  replacement: string | null;
  removal_after: string;
}

export interface ToolRow {
  name: string;
  title: string;
  description: string;
  level: ToolLevel;
  cost_unit: ToolCostUnit;
  input_schema: Record<string, unknown>;
  output_schema: Record<string, unknown> | null;
  deprecated: ToolDeprecation | null;
}

export interface ToolTable {
  schema: typeof TOOL_TABLE_SCHEMA;
  source: string;
  protocol_version: string;
  toolset: string;
  tools: ToolRow[];
}

export interface ToolAnnotations {
  readOnlyHint: boolean;
  destructiveHint: boolean;
  idempotentHint: boolean;
  openWorldHint: boolean;
}

const LEVELS: ReadonlySet<string> = new Set<ToolLevel>(["read", "L1", "L2", "L3"]);
const COST_UNITS: ReadonlySet<string> = new Set<ToolCostUnit>(["read", "ingest", "model_op", "run"]);
const NAME_RE = /^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$/;

/** The one rule (docs/api/tool-table.json.md): L3 is the only tier that leaves the device. It is
 * the core's rule (`shogun_mcp::core_tools::annotations`, wire-frozen, invariant 11): L1 and L2
 * writes are both `idempotentHint: false` (E34: the card said `true` for L1, so `memory.ingest` /
 * `memory.append_note` disagreed with the core's `tools/list`; the derived card follows the frozen
 * core, never the reverse, invariant 12). */
export function annotationsFor(level: ToolLevel): ToolAnnotations {
  switch (level) {
    case "read":
      return { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false };
    case "L1":
      return { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false };
    case "L2":
      return { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false };
    case "L3":
      return { readOnlyHint: false, destructiveHint: true, idempotentHint: false, openWorldHint: true };
  }
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Validate a parsed JSON document as a tool table. Returns the problems, empty when valid. */
export function toolTableProblems(raw: unknown): string[] {
  const problems: string[] = [];
  if (!isObject(raw)) return ["the table must be a JSON object"];
  if (raw.schema !== TOOL_TABLE_SCHEMA) problems.push(`schema must be ${TOOL_TABLE_SCHEMA}`);
  if (typeof raw.source !== "string" || raw.source.length === 0) problems.push("source must be a non-empty string");
  if (typeof raw.protocol_version !== "string") problems.push("protocol_version must be a string");
  if (typeof raw.toolset !== "string") problems.push("toolset must be a string");
  if (!Array.isArray(raw.tools)) return [...problems, "tools must be an array"];
  const seen = new Set<string>();
  raw.tools.forEach((t, i) => {
    const where = `tools[${i}]`;
    if (!isObject(t)) { problems.push(`${where} must be an object`); return; }
    const name = t.name;
    if (typeof name !== "string" || !NAME_RE.test(name)) problems.push(`${where}.name must be <group>.<verb> in lowercase`);
    else if (seen.has(name)) problems.push(`${where}.name ${name} is listed twice`);
    else seen.add(name);
    if (typeof t.title !== "string" || t.title.length === 0) problems.push(`${where}.title must be a non-empty string`);
    if (typeof t.description !== "string" || t.description.length === 0) problems.push(`${where}.description must be a non-empty string`);
    if (typeof t.level !== "string" || !LEVELS.has(t.level)) problems.push(`${where}.level must be one of read, L1, L2, L3`);
    if (typeof t.cost_unit !== "string" || !COST_UNITS.has(t.cost_unit)) problems.push(`${where}.cost_unit must be one of read, ingest, model_op, run`);
    if (!isObject(t.input_schema) || t.input_schema.type !== "object") problems.push(`${where}.input_schema must be an object schema`);
    if (!(t.output_schema === null || isObject(t.output_schema))) problems.push(`${where}.output_schema must be an object schema or null`);
    if (!(t.deprecated === null || (isObject(t.deprecated) && typeof t.deprecated.since === "string" && typeof t.deprecated.removal_after === "string"))) {
      problems.push(`${where}.deprecated must be null or { since, replacement, removal_after }`);
    }
    // A read that is billed, or a write that is free, is a mismatch between the table and the
    // metering table — the two must agree (invariant 10).
    if (typeof t.level === "string" && typeof t.cost_unit === "string" && LEVELS.has(t.level) && COST_UNITS.has(t.cost_unit)) {
      if ((t.level === "read") !== (t.cost_unit === "read")) problems.push(`${where}: level ${t.level} does not match cost_unit ${t.cost_unit}`);
      if (typeof name === "string" && classifyCall("tools/call", name) !== t.cost_unit) {
        problems.push(`${where}: pricing.ts classifies ${name} as ${classifyCall("tools/call", name)}, the table says ${t.cost_unit}`);
      }
    }
  });
  return problems;
}

export class ToolTableError extends Error {
  constructor(readonly problems: string[]) {
    super(`tool table: ${problems.length} problem(s):\n  ${problems.join("\n  ")}`);
  }
}

/** Parse and validate, or throw `ToolTableError`. */
export function loadToolTable(raw: unknown): ToolTable {
  const problems = toolTableProblems(raw);
  if (problems.length > 0) throw new ToolTableError(problems);
  return raw as ToolTable;
}

/** The names of the tools an agent may call without writing anything (the read-only set). */
export function readOnlyToolNames(table: ToolTable): string[] {
  return table.tools.filter((t) => t.level === "read").map((t) => t.name);
}
