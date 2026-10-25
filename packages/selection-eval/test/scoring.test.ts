import { describe, expect, it } from "vitest";
import { outcomeOf, rate, scoreAdoption, scoreCell, taskOutcome, toolNameFor, vendorOf } from "../src/scoring.js";
import type { Task } from "../src/types.js";

const yes: Task = { id: "y", prompt: "what did I promise", expected_category: "state", should_pick_shogunagent: true };
const no: Task = { id: "n", prompt: "search the web", expected_category: "web", should_pick_shogunagent: false };

describe("tool names", () => {
  it("prefixes the vendor and sanitises to the function-name charset", () => {
    expect(toolNameFor("shogunagent", "state.commitments.list")).toBe("shogunagent__state_commitments_list");
    expect(toolNameFor("contextdev", "web/scrape/markdown")).toBe("contextdev__web_scrape_markdown");
    expect(toolNameFor("io.shogunagent", "x")).toBe("io_shogunagent__x");
  });
  it("caps at 64 characters", () => {
    expect(toolNameFor("v", "a".repeat(100)).length).toBe(64);
  });
  it("recovers the vendor", () => {
    expect(vendorOf("shogunagent__state_people_get")).toBe("shogunagent");
    expect(vendorOf("report_plan")).toBe("report_plan");
    expect(vendorOf(null)).toBeNull();
  });
});

describe("outcomes", () => {
  it("maps the four cases", () => {
    expect(outcomeOf(true, true)).toBe("tp");
    expect(outcomeOf(true, false)).toBe("fn");
    expect(outcomeOf(false, true)).toBe("fp");
    expect(outcomeOf(false, false)).toBe("tn");
  });
  it("a shogunagent pick is correct only when it should be picked", () => {
    expect(taskOutcome(yes, "shogunagent__x").correct).toBe(true);
    expect(taskOutcome(yes, "mem0__x").correct).toBe(false);
    expect(taskOutcome(yes, null).correct).toBe(false);
    expect(taskOutcome(no, "shogunagent__x").correct).toBe(false);
    expect(taskOutcome(no, "exa__search").correct).toBe(true);
    expect(taskOutcome(no, null).correct).toBe(true);
  });
});

describe("rates", () => {
  it("is a percentage with two decimals and 0 for an empty cell", () => {
    expect(rate(1, 3)).toBe(33.33);
    expect(rate(2, 3)).toBe(66.67);
    expect(rate(0, 0)).toBe(0);
  });
  it("scoreCell tallies tp/fp/fn/tn and the selection rate", () => {
    const cell = scoreCell("s", "m", [
      taskOutcome(yes, "shogunagent__a"),
      taskOutcome(yes, null),
      taskOutcome(no, "shogunagent__a"),
      taskOutcome(no, "exa__search"),
    ]);
    expect(cell).toMatchObject({ surface: "s", model: "m", n: 4, correct: 2, selection_rate: 50, tp: 1, fn: 1, fp: 1, tn: 1 });
  });
  it("scoreAdoption mirrors scoreCell", () => {
    const r = scoreAdoption("m", [
      { task_id: "a", should_propose: true, proposed: true, correct: true, outcome: "tp", evidence: [] },
      { task_id: "b", should_propose: false, proposed: false, correct: true, outcome: "tn", evidence: [] },
      { task_id: "c", should_propose: true, proposed: false, correct: false, outcome: "fn", evidence: [] },
    ]);
    expect(r).toMatchObject({ n: 3, correct: 2, adoption_rate: 66.67, tp: 1, tn: 1, fn: 1, fp: 0 });
  });
});
