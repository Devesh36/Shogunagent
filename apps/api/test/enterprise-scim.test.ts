/** E26: SCIM 2.0 provisioning with request fixtures modelled on Okta and Entra ID: users map to
 * principals (create, filter, paginate, PATCH, PUT, deactivate → every key and OAuth grant revoked
 * at once, delete → tombstone and a purge after the grace period), groups map to Teams (world-read
 * grants among members, pushed to the sidecar), and the discovery documents. */
import { randomUUID } from "node:crypto";
import { describe, expect, it } from "vitest";

import { enterpriseJobs } from "../src/enterprise/app.js";
import { MAX_GROUP_MEMBERS, applyGroupPatch, applyUserPatch, pageParams, parseFilter } from "../src/enterprise/scim.js";
import { hashSecret } from "../src/oauth/store.js";
import type { ScimGroup, ScimUser } from "../src/enterprise/store.js";
import { createTenant, insertKey, makeEnterpriseRig, scim, type EnterpriseRig } from "./enterprise-helpers.js";

// --- fixtures (shapes as the IdPs send them) ---

const oktaCreate = (userName: string, extra: Record<string, unknown> = {}): Record<string, unknown> => ({
  schemas: ["urn:ietf:params:scim:schemas:core:2.0:User"],
  userName,
  name: { givenName: "Ada", familyName: "Lovelace" },
  emails: [{ primary: true, value: userName, type: "work" }],
  displayName: "Ada Lovelace",
  locale: "en-US",
  externalId: "00u1abcdOKTA",
  groups: [],
  password: "never-stored-1!",
  active: true,
  ...extra,
});

const entraCreate = (userName: string): Record<string, unknown> => ({
  schemas: ["urn:ietf:params:scim:schemas:core:2.0:User", "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User"],
  externalId: "0a1b2c3d-entra",
  userName,
  active: true,
  addresses: [{ type: "work", formatted: "1 Main St", primary: true }],
  displayName: "Grace Hopper",
  emails: [{ primary: true, type: "work", value: userName }],
  meta: { resourceType: "User" },
  name: { formatted: "Grace Hopper", familyName: "Hopper", givenName: "Grace" },
  title: "Rear Admiral",
  "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User": { department: "Navy" },
});

const oktaDeactivate = { schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], Operations: [{ op: "replace", value: { active: false } }] };
const entraDeactivate = { schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], Operations: [{ op: "Replace", path: "active", value: "False" }] };
const entraUpdate = {
  schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
  Operations: [
    { op: "Replace", path: "displayName", value: "Grace B. Hopper" },
    { op: "Add", path: "emails[type eq \"work\"].value", value: "grace.hopper@acme.example" },
    { op: "Replace", path: "name.familyName", value: "Hopper-Murray" },
    { op: "Add", path: "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department", value: "Ignored" },
  ],
};

async function enterpriseTenant(rig: EnterpriseRig, region = "us"): Promise<{ tenantId: string; token: string }> {
  const { tenantId, scimToken } = await createTenant(rig, { name: "Acme", region, plan: "enterprise" });
  if (!scimToken) throw new Error("no SCIM token");
  return { tenantId, token: scimToken };
}

async function createUser(rig: EnterpriseRig, token: string, body: Record<string, unknown>): Promise<Record<string, unknown>> {
  const res = await scim(rig, token, "/Users", { method: "POST", body });
  expect(res.status, await res.clone().text()).toBe(201);
  expect(res.headers.get("content-type")).toContain("application/scim+json");
  return (await res.json()) as Record<string, unknown>;
}

