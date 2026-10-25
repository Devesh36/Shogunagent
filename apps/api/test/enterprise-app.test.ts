/** E26: the operator tenant routes, tenant membership (at most one tenant per principal), region
 * separation (421 use_region_endpoint), SSO-enforced OAuth consent, member key issuance, the
 * tenant audit export proxy, the enterprise store on Postgres SQL (SQLite double), the boot
 * settings and the log line. */
import { createHash, randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { SqlAgentStore } from "../src/agent-pg-store.js";
import { InMemoryAgentStore, type AgentStore } from "../src/agent-store.js";
import { BootError, enterpriseConfigFromEnv } from "../src/boot.js";
import { SESSION_COOKIE, auditExportStream, memberKeyId } from "../src/enterprise/app.js";
import { derive, signClaims } from "../src/enterprise/crypto.js";
import { parseRegionEndpoints } from "../src/enterprise/region.js";
import { InMemoryEnterpriseStore, SqlEnterpriseStore, type EnterpriseStore, type ScimUser } from "../src/enterprise/store.js";
import { enterpriseRouteTemplate, routeTemplate } from "../src/logging.js";
import type { CimdFetch } from "../src/oauth/cimd.js";
import { InMemoryOAuthStore, SqlOAuthStore, type OAuthStore } from "../src/oauth/store.js";
import { BASE, ENTERPRISE_SECRET, FakeSidecar, adminCall, createTenant, insertKey, makeEnterpriseRig, type EnterpriseRig } from "./enterprise-helpers.js";
import { sqliteDouble } from "./sql-double.js";
import { TEST_IDP_CERT } from "./saml-test-idp-cert.js";

const ENTERPRISE_SQL = readFileSync(new URL("../sql/enterprise.sql", import.meta.url), "utf8");
const AGENT_SQL = readFileSync(new URL("../sql/agent-keys.sql", import.meta.url), "utf8") + "\n" + readFileSync(new URL("../sql/agent-receipts-settled-quantity.sql", import.meta.url), "utf8");
const OAUTH_SQL = readFileSync(new URL("../sql/agent-oauth.sql", import.meta.url), "utf8");

async function errorOf(res: Response): Promise<{ code: string; message: string; next_action: string | null; [k: string]: unknown }> {
  return ((await res.json()) as { error: { code: string; message: string; next_action: string | null } }).error;
}

/** A session cookie for an admin of the tenant (what a completed SSO sign-in sets). */
async function adminSession(rig: EnterpriseRig, tenantId: string, email = "admin@acme.example"): Promise<string> {
  const res = await adminCall(rig, `/v1/admin/tenants/${tenantId}/admins`, { method: "POST", body: { email } });
  const adminId = ((await res.json()) as { admin: { admin_id: string } }).admin.admin_id;
  const iat = Math.floor(rig.now().getTime() / 1000);
  return `${SESSION_COOKIE}=${signClaims(derive(ENTERPRISE_SECRET, "session"), { a: adminId, t: tenantId, m: "oidc", iat, exp: iat + 600 })}`;
}

describe("operator tenant routes", () => {
  it("create is idempotent, validates region and plan, and shows the SCIM token once", async () => {
    const rig = await makeEnterpriseRig();
    const body = { name: "Acme", region: "eu", plan: "enterprise" };
    const first = await adminCall(rig, "/v1/admin/tenants", { method: "POST", body, idem: "k1" });
    expect(first.status).toBe(201);
    const j1 = (await first.json()) as { tenant: Record<string, unknown>; scim_token: string };
    expect(j1.tenant).toMatchObject({ name: "Acme", region: "eu", plan: "enterprise", served_here: false, region_endpoint: "https://eu.api.test" });
    expect(j1.scim_token).toMatch(/^sct_/);
    const again = await adminCall(rig, "/v1/admin/tenants", { method: "POST", body, idem: "k1" });
    expect(again.status).toBe(200);
    expect(await again.json()).toMatchObject({ created: false, scim_token: null, tenant: { tenant_id: j1.tenant.tenant_id } });
    expect((await adminCall(rig, "/v1/admin/tenants", { method: "POST", body })).status).toBe(400);
    expect(await errorOf(await adminCall(rig, "/v1/admin/tenants", { method: "POST", body: { name: "X", region: "mars" }, idem: "k2" }))).toMatchObject({ code: "unknown_region" });
    expect(await errorOf(await adminCall(rig, "/v1/admin/tenants", { method: "POST", body: { name: "X", plan: "free" }, idem: "k3" }))).toMatchObject({ code: "invalid_plan" });
    expect(await errorOf(await adminCall(rig, "/v1/admin/tenants", { method: "POST", body: { name: "X", sso: { protocol: "oidc", oidc: { issuer: "https://idp.example.com", client_id: "c" } } }, idem: "k4" }))).toMatchObject({ code: "sso_requires_enterprise" });
    const selfServe = (await (await adminCall(rig, "/v1/admin/tenants", { method: "POST", body: { name: "Solo" }, idem: "k5" })).json()) as { tenant: Record<string, unknown>; scim_token: null };
    expect(selfServe).toMatchObject({ scim_token: null, tenant: { plan: "self-serve", region: "us", served_here: true, scim: null, sso: null } });
    expect(rig.logLines.join("\n")).not.toContain(j1.scim_token);
  });

  it("is admin-key only, never an ordinary key", async () => {
    const rig = await makeEnterpriseRig();
    const key = await insertKey(rig, `prn_${randomUUID()}`, ["read", "write", "run"]);
    const res = await rig.app.request("/v1/admin/tenants", { method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json", "idempotency-key": "x" }, body: "{}" });
    expect(res.status).toBe(403);
    expect((await rig.app.request("/v1/admin/tenants/tnt_x")).status).toBe(401);
  });

  it("patches name, plan and SSO; refuses a region change; downgrading drops SSO", async () => {
    const rig = await makeEnterpriseRig();
    const { tenantId } = await createTenant(rig, { name: "Acme", plan: "enterprise", sso: { protocol: "saml", saml: { idp_entity_id: "urn:idp", sso_url: "https://idp.example.com/sso", certificates: [TEST_IDP_CERT] } } });
    expect(await errorOf(await adminCall(rig, `/v1/admin/tenants/${tenantId}`, { method: "PATCH", body: { region: "eu" } }))).toMatchObject({ code: "region_immutable" });
    expect(await errorOf(await adminCall(rig, `/v1/admin/tenants/${tenantId}`, { method: "PATCH", body: { sso: { protocol: "saml", saml: { idp_entity_id: "urn:idp", sso_url: "https://idp.example.com/sso", certificates: ["not a pem"] } } } }))).toMatchObject({ code: "invalid_sso" });
    const renamed = (await (await adminCall(rig, `/v1/admin/tenants/${tenantId}`, { method: "PATCH", body: { name: "Acme KK", sso: { protocol: "oidc", enforced: true, oidc: { issuer: "https://idp.example.com", client_id: "c", client_secret: "s3cret-value" } } } })).json()) as { tenant: { name: string; sso: { protocol: string; enforced: boolean; oidc: { client_secret_set: boolean } } } };
    expect(renamed.tenant).toMatchObject({ name: "Acme KK", sso: { protocol: "oidc", enforced: true, oidc: { client_secret_set: true } } });
    // The client secret is sealed at rest.
    const stored = await rig.enterprise.tenant(tenantId);
    expect(stored?.sso?.oidc?.clientSecretSealed).toMatch(/^s1\./);
    expect(JSON.stringify(stored)).not.toContain("s3cret-value");
    // Keeping the same issuer and client id without a secret keeps the sealed one.
    await adminCall(rig, `/v1/admin/tenants/${tenantId}`, { method: "PATCH", body: { sso: { protocol: "oidc", enforced: false, oidc: { issuer: "https://idp.example.com", client_id: "c" } } } });
    expect((await rig.enterprise.tenant(tenantId))?.sso?.oidc?.clientSecretSealed).toBe(stored?.sso?.oidc?.clientSecretSealed);
    const down = (await (await adminCall(rig, `/v1/admin/tenants/${tenantId}`, { method: "PATCH", body: { plan: "self-serve" } })).json()) as { tenant: { sso: unknown; scim: unknown } };
    expect(down.tenant).toMatchObject({ sso: null, scim: null });
  });

  it("attaches a principal to at most one tenant and records the tenant on its keys", async () => {
    const rig = await makeEnterpriseRig();
    const a = await createTenant(rig, { name: "A" });
    const b = await createTenant(rig, { name: "B" });
    const principalId = `prn_${randomUUID()}`;
    const key = await insertKey(rig, principalId, ["read"]);
    const attach = (t: string) => adminCall(rig, `/v1/admin/tenants/${t}/principals`, { method: "POST", body: { principal_id: principalId } });
    expect((await attach(a.tenantId)).status).toBe(201);
    expect((await attach(a.tenantId)).status).toBe(200);
    const other = await attach(b.tenantId);
    expect(other.status).toBe(409);
    expect((await errorOf(other)).code).toBe("principal_in_other_tenant");
    expect((await adminCall(rig, `/v1/admin/tenants/${a.tenantId}/principals`, { method: "POST", body: { principal_id: "prn_missing" } })).status).toBe(404);
    const me = (await (await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key}` } })).json()) as { tenant_id: string };
    expect(me.tenant_id).toBe(a.tenantId);
    const shown = (await (await adminCall(rig, `/v1/admin/tenants/${a.tenantId}`)).json()) as { member_count: number };
    expect(shown.member_count).toBe(1);
  });
});

describe("region separation", () => {
  it("refuses a principal homed in another region on key routes and /mcp, with the endpoint", async () => {
    const rig = await makeEnterpriseRig({ region: "us" });
    const eu = await createTenant(rig, { name: "EU Co", region: "eu" });
    const principalId = `prn_${randomUUID()}`;
    const key = await insertKey(rig, principalId, ["read", "write"]);
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key}` } })).status).toBe(200);
    await adminCall(rig, `/v1/admin/tenants/${eu.tenantId}/principals`, { method: "POST", body: { principal_id: principalId } });
    // A fresh isolate (the admission cache is per isolate, one minute).
    rig.setNow(new Date(rig.now().getTime() + 61_000));
    const me = await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key}` } });
    expect(me.status).toBe(421);
    expect(await errorOf(me)).toMatchObject({ code: "wrong_region", next_action: "use_region_endpoint", region: "eu", region_endpoint: "https://eu.api.test" });
    const mcp = await rig.app.request("/mcp", { method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json", "mcp-method": "tools/call", "mcp-name": "state.commitments" }, body: "{}" });
    expect(mcp.status).toBe(421);
    expect(rig.upstreamCalls).toHaveLength(0);
    expect(rig.logLines.some((l) => l.includes("event=region_refused"))).toBe(true);
    // A principal of a local tenant is served.
    const us = await createTenant(rig, { name: "US Co", region: "us" });
    const local = `prn_${randomUUID()}`;
    const localKey = await insertKey(rig, local, ["read"]);
    await adminCall(rig, `/v1/admin/tenants/${us.tenantId}/principals`, { method: "POST", body: { principal_id: local } });
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${localKey}` } })).status).toBe(200);
  });

  it("parses SHOGUN_REGION_ENDPOINTS strictly", () => {
    expect(parseRegionEndpoints('{"eu":"https://eu.api.example","jp":"https://jp.api.example"}')).toEqual({ eu: "https://eu.api.example", jp: "https://jp.api.example" });
    expect(parseRegionEndpoints(undefined)).toEqual({});
    for (const bad of ["[]", "{", '{"EU":"https://x.example"}', '{"eu":"http://x.example"}', '{"eu":"https://x.example/path"}']) expect(() => parseRegionEndpoints(bad), bad).toThrow();
  });
});

