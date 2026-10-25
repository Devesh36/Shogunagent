/** E26: SSO for tenant admins — OIDC (code + PKCE: discovery, JWKS, iss/aud/nonce, clock skew)
 * and SAML 2.0 SP-initiated (signed assertions only, audience/recipient, replay cache, signature
 * wrapping), the admin session cookie, and the SSRF guard on IdP fetches. */
import { generateKeyPairSync, sign as nodeSign } from "node:crypto";
import { inflateRawSync } from "node:zlib";
import { describe, expect, it } from "vitest";

import { FLOW_COOKIE, SESSION_COOKIE, SESSION_TTL_SECS } from "../src/enterprise/app.js";
import { seal, signClaims, unseal, verifyClaims, derive } from "../src/enterprise/crypto.js";
import { IdpFetchError, idpFetch, idpUrl, type IdpFetch } from "../src/enterprise/fetch-guard.js";
import { OidcCache, OidcError, discover, verifyIdToken } from "../src/enterprise/oidc.js";
import { SamlError, validateSamlResponse } from "../src/enterprise/saml.js";
import {
  BASE, ENTERPRISE_SECRET, FakeOidcIdp, IDP_ISSUER, SAML_IDP_ENTITY, adminCall, cookiesOf, createTenant, makeEnterpriseRig, samlKeys, samlResponse,
  type EnterpriseRig, type SamlFixture, type SamlKeys,
} from "./enterprise-helpers.js";

const NOW = new Date("2026-10-25T12:00:00Z").getTime();

async function oidcTenant(rig: EnterpriseRig, enforced = true): Promise<string> {
  const { tenantId } = await createTenant(rig, {
    name: "Acme", region: "us", plan: "enterprise",
    sso: { protocol: "oidc", enforced, oidc: { issuer: IDP_ISSUER, client_id: rig.idp.clientId, client_secret: "idp-client-secret" } },
  });
  const res = await adminCall(rig, `/v1/admin/tenants/${tenantId}/admins`, { method: "POST", body: { email: "Admin@Acme.example" } });
  expect(res.status).toBe(201);
  return tenantId;
}

async function startLogin(rig: EnterpriseRig, tenantId: string): Promise<{ location: string; flow: string }> {
  const res = await rig.app.request(`/sso/${tenantId}/login`);
  expect(res.status).toBe(302);
  const flow = cookiesOf(res)[FLOW_COOKIE];
  expect(flow?.raw).toMatch(/HttpOnly/);
  expect(flow?.raw).toMatch(/Secure/);
  return { location: res.headers.get("location") ?? "", flow: flow?.value ?? "" };
}

async function oidcSignIn(rig: EnterpriseRig, tenantId: string, user = { sub: "00u1", email: "admin@acme.example" }): Promise<Response> {
  const { location, flow } = await startLogin(rig, tenantId);
  const { code, state } = rig.idp.authorize(location, user);
  return rig.app.request(`/sso/${tenantId}/oidc/callback?code=${encodeURIComponent(code)}&state=${encodeURIComponent(state)}`, { headers: { cookie: `${FLOW_COOKIE}=${flow}` } });
}