describe("SCIM auth and discovery", () => {
  it("authenticates by the tenant's hashed token; refuses unknown, malformed and non-enterprise", async () => {
    const rig = await makeEnterpriseRig();
    const { token, tenantId } = await enterpriseTenant(rig);
    expect(token).toMatch(/^sct_[A-Za-z0-9_-]{43}$/);
    const stored = await rig.enterprise.tenant(tenantId);
    expect(stored?.scimTokenHash).toBe(hashSecret(token));
    expect(JSON.stringify(stored)).not.toContain(token);
    expect((await scim(rig, token, "/ServiceProviderConfig")).status).toBe(200);
    expect((await scim(rig, `sct_${"A".repeat(43)}`, "/Users")).status).toBe(401);
    expect((await scim(rig, "garbage", "/Users")).status).toBe(401);
    expect((await rig.app.request("/scim/v2/Users")).status).toBe(401);
    // Rotation: the old token stops at once.
    const rotated = await rig.app.request(`/v1/admin/tenants/${tenantId}/scim-token`, { method: "POST", headers: { authorization: `Bearer ${rig.adminKey}` } });
    const fresh = ((await rotated.json()) as { scim_token: string }).scim_token;
    expect((await scim(rig, token, "/Users")).status).toBe(401);
    expect((await scim(rig, fresh, "/Users")).status).toBe(200);
    expect(rig.logLines.join("\n")).not.toContain(token);
    expect(rig.logLines.join("\n")).not.toContain(fresh);
  });

  it("serves ServiceProviderConfig, Schemas and ResourceTypes", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    const spc = (await (await scim(rig, token, "/ServiceProviderConfig")).json()) as Record<string, { supported: boolean }>;
    expect(spc.patch?.supported).toBe(true);
    expect(spc.filter?.supported).toBe(true);
    expect(spc.bulk?.supported).toBe(false);
    const schemas = (await (await scim(rig, token, "/Schemas")).json()) as { totalResults: number; Resources: Array<{ id: string }> };
    expect(schemas.Resources.map((r) => r.id)).toContain("urn:ietf:params:scim:schemas:core:2.0:User");
    expect((await scim(rig, token, "/Schemas/urn:ietf:params:scim:schemas:core:2.0:Group")).status).toBe(200);
    const types = (await (await scim(rig, token, "/ResourceTypes")).json()) as { Resources: Array<{ id: string; endpoint: string }> };
    expect(types.Resources.map((r) => r.endpoint).sort()).toEqual(["/Groups", "/Users"]);
    expect((await scim(rig, token, "/ResourceTypes/User")).status).toBe(200);
    expect((await scim(rig, token, "/Nope")).status).toBe(404);
  });
});

