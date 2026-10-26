/** E10: the agent-readable documents are generated from code and config (invariant 12) — the
 * server card from the tool table, the agent card from SKILL.md and signed with Ed25519, the
 * OpenAPI document from the route table, status from the rolling window and incidents.json,
 * pricing from the metering table — and the static files at the repository root repeat the same
 * identity. Snapshots pin the generated shapes. */
import { generateKeyPairSync, createPublicKey } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { DEFAULT_CAPS } from "../src/caps.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS, RUN_MARKUP_BPS, classifyCall, pricingDocument, quoteCost } from "../src/pricing.js";
import { AGENT_SKILLS, agentCard, canonicalJson, parseSigningKey, signAgentCard, verifyAgentCard } from "../src/wellknown/agent-card.js";
import { COMMITTED_INCIDENTS, FIXTURE_TOOL_TABLE, createWellknown } from "../src/wellknown/app.js";
import { DISPLAY_NAME, REGISTRY_NAME, SHORT_DESCRIPTION, VERSION, HOSTED_MCP_URL } from "../src/wellknown/identity.js";
import { LLMS_FULL_TXT, LLMS_TXT } from "../src/wellknown/llms-content.generated.js";
import { mcpConfigDocument } from "../src/wellknown/mcp-config.js";
import { openapiDocument } from "../src/wellknown/openapi.js";
import { serverCard } from "../src/wellknown/server-card.js";
import { RollingWindow, healthDocument, incidentProblems, serviceStatus, statusDocument, type Incident } from "../src/wellknown/status.js";
import { annotationsFor, readOnlyToolNames, toolTableProblems } from "../src/wellknown/tool-table.js";
import { BASE, makeAgentRig, mcp, registerAgent } from "./agent-helpers.js";
// The generators' shared parser, so the test re-derives what the committed JSON holds.
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore — plain ESM without types
import { renderAgentSkills } from "../../../scripts/lib/agent-skills.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..", "..");
const read = (rel: string): string => readFileSync(join(repo, rel), "utf8");
const ctx = { publicBaseUrl: BASE };

