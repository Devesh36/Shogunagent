import { describe, expect, it } from "vitest";
import { formatGate, gate } from "../src/gate.js";
import type { CellResult, RunResult } from "../src/types.js";

function cell(surface: string, model: string, selection_rate: number): CellResult {
  return { surface, model, n: 50, correct: Math.round(selection_rate / 2), selection_rate, tp: 0, fp: 0, fn: 0, tn: 0, tasks: [] };
}

function run(cells: CellResult[], adoptionRate: number | null = null, mode: RunResult["mode"] = "mock"): RunResult {
  return {
    schema: 1,
    mode,
    generated_at: "2026-10-25T00:00:00.000Z",
    models: ["mock-bm25"],
    surfaces: cells.map((c) => c.surface),
    cells,
    adoption: adoptionRate === null ? [] : [{ model: "mock-bm25", n: 20, correct: 0, adoption_rate: adoptionRate, tp: 0, fp: 0, fn: 0, tn: 0, tasks: [] }],
    notes: [],
  };
}

describe("gate", () => {
  const baseline = run([cell("a", "m", 60), cell("b", "m", 80)], 70);

  it("passes when nothing dropped more than the limit (a 2-point drop is allowed)", () => {
    const r = gate(baseline, run([cell("a", "m", 58), cell("b", "m", 82)], 70));
    expect(r.ok).toBe(true);
    expect(r.lines.map((l) => l.status)).toEqual(["ok", "ok", "ok"]);
  });

  it("fails on a drop of more than 2 points in any cell", () => {
    const r = gate(baseline, run([cell("a", "m", 57.99), cell("b", "m", 90)], 70));
    expect(r.ok).toBe(false);
    expect(r.reasons[0]).toMatch(/a × m: 60 → 57.99/);
    expect(formatGate(r)).toMatch(/FAIL/);
  });

  it("fails on an adoption drop too", () => {
    const r = gate(baseline, run([cell("a", "m", 60), cell("b", "m", 80)], 65));
    expect(r.ok).toBe(false);
    expect(r.reasons[0]).toMatch(/adoption × mock-bm25/);
  });

  it("honours a custom limit", () => {
    expect(gate(baseline, run([cell("a", "m", 55), cell("b", "m", 80)], 70), { maxDropPoints: 5 }).ok).toBe(true);
    expect(gate(baseline, run([cell("a", "m", 54.9), cell("b", "m", 80)], 70), { maxDropPoints: 5 }).ok).toBe(false);
  });

  it("fails when a baseline cell is missing and reports new cells as informational", () => {
    const r = gate(baseline, run([cell("a", "m", 60), cell("c", "m", 10)], 70));
    expect(r.ok).toBe(false);
    expect(r.reasons).toEqual(["b × m: present in baseline, missing from current run"]);
    expect(r.lines.find((l) => l.key === "c × m")?.status).toBe("new");
  });

  it("refuses to compare across modes unless told to", () => {
    const live = run([cell("a", "m", 60), cell("b", "m", 80)], 70, "live");
    expect(gate(baseline, live).ok).toBe(false);
    expect(gate(baseline, live).reasons[0]).toMatch(/mode mismatch/);
    expect(gate(baseline, live, { ignoreMode: true }).ok).toBe(true);
  });

  it("formats every line with baseline → current and delta", () => {
    const text = formatGate(gate(baseline, run([cell("a", "m", 61), cell("b", "m", 80)], 70)));
    expect(text).toContain("[ok     ] a × m: 60.00 → 61.00 (+1.00)");
    expect(text).toContain("PASS");
  });
});