describe("OIDC sign-in for tenant admins", () => {
  it("runs the code flow with PKCE, binds the admin on a verified e-mail, and sets a strict session cookie", async () => {
    const rig = await makeEnterpriseRig();
    const tenantId = await oidcTenant(rig);
    const { location } = await startLogin(rig, tenantId);
    const url = new URL(location);
    expect(url.origin + url.pathname).toBe(`${IDP_ISSUER}/authorize`);
    expect(url.searchParams.get("code_challenge_method")).toBe("S256");
    expect(url.searchParams.get("code_challenge")).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(url.searchParams.get("redirect_uri")).toBe(`${BASE}/sso/${tenantId}/oidc/callback`);
    expect(url.searchParams.get("scope")).toBe("openid email profile");
    expect(url.searchParams.get("nonce")).toBeTruthy();

    const res = await oidcSignIn(rig, tenantId);
    expect(res.status).toBe(200);
    const session = cookiesOf(res)[SESSION_COOKIE];
    expect(session?.raw).toContain("HttpOnly");
    expect(session?.raw).toContain("Secure");
    expect(session?.raw).toContain("SameSite=Strict");
    expect(session?.raw).toContain(`Max-Age=${SESSION_TTL_SECS}`);
    expect(cookiesOf(res)[FLOW_COOKIE]?.raw).toContain("Max-Age=0");
    // The token request carried the client secret and the PKCE verifier (the fake IdP checks both).
    expect(rig.idp.requests.some((r) => r === `POST ${IDP_ISSUER}/token`)).toBe(true);

    const me = await rig.app.request("/v1/tenant/me", { headers: { cookie: `${SESSION_COOKIE}=${session?.value}` } });
    expect(me.status).toBe(200);
    const body = (await me.json()) as { admin: { email: string; sso_bound: boolean }; tenant: { tenant_id: string; sso: { oidc: { client_secret_set: boolean } } } };
    expect(body.admin).toMatchObject({ email: "admin@acme.example", sso_bound: true });
    expect(body.tenant.tenant_id).toBe(tenantId);
    expect(body.tenant.sso.oidc.client_secret_set).toBe(true);
    expect(JSON.stringify(body)).not.toContain("idp-client-secret");

    // A second sign-in matches the bound subject even with another e-mail.
    expect((await oidcSignIn(rig, tenantId, { sub: "00u1", email: "renamed@acme.example" })).status).toBe(200);
    // Another subject with the admin's e-mail does not take over the bound admin.
    expect((await oidcSignIn(rig, tenantId, { sub: "00u2", email: "admin@acme.example" })).status).toBe(403);
    const all = rig.logLines.join("\n");
    for (const secret of ["idp-client-secret", "admin@acme.example", "00u1", "code_"]) expect(all).not.toContain(secret);
    expect(all).toContain("event=sso_login_succeeded");
  });

  it("refuses an unverified e-mail, a non-admin, a wrong state and a missing flow cookie", async () => {
    const rig = await makeEnterpriseRig();
    const tenantId = await oidcTenant(rig);
    expect((await oidcSignIn(rig, tenantId, { sub: "x", email: "admin@acme.example", emailVerified: false } as never)).status).toBe(403);
    expect((await oidcSignIn(rig, tenantId, { sub: "y", email: "someone@acme.example" })).status).toBe(403);
    const { location, flow } = await startLogin(rig, tenantId);
    const { code } = rig.idp.authorize(location, { sub: "00u1", email: "admin@acme.example" });
    const wrongState = await rig.app.request(`/sso/${tenantId}/oidc/callback?code=${code}&state=forged`, { headers: { cookie: `${FLOW_COOKIE}=${flow}` } });
    expect(wrongState.status).toBe(400);
    expect(await wrongState.text()).toContain("state_mismatch");
    const noFlow = await rig.app.request(`/sso/${tenantId}/oidc/callback?code=${code}&state=x`);
    expect(await noFlow.text()).toContain("flow_missing");
    // A flow cookie for another tenant does not carry over.
    const other = await oidcTenant(rig);
    const cross = await rig.app.request(`/sso/${other}/oidc/callback?code=${code}&state=x`, { headers: { cookie: `${FLOW_COOKIE}=${flow}` } });
    expect(await cross.text()).toContain("flow_missing");
  });

  it("rejects ID tokens with a bad issuer, audience, nonce, expiry or algorithm", async () => {
    const cases: Array<[Record<string, unknown>, Record<string, unknown>, string]> = [
      [{ iss: "https://evil.example.com" }, {}, "id_token_issuer_mismatch"],
      [{ aud: "someone-else" }, {}, "id_token_audience_mismatch"],
      [{ aud: ["shogun-client", "other"] }, {}, "id_token_audience_mismatch"],
      [{ nonce: "replayed" }, {}, "id_token_nonce_mismatch"],
      [{ exp: Math.floor(NOW / 1000) - 600 }, {}, "id_token_expired"],
      [{ iat: Math.floor(NOW / 1000) + 3600 }, {}, "id_token_not_yet_valid"],
      [{}, { kid: "unknown" }, "id_token_key_unknown"],
    ];
    for (const [claims, header, code] of cases) {
      const rig = await makeEnterpriseRig();
      const tenantId = await oidcTenant(rig);
      rig.idp.claimOverrides = claims;
      rig.idp.headerOverrides = header;
      const res = await oidcSignIn(rig, tenantId);
      expect(res.status, code).toBe(403);
      expect(await res.text(), code).toContain(code);
    }
  });

  it("verifyIdToken: skew, alg allowlist, signature, kid rotation", async () => {
    const idp = new FakeOidcIdp();
    const deps = { fetch: idp.fetch };
    const cache = new OidcCache();
    const now = Math.floor(NOW / 1000);
    const base = { iss: IDP_ISSUER, sub: "s", aud: "shogun-client", iat: now, exp: now + 60, nonce: "n" };
    const params = { issuer: IDP_ISSUER, clientId: "shogun-client", nonce: "n", jwksUri: `${IDP_ISSUER}/jwks`, nowMs: NOW, skewSecs: 120 };
    expect((await verifyIdToken(idp.signJwt(base), params, deps, cache)).sub).toBe("s");
    // Within the skew both ways.
    await expect(verifyIdToken(idp.signJwt({ ...base, exp: now - 60 }), params, deps, cache)).resolves.toBeTruthy();
    await expect(verifyIdToken(idp.signJwt({ ...base, iat: now + 60, nbf: now + 60 }), params, deps, cache)).resolves.toBeTruthy();
    await expect(verifyIdToken(idp.signJwt({ ...base, nbf: now + 600 }), params, deps, cache)).rejects.toMatchObject({ code: "id_token_not_yet_valid" });
    // alg none / HS256 are refused before any key is looked at.
    const unsigned = `${Buffer.from(JSON.stringify({ alg: "none" })).toString("base64url")}.${Buffer.from(JSON.stringify(base)).toString("base64url")}.x`;
    await expect(verifyIdToken(unsigned, params, deps, cache)).rejects.toMatchObject({ code: "id_token_alg_refused" });
    await expect(verifyIdToken(idp.signJwt(base, { alg: "HS256" }), params, deps, cache)).rejects.toMatchObject({ code: "id_token_alg_refused" });
    // A tampered payload.
    const [h, , s] = idp.signJwt(base).split(".");
    const forged = `${h}.${Buffer.from(JSON.stringify({ ...base, sub: "attacker" })).toString("base64url")}.${s}`;
    await expect(verifyIdToken(forged, params, deps, cache)).rejects.toMatchObject({ code: "id_token_signature_invalid" });
    // Another key with the same kid.
    const other = generateKeyPairSync("rsa", { modulusLength: 2048 });
    const hh = Buffer.from(JSON.stringify({ alg: "RS256", kid: idp.kid })).toString("base64url");
    const pp = Buffer.from(JSON.stringify(base)).toString("base64url");
    const wrongKey = `${hh}.${pp}.${nodeSign("sha256", Buffer.from(`${hh}.${pp}`), other.privateKey).toString("base64url")}`;
    await expect(verifyIdToken(wrongKey, params, deps, cache)).rejects.toMatchObject({ code: "id_token_signature_invalid" });
    // Key rotation: a new kid is fetched once more.
    idp.kid = "k2";
    const before = idp.requests.filter((r) => r.endsWith("/jwks")).length;
    await expect(verifyIdToken(idp.signJwt(base), params, deps, cache)).resolves.toBeTruthy();
    expect(idp.requests.filter((r) => r.endsWith("/jwks")).length).toBe(before + 1);
    // ES256 with an EC key.
    const ec = generateKeyPairSync("ec", { namedCurve: "P-256" });
    const ecJwk = { ...(ec.publicKey.export({ format: "jwk" }) as Record<string, unknown>), kid: "ec1", alg: "ES256" };
    const ecFetch: IdpFetch = (url, init) => url.endsWith("/jwks") ? Promise.resolve(new Response(JSON.stringify({ keys: [ecJwk] }), { headers: { "content-type": "application/json" } })) : idp.fetch(url, init);
    const eh = Buffer.from(JSON.stringify({ alg: "ES256", kid: "ec1" })).toString("base64url");
    const esSig = nodeSign("sha256", Buffer.from(`${eh}.${pp}`), { key: ec.privateKey, dsaEncoding: "ieee-p1363" }).toString("base64url");
    await expect(verifyIdToken(`${eh}.${pp}.${esSig}`, params, { fetch: ecFetch }, new OidcCache())).resolves.toMatchObject({ sub: "s" });
  });

  it("discovery must name the configured issuer exactly", async () => {
    const idp = new FakeOidcIdp();
    idp.discoveryOverrides = { issuer: "https://idp.example.com/other" };
    await expect(discover(IDP_ISSUER, { fetch: idp.fetch }, new OidcCache(), NOW)).rejects.toMatchObject({ code: "discovery_issuer_mismatch" });
    idp.discoveryOverrides = { token_endpoint: "http://idp.example.com/token" };
    await expect(discover(IDP_ISSUER, { fetch: idp.fetch }, new OidcCache(), NOW)).rejects.toBeInstanceOf(OidcError);
  });
});