/** The §7.1 list of docs/implementation-plan.md: every backticked tool name in that section. */
function planCore(): string[] {
  const plan = read("docs/implementation-plan.md");
  const section = plan.split(/^### 7\.1 /m)[1]?.split(/^### /m)[0] ?? "";
  return [...new Set([...section.matchAll(/`([a-z_]+(?:\.[a-z_]+)+)`/g)].map((m) => m[1] ?? "").filter((n) => !n.startsWith("meeting.")))].sort();
}

describe("tool table (fixture pending E4)", () => {
  it("validates and names exactly the §7.1 core tools", () => {
    expect(toolTableProblems(FIXTURE_TOOL_TABLE)).toEqual([]);
    expect(FIXTURE_TOOL_TABLE.tools.map((t) => t.name).sort()).toEqual(planCore());
    expect(FIXTURE_TOOL_TABLE.source).toBe("fixture");
  });

  it("agrees with the metering table on every tool (invariant 10)", () => {
    for (const t of FIXTURE_TOOL_TABLE.tools) expect(classifyCall("tools/call", t.name), t.name).toBe(t.cost_unit);
    expect(readOnlyToolNames(FIXTURE_TOOL_TABLE)).toHaveLength(18);
  });

  it("rejects what the shape forbids", () => {
    const base = JSON.parse(JSON.stringify(FIXTURE_TOOL_TABLE)) as typeof FIXTURE_TOOL_TABLE;
    expect(toolTableProblems({ ...base, schema: "nope" })).toContain("schema must be shogunagent/tool-table/v1");
    const dup = { ...base, tools: [...base.tools, base.tools[0]] };
    expect(toolTableProblems(dup).some((p) => p.includes("listed twice"))).toBe(true);
    const billedRead = { ...base, tools: base.tools.map((t) => (t.name === "memory.search" ? { ...t, cost_unit: "model_op" } : t)) };
    expect(toolTableProblems(billedRead).some((p) => p.includes("does not match"))).toBe(true);
    const badName = { ...base, tools: base.tools.map((t, i) => (i === 0 ? { ...t, name: "Memory.Search" } : t)) };
    expect(toolTableProblems(badName).some((p) => p.includes("<group>.<verb>"))).toBe(true);
    expect(toolTableProblems("x")).toEqual(["the table must be a JSON object"]);
  });

  it("derives annotations by the one rule: Read → readOnly, L1 → not destructive, L3 → destructive + open world", () => {
    expect(annotationsFor("read")).toEqual({ readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false });
    expect(annotationsFor("L1")).toEqual({ readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false });
    expect(annotationsFor("L2")).toEqual({ readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false });
    expect(annotationsFor("L3")).toEqual({ readOnlyHint: false, destructiveHint: true, idempotentHint: false, openWorldHint: true });
  });

  it("gives every core tool the annotations the core's tools/list carries (E34, directory checklist X4)", () => {
    const here = dirname(fileURLToPath(import.meta.url));
    const frozen = JSON.parse(readFileSync(join(here, "../../../crates/shogun-mcp/tests/snapshots/core_tools.json"), "utf8")) as { tools: Array<{ name: string; annotations: Record<string, boolean> }> };
    const core = new Map(frozen.tools.map((t) => [t.name, t.annotations]));
    const card = serverCard(FIXTURE_TOOL_TABLE, ctx) as { tools: Array<{ name: string; annotations: unknown }> };
    const compared = card.tools.filter((t) => core.has(t.name));
    expect(compared.length).toBeGreaterThan(0);
    for (const tool of compared) expect({ name: tool.name, annotations: tool.annotations }).toEqual({ name: tool.name, annotations: core.get(tool.name) });
    expect(compared.map((t) => t.name)).toEqual(expect.arrayContaining(["memory.ingest", "memory.append_note"]));
  });
});

describe("/.well-known/mcp/server-card.json (E10-T1)", () => {
  it("matches the snapshot rendered from the tool table", async () => {
    const card = serverCard(FIXTURE_TOOL_TABLE, ctx);
    await expect(JSON.stringify(card, null, 2) + "\n").toMatchFileSnapshot("../fixtures/server-card.snapshot.json");
  });

  it("carries every tool with the derived annotations and the identity", () => {
    const card = serverCard(FIXTURE_TOOL_TABLE, ctx) as { tools: Array<{ name: string; annotations: unknown; inputSchema: unknown }>; serverInfo: { name: string; version: string }; transport: { url: string }; toolCount: number };
    expect(card.serverInfo).toEqual({ name: REGISTRY_NAME, title: DISPLAY_NAME, version: VERSION });
    expect(card.transport.url).toBe(`${BASE}/mcp`);
    expect(card.toolCount).toBe(FIXTURE_TOOL_TABLE.tools.length);
    for (const t of FIXTURE_TOOL_TABLE.tools) {
      const rendered = card.tools.find((x) => x.name === t.name);
      expect(rendered?.annotations, t.name).toEqual(annotationsFor(t.level));
      expect(rendered?.inputSchema).toEqual(t.input_schema);
    }
    const exec = card.tools.find((x) => x.name === "actions.execute")?.annotations as { destructiveHint: boolean; openWorldHint: boolean };
    expect(exec).toMatchObject({ destructiveHint: true, openWorldHint: true });
  });

  it("is served without auth with the rig, and the MCP URL can be overridden", async () => {
    const rig = makeAgentRig({ wellknown: { mcpUrl: HOSTED_MCP_URL } });
    const res = await rig.app.request("/.well-known/mcp/server-card.json");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("application/json");
    const body = (await res.json()) as { transport: { url: string }; tools: unknown[] };
    expect(body.transport.url).toBe(HOSTED_MCP_URL);
    expect(body.tools).toHaveLength(FIXTURE_TOOL_TABLE.tools.length);
    expect(rig.logLines.some((l) => l.startsWith("GET /.well-known/mcp/server-card.json 200"))).toBe(true);
  });
});

describe("/.well-known/agent-card.json (E10-T3)", () => {
  const seedHex = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

  it("skills are what SKILL.md's 'When to use' list yields today", () => {
    const derived = renderAgentSkills(read("SKILL.md")) as typeof AGENT_SKILLS;
    expect(AGENT_SKILLS).toEqual(derived);
    expect(AGENT_SKILLS.skill_version).toBe(VERSION);
    expect(AGENT_SKILLS.skills.length).toBeGreaterThanOrEqual(4);
    for (const s of AGENT_SKILLS.skills) {
      expect(s.id).toMatch(/^[a-z0-9-]+$/);
      expect(s.description.length).toBeGreaterThan(10);
      expect(s.description).not.toContain("**");
      expect(s.description).not.toContain("`");
    }
    // The tool names SKILL.md cites are real wire names: the core table, the router's verbs, or
    // the handoff pair (E35, outside core; crates/shogun-mcp/src/handoff_api.rs).
    const known = new Set([...FIXTURE_TOOL_TABLE.tools.map((t) => t.name), "router.run", "router.discover", "router.inspect", "handoff.create", "handoff.accept"]);
    for (const tag of AGENT_SKILLS.skills.flatMap((s) => s.tags)) expect(known.has(tag), tag).toBe(true);
  });

  it("is unsigned when no key is configured: no signatures field at all", async () => {
    const rig = makeAgentRig();
    const res = await rig.app.request("/.well-known/agent-card.json");
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.protocolVersion).toBe("1.0");
    expect(body.name).toBe(DISPLAY_NAME);
    expect(body.version).toBe(VERSION);
    expect(body.url).toBe(`${BASE}/mcp`);
    expect(body).not.toHaveProperty("signatures");
    expect(verifyAgentCard(body)).toEqual({ ok: false, reason: "unsigned" });
    expect((body.skills as unknown[]).length).toBe(AGENT_SKILLS.skills.length);
    expect((body.security as unknown[])).toEqual([{ agentKey: ["read", "write", "run"] }, {}]);
  });

  it("is signed with the configured Ed25519 key and the signature verifies from the card alone", async () => {
    const key = parseSigningKey(seedHex);
    expect(key).toBeDefined();
    const rig = makeAgentRig({ wellknown: { agentCardSigningKey: key! } });
    const body = (await (await rig.app.request("/.well-known/agent-card.json")).json()) as Record<string, unknown>;
    const sigs = body.signatures as Array<{ protected: string; signature: string }>;
    expect(sigs).toHaveLength(1);
    const header = JSON.parse(Buffer.from(sigs[0]?.protected ?? "", "base64url").toString()) as { alg: string; kid: string; jwk: { kty: string; crv: string; x: string } };
    expect(header.alg).toBe("EdDSA");
    expect(header.jwk).toMatchObject({ kty: "OKP", crv: "Ed25519" });
    const verdict = verifyAgentCard(body);
    expect(verdict.ok).toBe(true);
    if (verdict.ok) expect(verdict.kid).toBe(header.kid);
    // With the public key supplied out of band too.
    expect(verifyAgentCard(body, createPublicKey(key!)).ok).toBe(true);
  });

  it("rejects a tampered card, a wrong key and a malformed signature", () => {
    const key = parseSigningKey(seedHex)!;
    const signed = signAgentCard(agentCard(ctx), key);
    expect(verifyAgentCard({ ...signed, version: "9.9.9" })).toEqual({ ok: false, reason: "bad_signature" });
    const { publicKey: other } = generateKeyPairSync("ed25519");
    expect(verifyAgentCard(signed, other)).toEqual({ ok: false, reason: "bad_signature" });
    expect(verifyAgentCard({ ...signed, signatures: [{ protected: "!!", signature: "x" }] }).ok).toBe(false);
    expect(verifyAgentCard({ ...signed, signatures: [] })).toEqual({ ok: false, reason: "unsigned" });
    const rs = JSON.parse(Buffer.from((signed.signatures as Array<{ protected: string }>)[0]!.protected, "base64url").toString()) as Record<string, unknown>;
    const badAlg = Buffer.from(JSON.stringify({ ...rs, alg: "RS256" })).toString("base64url");
    expect(verifyAgentCard({ ...signed, signatures: [{ protected: badAlg, signature: "AA" }] })).toEqual({ ok: false, reason: "unsupported_alg" });
    // Signing does not mutate the input and is deterministic for Ed25519.
    const again = signAgentCard(agentCard(ctx), key);
    expect(again).toEqual(signed);
  });

  it("parses the key as hex, base64, base64url or PKCS#8 PEM, and refuses anything else", () => {
    const fromHex = parseSigningKey(seedHex)!;
    const seed = Buffer.from(seedHex, "hex");
    const pub = (k: ReturnType<typeof parseSigningKey>) => createPublicKey(k!).export({ format: "jwk" });
    expect(pub(parseSigningKey(seed.toString("base64")))).toEqual(pub(fromHex));
    expect(pub(parseSigningKey(seed.toString("base64url")))).toEqual(pub(fromHex));
    expect(pub(parseSigningKey(fromHex.export({ format: "pem", type: "pkcs8" }).toString()))).toEqual(pub(fromHex));
    expect(parseSigningKey(undefined)).toBeUndefined();
    expect(parseSigningKey("   ")).toBeUndefined();
    expect(() => parseSigningKey("not-a-key")).toThrow(/Ed25519/);
    expect(() => parseSigningKey(generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey.export({ format: "pem", type: "pkcs8" }).toString())).toThrow(/Ed25519/);
  });

  it("canonicalises JSON per RFC 8785: sorted members, no whitespace, undefined dropped", () => {
    expect(canonicalJson({ b: 1, a: [true, null, { z: "s", y: 2.5 }], c: undefined })).toBe('{"a":[true,null,{"y":2.5,"z":"s"}],"b":1}');
    expect(canonicalJson("é\n")).toBe(JSON.stringify("é\n"));
    expect(() => canonicalJson({ n: Number.NaN })).toThrow();
  });
});

