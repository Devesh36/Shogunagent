/**
 * `pnpm --filter @shogunagent/selection-eval eval [--mock] [--only anthropic,openai] [--no-adoption]
 *    [--tasks path] [--adoption-tasks path] [--surfaces dir] [--out dir] [--concurrency n] [--write-baseline]`
 *
 * Writes results/<timestamp>.json + .md and results/latest.json + .md. With --write-baseline also
 * results/baseline.json + .md (the gate reference; mock mode only unless --force).
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { runAdoption } from "../adoption.js";
import { resolveAdapters } from "../adapters/index.js";
import { runSelection } from "../harness.js";
import { loadAdoptionTasks, loadSkill, loadSurfaces, loadTasks, PACKAGE_ROOT } from "../load.js";
import { renderConsole, renderMarkdown } from "../report.js";
import type { RunResult } from "../types.js";
import { flag, parseArgs, str } from "./args.js";

async function main(): Promise<number> {
  const args = parseArgs(process.argv.slice(2));
  const only = str(args, "only")?.split(",").map((s) => s.trim()).filter(Boolean);
  const { mode, adapters } = resolveAdapters(process.env, { forceMock: flag(args, "mock"), ...(only ? { only } : {}) });

  const skill = loadSkill(process.env);
  const tasks = loadTasks(str(args, "tasks"));
  const surfaces = loadSurfaces(str(args, "surfaces"), skill);
  const adoptionTasks = flag(args, "no-adoption") ? [] : loadAdoptionTasks(str(args, "adoption-tasks"));
  const concurrency = Number(str(args, "concurrency") ?? (mode === "mock" ? "1" : "2"));
  const outDir = str(args, "out") ?? join(PACKAGE_ROOT, "results");

  const log = (m: string) => process.stderr.write(`${m}\n`);
  log(`selection-eval: mode=${mode} models=${adapters.map((a) => a.id).join(",")} tasks=${tasks.length} surfaces=${surfaces.length} skill=${skill.path}`);

  const cells = await runSelection(surfaces, adapters, tasks, { concurrency, onProgress: log });
  const adoption = [];
  for (const a of adapters) {
    if (adoptionTasks.length === 0) break;
    const r = await runAdoption(a, skill, adoptionTasks);
    log(`adoption × ${a.id}: ${r.adoption_rate}% (${r.correct}/${r.n})`);
    adoption.push(r);
  }

  const notes: string[] = [];
  const errors = cells.reduce((n, c) => n + c.tasks.filter((t) => t.error).length, 0) + adoption.reduce((n, a) => n + a.tasks.filter((t) => t.error).length, 0);
  if (errors) notes.push(`${errors} adapter error(s) were counted as "no tool"; see the per-task error fields.`);
  if (mode === "mock") notes.push("Mock numbers measure lexical fit of the listing text against the task text, not a model's judgement.");

  const run: RunResult = {
    schema: 1,
    mode,
    generated_at: new Date().toISOString(),
    models: adapters.map((a) => a.id),
    surfaces: surfaces.map((s) => s.id),
    cells,
    adoption,
    notes,
  };

  mkdirSync(outDir, { recursive: true });
  const stamp = run.generated_at.replace(/[:.]/g, "-");
  const json = `${JSON.stringify(run, null, 2)}\n`;
  const md = `${renderMarkdown(run)}\n`;
  writeFileSync(join(outDir, `${stamp}.json`), json);
  writeFileSync(join(outDir, `${stamp}.md`), md);
  writeFileSync(join(outDir, "latest.json"), json);
  writeFileSync(join(outDir, "latest.md"), md);
  if (flag(args, "write-baseline")) {
    if (mode !== "mock" && !flag(args, "force")) {
      log("refusing to write a live-mode baseline without --force (the CI gate runs in mock mode)");
      return 2;
    }
    writeFileSync(join(outDir, "baseline.json"), json);
    writeFileSync(join(outDir, "baseline.md"), md);
    log(`wrote ${join(outDir, "baseline.json")}`);
  }

  process.stdout.write(`${renderConsole(run)}\n`);
  process.stdout.write(`wrote ${join(outDir, `${stamp}.json`)} and ${join(outDir, "latest.json")}\n`);
  return 0;
}

main().then(
  (code) => process.exit(code),
  (e: unknown) => {
    process.stderr.write(`selection-eval: ${e instanceof Error ? e.message : String(e)}\n`);
    process.exit(1);
  },
);
