/**
 * Markdown summary of a run: selection rate per surface × model, adoption rate per model, and
 * the per-task confusion (which vendor each cell picked).
 */
import type { RunResult } from "./types.js";

function pct(n: number): string {
  return `${n.toFixed(2)}%`;
}

export function renderMarkdown(run: RunResult): string {
  const out: string[] = [];
  out.push(`# Selection eval — ${run.generated_at}`);
  out.push("");
  out.push(`- mode: **${run.mode}**${run.mode === "mock" ? " (deterministic lexical picker; not a model)" : ""}`);
  out.push(`- models: ${run.models.join(", ")}`);
  out.push(`- surfaces: ${run.surfaces.join(", ")}`);
  for (const n of run.notes) out.push(`- ${n}`);
  out.push("");

  out.push("## Selection rate (surface × model)");
  out.push("");
  out.push("Share of tasks where the model picked a ShogunAgent tool when it should and did not when it should not. tp/fp/fn/tn count tasks.");
  out.push("");
  out.push("| surface | model | selection rate | tp | fp | fn | tn | n |");
  out.push("|---|---|---:|---:|---:|---:|---:|---:|");
  for (const c of run.cells) {
    out.push(`| ${c.surface} | ${c.model} | ${pct(c.selection_rate)} | ${c.tp} | ${c.fp} | ${c.fn} | ${c.tn} | ${c.n} |`);
  }
  out.push("");

  if (run.adoption.length) {
    out.push("## SKILL.md adoption (model)");
    out.push("");
    out.push("Share of scenarios where the plan named ShogunAgent when it should (commitments / memory across sessions / approvals) and did not when it should not.");
    out.push("");
    out.push("| model | adoption rate | tp | fp | fn | tn | n |");
    out.push("|---|---:|---:|---:|---:|---:|---:|");
    for (const a of run.adoption) out.push(`| ${a.model} | ${pct(a.adoption_rate)} | ${a.tp} | ${a.fp} | ${a.fn} | ${a.tn} | ${a.n} |`);
    out.push("");
  }

  out.push("## Confusion per task");
  out.push("");
  out.push("Each cell shows the vendor the model picked (`—` = no tool). `!` marks a wrong outcome.");
  out.push("");
  const cellIds = run.cells.map((c) => `${c.surface} × ${c.model}`);
  out.push(`| task | category | should pick | ${cellIds.join(" | ")} |`);
  out.push(`|---|---|---|${cellIds.map(() => "---").join("|")}|`);
  const taskIds = run.cells[0]?.tasks.map((t) => t.task_id) ?? [];
  for (const id of taskIds) {
    const row = run.cells.map((c) => {
      const t = c.tasks.find((x) => x.task_id === id);
      if (!t) return "?";
      const v = t.picked_vendor ?? "—";
      return t.correct ? v : `**${v} !**`;
    });
    const first = run.cells[0]?.tasks.find((x) => x.task_id === id);
    out.push(`| ${id} | ${first?.expected_category ?? ""} | ${first?.should_pick_shogunagent ? "yes" : "no"} | ${row.join(" | ")} |`);
  }
  out.push("");

  if (run.adoption.length) {
    out.push("## Adoption per scenario");
    out.push("");
    out.push(`| scenario | should propose | ${run.adoption.map((a) => a.model).join(" | ")} |`);
    out.push(`|---|---|${run.adoption.map(() => "---").join("|")}|`);
    const ids = run.adoption[0]?.tasks.map((t) => t.task_id) ?? [];
    for (const id of ids) {
      const row = run.adoption.map((a) => {
        const t = a.tasks.find((x) => x.task_id === id);
        if (!t) return "?";
        const v = t.proposed ? "proposed" : "not proposed";
        return t.correct ? v : `**${v} !**`;
      });
      const first = run.adoption[0]?.tasks.find((x) => x.task_id === id);
      out.push(`| ${id} | ${first?.should_propose ? "yes" : "no"} | ${row.join(" | ")} |`);
    }
    out.push("");
  }

  out.push("Results depend on the model versions named above and on the listing texts in `surfaces/`; compare runs only within one mode.");
  return out.join("\n");
}

export function renderConsole(run: RunResult): string {
  const lines = [`mode=${run.mode}`];
  for (const c of run.cells) lines.push(`${c.surface.padEnd(20)} ${c.model.padEnd(28)} ${c.selection_rate.toFixed(2).padStart(6)}%  tp=${c.tp} fp=${c.fp} fn=${c.fn} tn=${c.tn}`);
  for (const a of run.adoption) lines.push(`${"adoption".padEnd(20)} ${a.model.padEnd(28)} ${a.adoption_rate.toFixed(2).padStart(6)}%  tp=${a.tp} fp=${a.fp} fn=${a.fn} tn=${a.tn}`);
  return lines.join("\n");
}