describe("/openapi.json (E10-T6 scaffolding)", () => {
  it("matches the snapshot and names every agent route that exists today", async () => {
    const doc = openapiDocument(FIXTURE_TOOL_TABLE, ctx) as { openapi: string; paths: Record<string, Record<string, unknown>>; info: Record<string, unknown> };
    expect(doc.openapi).toBe("3.1.0");
    for (const p of ["/.well-known/pricing.json", "/.well-known/mcp/server-card.json", "/.well-known/agent-card.json", "/.well-known/status.json", "/health", "/openapi.json", "/llms.txt",
      "/v1/agents/challenge", "/v1/agents", "/v1/agents/me", "/v1/keys/{id}/revoke", "/v1/keys/{id}",
      // E51: write-only ingest keys and the payments REST relay
      "/v1/keys", "/v1/payments/mandates", "/v1/payments/mandates/list", "/v1/payments/mandates/request", "/v1/payments/mandates/revoke", "/v1/payments/spend", "/v1/wallet", "/v1/wallet/topup", "/v1/stripe/webhook", "/v1/quote", "/v1/receipts", "/v1/receipts/{id}/dispute", "/mcp",
      "/v1/router/discover", "/v1/router/inspect", "/v1/router/run",
      // E19, E3-T2, E23
      "/v1/statements", "/v1/commit", "/v1/agents/me/licence", "/v1/admin/commits", "/v1/admin/adjustments", "/v1/admin/disputes/{id}/resolve",
      "/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-authorization-server", "/oauth/authorize", "/oauth/token", "/oauth/revoke",
      "/privacy", "/terms",
      // E29: the open catalog (apps/router) and provider payouts
      "/v1/catalog", "/v1/catalog/{tool_id}", "/catalog", "/v1/internal/router/settlements", "/v1/internal/router/connector-token", "/v1/providers/me", "/v1/providers/me/connect",
      "/v1/providers/connect/return", "/v1/providers/connect/refresh", "/v1/providers/me/statements", "/v1/admin/providers",
      "/v1/admin/providers/{id}/adjustments", "/v1/admin/providers/{id}/disputes/{entry}/resolve", "/v1/admin/payouts/run",
      // E26
      "/legal/dpa", "/legal/zdr", "/legal/sla", "/v1/admin/tenants", "/v1/admin/tenants/{id}", "/v1/admin/tenants/{id}/principals", "/v1/admin/tenants/{id}/scim-token",
      "/sso/{tenant}/login", "/sso/{tenant}/oidc/callback", "/sso/{tenant}/saml/acs", "/v1/tenant/audit/export", "/scim/v2/Users", "/scim/v2/Users/{id}", "/scim/v2/Groups", "/scim/v2/ServiceProviderConfig"]) {
      expect(doc.paths[p], p).toBeDefined();
    }
    expect(Object.keys(doc.paths["/mcp"] ?? {}).sort()).toEqual(["delete", "get", "post"]);
    expect((doc.paths["/v1/router/run"]?.post as { "x-served-by": string })["x-served-by"]).toBe("apps/router");
    await expect(JSON.stringify(doc, null, 2) + "\n").toMatchFileSnapshot("../fixtures/openapi.snapshot.json");
  });

  it("is served and carries the pricing constants it was rendered from", async () => {
    const rig = makeAgentRig();
    const res = await rig.app.request("/openapi.json");
    expect(res.status).toBe(200);
    const doc = (await res.json()) as { info: { "x-pricing": Record<string, unknown>; "x-mcp": { tools: Array<{ name: string }> } }; servers: Array<{ url: string }> };
    expect(doc.servers).toEqual([{ url: BASE }]);
    expect(doc.info["x-pricing"]).toMatchObject({ ingest_micro_usd_per_event: RATE_MICROS.ingest, model_op_micro_usd: RATE_MICROS.model_op, run_markup_bps: RUN_MARKUP_BPS, free_allowance_micro_usd: FREE_ALLOWANCE_MICROS });
    expect(doc.info["x-mcp"].tools.map((t) => t.name).sort()).toEqual(planCore());
  });
});