describe("SCIM users", () => {
  it("Okta: create provisions a principal under the tenant; filter by userName; uniqueness", async () => {
    const rig = await makeEnterpriseRig();
    const { token, tenantId } = await enterpriseTenant(rig);
    const user = await createUser(rig, token, oktaCreate("ada@acme.example"));
    const id = String(user.id);
    expect(id).toMatch(/^prn_/);
    expect(user).toMatchObject({ userName: "ada@acme.example", active: true, externalId: "00u1abcdOKTA", name: { givenName: "Ada", familyName: "Lovelace" } });
    expect(user["urn:shogunagent:params:scim:schemas:extension:principal:2.0:User"]).toEqual({ principalId: id, tenantId, region: "us" });
    expect(JSON.stringify(user)).not.toContain("never-stored");
    const principal = await rig.store.principal(id);
    expect(principal?.tenantId).toBe(tenantId);
    expect(await rig.enterprise.membership(id)).toBe(tenantId);

    const found = (await (await scim(rig, token, `/Users?filter=${encodeURIComponent('userName eq "ADA@acme.example"')}`)).json()) as { totalResults: number; Resources: Array<{ id: string }> };
    expect(found.totalResults).toBe(1);
    expect(found.Resources[0]?.id).toBe(id);
    const none = (await (await scim(rig, token, `/Users?filter=${encodeURIComponent('userName eq "nobody@acme.example"')}`)).json()) as { totalResults: number; Resources: unknown[] };
    expect(none).toMatchObject({ totalResults: 0, Resources: [] });
    expect((await scim(rig, token, `/Users?filter=${encodeURIComponent('name.familyName co "x"')}`)).status).toBe(400);

    const dup = await scim(rig, token, "/Users", { method: "POST", body: oktaCreate("Ada@ACME.example") });
    expect(dup.status).toBe(409);
    expect(await dup.json()).toMatchObject({ scimType: "uniqueness", status: "409" });
    // Another tenant may use the same userName.
    const other = await enterpriseTenant(rig);
    await createUser(rig, other.token, oktaCreate("ada@acme.example"));
    // And cannot read this tenant's user.
    expect((await scim(rig, other.token, `/Users/${id}`)).status).toBe(404);
  });

  it("paginates per RFC 7644 (1-based startIndex, count, totalResults)", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    for (let i = 0; i < 5; i += 1) {
      rig.setNow(new Date(rig.now().getTime() + 1_000));
      await createUser(rig, token, oktaCreate(`u${i}@acme.example`, { externalId: `x${i}` }));
    }
    const page = (await (await scim(rig, token, "/Users?startIndex=2&count=2")).json()) as { totalResults: number; startIndex: number; itemsPerPage: number; Resources: Array<{ userName: string }> };
    expect(page).toMatchObject({ totalResults: 5, startIndex: 2, itemsPerPage: 2 });
    expect(page.Resources.map((r) => r.userName)).toEqual(["u1@acme.example", "u2@acme.example"]);
    const zero = (await (await scim(rig, token, "/Users?count=0")).json()) as { totalResults: number; itemsPerPage: number };
    expect(zero).toMatchObject({ totalResults: 5, itemsPerPage: 0 });
    expect(pageParams("0", "1000")).toEqual({ startIndex: 1, offset: 0, limit: 200 });
    expect(pageParams(undefined, "-3")).toEqual({ startIndex: 1, offset: 0, limit: 0 });
  });

  it("Entra ID: create, PATCH with paths and string booleans, PUT", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    const user = await createUser(rig, token, entraCreate("grace@acme.example"));
    const id = String(user.id);
    const patched = await scim(rig, token, `/Users/${id}`, { method: "PATCH", body: entraUpdate });
    expect(patched.status).toBe(200);
    expect(await patched.json()).toMatchObject({ displayName: "Grace B. Hopper", name: { familyName: "Hopper-Murray" }, emails: [{ value: "grace.hopper@acme.example", type: "work" }] });
    const put = await scim(rig, token, `/Users/${id}`, { method: "PUT", body: { ...entraCreate("grace@acme.example"), displayName: "G. Hopper" } });
    expect(await put.json()).toMatchObject({ displayName: "G. Hopper", active: true });
    expect((await scim(rig, token, `/Users/${id}`, { method: "PATCH", body: { Operations: [{ op: "explode" }] } })).status).toBe(400);
  });

  for (const [idp, body] of [["Okta", oktaDeactivate], ["Entra ID", entraDeactivate]] as const) {
    it(`${idp}: deactivation revokes every key and OAuth grant of the principal immediately`, async () => {
      const rig = await makeEnterpriseRig();
      const { token, tenantId } = await enterpriseTenant(rig);
      const id = String((await createUser(rig, token, oktaCreate("ada@acme.example"))).id);
      const key1 = await insertKey(rig, id, ["read", "write"], tenantId);
      const key2 = await insertKey(rig, id, ["read"], tenantId);
      // An OAuth grant minted from key1.
      const keyRecord = (await rig.store.keysByPrefix(key1.slice(8, 18)))[0];
      if (!keyRecord) throw new Error("key not found");
      const access = `sat_${"a".repeat(43)}`;
      await rig.oauth.createFamily({ familyId: "ofam_1", clientId: "https://c.example/c.json", keyId: keyRecord.keyId, principalId: id, codeHash: "h", createdAt: 0 });
      await rig.oauth.insertToken({ tokenHash: hashSecret(access), kind: "access", familyId: "ofam_1", clientId: "https://c.example/c.json", keyId: keyRecord.keyId, principalId: id, scopes: ["read"], resource: "r", createdAt: 0, expiresAt: Date.parse("2027-01-01") });
      for (const k of [key1, key2, access]) expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${k}` } })).status, k.slice(0, 8)).toBe(200);

      const res = await scim(rig, token, `/Users/${id}`, { method: "PATCH", body });
      expect(res.status).toBe(200);
      expect(await res.json()).toMatchObject({ active: false });
      for (const k of [key1, key2, access]) {
        const me = await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${k}` } });
        expect(me.status, k.slice(0, 8)).toBe(401);
        expect(((await me.json()) as { error: { code: string } }).error.code).toBe("key_revoked");
      }
      expect((await rig.oauth.token(hashSecret(access)))?.familyRevoked).toBe(true);
      expect(rig.logLines.some((l) => l.includes("event=scim_user_deactivated"))).toBe(true);
      // Re-activation does not bring revoked keys back.
      await scim(rig, token, `/Users/${id}`, { method: "PATCH", body: { Operations: [{ op: "replace", path: "active", value: true }] } });
      expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key1}` } })).status).toBe(401);
    });
  }

  it("DELETE revokes now, leaves a tombstone, frees the userName, and the sweep purges after the grace period", async () => {
    const rig = await makeEnterpriseRig({ graceDays: 7 });
    const { token, tenantId } = await enterpriseTenant(rig);
    const id = String((await createUser(rig, token, oktaCreate("ada@acme.example"))).id);
    const key = await insertKey(rig, id, ["read"], tenantId);
    const del = await scim(rig, token, `/Users/${id}`, { method: "DELETE" });
    expect(del.status).toBe(204);
    expect((await rig.app.request("/v1/agents/me", { headers: { authorization: `Bearer ${key}` } })).status).toBe(401);
    expect((await scim(rig, token, `/Users/${id}`)).status).toBe(404);
    expect((await scim(rig, token, `/Users/${id}`, { method: "DELETE" })).status).toBe(404);
    const tomb = await rig.enterprise.user(tenantId, id);
    expect(tomb).toMatchObject({ userId: id, userName: "", emails: [], active: false });
    expect(tomb?.deleteAfter).toBe(rig.now().getTime() + 7 * 86_400_000);
    // The userName can be provisioned again, as a new principal.
    const again = await createUser(rig, token, oktaCreate("ada@acme.example"));
    expect(again.id).not.toBe(id);

    // Not yet due.
    expect(await enterpriseJobs(rig.enterprise, rig.sidecar, rig.now().getTime(), () => undefined)).toEqual({ purged: 0, failed: 0 });
    // Due; the sidecar refuses once: stays due.
    rig.sidecar.purgeAnswer = { ok: false, status: 404, code: "not_found" };
    const later = rig.now().getTime() + 8 * 86_400_000;
    expect(await enterpriseJobs(rig.enterprise, rig.sidecar, later, () => undefined)).toEqual({ purged: 0, failed: 1 });
    rig.sidecar.purgeAnswer = { ok: true };
    const lines: string[] = [];
    expect(await enterpriseJobs(rig.enterprise, rig.sidecar, later, (l) => lines.push(l))).toEqual({ purged: 1, failed: 0 });
    expect(rig.sidecar.purged).toEqual([id]);
    expect(await rig.enterprise.membership(id)).toBeUndefined();
    expect(lines).toEqual(["enterprise_purges purged=1 failed=0"]);
    expect(await enterpriseJobs(rig.enterprise, rig.sidecar, later, () => undefined)).toEqual({ purged: 0, failed: 0 });
  });

  it("refuses a tenant homed in another region with use_region_endpoint", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig, "eu");
    const res = await scim(rig, token, "/Users");
    expect(res.status).toBe(421);
    expect(await res.json()).toMatchObject({ next_action: "use_region_endpoint", region: "eu", region_endpoint: "https://eu.api.test" });
  });

  it("never logs SCIM bodies", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    const id = String((await createUser(rig, token, entraCreate("grace@acme.example"))).id);
    await scim(rig, token, `/Users/${id}`, { method: "PATCH", body: entraUpdate });
    const all = rig.logLines.join("\n");
    for (const s of ["grace", "Hopper", "acme.example", "Navy", id]) expect(all).not.toContain(s);
    expect(all).toContain("POST /scim/v2/Users 201");
    expect(all).toContain("PATCH /scim/v2/Users/:id 200");
  });
});

describe("SCIM groups = Teams", () => {
  async function users(rig: EnterpriseRig, token: string, n: number): Promise<string[]> {
    const ids: string[] = [];
    for (let i = 0; i < n; i += 1) ids.push(String((await createUser(rig, token, oktaCreate(`m${i}@acme.example`, { externalId: `e${i}` }))).id));
    return ids;
  }

  it("Okta: create with members grants world-read both ways; add, remove by filter, rename; delete revokes", async () => {
    const rig = await makeEnterpriseRig();
    const { token, tenantId } = await enterpriseTenant(rig);
    const [a = "", b = "", c = ""] = await users(rig, token, 3);
    const created = await scim(rig, token, "/Groups", { method: "POST", body: { schemas: ["urn:ietf:params:scim:schemas:core:2.0:Group"], displayName: "Engineering", members: [{ value: a, display: "m0" }, { value: b }] } });
    expect(created.status).toBe(201);
    const group = (await created.json()) as { id: string; members: Array<{ value: string }> };
    expect(group.members.map((m) => m.value).sort()).toEqual([a, b].sort());
    expect(rig.sidecar.teams.get(`${tenantId}/${group.id}`)?.sort()).toEqual([a, b].sort());
    expect([...rig.sidecar.grants].sort()).toEqual([`${a}>${b}`, `${b}>${a}`].sort());

    // Okta adds a member.
    await scim(rig, token, `/Groups/${group.id}`, { method: "PATCH", body: { schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], Operations: [{ op: "add", path: "members", value: [{ value: c, display: "m2" }] }] } });
    expect(rig.sidecar.grants.size).toBe(6);
    // Okta removes one by filter path.
    await scim(rig, token, `/Groups/${group.id}`, { method: "PATCH", body: { Operations: [{ op: "remove", path: `members[value eq "${a}"]` }] } });
    expect([...rig.sidecar.grants].sort()).toEqual([`${b}>${c}`, `${c}>${b}`].sort());
    // Okta renames with a no-path replace.
    const renamed = await scim(rig, token, `/Groups/${group.id}`, { method: "PATCH", body: { Operations: [{ op: "replace", value: { id: group.id, displayName: "Eng" } }] } });
    expect(await renamed.json()).toMatchObject({ displayName: "Eng" });
    const listed = (await (await scim(rig, token, `/Groups?filter=${encodeURIComponent('displayName eq "Eng"')}&excludedAttributes=members`)).json()) as { totalResults: number; Resources: Array<Record<string, unknown>> };
    expect(listed.totalResults).toBe(1);
    expect(listed.Resources[0]?.members).toBeUndefined();

    expect((await scim(rig, token, `/Groups/${group.id}`, { method: "DELETE" })).status).toBe(204);
    expect(rig.sidecar.grants.size).toBe(0);
    expect((await scim(rig, token, `/Groups/${group.id}`)).status).toBe(404);
  });

  it("Entra ID: Add / Remove members with value lists; a grant survives while another group is shared", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    const [a = "", b = ""] = await users(rig, token, 2);
    const g1 = ((await (await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "G1", externalId: "entra-g1", members: [] } })).json()) as { id: string }).id;
    const g2 = ((await (await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "G2", members: [{ value: a }, { value: b }] } })).json()) as { id: string }).id;
    await scim(rig, token, `/Groups/${g1}`, { method: "PATCH", body: { schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], Operations: [{ op: "Add", path: "members", value: [{ value: a }, { value: b }] }] } });
    expect(rig.sidecar.grants.size).toBe(2);
    await scim(rig, token, `/Groups/${g1}`, { method: "PATCH", body: { Operations: [{ op: "Remove", path: "members", value: [{ value: a }] }] } });
    // a and b still share G2: the grant stays.
    expect(rig.sidecar.grants.has(`${a}>${b}`)).toBe(true);
    expect(rig.sidecar.calls.some((c) => c.startsWith("revoke"))).toBe(false);
    await scim(rig, token, `/Groups/${g2}`, { method: "PATCH", body: { Operations: [{ op: "Replace", path: "displayName", value: "G2 renamed" }] } });
    // Deleting the user removes it from its groups and revokes what it no longer shares.
    await scim(rig, token, `/Users/${a}`, { method: "DELETE" });
    expect(rig.sidecar.grants.size).toBe(0);
    const g2now = (await (await scim(rig, token, `/Groups/${g2}`)).json()) as { members: Array<{ value: string }> };
    expect(g2now.members.map((m) => m.value)).toEqual([b]);
  });

  it("validates members, size, uniqueness, and fails closed when the sidecar does", async () => {
    const rig = await makeEnterpriseRig();
    const { token } = await enterpriseTenant(rig);
    const [a = ""] = await users(rig, token, 1);
    expect((await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "X", members: [{ value: "prn_not-a-member" }] } })).status).toBe(400);
    const tooMany = Array.from({ length: MAX_GROUP_MEMBERS + 1 }, () => ({ value: a }));
    expect((await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "Big", members: tooMany.map((_, i) => ({ value: `${a}${i}` })) } })).status).toBe(400);
    expect((await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "Dup" } })).status).toBe(201);
    expect((await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "Dup" } })).status).toBe(409);
    rig.sidecar.failNext = 1;
    const failed = await scim(rig, token, "/Groups", { method: "POST", body: { displayName: "Y", members: [{ value: a }] } });
    expect(failed.status).toBe(503);
    expect((await (await scim(rig, token, `/Groups?filter=${encodeURIComponent('displayName eq "Y"')}`)).json()) as { totalResults: number }).toMatchObject({ totalResults: 0 });
    const noSidecar = await makeEnterpriseRig({ sidecar: false });
    const t2 = await enterpriseTenant(noSidecar);
    expect((await scim(noSidecar, t2.token, "/Groups", { method: "POST", body: { displayName: "Z" } })).status).toBe(503);
  });
});

describe("SCIM parsing helpers", () => {
  it("parses eq filters only", () => {
    expect(parseFilter('userName eq "a@b.c"', ["userName"])).toEqual({ attribute: "userName", value: "a@b.c" });
    expect(parseFilter('USERNAME EQ "a\\"b"', ["userName"])).toEqual({ attribute: "userName", value: 'a"b' });
    expect(parseFilter('urn:ietf:params:scim:schemas:core:2.0:User:userName eq "x"', ["userName"])).toEqual({ attribute: "userName", value: "x" });
    expect(parseFilter('userName sw "a"', ["userName"])).toBe("invalid");
    expect(parseFilter('emails.value eq "a"', ["userName"])).toBe("invalid");
    expect(parseFilter(undefined, ["userName"])).toBeUndefined();
  });

  it("applies user and group PATCH operations", () => {
    const u: ScimUser = { userId: "prn_1", tenantId: "t", userName: "a", emails: [], active: true, createdAt: 0, updatedAt: 0 };
    expect(applyUserPatch(u, [{ op: "replace", value: { active: "false", "name.givenName": "A" } }])).toMatchObject({ active: false, givenName: "A" });
    expect(applyUserPatch(u, [{ op: "add", path: 'emails[type eq "work"].value', value: "a@b.c" }]).emails).toEqual([{ value: "a@b.c", type: "work", primary: true }]);
    expect(() => applyUserPatch(u, [{ op: "remove", path: "userName" }])).toThrow();
    expect(() => applyUserPatch(u, [{ op: "replace", path: "active", value: "maybe" }])).toThrow();
    const g: ScimGroup = { groupId: "g", tenantId: "t", displayName: "G", members: ["a", "b"], createdAt: 0, updatedAt: 0 };
    expect(applyGroupPatch(g, [{ op: "remove", path: "members" }]).members).toEqual([]);
    expect(applyGroupPatch(g, [{ op: "replace", path: "members", value: [{ value: "c" }] }]).members).toEqual(["c"]);
    expect(applyGroupPatch(g, [{ op: "add", path: "members", value: [{ value: "a" }, { value: "c" }] }]).members).toEqual(["a", "b", "c"]);
    expect(applyGroupPatch(g, [{ op: "add", path: "urn:ietf:params:scim:schemas:core:2.0:Group:externalId", value: "x" }]).externalId).toBe("x");
    void randomUUID;
  });
});