describe("SSO-enforced tenants and the OAuth consent page", () => {
  const CLIENT_ID = "https://client.example.com/oauth/client.json";
  const REDIRECT = "http://127.0.0.1:33418/callback";
  const cimd: CimdFetch = () => Promise.resolve(new Response(JSON.stringify({ client_id: CLIENT_ID, client_name: "Client", redirect_uris: [REDIRECT] }), { headers: { "content-type": "application/json" } }));
  const challenge = createHash("sha256").update("v".repeat(43)).digest("base64url");

  async function consent(rig: EnterpriseRig, key: string): Promise<Response> {
    const q = new URLSearchParams({ response_type: "code", client_id: CLIENT_ID, redirect_uri: REDIRECT, code_challenge: challenge, code_challenge_method: "S256", state: "s", scope: "read" });
    const html = await (await rig.app.request(`/oauth/authorize?${q.toString()}`)).text();
    const token = /name="consent" value="([^"]+)"/.exec(html)?.[1] ?? "";
    return rig.app.request("/oauth/authorize", { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ ...Object.fromEntries(q), consent: token, decision: "approve", api_key: key }).toString() });
  }

  it("refuses key-pasting consent for principals of an SSO-enforced tenant only", async () => {
    const rig = await makeEnterpriseRig({ cimdFetch: cimd });
    const sso = { protocol: "oidc", oidc: { issuer: "https://idp.example.com", client_id: "c" } };
    const enforced = await createTenant(rig, { name: "Strict", plan: "enterprise", sso: { ...sso, enforced: true } });
    const relaxed = await createTenant(rig, { name: "Relaxed", plan: "enterprise", sso: { ...sso, enforced: false } });
    const p1 = `prn_${randomUUID()}`;
    const p2 = `prn_${randomUUID()}`;
    const k1 = await insertKey(rig, p1, ["read"]);
    const k2 = await insertKey(rig, p2, ["read"]);
    await adminCall(rig, `/v1/admin/tenants/${enforced.tenantId}/principals`, { method: "POST", body: { principal_id: p1 } });
    await adminCall(rig, `/v1/admin/tenants/${relaxed.tenantId}/principals`, { method: "POST", body: { principal_id: p2 } });
    const refused = await consent(rig, k1);
    expect(refused.status).toBe(403);
    expect(await refused.text()).toContain("requires single sign-on");
    expect(rig.logLines.some((l) => l.includes("event=oauth_sso_required"))).toBe(true);
    expect((await consent(rig, k2)).status).toBe(302);
    const solo = await insertKey(rig, `prn_${randomUUID()}`, ["read"]);
    expect((await consent(rig, solo)).status).toBe(302);
  });
});