describe("/.well-known/pricing.json is one source with the metering (E10-T7)", () => {
  it("publishes exactly the rates quoteCost charges", () => {
    const doc = pricingDocument({ caps: DEFAULT_CAPS }) as { units: Record<string, { micro_usd: number | null; usd_per_1000?: number; markup_bps?: number; formula?: string }>; free_allowance_micro_usd: number };
    for (const unit of ["ingest", "model_op", "storage", "read", "protocol"] as const) {
      expect(doc.units[unit]?.micro_usd, unit).toBe(RATE_MICROS[unit]);
      if (unit !== "storage") expect(quoteCost(unit, 1).cost_micro_usd, unit).toBe(doc.units[unit]?.micro_usd);
    }
    expect(doc.units.ingest?.usd_per_1000).toBe(0.5);
    expect(doc.units.model_op?.usd_per_1000).toBe(0.1);
    expect(doc.units.run?.markup_bps).toBe(RUN_MARKUP_BPS);
    // The published formula is the one quoteCost applies.
    expect(quoteCost("run", 1, 10_000).cost_micro_usd).toBe(Math.ceil(10_000 * (1 + RUN_MARKUP_BPS / 10_000)));
    expect(doc.units.run?.formula).toContain(String(RUN_MARKUP_BPS / 10_000));
    expect(doc.free_allowance_micro_usd).toBe(FREE_ALLOWANCE_MICROS);
  });
});

