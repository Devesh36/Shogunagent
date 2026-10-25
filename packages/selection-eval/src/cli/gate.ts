/**
 * `pnpm --filter @shogunagent/selection-eval gate --baseline results/baseline.json [--current results/latest.json]
 *    [--max-drop 2] [--ignore-mode]`
 *
 * Exit 0 when no surface × model (or adoption) rate dropped by more than --max-drop points
 * against the baseline; exit 1 otherwise; exit 2 on a usage error.
 */
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { formatGate, gate } from "../gate.js";
import { PACKAGE_ROOT } from "../load.js";
import type { RunResult } from "../types.js";
import { flag, parseArgs, str } from "./args.js";

function readRun(path: string): RunResult {
  const raw = JSON.parse(readFileSync(path, "utf8")) as Partial<RunResult>;
  if (raw.schema !== 1 || !Array.isArray(raw.cells) || !Array.isArray(raw.adoption) || (raw.mode !== "mock" && raw.mode !== "live")) {
    throw new Error(`${path}: not a selection-eval run (schema 1 with mode, cells and adoption)`);
  }
  return raw as RunResult;
}

function main(): number {
  const args = parseArgs(process.argv.slice(2));
  const baselinePath = str(args, "baseline");
  if (!baselinePath) {
    process.stderr.write("usage: gate --baseline <run.json> [--current <run.json>] [--max-drop <points>] [--ignore-mode]\n");
    return 2;
  }
  const currentPath = str(args, "current") ?? join(PACKAGE_ROOT, "results", "latest.json");
  const maxDrop = str(args, "max-drop");
  const report = gate(readRun(resolve(baselinePath)), readRun(resolve(currentPath)), {
    ...(maxDrop !== undefined ? { maxDropPoints: Number(maxDrop) } : {}),
    ignoreMode: flag(args, "ignore-mode"),
  });
  process.stdout.write(`${formatGate(report)}\n`);
  return report.ok ? 0 : 1;
}

try {
  process.exit(main());
} catch (e) {
  process.stderr.write(`selection-eval gate: ${e instanceof Error ? e.message : String(e)}\n`);
  process.exit(2);
}
