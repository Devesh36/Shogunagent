/**
 * E29: the open public catalog — no auth, generated from the compiled catalog, the score formula
 * published, first-party labeled, and nothing a provider declared privately in provider.json.
 */
import { describe, expect, it } from "vitest";
import { allProviders } from "../connectors/index.js";
import { createApp } from "../src/app.js";
import { RANKING_FORMULA } from "../src/discover.js";
import { openCatalog, type OpenCatalogDetail, type OpenCatalogDocument } from "../src/open-catalog.js";
import { fullCatalog, makeDeps } from "./helpers.js";

/** Every private value any listed provider declared: legal name, contact, payout reference. */
function privateValues(): string[] {
  const out: string[] = [];
  for (const p of allProviders()) {
    const m = p.manifest as Record<string, unknown> | undefined;
    if (!m) continue;
    out.push(String(m["legal_name"]), String(m["payout_account_ref"]));
  }
  return out;
}

/** Contact values, as they would appear if published as a value (a homepage URL can legitimately
 * prefix a docs URL, so the check is on the value itself, quoted). */
function contactValues(): string[] {
  const out: string[] = [];
  for (const p of allProviders()) {
    const contact = ((p.manifest as Record<string, unknown> | undefined)?.["contact"] ?? {}) as Record<string, unknown>;
    for (const v of Object.values(contact)) out.push(String(v));
  }
  return out;
}

describe("open catalog (E29)", () => {
  it("GET /v1/catalog lists every compiled entry, without a key, with price, health, p50/p95, categories, label, data handling and the formula", async () => {
    const deps = makeDeps();
    const app = createApp(deps);
    const res = await app.request("/v1/catalog");
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toContain("public");
    const doc = (await res.json()) as OpenCatalogDocument;
    expect(doc.entry_count).toBe(deps.catalog.manifest.entry_count);
    expect(doc.catalog_hash).toBe(deps.catalog.manifest.hash);
    expect(doc.ranking.formula).toBe(RANKING_FORMULA);
    expect(doc.ranking.policy).toMatch(/same published formula/);
    for (const e of doc.entries) {
      expect(e.label).toBe(e.first_party ? "first_party" : "third_party");
      expect(typeof e.price.quote_micro_usd).toBe("number");
      expect(e.price.failures_cost).toBe(0);
      expect(e.health.p95_ms).toBeGreaterThanOrEqual(e.health.p50_ms);
      expect(e.categories.length).toBeGreaterThan(0);
      if (e.provider_id !== "monid") expect(e.data_handling?.statement.length).toBeGreaterThan(19);
    }
    expect(doc.entries.some((e) => e.first_party)).toBe(true);
    expect(doc.entries.some((e) => !e.first_party)).toBe(true);
  });

  it("never exposes legal names, contacts or payout references", async () => {
    const app = createApp(makeDeps());
    const bodies = [await (await app.request("/v1/catalog")).text(), await (await app.request("/catalog")).text()];
    for (const e of fullCatalog().entries()) bodies.push(await (await app.request(`/v1/catalog/${encodeURIComponent(e.tool_id)}`)).text());
    const secrets = privateValues();
    expect(secrets.length).toBeGreaterThan(0);
    for (const body of bodies) {
      for (const v of secrets) expect(body).not.toContain(v);
      for (const v of contactValues()) {
        expect(body).not.toContain(`"${v}"`);
        expect(body).not.toContain(`>${v}<`);
      }
      expect(body).not.toContain('"contact":');
      expect(body).not.toContain("payout_account_ref");
      expect(body).not.toContain("legal_name");
    }
  });

  it("GET /v1/catalog/{tool_id} adds schemas and the formula; an unknown id is 404 discover", async () => {
    const app = createApp(makeDeps());
    const ok = await app.request("/v1/catalog/shogunagent.state.commitments.list");
    expect(ok.status).toBe(200);
    const d = (await ok.json()) as OpenCatalogDetail;
    expect(d.first_party).toBe(true);
    expect(d.input_schema).toBeTypeOf("object");
    expect(d.ranking.formula).toBe(RANKING_FORMULA);
    const nf = await app.request("/v1/catalog/nope.nothing");
    expect(nf.status).toBe(404);
    expect(((await nf.json()) as { error: { next_action: string } }).error.next_action).toBe("discover");
  });

  it("filters by category, provider and first_party", () => {
    const deps = makeDeps();
    const state = openCatalog(deps.catalog, deps.health, { category: "state" });
    expect(state.entries.every((e) => (e.categories as string[]).includes("state"))).toBe(true);
    const fp = openCatalog(deps.catalog, deps.health, { first_party: false });
    expect(fp.entries.every((e) => !e.first_party)).toBe(true);
    const monid = openCatalog(deps.catalog, deps.health, { provider: "monid" });
    expect(monid.provider_count).toBe(1);
  });

  it("GET /catalog is server-rendered HTML with no script, every entry escaped, and a CSP that forbids scripts", async () => {
    const deps = makeDeps();
    const res = await createApp(deps).request("/catalog");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/html");
    expect(res.headers.get("content-security-policy")).toContain("default-src 'none'");
    const html = await res.text();
    expect(html).not.toMatch(/<script/i);
    expect(html).not.toMatch(/\son[a-z]+=/i);
    for (const e of deps.catalog.entries()) expect(html).toContain(e.tool_id);
    expect(html).toContain("first-party");
  });

  it("health reflects observations once enough runs are seen", () => {
    const deps = makeDeps();
    for (let i = 0; i < 25; i++) deps.health.observe("monid.exa.search", { latency_ms: 50, ok: i % 5 !== 0 });
    const e = openCatalog(deps.catalog, deps.health, { provider: "monid" }).entries.find((x) => x.tool_id === "monid.exa.search");
    expect(e?.health.source).toBe("observed");
    expect(e?.health.success_rate).toBeCloseTo(0.8, 5);
  });
});