describe("/.well-known/status.json and /health (E10-T7)", () => {
  const t0 = Date.parse("2026-10-25T12:00:00Z");

  it("the rolling window reports p50 / p95 / error rate over five minutes and forgets older minutes", () => {
    const w = new RollingWindow();
    for (let i = 1; i <= 100; i++) w.observe(i % 10 === 0 ? 500 : 200, i, t0 + i);
    let s = w.snapshot(t0 + 1_000);
    expect(s.requests).toBe(100);
    expect(s.p50_ms).toBe(50);
    expect(s.p95_ms).toBe(95);
    expect(s.error_rate).toBe(0.1);
    expect(s.rate_4xx).toBe(0);
    expect(s.window_seconds).toBe(300);
    w.observe(404, 7, t0 + 3 * 60_000);
    s = w.snapshot(t0 + 3 * 60_000);
    expect(s.requests).toBe(101);
    expect(s.rate_4xx).toBeCloseTo(1 / 101, 4);
    // Six minutes on: the first minute is gone, the 404 remains.
    s = w.snapshot(t0 + 6 * 60_000);
    expect(s.requests).toBe(1);
    expect(s.p50_ms).toBe(7);
    expect(s.error_rate).toBe(0);
    expect(new RollingWindow().snapshot(t0)).toMatchObject({ requests: 0, p50_ms: null, p95_ms: null, error_rate: 0, rps: 0 });
  });

  it("judges degraded only on enough traffic, and incidents override the window", () => {
    const quietBad = { window_seconds: 300, requests: 5, rps: 0, p50_ms: 1, p95_ms: 9_000, error_rate: 0.6, rate_4xx: 0, sampled: 5 };
    expect(serviceStatus(quietBad, [])).toBe("ok");
    expect(serviceStatus({ ...quietBad, requests: 50 }, [])).toBe("degraded");
    expect(serviceStatus({ ...quietBad, requests: 50, error_rate: 0, p95_ms: 2_001 }, [])).toBe("degraded");
    expect(serviceStatus({ ...quietBad, requests: 50, error_rate: 0.05, p95_ms: 2_000 }, [])).toBe("ok");
    const open = (severity: "minor" | "major"): Incident => ({ id: `inc_${severity}`, title: "x", severity, started_at: "2026-10-25T11:00:00Z", resolved_at: null, affected: ["/mcp"], updates: [] });
    expect(serviceStatus({ ...quietBad, requests: 50, error_rate: 0, p95_ms: 10 }, [open("minor")])).toBe("degraded");
    expect(serviceStatus({ ...quietBad, requests: 50, error_rate: 0, p95_ms: 10 }, [open("major")])).toBe("major_outage");
    expect(serviceStatus({ ...quietBad, requests: 50, error_rate: 0, p95_ms: 10 }, [{ ...open("major"), resolved_at: "2026-10-25T11:30:00Z" }])).toBe("ok");
  });

  it("incidents.json is committed, empty, and the validator rejects bad entries", () => {
    expect(COMMITTED_INCIDENTS).toEqual([]);
    expect(incidentProblems([])).toEqual([]);
    expect(incidentProblems({})).toEqual(["incidents.json must be a JSON array"]);
    const good: Incident = { id: "inc_2026-10-25-01", title: "Elevated 5xx on /mcp", severity: "minor", started_at: "2026-10-25T10:00:00Z", resolved_at: "2026-10-25T10:20:00Z", affected: ["/mcp"], updates: [{ at: "2026-10-25T10:20:00Z", text: "Resolved." }] };
    expect(incidentProblems([good])).toEqual([]);
    expect(incidentProblems([{ ...good, id: "2026" }]).some((p) => p.includes("inc_<slug>"))).toBe(true);
    expect(incidentProblems([good, good]).some((p) => p.includes("twice"))).toBe(true);
    expect(incidentProblems([{ ...good, severity: "sev1" }]).some((p) => p.includes("severity"))).toBe(true);
    expect(incidentProblems([{ ...good, started_at: "yesterday" }]).some((p) => p.includes("started_at"))).toBe(true);
  });

  it("renders status with open incidents first and health 503 only in a major outage", () => {
    const w = new RollingWindow();
    const now = () => new Date(t0);
    const older: Incident = { id: "inc_old", title: "old", severity: "minor", started_at: "2026-01-01T00:00:00Z", resolved_at: "2026-01-01T01:00:00Z", affected: [], updates: [] };
    const recent: Incident = { id: "inc_recent", title: "recent", severity: "minor", started_at: "2026-10-20T00:00:00Z", resolved_at: "2026-10-20T01:00:00Z", affected: [], updates: [] };
    const open: Incident = { id: "inc_open", title: "open", severity: "major", started_at: "2026-10-01T00:00:00Z", resolved_at: null, affected: ["/mcp"], updates: [] };
    const doc = statusDocument({ window: w, incidents: [older, recent, open], now, ctx }) as { status: string; incidents: Incident[]; window: { requests: number }; checked_at: string };
    expect(doc.status).toBe("major_outage");
    expect(doc.incidents.map((i) => i.id)).toEqual(["inc_open", "inc_recent"]); // the January one is past 90 days
    expect(doc.checked_at).toBe("2026-10-25T12:00:00.000Z");
    expect(healthDocument({ window: w, incidents: [open], now, ctx }).status).toBe(503);
    expect(healthDocument({ window: w, incidents: [recent], now, ctx })).toMatchObject({ status: 200, body: { ok: true, status: "ok", open_incidents: 0 } });
  });

  it("observes the metered routes through the agent app: a call to /mcp shows up in /health", async () => {
    const rig = makeAgentRig();
    const before = (await (await rig.app.request("/health")).json()) as { window_requests: number; ok: boolean };
    expect(before).toMatchObject({ ok: true, window_requests: 0 });
    const { key } = await registerAgent(rig);
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.search" });
    const after = (await (await rig.app.request("/health")).json()) as { window_requests: number; p50_ms: number | null };
    // registration (POST /v1/agents, GET challenge) and the /mcp call were all observed.
    expect(after.window_requests).toBeGreaterThanOrEqual(3);
    expect(after.p50_ms).not.toBeNull();
    const status = (await (await rig.app.request("/.well-known/status.json")).json()) as { schema: string; status: string; incidents: unknown[]; window: { requests: number } };
    expect(status.schema).toBe("shogunagent/status/v1");
    expect(status.status).toBe("ok");
    expect(status.incidents).toEqual([]);
    expect(status.window.requests).toBe(after.window_requests);
    // The documents themselves are not counted as traffic.
    const again = (await (await rig.app.request("/health")).json()) as { window_requests: number };
    expect(again.window_requests).toBe(after.window_requests);
  });
});