describe("the SSRF guard on IdP fetches", () => {
  it("refuses non-https, private and reserved hosts, private resolutions and redirects", async () => {
    for (const bad of ["http://idp.example.com/x", "https://localhost/x", "https://10.0.0.1/x", "https://169.254.169.254/latest", "https://[::1]/x", "https://idp.internal/x", "https://u:p@idp.example.com/x"]) {
      expect(() => idpUrl(bad), bad).toThrow(IdpFetchError);
    }
    const ok: IdpFetch = () => Promise.resolve(new Response("{}", { status: 200 }));
    await expect(idpFetch("https://idp.example.com/x", { method: "GET" }, { fetch: ok, resolveHost: () => Promise.resolve(["10.1.2.3"]) })).rejects.toMatchObject({ code: "idp_host_refused" });
    const redirect: IdpFetch = () => Promise.resolve(new Response(null, { status: 302, headers: { location: "http://169.254.169.254/" } }));
    await expect(idpFetch("https://idp.example.com/x", { method: "GET" }, { fetch: redirect })).rejects.toMatchObject({ code: "idp_redirected" });
    const huge: IdpFetch = () => Promise.resolve(new Response("x".repeat(70_000), { status: 200 }));
    await expect(idpFetch("https://idp.example.com/x", { method: "GET" }, { fetch: huge })).rejects.toMatchObject({ code: "idp_response_too_large" });
    let seen: RequestInit | undefined;
    const spy: IdpFetch = (_u, init) => { seen = init as RequestInit; return Promise.resolve(new Response("{}")); };
    await idpFetch("https://idp.example.com/x", { method: "GET" }, { fetch: spy });
    expect(seen?.redirect).toBe("manual");
  });

  it("an SSO configuration on a private host is refused at the operator route", async () => {
    const rig = await makeEnterpriseRig();
    const res = await adminCall(rig, "/v1/admin/tenants", { method: "POST", idem: "i1", body: { name: "X", plan: "enterprise", sso: { protocol: "oidc", oidc: { issuer: "https://10.0.0.1", client_id: "c" } } } });
    expect(res.status).toBe(400);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("idp_host_refused");
  });
});

