/**
 * E20: personalised discover. The personal factor is learned per principal from the run-record
 * stream, is neutral below PERSONAL_MIN_RUNS, never crosses principals and never favours a
 * first-party tool over a third-party tool with a better history (invariant 13).
 */
import { describe, expect, it } from "vitest";
import { compileCatalog, type CompiledCatalog } from "../src/catalog.js";
import { combineScore, PERSONAL_WEIGHT, RANKING_WEIGHTS, type DiscoverCandidate, type DiscoverResultBody } from "../src/discover.js";
import { callVerb } from "../src/engine.js";
import type { Envelope } from "../src/responses.js";
import { shortHash } from "../src/hash.js";
import { InMemoryPersonalStore, PERSONAL_MIN_RUNS, PersonalRanker, personalValue, taskCluster } from "../src/personal.js";
import { run } from "../src/run.js";
import { TOOL_RUN_RECORD_KEYS } from "../src/runlog.js";
import { Secret } from "../src/secret.js";
import type { TransportCall, TransportResult } from "../src/transport.js";
import { fullCatalog, makeDeps, synthEndpoint, synthProvider, type TestDeps } from "./helpers.js";

const KEY_A = "sa_principal_a_0123456789";
const KEY_B = "sa_principal_b_0123456789";
const TASK = "look up company revenue figures for a quarter";

function withPersonal(catalog: CompiledCatalog, responder?: (call: TransportCall) => TransportResult): TestDeps & { personal: PersonalRanker; store: InMemoryPersonalStore } {
  const store = new InMemoryPersonalStore();
  const deps = makeDeps({ catalog, ...(responder ? { responder } : {}) });
  const personal = new PersonalRanker(store, catalog.index);
  return Object.assign(deps, { personal, store });
}

async function discoverAs(deps: TestDeps, key: string | undefined, task = TASK): Promise<DiscoverResultBody> {
  const out = await callVerb(deps, "router.discover", { task, limit: 50 }, key ? new Secret(key) : undefined);
  expect(out.status).toBe(200);
  return (out.body as unknown as Envelope<DiscoverResultBody>).result;
}

async function runTimes(deps: TestDeps, key: string, toolId: string, n: number, taskHint = TASK, prefix = "r"): Promise<void> {
  for (let i = 0; i < n; i++) {
    await run(deps, { input: { tool_id: toolId, input: { q: "x" }, idempotency_key: `${prefix}-${toolId}-${i}`, task_hint: taskHint }, bearer: new Secret(key) });
  }
}

/** Two endpoints with the same text and price: one first-party, one third-party. */
function twinCatalog(): CompiledCatalog {
  const text = "Look up a company's revenue figures and financial statements for a given quarter.";
  return compileCatalog([
    synthProvider("fp", [synthEndpoint("revenue", { summary: text, cost: 2000, categories: ["finance"] })], { first_party: true }),
    synthProvider("tp", [synthEndpoint("revenue", { summary: text, cost: 2000, categories: ["finance"] })]),
  ]);
}

const ranks = (r: DiscoverResultBody) => r.candidates.map((c) => c.tool_id);
const byId = (r: DiscoverResultBody, id: string): DiscoverCandidate => {
  const c = r.candidates.find((x) => x.tool_id === id);
  if (!c) throw new Error(`missing ${id}`);
  return c;
};

