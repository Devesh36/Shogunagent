/**
 * The A2A state-of-work handoff (docs/api/handoff.md, docs/api/handoff.schema.json): a strict
 * subset of export v1 that one agent hands to another with a task. `acceptHandoff` sends it to
 * `handoff.accept`, which lands it in the receiver's own scratch namespace, never in the principal's world: what another agent claims is
 * working context for the receiver, not state the user's store has observed.
 */
import { HandoffError } from "./errors.js";
import type { IngestEvent } from "./types.js";

export const HANDOFF_FORMAT = "shogunagent-handoff/v1";
export const HANDOFF_SCHEMA_URL = "https://shogunagent.dev/schemas/handoff.schema.json";

export interface HandoffProvenance {
  event_id: number;
  quote: string | null;
}

/** An export v1 `entity`, cut to what a handoff carries. */
export interface HandoffEntity {
  type: "entity";
  kind: "person" | "project";
  id: number;
  name: string;
  confidence: number;
  /** Low never travels in a handoff. */
  band: "high" | "medium";
  provenance: HandoffProvenance[];
  aliases?: string[];
  summary?: string | null;
  status?: "active" | "waiting" | "paused" | "done" | null;
  namespace?: string;
}

/** An export v1 `fact` (a commitment or an open loop), cut to what a handoff carries. */
export interface HandoffFact {
  type: "fact";
  kind: "commitment" | "open_loop";
  id: number;
  text: string;
  confidence: number;
  band: "high" | "medium";
  provenance: HandoffProvenance[];
  subtype?: string;
  status?: string;
  due_at?: number | null;
  counterparty_id?: number | null;
  project_id?: number | null;
  namespace?: string;
}

export interface Handoff {
  format: typeof HANDOFF_FORMAT;
  handoff_id: string;
  created_at: number;
  expires_at: number;
  from_agent: string;
  to_agent: string;
  task: { summary: string; query: string; thread_key: string | null };
  entities: HandoffEntity[];
  facts: HandoffFact[];
  truncated: boolean;
}

/** The A2A message part that carries a handoff. */
export interface HandoffDataPart {
  kind: "data";
  data: Handoff;
  metadata: { schema: string; [key: string]: unknown };
}

export interface HandoffResult {
  handoff: Handoff;
  a2a: {
    part: HandoffDataPart;
    message: { kind: "message"; role: "agent"; messageId: string; parts: HandoffDataPart[]; metadata?: Record<string, unknown> };
  };
}

export interface HandoffParams {
  /** What the receiving agent is asked to do. */
  task: string;
  /** The receiving agent's id. */
  toAgent: string;
  /** Your agent id; defaults to the client's actor. */
  fromAgent?: string;
  /** What to recall for the task; defaults to `task`. */
  query?: string;
  budgetTokens?: number;
  /** How long the handoff is valid; 60–86,400 s, default 3,600. */
  ttlSeconds?: number;
  threadKey?: string;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Pull a handoff out of whatever arrived: the object, a data part, an A2A message, or a result. */
export function extractHandoff(input: unknown): Handoff {
  let value: unknown = input;
  if (isObj(value) && isObj(value.handoff)) value = value.handoff;
  if (isObj(value) && Array.isArray(value.parts)) {
    value = (value.parts as unknown[]).find((p) => isObj(p) && p.kind === "data" && isObj(p.data) && p.data.format === HANDOFF_FORMAT);
  }
  if (isObj(value) && value.kind === "data" && isObj(value.data)) value = value.data;
  if (!isObj(value) || value.format !== HANDOFF_FORMAT) {
    throw new HandoffError({ status: 0, code: "invalid_handoff", message: `Not a ${HANDOFF_FORMAT} object.`, nextAction: "fix_input" });
  }
  const h = value as unknown as Handoff;
  if (typeof h.handoff_id !== "string" || typeof h.expires_at !== "number" || !Array.isArray(h.entities) || !Array.isArray(h.facts) || !isObj(h.task)) {
    throw new HandoffError({ status: 0, code: "invalid_handoff", message: "The handoff is missing handoff_id, expires_at, task, entities or facts.", nextAction: "fix_input" });
  }
  return h;
}

/** Check a handoff is usable by `receiver` at `nowMs`. */
export function checkHandoff(h: Handoff, receiver: string | undefined, nowMs: number): void {
  if (h.expires_at <= nowMs) {
    throw new HandoffError({ status: 0, code: "handoff_expired", message: `The handoff expired at ${new Date(h.expires_at).toISOString()}. Ask the sender for a fresh one.`, nextAction: "retry_after" });
  }
  if (receiver !== undefined && h.to_agent !== receiver) {
    throw new HandoffError({ status: 0, code: "handoff_not_for_you", message: `The handoff is addressed to ${h.to_agent}, not ${receiver}.`, nextAction: "not_allowed" });
  }
}

function provenanceLine(p: HandoffProvenance[]): string {
  const quoted = p.filter((x) => x.quote).map((x) => `"${x.quote}" (event ${x.event_id})`);
  return quoted.length ? ` Source: ${quoted.join("; ")}` : "";
}

/**
 * The events `handoff.accept` stores (the core builds the same list, crates/shogun-mcp/src/
 * handoff_api.rs `accept_items`): one for the task, one per entity and fact. Medium items keep
 * their `possibly:` prefix; Low items are never in a handoff and are skipped if one slips in.
 * `acceptHandoff` uses its length as the call's `Mcp-Quantity`.
 */
export function handoffEvents(h: Handoff): IngestEvent[] {
  const events: IngestEvent[] = [
    { kind: "handoff_task", title: `handoff from ${h.from_agent}`.slice(0, 200), content: `Task handed over by ${h.from_agent}: ${h.task.summary}` },
  ];
  const mark = (band: string) => (band === "medium" ? "possibly: " : "");
  for (const e of h.entities) {
    if (e.band !== "high" && e.band !== "medium") continue;
    events.push({ kind: "handoff_entity", title: `${e.kind} #${e.id}`, content: `${mark(e.band)}${e.kind} ${e.name} (confidence ${e.confidence}).${provenanceLine(e.provenance)}` });
  }
  for (const f of h.facts) {
    if (f.band !== "high" && f.band !== "medium") continue;
    events.push({ kind: "handoff_fact", title: `${f.kind} #${f.id}`, content: `${mark(f.band)}${f.kind.replace("_", " ")}: ${f.text} (confidence ${f.confidence}).${provenanceLine(f.provenance)}` });
  }
  return events.map((e) => ({ ...e, content: e.content.slice(0, 16_000) }));
}