describe("admin sessions", () => {
  it("expire, end when the admin is removed, refuse foreign origins, and cannot be forged", async () => {
    const rig = await makeEnterpriseRig();
    const tenantId = await oidcTenant(rig);
    const res = await oidcSignIn(rig, tenantId);
    const cookie = `${SESSION_COOKIE}=${cookiesOf(res)[SESSION_COOKIE]?.value}`;
    expect((await rig.app.request("/v1/tenant/me", { headers: { cookie } })).status).toBe(200);
    expect((await rig.app.request("/v1/tenant/logout", { method: "POST", headers: { cookie, origin: "https://evil.example" } })).status).toBe(403);
    // Forged: signed with another key.
    const forged = signClaims(derive("another-secret-of-at-least-32-chars!!", "session"), { a: "tad_x", t: tenantId, exp: Math.floor(NOW / 1000) + 600 });
    expect((await rig.app.request("/v1/tenant/me", { headers: { cookie: `${SESSION_COOKIE}=${forged}` } })).status).toBe(401);
    // Removal.
    const admins = (await (await adminCall(rig, `/v1/admin/tenants/${tenantId}`)).json()) as { admins: Array<{ admin_id: string }> };
    const adminId = admins.admins[0]?.admin_id ?? "";
    expect((await adminCall(rig, `/v1/admin/tenants/${tenantId}/admins/${adminId}`, { method: "DELETE" })).status).toBe(200);
    expect((await rig.app.request("/v1/tenant/me", { headers: { cookie } })).status).toBe(401);
    // Expiry.
    const rig2 = await makeEnterpriseRig();
    const t2 = await oidcTenant(rig2);
    const c2 = `${SESSION_COOKIE}=${cookiesOf(await oidcSignIn(rig2, t2))[SESSION_COOKIE]?.value}`;
    rig2.setNow(new Date(NOW + (SESSION_TTL_SECS + 1) * 1000));
    const expired = await rig2.app.request("/v1/tenant/me", { headers: { cookie: c2 } });
    expect(expired.status).toBe(401);
    expect(((await expired.json()) as { error: { next_action: string } }).error.next_action).toBe("sign_in");
  });

  it("seal and the session signature are bound to their keys and labels", async () => {
    const key = derive(ENTERPRISE_SECRET, "sso-flow");
    const sealed = await seal(key, "sso-flow", "secret-verifier");
    expect(sealed).not.toContain("secret-verifier");
    expect(await unseal(key, "sso-flow", sealed)).toBe("secret-verifier");
    expect(await unseal(key, "other-label", sealed)).toBeUndefined();
    expect(await unseal(derive(ENTERPRISE_SECRET, "config"), "sso-flow", sealed)).toBeUndefined();
    expect(await unseal(key, "sso-flow", sealed.slice(0, -2) + "AA")).toBeUndefined();
    const s = derive(ENTERPRISE_SECRET, "session");
    expect(verifyClaims(s, signClaims(s, { a: 1 }))).toEqual({ a: 1 });
    expect(verifyClaims(s, signClaims(s, { a: 1 }).replace(/\.[^.]+$/, ".AAAA"))).toBeUndefined();
  });
});

