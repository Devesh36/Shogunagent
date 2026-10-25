/**
 * Gate (E11-T2): compare a run against the committed baseline. Red when any surface × model
 * selection rate — or any adoption rate — drops by more than `maxDropPoints` percentage points,
 * when a baseline cell is missing from the current run, or when the two runs are not in the
 * same mode (a mock number is not comparable to a live one).
 */
import type { RunResult } from "./types.js";

export const DEFAULT_MAX_DROP_POINTS = 2;

export interface GateOptions {
  maxDropPoints?: number;
  /** Compare across modes anyway (for a one-off look, never in CI). */
  ignoreMode?: boolean;
}

export interface GateLine {
  key: string;
  baseline: number | null;
  current: number | null;
  delta: number | null;
  status: "ok" | "drop" | "missing" | "new";
}

export interface GateReport {
  ok: boolean;
  maxDropPoints: number;
  lines: GateLine[];
  reasons: string[];
}

function cellKeys(run: RunResult): Map<string, number> {
  const m = new Map<string, number>();
  for (const c of run.cells) m.set(`${c.surface} × ${c.model}`, c.selection_rate);
  for (const a of run.adoption) m.set(`adoption × ${a.model}`, a.adoption_rate);
  return m;
}

export function gate(baseline: RunResult, current: RunResult, opts: GateOptions = {}): GateReport {
  const maxDrop = opts.maxDropPoints ?? DEFAULT_MAX_DROP_POINTS;
  const reasons: string[] = [];
  const lines: GateLine[] = [];

  if (baseline.mode !== current.mode && !opts.ignoreMode) {
    reasons.push(`mode mismatch: baseline is "${baseline.mode}", current is "${current.mode}" (pass --ignore-mode to compare anyway)`);
  }

  const b = cellKeys(baseline);
  const c = cellKeys(current);
  for (const [key, bRate] of b) {
    const cRate = c.get(key);
    if (cRate === undefined) {
      lines.push({ key, baseline: bRate, current: null, delta: null, status: "missing" });
      reasons.push(`${key}: present in baseline, missing from current run`);
      continue;
    }
    const delta = Math.round((cRate - bRate) * 100) / 100;
    const drop = delta < -maxDrop;
    lines.push({ key, baseline: bRate, current: cRate, delta, status: drop ? "drop" : "ok" });
    if (drop) reasons.push(`${key}: ${bRate} → ${cRate} (${delta} points; limit −${maxDrop})`);
  }
  for (const [key, cRate] of c) {
    if (!b.has(key)) lines.push({ key, baseline: null, current: cRate, delta: null, status: "new" });
  }

  return { ok: reasons.length === 0, maxDropPoints: maxDrop, lines, reasons };
}

export function formatGate(report: GateReport): string {
  const out: string[] = [];
  out.push(`selection-eval gate (max drop ${report.maxDropPoints} points)`);
  for (const l of report.lines) {
    const b = l.baseline === null ? "—" : l.baseline.toFixed(2);
    const cu = l.current === null ? "—" : l.current.toFixed(2);
    const d = l.delta === null ? "" : ` (${l.delta >= 0 ? "+" : ""}${l.delta.toFixed(2)})`;
    out.push(`  [${l.status.padEnd(7)}] ${l.key}: ${b} → ${cu}${d}`);
  }
  if (report.ok) out.push("PASS: no cell dropped beyond the limit.");
  else {
    out.push("FAIL:");
    for (const r of report.reasons) out.push(`  - ${r}`);
  }
  return out.join("\n");
}