describe("personal factor (E20)", () => {
  it("publishes its weight (0.15) inside a weight set that still sums to 1, and every candidate explains it", async () => {
    expect(PERSONAL_WEIGHT).toBe(0.15);
    expect(RANKING_WEIGHTS.personal).toBe(0.15);
    expect(RANKING_WEIGHTS.price + RANKING_WEIGHTS.health + RANKING_WEIGHTS.latency + RANKING_WEIGHTS.personal).toBeCloseTo(1, 6);
    const deps = withPersonal(fullCatalog());
    const r = await discoverAs(deps, KEY_A, "what did the user commit to");
    expect(r.ranking.formula).toContain("0.15*personal");
    expect(r.ranking.personal.min_runs).toBe(PERSONAL_MIN_RUNS);
    for (const c of r.candidates) {
      expect(c.personal).toMatchObject({ basis: "none", runs: 0, successes: 0 });
      expect(c.scoreBreakdown.personal).toBe(c.personal.value);
      expect(c.scoreBreakdown.personal).toBeGreaterThanOrEqual(0);
      expect(c.scoreBreakdown.personal).toBeLessThanOrEqual(1);
      expect(Math.abs(combineScore(c.scoreBreakdown) - c.score)).toBeLessThan(0.001);
    }
  });

  it("is neutral below the minimum: with < 5 runs the ranking and scores equal the anonymous ones", async () => {
    const catalog = twinCatalog();
    const deps = withPersonal(catalog, (call) => (call.provider.id === "fp" ? { status: 500, body: null } : { status: 200, body: { items: [1] } }));
    const anonymous = await discoverAs(deps, undefined);
    await runTimes(deps, KEY_A, "fp.revenue", PERSONAL_MIN_RUNS - 1);
    const a = await discoverAs(deps, KEY_A);
    expect(ranks(a)).toEqual(ranks(anonymous));
    expect(a.candidates.map((c) => c.score)).toEqual(anonymous.candidates.map((c) => c.score));
    expect(byId(a, "fp.revenue").personal).toMatchObject({ basis: "none", runs: PERSONAL_MIN_RUNS - 1, successes: 0 });
  });

  it("no self-preferencing: at equal fit and price a first-party tool with a worse history ranks below a third-party tool with a better one", async () => {
    const catalog = twinCatalog();
    const deps = withPersonal(catalog, (call) => (call.provider.id === "fp" ? { status: 500, body: null } : { status: 200, body: { items: [1] } }));
    const before = await discoverAs(deps, KEY_A);
    expect(byId(before, "fp.revenue").scoreBreakdown.task_fit).toBe(byId(before, "tp.revenue").scoreBreakdown.task_fit);
    expect(ranks(before)).toEqual(["fp.revenue", "tp.revenue"]); // a pure tie breaks on tool_id
    await runTimes(deps, KEY_A, "fp.revenue", 6);
    await runTimes(deps, KEY_A, "tp.revenue", 6);
    const after = await discoverAs(deps, KEY_A);
    expect(ranks(after)).toEqual(["tp.revenue", "fp.revenue"]);
    const fp = byId(after, "fp.revenue");
    const tp = byId(after, "tp.revenue");
    expect(fp.first_party).toBe(true);
    expect(fp.personal).toMatchObject({ basis: "principal", runs: 6, successes: 0, level: "task_cluster" });
    expect(tp.personal).toMatchObject({ basis: "principal", runs: 6, successes: 6, level: "task_cluster" });
    expect(tp.scoreBreakdown.personal).toBeGreaterThan(fp.scoreBreakdown.personal);
    expect(tp.score).toBeGreaterThan(fp.score);
  });

  it("never crosses principals: A's history does not change B's ranking or scores", async () => {
    const catalog = twinCatalog();
    const deps = withPersonal(catalog, (call) => (call.provider.id === "fp" ? { status: 200, body: { items: [1] } } : { status: 500, body: null }));
    const bBefore = await discoverAs(deps, KEY_B);
    await runTimes(deps, KEY_A, "tp.revenue", 10);
    await runTimes(deps, KEY_A, "fp.revenue", 10);
    const a = await discoverAs(deps, KEY_A);
    const bAfter = await discoverAs(deps, KEY_B);
    expect(byId(a, "tp.revenue").personal.basis).toBe("principal");
    expect(bAfter).toEqual(bBefore);
    for (const c of bAfter.candidates) expect(c.personal).toMatchObject({ basis: "none", runs: 0 });
    // The store itself holds A's rows under A only.
    expect(deps.store.profile(shortHash(KEY_B)).rows).toHaveLength(0);
    expect(deps.store.profile(shortHash(KEY_A)).rows.every((r) => r.runs > 0)).toBe(true);
  });

  it("learns per task cluster and falls back to the tool across clusters", async () => {
    const catalog = twinCatalog();
    const deps = withPersonal(catalog, () => ({ status: 200, body: { items: [1] } }));
    await runTimes(deps, KEY_A, "tp.revenue", 3, "look up company revenue figures", "x");
    await runTimes(deps, KEY_A, "tp.revenue", 3, "quarterly financial statements of a company", "y");
    const r = await discoverAs(deps, KEY_A, "company revenue figures");
    const tp = byId(r, "tp.revenue").personal;
    expect(tp).toMatchObject({ basis: "principal", level: "tool", runs: 6, successes: 6 });
    expect(r.ranking.personal.cluster).toBe(taskCluster(catalog.index, "company revenue figures"));
    expect(taskCluster(catalog.index, "Revenue figures, company!")).toBe(taskCluster(catalog.index, "company revenue figures"));
    expect(taskCluster(catalog.index, "the of and")).toBeNull();
  });

  it("counts only outcomes that say something about the tool; approvals, input errors and anonymous runs are not learned", async () => {
    const catalog = fullCatalog();
    const deps = withPersonal(catalog);
    await run(deps, { input: { tool_id: "shogunagent.actions.execute", input: { action: { kind: "post_message", channel: "c", text: "t" } }, idempotency_key: "l3", task_hint: TASK }, bearer: new Secret(KEY_A) });
    await run(deps, { input: { tool_id: "monid.tinyfish.web-search", input: { nope: 1 }, idempotency_key: "bad", task_hint: TASK }, bearer: new Secret(KEY_A) });
    await run(deps, { input: { tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "anon", task_hint: TASK } });
    expect(deps.store.profile(shortHash(KEY_A)).rows).toHaveLength(0);
    expect(deps.store.profile("anonymous").rows).toHaveLength(0);
    // The record the learner reads is still the body-less one.
    for (const r of deps.runLog.records) expect(Object.keys(r).sort()).toEqual([...TOOL_RUN_RECORD_KEYS].sort());
  });

  it("personalValue rewards success, cheap successes and successful hint-follows, within [0, 1]", () => {
    const perfect = personalValue({ runs: 5, successes: 5, hint_followed_runs: 0, hint_followed_successes: 0, cost_micro_usd: 0 });
    expect(perfect).toBe(1);
    expect(personalValue({ runs: 5, successes: 0, hint_followed_runs: 0, hint_followed_successes: 0, cost_micro_usd: 0 })).toBe(0);
    const pricey = personalValue({ runs: 5, successes: 5, hint_followed_runs: 0, hint_followed_successes: 0, cost_micro_usd: 5 * 50_000 });
    expect(pricey).toBeLessThan(perfect);
    const badHints = personalValue({ runs: 10, successes: 8, hint_followed_runs: 2, hint_followed_successes: 0, cost_micro_usd: 0 });
    const goodHints = personalValue({ runs: 10, successes: 8, hint_followed_runs: 2, hint_followed_successes: 2, cost_micro_usd: 0 });
    expect(goodHints).toBeGreaterThan(badHints);
  });
});
