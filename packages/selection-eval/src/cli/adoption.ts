/**
 * `pnpm --filter @shogunagent/selection-eval adoption [--mock] [--only anthropic] [--adoption-tasks path]`
 *
 * Runs only E11-T3 (SKILL.md adoption) and prints the per-scenario decisions. The full run
 * (`eval`) already includes adoption; this is the quick loop while editing SKILL.md.
 */
import { runAdoption } from "../adoption.js";
import { resolveAdapters } from "../adapters/index.js";
import { loadAdoptionTasks, loadSkill } from "../load.js";
import { flag, parseArgs, str } from "./args.js";

async function main(): Promise<number> {
  const args = parseArgs(process.argv.slice(2));
  const only = str(args, "only")?.split(",").map((s) => s.trim()).filter(Boolean);
  const { mode, adapters } = resolveAdapters(process.env, { forceMock: flag(args, "mock"), ...(only ? { only } : {}) });
  const skill = loadSkill(process.env);
  const tasks = loadAdoptionTasks(str(args, "adoption-tasks"));
  process.stdout.write(`mode=${mode} skill=${skill.path} scenarios=${tasks.length}\n`);
  for (const a of adapters) {
    const r = await runAdoption(a, skill, tasks);
    process.stdout.write(`\n${a.id}: adoption rate ${r.adoption_rate}% (tp=${r.tp} fp=${r.fp} fn=${r.fn} tn=${r.tn})\n`);
    for (const t of r.tasks) {
      const mark = t.correct ? "ok  " : "MISS";
      process.stdout.write(`  ${mark} ${t.task_id.padEnd(14)} should=${t.should_propose ? "yes" : "no "} proposed=${t.proposed ? "yes" : "no "}${t.error ? `  error: ${t.error}` : ""}\n`);
      if (!t.correct && t.evidence.length) process.stdout.write(`       ${t.evidence.join(" | ").slice(0, 300)}\n`);
    }
  }
  return 0;
}

main().then(
  (code) => process.exit(code),
  (e: unknown) => {
    process.stderr.write(`selection-eval adoption: ${e instanceof Error ? e.message : String(e)}\n`);
    process.exit(1);
  },
);