// --- SAML ---

async function samlTenant(rig: EnterpriseRig, keys: SamlKeys, extraCerts: string[] = []): Promise<string> {
  const { tenantId } = await createTenant(rig, {
    name: "Acme SAML", region: "us", plan: "enterprise",
    sso: { protocol: "saml", enforced: false, saml: { idp_entity_id: SAML_IDP_ENTITY, sso_url: "https://acme.okta.example/app/sso/saml", certificates: [...extraCerts, keys.publicPem] } },
  });
  expect((await adminCall(rig, `/v1/admin/tenants/${tenantId}/admins`, { method: "POST", body: { email: "admin@acme.example" } })).status).toBe(201);
  return tenantId;
}

async function samlStart(rig: EnterpriseRig, tenantId: string): Promise<{ requestId: string; relayState: string; flow: string }> {
  const res = await rig.app.request(`/sso/${tenantId}/login`);
  expect(res.status).toBe(302);
  const url = new URL(res.headers.get("location") ?? "");
  expect(url.origin + url.pathname).toBe("https://acme.okta.example/app/sso/saml");
  const xml = inflateRawSync(Buffer.from(url.searchParams.get("SAMLRequest") ?? "", "base64")).toString("utf8");
  expect(xml).toContain(`AssertionConsumerServiceURL="${BASE}/sso/${tenantId}/saml/acs"`);
  expect(xml).toContain(`<saml:Issuer>${BASE}/sso/${tenantId}/saml/metadata</saml:Issuer>`);
  const requestId = /ID="([^"]+)"/.exec(xml)?.[1] ?? "";
  const flow = cookiesOf(res)[FLOW_COOKIE];
  expect(flow?.raw).toContain("SameSite=None");
  return { requestId, relayState: url.searchParams.get("RelayState") ?? "", flow: flow?.value ?? "" };
}

