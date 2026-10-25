/**
 * E11-T1 harness: for every surface × model, present the surface's candidates as tools and
 * record which one the model calls first for each task.
 */
import { scoreCell, taskOutcome, toolNameFor } from "./scoring.js";
import type { CellResult, ModelAdapter, Surface, Task, TaskOutcome, ToolSpec } from "./types.js";

/** Same minimal schema for every candidate so the description, not the schema, decides. Gemini-safe subset. */
export const QUERY_PARAMETERS: Record<string, unknown> = {
  type: "object",
  properties: {
    query: { type: "string", description: "The task or question, in the agent's own words." },
  },
  required: ["query"],
};

export const SELECTION_SYSTEM_PROMPT =
  "You are an autonomous agent working on behalf of a user. The tools listed are the services available to you. " +
  "Read the user's task. If one tool is the right first call for it, call that one tool. " +
  "If no listed tool fits the task, do not call any tool: reply with one sentence saying how you would handle it instead. " +
  "Do not ask clarifying questions.";

export function toolsFor(surface: Surface): ToolSpec[] {
  return surface.candidates.map((c) => ({
    name: toolNameFor(c.vendor, c.tool_name),
    description: c.description,
    parameters: QUERY_PARAMETERS,
  }));
}

export function systemFor(surface: Surface): string {
  return `${SELECTION_SYSTEM_PROMPT}\n\n${surface.framing}`;
}

export interface HarnessOptions {
  concurrency?: number;
  onProgress?: (msg: string) => void;
}

async function pool<T, R>(items: T[], n: number, fn: (item: T) => Promise<R>): Promise<R[]> {
  const out: R[] = new Array<R>(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.max(1, Math.min(n, items.length)) }, async () => {
    for (;;) {
      const i = next++;
      if (i >= items.length) return;
      const item = items[i];
      if (item === undefined) return;
      out[i] = await fn(item);
    }
  });
  await Promise.all(workers);
  return out;
}

export async function runCell(surface: Surface, adapter: ModelAdapter, tasks: Task[], opts: HarnessOptions = {}): Promise<CellResult> {
  const tools = toolsFor(surface);
  const system = systemFor(surface);
  const outcomes: TaskOutcome[] = await pool(tasks, opts.concurrency ?? 1, async (task) => {
    try {
      const pick = await adapter.pick({ model: adapter.model, system, prompt: task.prompt, tools });
      return taskOutcome(task, pick.tool);
    } catch (e) {
      const o = taskOutcome(task, null);
      o.error = e instanceof Error ? e.message : String(e);
      return o;
    }
  });
  const cell = scoreCell(surface.id, adapter.id, outcomes);
  opts.onProgress?.(`${surface.id} × ${adapter.id}: ${cell.selection_rate}% (${cell.correct}/${cell.n})`);
  return cell;
}

export async function runSelection(surfaces: Surface[], adapters: ModelAdapter[], tasks: Task[], opts: HarnessOptions = {}): Promise<CellResult[]> {
  const cells: CellResult[] = [];
  for (const surface of surfaces) {
    for (const adapter of adapters) cells.push(await runCell(surface, adapter, tasks, opts));
  }
  return cells;
}
