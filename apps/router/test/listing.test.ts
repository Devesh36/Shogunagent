/**
 * E21: provider.json validation by the catalog compiler and the listing pipeline's five checks.
 */
import { describe, expect, it } from "vitest";
import { allProviders } from "../connectors/index.js";
import { defineEndpoint, z, zodInput } from "../connectors/define.js";
import type { EndpointDefinition, ProviderDefinition } from "../connectors/types.js";
import { CatalogCompileError, compileCatalog } from "../src/catalog.js";
import { checkListing, findSuperlatives, renderMarkdown, type CheckName, type ListingReport } from "../src/listing/check.js";
import { looksLikeBankData } from "../src/listing/manifest.js";
import { synthProvider } from "./helpers.js";

/** A listable endpoint: agent-oriented text, fixtures for success and failure, $0.001 per call. */
function goodEndpoint(id = "lookup", over: Partial<EndpointDefinition> = {}): EndpointDefinition {
  return defineEndpoint({
    id,
    meta: {
      summary: "Look up a company's registered address by its registration number.",
      description: "Returns the registered address, status and incorporation date of a company from its registration number. Use for: where is this company registered, is this company still active. $0.001 per call.",
      exclusions: "Not for people search or financial statements.",
      categories: ["data"],
    },
    request: { method: "GET", path: "/v1/company" },
    input: { schema: zodInput(z.object({ number: z.string().min(1) }).strict()) },
    output: {},
    usage: { model: "per_call", cost_micro_usd: 1000 },
    scope: { class: "read" },
    health: { p50_ms: 100, p95_ms: 300, success_rate: 0.99 },
    fixtures: [
      { scenario: "found", input: { number: "123" }, response: { status: 200, body: { items: [{ address: "1 Main St" }] } }, expect: { usage: { quantity: 1, cost_micro_usd: 1000 } } },
      { scenario: "vendor-500", input: { number: "123" }, response: { status: 500, body: null }, expect: { error: true } },
    ],
    ...over,
  });
}

function manifestFor(p: ProviderDefinition, over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    format: "shogunagent-provider/1",
    provider_id: p.id,
    legal_name: "Example Data Ltd",
    contact: { email: "listings@example.com" },
    payout_account_ref: "acct_1Nq2Example",
    pricing: p.endpoints.map((e) => ({ endpoint: e.id, model: "per_call", cost_micro_usd: e.usage.model === "per_call" ? e.usage.cost_micro_usd : 0 })),
    data_handling: { retention: "none", retention_days: 0, zero_retention: true, regions: ["eu"], statement: "Requests are processed in memory and not stored." },
    ...over,
  };
}

function listed(endpoints: EndpointDefinition[], manifestOver: Record<string, unknown> = {}): ProviderDefinition {
  const p = synthProvider("example", endpoints);
  return { ...p, manifest: manifestFor(p, manifestOver) };
}

const errorsIn = (r: ListingReport, check: CheckName) => r.problems.filter((p) => p.check === check && p.severity === "error").map((p) => p.message);