function fixture(rig: EnterpriseRig, tenantId: string, requestId: string, keys: SamlKeys, over: Partial<SamlFixture> = {}): SamlFixture {
  return {
    inResponseTo: requestId, destination: `${BASE}/sso/${tenantId}/saml/acs`, recipient: `${BASE}/sso/${tenantId}/saml/acs`,
    audience: `${BASE}/sso/${tenantId}/saml/metadata`, nameId: "admin@acme.example", nowMs: rig.now().getTime(), keys, ...over,
  };
}

function postAcs(rig: EnterpriseRig, tenantId: string, flow: string, relayState: string, samlB64: string): Promise<Response> {
  const form = new URLSearchParams({ SAMLResponse: samlB64, RelayState: relayState });
  return Promise.resolve(rig.app.request(`/sso/${tenantId}/saml/acs`, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded", cookie: `${FLOW_COOKIE}=${flow}` }, body: form.toString() }));
}

describe("SAML 2.0 sign-in for tenant admins", () => {
  for (const shape of ["okta", "entra"] as const) {
    it(`accepts a signed ${shape} assertion once, and refuses its replay`, async () => {
      const rig = await makeEnterpriseRig();
      const keys = samlKeys();
      const tenantId = await samlTenant(rig, keys, [samlKeys().publicPem]);
      const { requestId, relayState, flow } = await samlStart(rig, tenantId);
      const b64 = samlResponse(fixture(rig, tenantId, requestId, keys, shape === "entra" ? { shape, nameId: "8f3a-opaque-object-id", nameIdFormat: "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent", email: "admin@acme.example" } : { shape }));
      const ok = await postAcs(rig, tenantId, flow, relayState, b64);
      expect(ok.status, await ok.clone().text()).toBe(200);
      const session = cookiesOf(ok)[SESSION_COOKIE];
      expect(session?.raw).toContain("SameSite=Strict");
      expect((await rig.app.request("/v1/tenant/me", { headers: { cookie: `${SESSION_COOKIE}=${session?.value}` } })).status).toBe(200);
      // Replay: the same assertion again (even with a fresh flow cookie for the same request).
      const replay = await postAcs(rig, tenantId, flow, relayState, b64);
      expect(replay.status).toBe(403);
      expect(await replay.text()).toContain("saml_replay");
      expect(rig.logLines.join("\n")).not.toContain("admin@acme.example");
      expect(rig.logLines.join("\n")).not.toContain(b64.slice(0, 40));
    });
  }

  it("serves SP metadata", async () => {
    const rig = await makeEnterpriseRig();
    const tenantId = await samlTenant(rig, samlKeys());
    const res = await rig.app.request(`/sso/${tenantId}/saml/metadata`);
    expect(res.status).toBe(200);
    const xml = await res.text();
    expect(xml).toContain(`entityID="${BASE}/sso/${tenantId}/saml/metadata"`);
    expect(xml).toContain('WantAssertionsSigned="true"');
  });

  it("refuses unsigned, wrongly signed, misaddressed, expired and wrapped responses", async () => {
    const rig = await makeEnterpriseRig();
    const keys = samlKeys();
    const tenantId = await samlTenant(rig, keys);
    const requestId = "_req1";
    const acs = `${BASE}/sso/${tenantId}/saml/acs`;
    const p = { spEntityId: `${BASE}/sso/${tenantId}/saml/metadata`, acsUrl: acs, idpEntityId: SAML_IDP_ENTITY, certificates: [keys.publicPem], requestId, nowMs: NOW, skewSecs: 120 };
    const f = (over: Partial<SamlFixture> = {}): string => samlResponse(fixture(rig, tenantId, requestId, keys, over));
    const code = (b64: string, params = p): string | undefined => {
      try { validateSamlResponse(b64, params); return undefined; } catch (e) { return (e as SamlError).code; }
    };
    expect(code(f())).toBeUndefined();
    expect(validateSamlResponse(f(), p)).toMatchObject({ nameId: "admin@acme.example", email: "admin@acme.example", issuer: SAML_IDP_ENTITY });
    expect(code(f({ sign: "none" }))).toBe("saml_assertion_unsigned");
    expect(code(f({ sign: "response" }))).toBe("saml_assertion_unsigned");
    expect(code(f({ keys: { ...samlKeys(), publicPem: keys.publicPem } }))).toBe("saml_signature_invalid");
    expect(code(f({ audience: "https://someone-else.example/sp" }))).toBe("saml_audience_mismatch");
    expect(code(f({ recipient: "https://evil.example/acs" }))).toBe("saml_recipient_mismatch");
    expect(code(f({ destination: "https://evil.example/acs" }))).toBe("saml_destination_mismatch");
    expect(code(f({ inResponseTo: "_other" }))).toBe("saml_in_response_to_mismatch");
    expect(code(f({ issuer: "https://evil.example/idp" }))).toBe("saml_issuer_mismatch");
    expect(code(f({ notOnOrAfterMs: NOW - 10 * 60_000 }))).toBe("saml_expired");
    expect(code(f({ notOnOrAfterMs: NOW - 60_000 }))).toBeUndefined(); // within the skew
    expect(code(f({ notBeforeMs: NOW + 10 * 60_000 }))).toBe("saml_not_yet_valid");
    expect(code(f({ status: "urn:oasis:names:tc:SAML:2.0:status:Requester" }))).toBe("saml_status_not_success");
    expect(code(f({ signatureAlgorithm: "http://www.w3.org/2000/09/xmldsig#rsa-sha1" }))).toBe("saml_algorithm_refused");
    expect(code(f({ digestAlgorithm: "http://www.w3.org/2000/09/xmldsig#sha1" }))).toBe("saml_algorithm_refused");
    // Tampering after signing: the NameID changes, the digest no longer matches.
    expect(code(f({ after: (x) => x.replace(">admin@acme.example<", ">attacker@acme.example<") }))).toBe("saml_signature_invalid");
    // Wrapping: a second, unsigned assertion naming the attacker.
    expect(code(f({ after: (x) => x.replace("</saml2p:Response>", `<saml2:Assertion xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion" ID="evil"><saml2:Issuer>${SAML_IDP_ENTITY}</saml2:Issuer></saml2:Assertion></saml2p:Response>`) }))).toBe("saml_assertion_count");
    // Wrapping: the signed assertion moved inside an Extensions element, an evil one in its place.
    expect(code(f({ after: (x) => x.replace(/(<saml2:Assertion[\s\S]*<\/saml2:Assertion>)/, (m) => `<saml2p:Extensions>${m}</saml2p:Extensions>${m.replace(/ID="[^"]+"/, 'ID="evil"').replace("admin@acme.example", "attacker@acme.example")}`) }))).toBe("saml_assertion_count");
    // DOCTYPE / entities.
    const doctype = Buffer.from(`<!DOCTYPE r [<!ENTITY x "y">]>${Buffer.from(f(), "base64").toString("utf8")}`).toString("base64");
    expect(code(doctype)).toBe("saml_doctype_refused");
    // Encrypted assertions are not supported.
    expect(code(f({ after: (x) => x.replace("</saml2p:Response>", '<saml2:EncryptedAssertion xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion"/></saml2p:Response>') }))).toBe("saml_encrypted_assertion_unsupported");
    // E39 strictness. An encrypted NameID or attribute is refused like an encrypted assertion.
    expect(code(f({ after: (x) => x.replace("</saml2p:Response>", '<saml2:EncryptedID xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion"/></saml2p:Response>') }))).toBe("saml_encrypted_assertion_unsupported");
    expect(code(f({ after: (x) => x.replace("</saml2p:Response>", '<saml2:EncryptedAttribute xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion"/></saml2p:Response>') }))).toBe("saml_encrypted_assertion_unsupported");
    // The Response's own Issuer, when present, is the IdP (it can only refuse: it is unsigned here).
    expect(code(f({ after: (x) => x.replace(/(<saml2p:Response[^>]*>\s*<saml2:Issuer[^>]*>)[^<]+/, `$1https://evil.example/idp`) }))).toBe("saml_issuer_mismatch");
    // SAML 2.0 only.
    expect(code(f({ after: (x) => x.replace(/(<saml2p:Response[^>]*?)Version="2.0"/, '$1Version="1.1"') }))).toBe("saml_malformed");
    // An assertion issued in the future, or before the sign-in flow could have started.
    expect(code(f({ nowMs: NOW + 10 * 60_000, notBeforeMs: NOW - 60_000, notOnOrAfterMs: NOW + 30 * 60_000, subjectNotOnOrAfterMs: NOW + 30 * 60_000 }))).toBe("saml_not_yet_valid");
    expect(code(f({ nowMs: NOW - 20 * 60_000, notBeforeMs: NOW - 21 * 60_000, notOnOrAfterMs: NOW + 30 * 60_000, subjectNotOnOrAfterMs: NOW + 30 * 60_000 }))).toBe("saml_expired");
    expect(code(f({ nowMs: NOW - 5 * 60_000, notBeforeMs: NOW - 6 * 60_000 }))).toBeUndefined(); // five minutes old: fine
    // A bearer confirmation that only starts later confirms nothing yet.
    expect(code(f({ subjectNotBeforeMs: NOW + 10 * 60_000 }))).toBe("saml_not_yet_valid");
    expect(code(f({ subjectNotBeforeMs: NOW - 60_000 }))).toBeUndefined();
    expect(code("not base64 xml at all")).toBe("saml_malformed");
  });

  it("refuses a response without the flow cookie or with the wrong RelayState", async () => {
    const rig = await makeEnterpriseRig();
    const keys = samlKeys();
    const tenantId = await samlTenant(rig, keys);
    const { requestId, relayState, flow } = await samlStart(rig, tenantId);
    const b64 = samlResponse(fixture(rig, tenantId, requestId, keys));
    expect(await (await postAcs(rig, tenantId, "", relayState, b64)).text()).toContain("flow_missing");
    expect(await (await postAcs(rig, tenantId, flow, "forged", b64)).text()).toContain("state_mismatch");
    // A response for a different request (IdP-initiated or stolen) is refused.
    const unsolicited = samlResponse(fixture(rig, tenantId, "_someone-elses-request", keys));
    expect(await (await postAcs(rig, tenantId, flow, relayState, unsolicited)).text()).toContain("saml_in_response_to_mismatch");
  });
});
