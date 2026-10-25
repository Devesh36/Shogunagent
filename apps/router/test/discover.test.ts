import { describe, expect, it } from "vitest";
import { compileCatalog } from "../src/catalog.js";
import { combineScore, discover, priceScore, RANKING_WEIGHTS } from "../src/discover.js";
import { noopHealthStore } from "../src/health.js";
import { fullCatalog, synthEndpoint, synthProvider } from "./helpers.js";

describe("discover ranking (E8-T3, invariant 13)", () => {
  it("a 10x more expensive endpoint with the same task fit never outranks the cheaper one", () => {
    const same = "Search the public web for pages matching a query and return links with snippets.";
    const catalog = compileCatalog([
      synthProvider("pricey", [synthEndpoint("search", { summary: same, cost: 13_000, categories: ["web_search"] })]),
      synthProvider("cheap", [synthEndpoint("search", { summary: same, cost: 1_300, categories: ["web_search"] })]),
    ]);
    const out = discover(catalog, noopHealthStore, { task: "search the web for pages about a query" });
    const ids = out.result.candidates.map((c) => c.tool_id);
    expect(ids[0]).toBe("cheap.search");
    expect(ids[1]).toBe("pricey.search");
    const [cheap, pricey] = out.result.candidates;
    expect(cheap?.scoreBreakdown.task_fit).toBe(pricey?.scoreBreakdown.task_fit);
    expect(cheap?.scoreBreakdown.price).toBeGreaterThan(pricey?.scoreBreakdown.price ?? 1);
    expect(cheap?.score).toBeGreaterThan(pricey?.score ?? 1);
  });

  it("price is strictly decreasing and $0 scores 1", () => {
    expect(priceScore(0)).toBe(1);
    let prev = priceScore(0);
    for (const micro of [1, 100, 1000, 10_000, 100_000, 1_000_000, 10_000_000]) {
      const s = priceScore(micro);
      expect(s).toBeLessThan(prev);
      prev = s;
    }
  });

  it("ranks the first-party state endpoint first for 'what did the user commit to', with the full breakdown", () => {
    const catalog = fullCatalog();
    const out = discover(catalog, noopHealthStore, { task: "what did the user commit to" });
    const top = out.result.candidates[0];
    expect(top).toBeDefined();
    expect(top?.tool_id).toBe("shogunagent.state.commitments.list");
    expect(top?.first_party).toBe(true);
    expect(top?.categories).toContain("state");
    expect(top?.permission_level).toBe("L1");
    expect(top?.price.quote_micro_usd).toBe(0);
    const b = top?.scoreBreakdown;
    expect(b).toBeDefined();
    if (!b || !top) return;
    for (const k of ["task_fit", "price", "health", "latency", "personal"] as const) {
      expect(b[k]).toBeGreaterThanOrEqual(0);
      expect(b[k]).toBeLessThanOrEqual(1);
    }
    expect(b.task_fit).toBeGreaterThan(0);
    expect(b.price).toBe(1);
    expect(b.personal).toBe(0);
    const recomputed = b.task_fit * (RANKING_WEIGHTS.price * b.price + RANKING_WEIGHTS.health * b.health + RANKING_WEIGHTS.latency * b.latency + RANKING_WEIGHTS.personal * b.personal);
    expect(Math.abs(recomputed - top.score)).toBeLessThan(0.001);
    expect(Math.abs(combineScore(b) - top.score)).toBeLessThan(0.001);
    expect(RANKING_WEIGHTS.price + RANKING_WEIGHTS.health + RANKING_WEIGHTS.latency + RANKING_WEIGHTS.personal).toBeCloseTo(1);
    expect(typeof top.p50_ms).toBe("number");
    expect(typeof top.p95_ms).toBe("number");
    expect(out.result.ranking.weights).toEqual(RANKING_WEIGHTS);
    expect(out.result.ranking.formula).toContain("price");
    expect(out.usage.cost_micro_usd).toBe(0);
  });

  it("third-party tools win on their own tasks: a web search task ranks a search endpoint first", () => {
    const out = discover(fullCatalog(), noopHealthStore, { task: "search the web for recent news about a company" });
    const top = out.result.candidates[0];
    expect(top?.first_party).toBe(false);
    expect(top?.categories).toContain("web_search");
  });

  it("budget excludes tools above it, limit caps the list, and an unmatched task says broaden_query", () => {
    const catalog = fullCatalog();
    const all = discover(catalog, noopHealthStore, { task: "read the text content of a web page url", limit: 50 });
    expect(all.result.candidates.some((c) => c.tool_id === "monid.strale.x402.url-to-text")).toBe(true);
    const budgeted = discover(catalog, noopHealthStore, { task: "read the text content of a web page url", budget_micro_usd: 2000, limit: 50 });
    expect(budgeted.result.candidates.every((c) => c.price.quote_micro_usd <= 2000)).toBe(true);
    expect(budgeted.result.candidates.some((c) => c.tool_id === "monid.strale.x402.url-to-text")).toBe(false);
    expect(budgeted.result.candidates[0]?.tool_id).toBe("monid.contextdev.web.scrape.markdown");
    expect(discover(catalog, noopHealthStore, { task: "read a web page", limit: 2 }).result.candidates).toHaveLength(2);
    const none = discover(catalog, noopHealthStore, { task: "zzqx" });
    expect(none.result.candidates).toHaveLength(0);
    expect(none.next_action).toBe("broaden_query");
  });
});