describe("/.well-known/mcp-config (Smithery) and llms.txt", () => {
  it("serves a JSON Schema with an optional key and the connection facts", async () => {
    const rig = makeAgentRig();
    const res = await rig.app.request("/.well-known/mcp-config");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("schema+json");
    const doc = (await res.json()) as Record<string, unknown>;
    expect(doc).toEqual(mcpConfigDocument(ctx));
    expect(doc.required).toEqual([]);
    expect((doc["x-mcp"] as { url: string }).url).toBe(`${BASE}/mcp`);
  });

  it("serves the generated llms.txt and llms-full.txt, equal to the committed root files", async () => {
    const rig = makeAgentRig();
    const short = await rig.app.request("/llms.txt");
    const full = await rig.app.request("/llms-full.txt");
    expect(short.status).toBe(200);
    expect(short.headers.get("content-type")).toContain("text/plain");
    expect(await short.text()).toBe(LLMS_TXT);
    expect(await full.text()).toBe(LLMS_FULL_TXT);
    expect(LLMS_TXT).toBe(read("llms.txt"));
    expect(LLMS_FULL_TXT).toBe(read("llms-full.txt"));
    expect(LLMS_TXT.startsWith("# ShogunAgent\n")).toBe(true);
    expect(LLMS_TXT).toContain("/.well-known/mcp/server-card.json");
    expect(LLMS_FULL_TXT).toContain("<!-- file: SKILL.md -->");
  });
});

