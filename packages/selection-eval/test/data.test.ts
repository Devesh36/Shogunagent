/**
 * The committed task sets and surfaces are inputs to the gate; this pins their shape so a stray
 * edit (a duplicate id, a missing shogunagent candidate, a surface without a web tool) is caught
 * before it moves the baseline for the wrong reason.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { adoptionSystemPrompt, detectProposal, REPORT_PLAN_TOOL } from "../src/adoption.js";
import { systemFor, toolsFor } from "../src/harness.js";
import { loadAdoptionTasks, loadSkill, loadSurfaces, loadTasks, PACKAGE_ROOT, parseSkill } from "../src/load.js";
import { gate } from "../src/gate.js";
import { renderMarkdown } from "../src/report.js";
import { mockAdapter } from "../src/mock.js";
import { runSelection } from "../src/harness.js";
import type { RunResult } from "../src/types.js";

const skill = loadSkill({ SELECTION_EVAL_SKILL_PATH: join(PACKAGE_ROOT, "fixtures", "SKILL.snapshot.md") });

describe("tasks", () => {
  const tasks = loadTasks();
  it("has 50 tasks covering every category with both labels", () => {
    expect(tasks).toHaveLength(50);
    const cats = new Set(tasks.map((t) => t.expected_category));
    expect([...cats].sort()).toEqual(["approval", "memory", "none", "state", "web"]);
    expect(tasks.filter((t) => t.should_pick_shogunagent).length).toBeGreaterThanOrEqual(20);
    expect(tasks.filter((t) => !t.should_pick_shogunagent).length).toBeGreaterThanOrEqual(20);
  });
  it("labels are consistent with categories", () => {
    for (const t of tasks) {
      if (t.expected_category === "web" || t.expected_category === "none" || t.expected_category === "memory") expect(t.should_pick_shogunagent, t.id).toBe(false);
      if (t.expected_category === "approval") expect(t.should_pick_shogunagent, t.id).toBe(true);
    }
  });
  it("has 20 adoption scenarios with both labels", () => {
    const a = loadAdoptionTasks();
    expect(a).toHaveLength(20);
    expect(a.filter((t) => t.should_propose).length).toBe(12);
  });
});

describe("surfaces", () => {
  const surfaces = loadSurfaces(undefined, skill);
  it("are the four discovery surfaces", () => {
    expect(surfaces.map((s) => s.id)).toEqual(["claude-code-plugin", "mcp-registry", "openai-plugin", "router-discover"]);
  });
  it("each lists ShogunAgent, the three memory competitors and a web tool", () => {
    for (const s of surfaces) {
      const vendors = new Set(s.candidates.map((c) => c.vendor));
      for (const v of ["shogunagent", "mem0", "supermemory", "zep"]) expect(vendors.has(v), `${s.id} lacks ${v}`).toBe(true);
      expect([...vendors].some((v) => ["websearch", "exa", "tinyfish"].includes(v)), `${s.id} lacks a web tool`).toBe(true);
    }
  });
  it("every candidate names its source and whether it is verbatim", () => {
    for (const s of surfaces) for (const c of s.candidates) {
      expect(c.source, `${s.id}/${c.tool_name}`).toBeTruthy();
      expect(typeof c.verbatim, `${s.id}/${c.tool_name}`).toBe("boolean");
    }
  });
  it("the claude-code-plugin surface takes its ShogunAgent text from SKILL.md", () => {
    const s = surfaces.find((x) => x.id === "claude-code-plugin")!;
    const c = s.candidates.find((x) => x.vendor === "shogunagent")!;
    expect(c.description).toBe(skill.frontmatter["description"]);
    expect(c.description).toMatch(/state layer for AI agents/);
  });
  it("tool specs have unique provider-safe names and one shared schema", () => {
    for (const s of surfaces) {
      const names = toolsFor(s).map((t) => t.name);
      expect(new Set(names).size).toBe(names.length);
      for (const n of names) expect(n).toMatch(/^[a-zA-Z0-9_-]{1,64}$/);
      expect(systemFor(s)).toContain(s.framing);
    }
  });
  it("router-discover quotes the connector files verbatim", () => {
    const s = surfaces.find((x) => x.id === "router-discover")!;
    const c = s.candidates.find((x) => x.tool_name === "state.commitments.list")!;
    expect(c.verbatim).toBe(true);
    expect(c.description).toContain("Find what the user committed to: promises, deliverables and deadlines they made to others.");
  });
});

describe("skill", () => {
  it("parses frontmatter and the trigger / non-trigger sections", () => {
    expect(skill.frontmatter["name"]).toBe("shogunagent");
    expect(skill.triggerText).toContain("Use ShogunAgent when the task needs");
    expect(skill.nonTriggerText).toContain("Do not use it for:");
    expect(skill.nonTriggerText).not.toContain("## Precedence");
  });
  it("rejects a skill without a description", () => {
    expect(() => parseSkill("---\nname: x\n---\n# X\n")).toThrow(/no description/);
  });
  it("the adoption prompt embeds the skill and the report tool is the only tool", () => {
    expect(adoptionSystemPrompt(skill)).toContain(skill.text);
    expect(REPORT_PLAN_TOOL.name).toBe("report_plan");
  });
  it("detects a proposal in arguments or text, case-insensitively", () => {
    expect(detectProposal({ services_or_skills_to_use: ["ShogunAgent state.commitments.list"] }, undefined).proposed).toBe(true);
    expect(detectProposal({ setup_proposed: ["npx shogunagent@0.1"] }, undefined).proposed).toBe(true);
    expect(detectProposal({ steps: ["grep the repo"] }, "I would use Shogun Agent here").proposed).toBe(true);
    expect(detectProposal({ steps: ["grep the repo"], services_or_skills_to_use: [] }, "").proposed).toBe(false);
  });
});

describe("end to end in mock mode", () => {
  it("runs every surface deterministically and renders a report", async () => {
    const tasks = loadTasks();
    const surfaces = loadSurfaces(undefined, skill);
    const a = await runSelection(surfaces, [mockAdapter()], tasks);
    const b = await runSelection(surfaces, [mockAdapter()], tasks);
    expect(a.map((c) => c.selection_rate)).toEqual(b.map((c) => c.selection_rate));
    expect(a).toHaveLength(4);
    for (const c of a) {
      expect(c.n).toBe(50);
      expect(c.tp + c.fp + c.fn + c.tn).toBe(50);
    }
    const md = renderMarkdown({ schema: 1, mode: "mock", generated_at: "t", models: ["mock-bm25"], surfaces: surfaces.map((s) => s.id), cells: a, adoption: [], notes: [] });
    expect(md).toContain("## Selection rate (surface × model)");
    expect(md).toContain("| router-discover | mock-bm25 |");
  });
  it("the committed baseline passes the gate against the committed inputs (same skill resolution as the CLI)", async () => {
    const baseline = JSON.parse(readFileSync(join(PACKAGE_ROOT, "results", "baseline.json"), "utf8")) as RunResult;
    expect(baseline.mode).toBe("mock");
    const cliSkill = loadSkill(process.env);
    const cells = await runSelection(loadSurfaces(undefined, cliSkill), [mockAdapter()], loadTasks());
    const current: RunResult = { ...baseline, cells, adoption: [] };
    const report = gate({ ...baseline, adoption: [] }, current);
    expect(report.reasons, "run 'pnpm --filter @shogunagent/selection-eval harness --mock --write-baseline' after an intended change").toEqual([]);
  });
});
