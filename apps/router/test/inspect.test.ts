import { describe, expect, it } from "vitest";
import { noopHealthStore } from "../src/health.js";
import { inspect } from "../src/inspect.js";
import { fullCatalog } from "./helpers.js";

describe("inspect (E8-T4)", () => {
  it("returns schema, price, permission meaning, examples and hints, free and without auth", () => {
    const out = inspect(fullCatalog(), noopHealthStore, "monid.strale.x402.url-to-text");
    expect(out.status).toBe(200);
    if (out.status !== 200) return;
    const r = out.body.result;
    expect(r.tool.input_schema["type"]).toBe("object");
    expect(r.price.model).toBe("per_call");
    expect(r.price.quote_micro_usd).toBe(24948); // 0.02376 USD + 5%
    expect(r.price.failures_cost).toBe(0);
    expect(r.permission.level).toBe("L1");
    expect(r.permission.meaning).toContain("Reads only");
    expect(r.health.source).toBe("declared");
    expect(r.run_example.tool_id).toBe("monid.strale.x402.url-to-text");
    expect(r.run_example.dryRun).toBe(true);
    expect(out.body.usage.cost_micro_usd).toBe(0);
    // cheaper alternatives in the same category, cheapest first, including a competitor's connector
    expect(out.body.hints.length).toBeGreaterThan(0);
    expect(out.body.hints[0]?.tool_id).toBe("monid.contextdev.web.scrape.markdown");
    expect(out.body.hints.every((h) => h.estimated_cost_micro_usd < 24948)).toBe(true);
  });

  it("describes an L3 tool as needing a human approval URL", () => {
    const out = inspect(fullCatalog(), noopHealthStore, "shogunagent.actions.execute");
    expect(out.status).toBe(200);
    if (out.status !== 200) return;
    expect(out.body.result.permission.level).toBe("L3");
    expect(out.body.result.permission.meaning).toContain("approval URL");
    expect(out.body.result.tool.first_party).toBe(true);
    expect(out.body.result.tool.examples.length).toBeGreaterThan(0);
  });

  it("404s an unknown tool with next_action discover", () => {
    const out = inspect(fullCatalog(), noopHealthStore, "nope.nothing");
    expect(out.status).toBe(404);
    if (out.status !== 404) return;
    expect(out.body.error.code).toBe("tool_not_found");
    expect(out.body.error.next_action).toBe("discover");
  });
});