describe("tenant admin routes", () => {
  it("lists members and issues a key for one (never admin), refusing deactivated and foreign principals", async () => {
    const rig = await makeEnterpriseRig();
    const { tenantId } = await createTenant(rig, { name: "Acme", plan: "enterprise" });
    const cookie = await adminSession(rig, tenantId);
    const member = `prn_${randomUUID()}`;
    await rig.store.createPrincipal({ principalId: member, tenantId: member, createdAt: 0 });
    await adminCall(rig, `/v1/admin/tenants/${tenantId}/principals`, { method: "POST", body: { principal_id: member } });
    const list = (await (await rig.app.request("/v1/tenant/members", { headers: { cookie } })).json()) as { members: Array<{ principal_id: string }> };
    expect(list.members.map((m) => m.principal_id)).toEqual([member]);
    const issue = (body: unknown, id = member) => rig.app.request(`/v1/tenant/members/${id}/keys`, { method: "POST", headers: { cookie, "content-type": "application/json", origin: BASE }, body: JSON.stringify(body) });
    const res = await issue({ scopes: ["read", "write"] });
    expect(res.status).toBe(201);
    const { key, scopes } = (await res.json()) as { key: string; scopes: string[] };
    expect(scopes).toEqual(["read", "write"]);
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key}` } })).status).toBe(200);
    expect(rig.logLines.join("\n")).not.toContain(key);
    expect((await issue({ scopes: ["admin"] })).status).toBe(400);
    expect((await issue({}, `prn_${randomUUID()}`)).status).toBe(404);
    // A SCIM-deactivated member gets no key.
    const user: ScimUser = { userId: member, tenantId, userName: "m@acme.example", emails: [], active: false, createdAt: 0, updatedAt: 0 };
    await rig.enterprise.insertUser(user);
    expect((await issue({})).status).toBe(409);
    expect((await rig.app.request("/v1/tenant/members", { headers: { cookie: "x=y" } })).status).toBe(401);
  });

  it("issues a member key once per Idempotency-Key: a retry answers the same key reference, never the secret (E39)", async () => {
    const rig = await makeEnterpriseRig();
    const { tenantId } = await createTenant(rig, { name: "Acme", plan: "enterprise" });
    const cookie = await adminSession(rig, tenantId);
    const member = `prn_${randomUUID()}`;
    await rig.store.createPrincipal({ principalId: member, tenantId: member, createdAt: 0 });
    await adminCall(rig, `/v1/admin/tenants/${tenantId}/principals`, { method: "POST", body: { principal_id: member } });
    const list = (await (await rig.app.request("/v1/tenant/members", { headers: { cookie } })).json()) as { members: Array<{ principal_id: string }> };
    expect(list.members.map((m) => m.principal_id)).toEqual([member]);
    const issue = (body: unknown, idem?: string, id = member) => rig.app.request(`/v1/tenant/members/${id}/keys`, {
      method: "POST", headers: { cookie, "content-type": "application/json", origin: BASE, ...(idem !== undefined ? { "idempotency-key": idem } : {}) }, body: JSON.stringify(body),
    });
    const first = await issue({ scopes: ["read", "write"] }, "onboard-1");
    expect(first.status).toBe(201);
    const a = (await first.json()) as { key: string; key_id: string; created: boolean };
    expect(a.created).toBe(true);
    // The same request again: the same key id, no secret, no second key.
    const again = await issue({ scopes: ["write", "read"] }, "onboard-1");
    expect(again.status).toBe(200);
    const b = (await again.json()) as { key: string | null; key_id: string; created: boolean; revoked: boolean };
    expect(b).toMatchObject({ key: null, key_id: a.key_id, created: false, revoked: false });
    expect(await rig.store.keyById(a.key_id)).toBeDefined();
    // The first secret still works: the replay did not replace the stored hash.
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${a.key}` } })).status).toBe(200);
    // Same key, other scopes: a conflict, not a silent second key.
    const conflict = await issue({ scopes: ["read"] }, "onboard-1");
    expect(conflict.status).toBe(409);
    expect(((await conflict.json()) as { error: { code: string } }).error.code).toBe("idempotency_key_reused");
    // A new Idempotency-Key (or none) issues a new key.
    const other = (await (await issue({ scopes: ["read"] }, "onboard-2")).json()) as { key_id: string; created: boolean };
    expect(other.created).toBe(true);
    expect(other.key_id).not.toBe(a.key_id);
    const plain = await issue({ scopes: ["read"] });
    expect(plain.status).toBe(201);
    // Concurrent retries: exactly one secret is minted, the others answer its reference.
    const racers = await Promise.all([0, 1, 2, 3].map(() => issue({ scopes: ["read"] }, "race-1")));
    const bodies = (await Promise.all(racers.map((r) => r.json()))) as Array<{ key: string | null; key_id: string }>;
    expect(bodies.filter((x) => x.key !== null)).toHaveLength(1);
    expect(new Set(bodies.map((x) => x.key_id)).size).toBe(1);
    const winner = bodies.find((x) => x.key !== null)!;
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${winner.key}` } })).status).toBe(200);
    // The key id is scoped to the member: the same Idempotency-Key for another member is another key.
    expect(memberKeyId(tenantId, member, "onboard-1")).toBe(a.key_id);
    expect(memberKeyId(tenantId, "prn_other", "onboard-1")).not.toBe(a.key_id);
    expect((await issue({}, "")).status).toBe(400);
    expect(rig.logLines.join("\n")).not.toContain(a.key);
  });

  it("streams every member's sidecar audit export, framed per principal, bodies untouched", async () => {
    const rig = await makeEnterpriseRig();
    const { tenantId } = await createTenant(rig, { name: "Acme", plan: "enterprise" });
    const cookie = await adminSession(rig, tenantId);
    const [a, b, c] = ["prn_a", "prn_b", "prn_c"];
    for (const p of [a, b, c]) {
      await rig.store.createPrincipal({ principalId: p, tenantId: p, createdAt: 0 });
      await adminCall(rig, `/v1/admin/tenants/${tenantId}/principals`, { method: "POST", body: { principal_id: p } });
    }
    // Records are opaque to the gateway: even one that is not JSON passes through as is.
    rig.sidecar.exports.set(a, '{"ts":1,"route":"memory.ingest","outcome":"ok"}\n{"ts":2,"route":"acl.grant"}');
    rig.sidecar.exports.set(b, { status: 503, code: "sidecar_unavailable" });
    rig.sidecar.exports.set(c, "not-json but kept verbatim\n");
    const res = await rig.app.request("/v1/tenant/audit/export?since_ms=0", { headers: { cookie } });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("application/x-ndjson");
    const lines = (await res.text()).split("\n");
    expect(JSON.parse(lines[0] ?? "")).toMatchObject({ type: "export", tenant_id: tenantId, principals: 3 });
    expect(lines.slice(1)).toEqual([
      JSON.stringify({ type: "principal", principal_id: a, status: 200 }),
      '{"ts":1,"route":"memory.ingest","outcome":"ok"}',
      '{"ts":2,"route":"acl.grant"}',
      JSON.stringify({ type: "principal", principal_id: b, status: 503, error: "sidecar_unavailable" }),
      JSON.stringify({ type: "principal", principal_id: c, status: 200 }),
      "not-json but kept verbatim",
      JSON.stringify({ type: "end", principals: 3, failed: 1 }),
      "",
    ]);
    expect((await rig.app.request("/v1/tenant/audit/export?since_ms=abc", { headers: { cookie } })).status).toBe(400);
    expect(rig.logLines.join("\n")).not.toContain("memory.ingest");
    const noSidecar = await makeEnterpriseRig({ sidecar: false });
    const t2 = await createTenant(noSidecar, { name: "N", plan: "enterprise" });
    expect((await noSidecar.app.request("/v1/tenant/audit/export", { headers: { cookie: await adminSession(noSidecar, t2.tenantId) } })).status).toBe(503);
  });

  it("the export stream is lazy: a member is fetched only when the reader pulls", async () => {
    const sidecar = new FakeSidecar();
    const stream = auditExportStream(sidecar, "tnt_1", ["p1", "p2"], {}, () => 0);
    const reader = stream.getReader();
    await reader.read();
    expect(sidecar.calls).toEqual([]);
    await reader.read();
    expect(sidecar.calls).toEqual(["export p1"]);
    await reader.cancel();
  });
});

// --- the store, both implementations ---

const stores: Array<[string, () => EnterpriseStore]> = [
  ["memory", () => new InMemoryEnterpriseStore()],
  ["sql (sqlite double)", () => new SqlEnterpriseStore(sqliteDouble(ENTERPRISE_SQL))],
];

describe.each(stores)("EnterpriseStore (%s)", (_name, make) => {
  it("tenants: idempotent create, SCIM hash lookup, update", async () => {
    const s = make();
    if (s instanceof SqlEnterpriseStore) await s.checkSchema();
    const t = { tenantId: "tnt_1", name: "A", region: "us", plan: "enterprise" as const, scimTokenHash: "h1", idempotencyKey: "i1", createdAt: 1, updatedAt: 1 };
    expect((await s.createTenant(t)).created).toBe(true);
    expect(await s.createTenant({ ...t, tenantId: "tnt_2" })).toMatchObject({ created: false, tenant: { tenantId: "tnt_1" } });
    expect((await s.tenantByScimHash("h1"))?.tenantId).toBe("tnt_1");
    expect(await s.setScimTokenHash("tnt_1", "h2", 2)).toBe(true);
    expect(await s.tenantByScimHash("h1")).toBeUndefined();
    const sso = { protocol: "saml" as const, enforced: true, saml: { idpEntityId: "e", ssoUrl: "https://x.example", certificates: ["c"] } };
    expect((await s.updateTenant("tnt_1", { name: "B", sso }, 3))?.sso).toEqual(sso);
    expect((await s.updateTenant("tnt_1", { sso: null }, 4))?.sso).toBeUndefined();
    expect(await s.updateTenant("tnt_x", { name: "C" }, 5)).toBeUndefined();
  });

  it("membership is unique per principal", async () => {
    const s = make();
    expect(await s.attachPrincipal("t1", "p1", 1)).toBe("attached");
    expect(await s.attachPrincipal("t1", "p1", 1)).toBe("already");
    expect(await s.attachPrincipal("t2", "p1", 1)).toBe("other_tenant");
    expect(await s.membership("p1")).toBe("t1");
    expect(await s.members("t1")).toEqual(["p1"]);
    await s.detachPrincipal("p1");
    expect(await s.membership("p1")).toBeUndefined();
  });

  it("admins: unique live e-mail per tenant, subject binds once", async () => {
    const s = make();
    expect(await s.addAdmin({ adminId: "a1", tenantId: "t", email: "x@y.z", createdAt: 1 })).toBe("created");
    expect(await s.addAdmin({ adminId: "a2", tenantId: "t", email: "x@y.z", createdAt: 1 })).toBe("duplicate");
    expect(await s.bindAdminSubject("a1", "oidc:i|s")).toBe(true);
    expect(await s.bindAdminSubject("a1", "oidc:i|s")).toBe(true);
    expect(await s.bindAdminSubject("a1", "oidc:i|other")).toBe(false);
    expect(await s.removeAdmin("a1", 2)).toBe(true);
    expect(await s.admins("t")).toEqual([]);
    expect(await s.addAdmin({ adminId: "a3", tenantId: "t", email: "x@y.z", createdAt: 3 })).toBe("created");
  });

  it("users: case-insensitive unique userName, filters, pagination, tombstones, purges", async () => {
    const s = make();
    const u = (id: string, name: string, at: number): ScimUser => ({ userId: id, tenantId: "t", userName: name, externalId: `x-${id}`, emails: [{ value: name, type: "work" }], active: true, createdAt: at, updatedAt: at });
    expect(await s.insertUser(u("p1", "A@x.z", 1))).toBe("created");
    expect(await s.insertUser(u("p2", "a@X.z", 2))).toBe("conflict");
    expect(await s.insertUser(u("p3", "b@x.z", 3))).toBe("created");
    expect(await s.insertUser({ ...u("p4", "a@x.z", 4), tenantId: "other" })).toBe("created");
    expect((await s.listUsers("t", { attribute: "userName", value: "a@x.Z" }, 0, 10)).rows.map((r) => r.userId)).toEqual(["p1"]);
    expect((await s.listUsers("t", { attribute: "externalId", value: "x-p3" }, 0, 10)).rows.map((r) => r.userId)).toEqual(["p3"]);
    expect(await s.listUsers("t", undefined, 1, 1)).toMatchObject({ total: 2, rows: [{ userId: "p3" }] });
    expect(await s.updateUser({ ...u("p3", "A@x.z", 3), updatedAt: 5 })).toBe("conflict");
    expect(await s.updateUser({ ...u("p3", "c@x.z", 3), active: false, updatedAt: 5 })).toBe("ok");
    expect((await s.user("t", "p3"))).toMatchObject({ userName: "c@x.z", active: false, updatedAt: 5 });
    expect(await s.user("other", "p3")).toBeUndefined();
    expect(await s.deleteUser("t", "p1", 10, 100)).toBe(true);
    expect(await s.deleteUser("t", "p1", 11, 101)).toBe(false);
    expect(await s.user("t", "p1")).toMatchObject({ userName: "", emails: [], deletedAt: 10, deleteAfter: 100 });
    expect(await s.insertUser(u("p5", "a@x.z", 12))).toBe("created");
    expect(await s.updateUser({ ...u("p1", "zz@x.z", 1) })).toBe("not_found");
    expect((await s.duePurges(99, 10)).length).toBe(0);
    expect((await s.duePurges(100, 10)).map((r) => r.userId)).toEqual(["p1"]);
    await s.markPurged("p1", 100);
    expect((await s.duePurges(200, 10)).length).toBe(0);
  });

  it("groups: unique live displayName, exact membership match, delete", async () => {
    const s = make();
    const g = { groupId: "g1", tenantId: "t", displayName: "Eng", members: ["prn_1", "prn_12"], createdAt: 1, updatedAt: 1 };
    expect(await s.insertGroup(g)).toBe("created");
    expect(await s.insertGroup({ ...g, groupId: "g2" })).toBe("conflict");
    expect((await s.groupsOf("t", "prn_1")).map((x) => x.groupId)).toEqual(["g1"]);
    expect((await s.groupsOf("t", "prn_2")).length).toBe(0);
    expect(await s.updateGroup({ ...g, members: ["prn_2"], updatedAt: 2 })).toBe("ok");
    expect((await s.group("t", "g1"))?.members).toEqual(["prn_2"]);
    expect((await s.listGroups("t", { attribute: "displayName", value: "Eng" }, 0, 10)).total).toBe(1);
    expect(await s.deleteGroup("t", "g1", 3)).toBe(true);
    expect(await s.group("t", "g1")).toBeUndefined();
    expect(await s.insertGroup({ ...g, groupId: "g3" })).toBe("created");
  });

  it("SAML replay cache: once per id until expiry", async () => {
    const s = make();
    expect(await s.spendAssertion("k", 100, 10)).toBe(true);
    expect(await s.spendAssertion("k", 100, 50)).toBe(false);
    expect(await s.spendAssertion("k", 200, 100)).toBe(true);
    await s.maintenance(300);
    expect(await s.spendAssertion("k", 400, 300)).toBe(true);
  });
});

describe.each<[string, () => { agent: AgentStore; oauth: OAuthStore }]>([
  ["memory", () => ({ agent: new InMemoryAgentStore(), oauth: new InMemoryOAuthStore() })],
  ["sql (sqlite double)", () => ({ agent: new SqlAgentStore(sqliteDouble(AGENT_SQL)), oauth: new SqlOAuthStore(sqliteDouble(OAUTH_SQL)) })],
])("principal-wide revocation and tenant assignment (%s)", (_name, make) => {
  it("revokes every live key and family of a principal, and moves its tenant id", async () => {
    const { agent, oauth } = make();
    await agent.createPrincipal({ principalId: "p", tenantId: "p", createdAt: 0 });
    for (const [id, revoked] of [["k1", undefined], ["k2", 5], ["k3", undefined]] as const) {
      await agent.insertKey({ keyId: id, prefix: id, hash: "h", mode: "live", principalId: "p", tenantId: "p", scopes: ["read"], createdAt: 0, ...(revoked !== undefined ? { revokedAt: revoked } : {}) });
    }
    await agent.insertKey({ keyId: "other", prefix: "o", hash: "h", mode: "live", principalId: "q", tenantId: "q", scopes: ["read"], createdAt: 0 });
    expect(await agent.revokePrincipalKeys("p", 10)).toBe(2);
    expect((await agent.keyById("k1"))?.revokedAt).toBe(10);
    expect((await agent.keyById("k2"))?.revokedAt).toBe(5);
    expect((await agent.keyById("other"))?.revokedAt).toBeUndefined();
    expect(await agent.assignTenant("p", "tnt_1")).toBe(true);
    expect((await agent.principal("p"))?.tenantId).toBe("tnt_1");
    expect((await agent.keyById("k3"))?.tenantId).toBe("tnt_1");
    expect(await agent.assignTenant("missing", "tnt_1")).toBe(false);
    await oauth.createFamily({ familyId: "f1", clientId: "c", keyId: "k1", principalId: "p", codeHash: "c1", createdAt: 0 });
    await oauth.createFamily({ familyId: "f2", clientId: "c", keyId: "other", principalId: "q", codeHash: "c2", createdAt: 0 });
    await oauth.insertToken({ tokenHash: "t1", kind: "access", familyId: "f1", clientId: "c", keyId: "k1", principalId: "p", scopes: ["read"], resource: "r", createdAt: 0, expiresAt: 100 });
    await oauth.insertToken({ tokenHash: "t2", kind: "access", familyId: "f2", clientId: "c", keyId: "other", principalId: "q", scopes: ["read"], resource: "r", createdAt: 0, expiresAt: 100 });
    await oauth.revokePrincipal("p", 10);
    expect((await oauth.token("t1"))?.familyRevoked).toBe(true);
    expect((await oauth.token("t2"))?.familyRevoked).toBe(false);
  });
});

describe("boot settings", () => {
  const env = { ENTERPRISE_ENABLED: "1", ENTERPRISE_SECRET: "x".repeat(32) };
  it("parses and validates, naming the variable and never echoing a value", () => {
    expect(enterpriseConfigFromEnv({})).toBeUndefined();
    expect(enterpriseConfigFromEnv(env)).toEqual({ secret: "x".repeat(32), region: { served: "us", endpoints: {} }, deleteGraceDays: 30, skewSecs: 120 });
    expect(enterpriseConfigFromEnv({ ...env, SHOGUN_REGION: "jp", SHOGUN_REGION_ENDPOINTS: '{"us":"https://api.example"}', SCIM_DELETE_GRACE_DAYS: "7", SSO_CLOCK_SKEW_SECS: "30", SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464", SHOGUN_SIDECAR_ADMIN_TOKEN: "t".repeat(20) }))
      .toEqual({ secret: "x".repeat(32), region: { served: "jp", endpoints: { us: "https://api.example" } }, deleteGraceDays: 7, skewSecs: 30, sidecar: { url: "http://127.0.0.1:7464", adminToken: "t".repeat(20) } });
    const refused = (e: Record<string, string>): string => {
      try { enterpriseConfigFromEnv(e); return "accepted"; } catch (err) { expect(err).toBeInstanceOf(BootError); return (err as Error).message; }
    };
    expect(refused({ ENTERPRISE_ENABLED: "1", ENTERPRISE_SECRET: "short" })).toContain("ENTERPRISE_SECRET");
    expect(refused({ ...env, SHOGUN_REGION: "Europe" })).toContain("SHOGUN_REGION");
    expect(refused({ ...env, SHOGUN_REGION_ENDPOINTS: '{"eu":"ftp://secret-host"}' })).not.toContain("secret-host");
    expect(refused({ ...env, SCIM_DELETE_GRACE_DAYS: "0" })).toContain("SCIM_DELETE_GRACE_DAYS");
    expect(refused({ ...env, SHOGUN_SIDECAR_ADMIN_TOKEN: "t".repeat(20) })).toContain("SHOGUN_SIDECAR_URL");
    expect(refused({ ...env, SHOGUN_SIDECAR_ADMIN_TOKEN: "short", SHOGUN_SIDECAR_URL: "http://x" })).not.toContain("short");
  });
});

describe("the log line", () => {
  it("collapses enterprise ids to templates", () => {
    expect(routeTemplate("/scim/v2/Users/prn_123")).toBe("/scim/v2/Users/:id");
    expect(routeTemplate("/scim/v2/Groups/grp_1")).toBe("/scim/v2/Groups/:id");
    expect(routeTemplate("/sso/tnt_1/saml/acs")).toBe("/sso/:tenant/saml/acs");
    expect(routeTemplate("/sso/tnt_1/oidc/callback")).toBe("/sso/:tenant/oidc/callback");
    expect(routeTemplate("/v1/admin/tenants/tnt_1/admins/tad_2")).toBe("/v1/admin/tenants/:id/admins/:admin");
    expect(routeTemplate("/v1/admin/tenants/tnt_1/scim-token")).toBe("/v1/admin/tenants/:id/scim-token");
    expect(routeTemplate("/v1/tenant/members/prn_1/keys")).toBe("/v1/tenant/members/:id/keys");
    expect(routeTemplate("/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:User")).toBe("/scim/v2/Schemas/:id");
    expect(routeTemplate("/legal/dpa")).toBe("/legal/dpa");
    expect(enterpriseRouteTemplate("/v1/wallet")).toBeUndefined();
  });
});