describe("the static files at the repository root repeat the same identity", () => {
  it("server.json (E10-T2)", () => {
    const s = JSON.parse(read("server.json")) as { name: string; title: string; description: string; version: string; remotes: Array<{ type: string; url: string }>; packages: Array<{ identifier: string; version: string; transport: { type: string } }> };
    expect(s.name).toBe(REGISTRY_NAME);
    expect(s.title).toBe(DISPLAY_NAME);
    expect(s.description).toBe(SHORT_DESCRIPTION);
    expect(s.description.length).toBeLessThanOrEqual(100);
    expect(s.version).toBe(VERSION);
    expect(s.remotes).toEqual([expect.objectContaining({ type: "streamable-http", url: HOSTED_MCP_URL })]);
    expect(s.packages[0]).toMatchObject({ identifier: "shogunagent", version: VERSION, transport: { type: "stdio" } });
    const cli = JSON.parse(read("packages/cli/package.json")) as { name: string; version: string };
    expect(cli).toMatchObject({ name: "shogunagent", version: VERSION });
  });

  it(".claude-plugin manifests and smithery.yaml (E10-T9)", () => {
    const plugin = JSON.parse(read(".claude-plugin/plugin.json")) as { name: string; version: string; mcpServers: Record<string, { type?: string; url?: string; command?: string; args?: string[] }>; skills: string[] };
    expect(plugin.name).toBe("shogunagent");
    expect(plugin.version).toBe(VERSION);
    expect(plugin.mcpServers.shogunagent).toEqual({ type: "http", url: HOSTED_MCP_URL });
    expect(plugin.mcpServers["shogunagent-local"]?.command).toBe("npx");
    expect(plugin.mcpServers["shogunagent-local"]?.args).toContain("mcp");
    expect(plugin.skills).toEqual(["./"]); // the root SKILL.md is the skill
    const market = JSON.parse(read(".claude-plugin/marketplace.json")) as { name: string; plugins: Array<{ name: string; source: string; version: string }> };
    expect(market.plugins).toEqual([expect.objectContaining({ name: "shogunagent", source: "./", version: VERSION })]);
    const smithery = read("smithery.yaml");
    expect(smithery).toContain(`name: ${REGISTRY_NAME}`);
    expect(smithery).toContain(`version: ${VERSION}`);
    expect(smithery).toContain(`url: ${HOSTED_MCP_URL}`);
    expect(smithery).toContain("type: http");
  });

  it("SKILL.md's version is the skill version every surface names", () => {
    expect(/^version: (\S+)$/m.exec(read("SKILL.md"))?.[1]).toBe(VERSION);
  });
});