describe("provider.json (E21)", () => {
  it("every listed provider in the registry carries a valid manifest; the catalog publishes data handling, never contact or payout", () => {
    const catalog = compileCatalog(allProviders());
    for (const p of allProviders()) if (p.id !== "monid") expect(p.manifest, p.id).toBeDefined();
    const e = catalog.get("shogunagent.state.commitments.list");
    expect(e?.data_handling).toMatchObject({ retention: "user_account", zero_retention: false });
    expect(catalog.get("monid.exa.search")?.data_handling).toBeNull();
    const text = JSON.stringify(catalog.manifest);
    expect(text).not.toContain("payout_account_ref");
    expect(text).not.toContain("first-party:select");
    expect(text).not.toContain("legal_name");
  });

  it("the compiler rejects a manifest that is malformed, mismatched or carries bank data", () => {
    expect(() => compileCatalog([listed([goodEndpoint()])])).not.toThrow();
    const bad = (over: Record<string, unknown>) => () => compileCatalog([listed([goodEndpoint()], over)]);
    expect(bad({ provider_id: "other" })).toThrow(/does not match/);
    expect(bad({ payout_account_ref: "DE89370400440532013000" })).toThrow(/bank data/);
    expect(bad({ payout_account_ref: "bank-12345678" })).toThrow(/bank data/);
    expect(bad({ pricing: [{ endpoint: "lookup", model: "per_call", cost_micro_usd: 999 }] })).toThrow(/differs from the endpoint's usage model/);
    expect(bad({ pricing: [{ endpoint: "ghost", model: "per_call", cost_micro_usd: 0 }] })).toThrow(/not an endpoint/);
    expect(bad({ data_handling: { retention: "days", retention_days: 30, zero_retention: true, regions: ["eu"], statement: "Kept for thirty days for abuse review." } })).toThrow(/zero_retention/);
    expect(bad({ contact: {} })).toThrow(CatalogCompileError);
    expect(bad({ legal_name: undefined })).toThrow(/legal_name/);
    expect(looksLikeBankData("acct_1Nq2Example")).toBeNull();
    expect(looksLikeBankData("first-party:select")).toBeNull();
    expect(looksLikeBankData("routing-021000021")).not.toBeNull();
  });
});

describe("listing:check (E21)", () => {
  it("passes a well-formed listing with every check green and a per-endpoint summary", async () => {
    const r = await checkListing(listed([goodEndpoint()]), "connectors/example");
    expect(r.ok, JSON.stringify(r.problems)).toBe(true);
    expect(r.checks.map((c) => c.name)).toEqual(["schema", "fixtures", "scope", "descriptions", "price"]);
    expect(r.endpoints[0]).toMatchObject({ tool_id: "example.lookup", class: "read", level: "L1", quote_micro_usd: 1000, fixtures: 2, fixtures_replayed: 2 });
    expect(renderMarkdown([r])).toContain("| example |");
  });

  it("schema: a provider without provider.json is not listable", async () => {
    const r = await checkListing(synthProvider("example", [goodEndpoint()]), "connectors/example");
    expect(r.ok).toBe(false);
    expect(errorsIn(r, "schema").join()).toMatch(/provider.json is missing/);
  });

  it("scope: a send declared as a read, or a write verb declared as a read, fails", async () => {
    const send = goodEndpoint("messages.send");
    const write = goodEndpoint("records.update", { request: { method: "PATCH", path: "/v1/records" } });
    const r = await checkListing(listed([send, write]), "d");
    expect(errorsIn(r, "scope").some((m) => /names a send.*external_send/.test(m))).toBe(true);
    expect(errorsIn(r, "scope").some((m) => /names a write/.test(m))).toBe(true);
    expect(errorsIn(r, "scope").some((m) => /PATCH is a write/.test(m))).toBe(true);
    const pay = goodEndpoint("invoice.pay", { scope: { class: "user_data_write" } });
    expect(errorsIn(await checkListing(listed([pay]), "d"), "scope").join()).toMatch(/payment/);
  });

  it("descriptions: length, tasks, exclusions, cost class and superlatives are linted", async () => {
    const long = goodEndpoint("a", { meta: { ...goodEndpoint().meta, description: `${"Returns data. ".repeat(40)}Use for: x. Free.` } });
    const noTask = goodEndpoint("b", { meta: { ...goodEndpoint().meta, description: "Returns the registered address. $0.001 per call." } });
    const noExcl = goodEndpoint("c", { meta: { summary: "Look up an address.", description: "Returns an address. Use for: where is X. Free read.", categories: ["data"] } });
    const hype = goodEndpoint("d", { meta: { ...goodEndpoint().meta, description: "The best, industry-leading address API. Use for: where is X. Free read." } });
    const noCost = goodEndpoint("e", { meta: { ...goodEndpoint().meta, description: "Returns an address. Use for: where is X." } });
    const r = await checkListing(listed([long, noTask, noExcl, hype, noCost]), "d");
    const errs = errorsIn(r, "descriptions");
    expect(errs.some((m) => /≤ 400/.test(m))).toBe(true);
    expect(errs.some((m) => /Use for/.test(m))).toBe(true);
    expect(errs.some((m) => /exclusions is required/.test(m))).toBe(true);
    expect(errs.some((m) => /superlatives.*best.*industry-leading/.test(m))).toBe(true);
    expect(errs.some((m) => /cost class/.test(m))).toBe(true);
    expect(findSuperlatives("a powerful, seamless tool")).toEqual(["seamless", "powerful"]);
    expect(findSuperlatives("returns the best match")).toEqual(["best"]);
    expect(findSuperlatives("lists the leaderboard")).toEqual([]);
  });

  it("fixtures: a failure fixture the connector treats as success, a missing success fixture and output drift fail", async () => {
    const noFailureDetect = goodEndpoint("a", {
      fixtures: [
        { scenario: "ok", input: { number: "1" }, response: { status: 200, body: { items: [1] } } },
        { scenario: "failed-in-200", input: { number: "1" }, response: { status: 200, body: { ok: false } }, expect: { error: true } },
      ],
    });
    const onlyErrors = goodEndpoint("b", { fixtures: [{ scenario: "500", input: { number: "1" }, response: { status: 500, body: null }, expect: { error: true } }] });
    const drift = goodEndpoint("c", { fixtures: [{ scenario: "drift", input: { number: "1" }, response: { status: 200, body: { items: [1] } }, expect: { output: { items: [2] } } }] });
    const r = await checkListing(listed([noFailureDetect, onlyErrors, drift]), "d");
    const errs = errorsIn(r, "fixtures");
    expect(errs.some((m) => /failed-in-200.*treats the response as a success/.test(m))).toBe(true);
    expect(errs.some((m) => /no successful fixture/.test(m))).toBe(true);
    expect(errs.some((m) => /differs from expect.output/.test(m))).toBe(true);
  });

  it("fixtures: L2 asks once then runs, L3 never reaches the transport", async () => {
    const draft = goodEndpoint("drafts.create", { scope: { class: "user_data_write" }, meta: { ...goodEndpoint().meta, description: "Creates a draft. Use for: draft a note. L2 (confirm once). $0.001 per call." } });
    const send = goodEndpoint("messages.send", { scope: { class: "external_send" }, meta: { ...goodEndpoint().meta, description: "Sends a message. Use for: send this. Always L3: approval URL first. $0.001 per call." } });
    const r = await checkListing(listed([draft, send]), "d");
    expect(r.ok, JSON.stringify(r.problems)).toBe(true);
    expect(r.endpoints.map((e) => e.level)).toEqual(["L2", "L3"]);
  });

  it("price: observed usage must follow the declared model; a quote far from observed cost is flagged", async () => {
    const perResult: EndpointDefinition = goodEndpoint("search", {
      usage: { model: "per_result", cost_micro_usd_per_result: 100, typical_results: 500 },
      fixtures: [
        { scenario: "three", input: { number: "1" }, response: { status: 200, body: { items: [1, 2, 3] } }, expect: { usage: { quantity: 3, cost_micro_usd: 300 } } },
        { scenario: "500", input: { number: "1" }, response: { status: 500, body: null }, expect: { error: true } },
      ],
    });
    const p = listed([perResult], { pricing: [{ endpoint: "search", model: "per_result", cost_micro_usd_per_result: 100 }] });
    const r = await checkListing(p, "d");
    expect(r.ok, JSON.stringify(r.problems)).toBe(true);
    expect(r.problems.some((x) => x.check === "price" && x.severity === "warning" && /over 10×/.test(x.message))).toBe(true);

    const lying: EndpointDefinition = goodEndpoint("x", {
      usage: {
        model: "per_unit",
        unit: "1k_chars",
        cost_micro_usd_per_unit: 50,
        typical_units: 4,
        units: () => 3,
      },
      output: { isEmpty: () => false },
      fixtures: [{ scenario: "u", input: { number: "1" }, response: { status: 200, body: { text: "abc" } }, expect: { usage: { quantity: 3, cost_micro_usd: 999 } } }],
    });
    const r2 = await checkListing(listed([lying], { pricing: [{ endpoint: "x", model: "per_unit", unit: "1k_chars", cost_micro_usd_per_unit: 50 }] }), "d");
    expect(errorsIn(r2, "fixtures").some((m) => /expect.usage says 3 → 999/.test(m))).toBe(true);
  });

  it("every provider directory in the registry passes", async () => {
    for (const p of allProviders()) {
      if (p.id === "monid") continue;
      const r = await checkListing(p, `connectors/${p.id}`);
      expect(r.ok, `${p.id}: ${JSON.stringify(r.problems.filter((x) => x.severity === "error"))}`).toBe(true);
    }
  });
});
