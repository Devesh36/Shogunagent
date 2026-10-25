/**
 * Scoring. The selection rate of a surface × model cell is the share of tasks the model got
 * right: it picked a ShogunAgent tool when `should_pick_shogunagent` is true, and did not pick
 * one (a competitor or no tool) when it is false. Both halves count, so a listing that wins
 * every task would score badly on the web / none tasks and vice versa.
 */
import type { AdoptionOutcome, AdoptionResult, CellResult, Outcome, Task, TaskOutcome } from "./types.js";

export const SHOGUNAGENT_VENDOR = "shogunagent";

/** Tool names are `<vendor>__<tool>`; the vendor is what the score looks at. */
export function toolNameFor(vendor: string, toolName: string): string {
  const raw = `${vendor}__${toolName}`.replace(/[^a-zA-Z0-9_-]/g, "_");
  return raw.length <= 64 ? raw : raw.slice(0, 64);
}

export function vendorOf(toolSpecName: string | null): string | null {
  if (!toolSpecName) return null;
  const i = toolSpecName.indexOf("__");
  return i === -1 ? toolSpecName : toolSpecName.slice(0, i);
}

export function outcomeOf(shouldPick: boolean, picked: boolean): Outcome {
  if (shouldPick && picked) return "tp";
  if (shouldPick && !picked) return "fn";
  if (!shouldPick && picked) return "fp";
  return "tn";
}

export function taskOutcome(task: Task, pickedTool: string | null): TaskOutcome {
  const vendor = vendorOf(pickedTool);
  const picked = vendor === SHOGUNAGENT_VENDOR;
  const outcome = outcomeOf(task.should_pick_shogunagent, picked);
  return {
    task_id: task.id,
    expected_category: task.expected_category,
    should_pick_shogunagent: task.should_pick_shogunagent,
    picked_tool: pickedTool,
    picked_vendor: vendor,
    picked_shogunagent: picked,
    correct: outcome === "tp" || outcome === "tn",
    outcome,
  };
}

export function rate(correct: number, n: number): number {
  if (n === 0) return 0;
  return Math.round((correct / n) * 10_000) / 100;
}

function tally(outcomes: Outcome[]): { tp: number; fp: number; fn: number; tn: number } {
  const t = { tp: 0, fp: 0, fn: 0, tn: 0 };
  for (const o of outcomes) t[o] += 1;
  return t;
}

export function scoreCell(surface: string, model: string, tasks: TaskOutcome[]): CellResult {
  const correct = tasks.filter((t) => t.correct).length;
  return {
    surface,
    model,
    n: tasks.length,
    correct,
    selection_rate: rate(correct, tasks.length),
    ...tally(tasks.map((t) => t.outcome)),
    tasks,
  };
}

export function scoreAdoption(model: string, tasks: AdoptionOutcome[]): AdoptionResult {
  const correct = tasks.filter((t) => t.correct).length;
  return {
    model,
    n: tasks.length,
    correct,
    adoption_rate: rate(correct, tasks.length),
    ...tally(tasks.map((t) => t.outcome)),
    tasks,
  };
}
