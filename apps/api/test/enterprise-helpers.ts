/** Shared rig for the enterprise lane (E26): the agent rig with the enterprise deps wired, an
 * operator `admin` key, a fake sidecar that records operator calls, a fake OIDC IdP and SAML
 * fixture builders modelled on what Okta and Entra ID send. Keys are generated per run; no key
 * material is committed. */
import { createHash, generateKeyPairSync, randomUUID, sign as nodeSign, type KeyObject } from "node:crypto";
import { SignedXml } from "xml-crypto";

import { generateAgentKey, hashAgentKey } from "../src/agent-keys.js";
import { InMemoryAgentStore } from "../src/agent-store.js";
import type { EnterpriseDeps } from "../src/enterprise/app.js";
import type { IdpFetch } from "../src/enterprise/fetch-guard.js";
import { OidcCache } from "../src/enterprise/oidc.js";
import type { EnterpriseSidecar, ExportResult, OpResult } from "../src/enterprise/sidecar-ops.js";
import { InMemoryEnterpriseStore } from "../src/enterprise/store.js";
import { CimdCache, type CimdFetch } from "../src/oauth/cimd.js";
import { InMemoryOAuthStore } from "../src/oauth/store.js";
import { BASE, TEST_KEY_ITERATIONS, makeAgentRig, type AgentRig } from "./agent-helpers.js";

export const ENTERPRISE_SECRET = "test-enterprise-secret-at-least-32-characters";
export const OAUTH_SECRET = "test-oauth-secret-at-least-32-characters!!";
export const EU_ENDPOINT = "https://eu.api.test";
export const JP_ENDPOINT = "https://jp.api.test";

// --- fake sidecar ---

export class FakeSidecar implements EnterpriseSidecar {
  readonly calls: string[] = [];
  readonly teams = new Map<string, string[]>();
  readonly grants = new Set<string>();
  readonly purged: string[] = [];
  exports = new Map<string, string | { status: number; code: string }>();
  failNext = 0;
  purgeAnswer: OpResult = { ok: true };

  private fail(): OpResult | undefined {
    if (this.failNext > 0) {
      this.failNext -= 1;
      return { ok: false, status: 503, code: "sidecar_unavailable" };
    }
    return undefined;
  }

  setTeam(tenantId: string, teamId: string, members: readonly string[]): Promise<OpResult> {
    this.calls.push(`team ${teamId} ${members.length}`);
    const f = this.fail();
    if (f) return Promise.resolve(f);
    this.teams.set(`${tenantId}/${teamId}`, [...members]);
    return Promise.resolve({ ok: true });
  }

  grantWorldRead(owner: string, grantee: string): Promise<OpResult> {
    this.calls.push(`grant ${owner}>${grantee}`);
    const f = this.fail();
    if (f) return Promise.resolve(f);
    this.grants.add(`${owner}>${grantee}`);
    return Promise.resolve({ ok: true });
  }

  revokeWorldRead(owner: string, grantee: string): Promise<OpResult> {
    this.calls.push(`revoke ${owner}>${grantee}`);
    const f = this.fail();
    if (f) return Promise.resolve(f);
    this.grants.delete(`${owner}>${grantee}`);
    return Promise.resolve({ ok: true });
  }

  auditExport(principalId: string): Promise<ExportResult> {
    this.calls.push(`export ${principalId}`);
    const body = this.exports.get(principalId);
    if (body === undefined) return Promise.resolve({ ok: true, body: new Response("").body, contentType: "application/x-ndjson" });
    if (typeof body !== "string") return Promise.resolve({ ok: false, ...body });
    // Deliver in two chunks to exercise the streaming path.
    const bytes = new TextEncoder().encode(body);
    const cut = Math.floor(bytes.length / 2);
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(bytes.slice(0, cut));
        controller.enqueue(bytes.slice(cut));
        controller.close();
      },
    });
    return Promise.resolve({ ok: true, body: stream, contentType: "application/x-ndjson" });
  }

  purgePrincipal(principalId: string): Promise<OpResult> {
    this.calls.push(`purge ${principalId}`);
    if (this.purgeAnswer.ok) this.purged.push(principalId);
    return Promise.resolve(this.purgeAnswer);
  }
}

// --- fake OIDC IdP ---

export const IDP_ISSUER = "https://idp.example.com";

function b64urlJson(v: unknown): string {
  return Buffer.from(JSON.stringify(v)).toString("base64url");
}

export interface IdpUser {
  sub: string;
  email?: string;
  emailVerified?: boolean;
}

export class FakeOidcIdp {
  readonly key: { privateKey: KeyObject; publicKey: KeyObject };
  kid = "k1";
  readonly codes = new Map<string, { nonce: string; challenge: string; clientId: string; redirectUri: string; user: IdpUser }>();
  /** Overrides applied to the next ID token's claims (or header). */
  claimOverrides: Record<string, unknown> = {};
  headerOverrides: Record<string, unknown> = {};
  clientSecret: string | undefined = "idp-client-secret";
  discoveryOverrides: Record<string, unknown> = {};
  readonly requests: string[] = [];
  nowMs: () => number = Date.now;

  constructor(readonly issuer = IDP_ISSUER, readonly clientId = "shogun-client") {
    this.key = generateKeyPairSync("rsa", { modulusLength: 2048 });
  }

  jwks(): Record<string, unknown> {
    return { keys: [{ ...(this.key.publicKey.export({ format: "jwk" }) as Record<string, unknown>), kid: this.kid, use: "sig", alg: "RS256" }] };
  }

  signJwt(claims: Record<string, unknown>, header: Record<string, unknown> = {}): string {
    const h = b64urlJson({ alg: "RS256", typ: "JWT", kid: this.kid, ...header });
    const p = b64urlJson(claims);
    const sig = nodeSign("sha256", Buffer.from(`${h}.${p}`), this.key.privateKey).toString("base64url");
    return `${h}.${p}.${sig}`;
  }

  /** The human signing in at the authorization endpoint: returns the code the IdP redirects with. */
  authorize(location: string, user: IdpUser): { code: string; state: string; redirectUri: string } {
    const url = new URL(location);
    const code = `code_${randomUUID()}`;
    this.codes.set(code, {
      nonce: url.searchParams.get("nonce") ?? "", challenge: url.searchParams.get("code_challenge") ?? "",
      clientId: url.searchParams.get("client_id") ?? "", redirectUri: url.searchParams.get("redirect_uri") ?? "", user,
    });
    return { code, state: url.searchParams.get("state") ?? "", redirectUri: url.searchParams.get("redirect_uri") ?? "" };
  }

  readonly fetch: IdpFetch = async (url, init) => {
    this.requests.push(`${init.method} ${url}`);
    const json = (v: unknown, status = 200): Response => new Response(JSON.stringify(v), { status, headers: { "content-type": "application/json" } });
    if (url === `${this.issuer}/.well-known/openid-configuration`) {
      return json({ issuer: this.issuer, authorization_endpoint: `${this.issuer}/authorize`, token_endpoint: `${this.issuer}/token`, jwks_uri: `${this.issuer}/jwks`, ...this.discoveryOverrides });
    }
    if (url === `${this.issuer}/jwks`) return json(this.jwks());
    if (url === `${this.issuer}/token` && init.method === "POST") {
      const form = new URLSearchParams(init.body ?? "");
      const entry = this.codes.get(form.get("code") ?? "");
      if (!entry) return json({ error: "invalid_grant" }, 400);
      this.codes.delete(form.get("code") ?? "");
      const verifier = form.get("code_verifier") ?? "";
      if (createHash("sha256").update(verifier).digest("base64url") !== entry.challenge) return json({ error: "invalid_grant" }, 400);
      if (form.get("client_id") !== entry.clientId || form.get("redirect_uri") !== entry.redirectUri) return json({ error: "invalid_grant" }, 400);
      if (this.clientSecret !== undefined && form.get("client_secret") !== this.clientSecret) return json({ error: "invalid_client" }, 401);
      const now = Math.floor(this.nowMs() / 1000);
      const claims = {
        iss: this.issuer, sub: entry.user.sub, aud: entry.clientId, iat: now, exp: now + 300, nonce: entry.nonce,
        ...(entry.user.email ? { email: entry.user.email, email_verified: entry.user.emailVerified ?? true } : {}),
        ...this.claimOverrides,
      };
      return json({ access_token: "idp-access-token", token_type: "Bearer", id_token: this.signJwt(claims, this.headerOverrides) });
    }
    return json({ error: "not_found" }, 404);
  };
}

// --- SAML fixtures ---

export const SAML_IDP_ENTITY = "http://www.okta.com/exk1fake";

export interface SamlKeys {
  privatePem: string;
  publicPem: string;
}

export function samlKeys(): SamlKeys {
  const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  return { privatePem: privateKey.export({ format: "pem", type: "pkcs8" }).toString(), publicPem: publicKey.export({ format: "pem", type: "spki" }).toString() };
}

export interface SamlFixture {
  shape?: "okta" | "entra";
  responseId?: string;
  assertionId?: string;
  inResponseTo: string;
  destination: string;
  recipient: string;
  audience: string;
  issuer?: string;
  nameId: string;
  nameIdFormat?: string;
  email?: string;
  nowMs: number;
  notBeforeMs?: number;
  notOnOrAfterMs?: number;
  subjectNotOnOrAfterMs?: number;
  status?: string;
  /** What carries the signature. */
  sign?: "assertion" | "response" | "none";
  signatureAlgorithm?: string;
  digestAlgorithm?: string;
  keys: SamlKeys;
  /** Applied to the signed XML string before encoding (tampering). */
  after?: (xml: string) => string;
}

const iso = (ms: number): string => new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");

/** A base64 `SAMLResponse` shaped like Okta's (Signature after the assertion's Issuer, NameID
 * emailAddress) or Entra ID's (an AttributeStatement with the claims-namespace e-mail). */
export function samlResponse(f: SamlFixture): string {
  const assertionId = f.assertionId ?? `id${randomUUID().replace(/-/g, "")}`;
  const responseId = f.responseId ?? `id${randomUUID().replace(/-/g, "")}`;
  const issuer = f.issuer ?? SAML_IDP_ENTITY;
  const notBefore = iso(f.notBeforeMs ?? f.nowMs - 60_000);
  const notOnOrAfter = iso(f.notOnOrAfterMs ?? f.nowMs + 5 * 60_000);
  const subjectUntil = iso(f.subjectNotOnOrAfterMs ?? f.nowMs + 5 * 60_000);
  const format = f.nameIdFormat ?? "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
  const attributes = f.shape === "entra"
    ? `<AttributeStatement><Attribute Name="http://schemas.microsoft.com/identity/claims/tenantid"><AttributeValue>00000000-0000-0000-0000-000000000000</AttributeValue></Attribute>${f.email ? `<Attribute Name="http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress"><AttributeValue>${f.email}</AttributeValue></Attribute>` : ""}</AttributeStatement>`
    : f.email ? `<saml2:AttributeStatement><saml2:Attribute Name="email" NameFormat="urn:oasis:names:tc:SAML:2.0:attrname-format:unspecified"><saml2:AttributeValue>${f.email}</saml2:AttributeValue></saml2:Attribute></saml2:AttributeStatement>` : "";
  let xml: string;
  if (f.shape === "entra") {
    xml = `<samlp:Response ID="_${responseId}" Version="2.0" IssueInstant="${iso(f.nowMs)}" Destination="${f.destination}" InResponseTo="${f.inResponseTo}" xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"><Issuer xmlns="urn:oasis:names:tc:SAML:2.0:assertion">${issuer}</Issuer><samlp:Status><samlp:StatusCode Value="${f.status ?? "urn:oasis:names:tc:SAML:2.0:status:Success"}"/></samlp:Status><Assertion ID="_${assertionId}" IssueInstant="${iso(f.nowMs)}" Version="2.0" xmlns="urn:oasis:names:tc:SAML:2.0:assertion"><Issuer>${issuer}</Issuer><Subject><NameID Format="${format}">${f.nameId}</NameID><SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><SubjectConfirmationData InResponseTo="${f.inResponseTo}" NotOnOrAfter="${subjectUntil}" Recipient="${f.recipient}"/></SubjectConfirmation></Subject><Conditions NotBefore="${notBefore}" NotOnOrAfter="${notOnOrAfter}"><AudienceRestriction><Audience>${f.audience}</Audience></AudienceRestriction></Conditions>${attributes}<AuthnStatement AuthnInstant="${iso(f.nowMs)}" SessionIndex="_${assertionId}"><AuthnContext><AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:Password</AuthnContextClassRef></AuthnContext></AuthnStatement></Assertion></samlp:Response>`;
  } else {
    xml = `<saml2p:Response xmlns:saml2p="urn:oasis:names:tc:SAML:2.0:protocol" Destination="${f.destination}" ID="${responseId}" InResponseTo="${f.inResponseTo}" IssueInstant="${iso(f.nowMs)}" Version="2.0"><saml2:Issuer xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion" Format="urn:oasis:names:tc:SAML:2.0:nameid-format:entity">${issuer}</saml2:Issuer><saml2p:Status><saml2p:StatusCode Value="${f.status ?? "urn:oasis:names:tc:SAML:2.0:status:Success"}"/></saml2p:Status><saml2:Assertion xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion" ID="${assertionId}" IssueInstant="${iso(f.nowMs)}" Version="2.0"><saml2:Issuer Format="urn:oasis:names:tc:SAML:2.0:nameid-format:entity">${issuer}</saml2:Issuer><saml2:Subject><saml2:NameID Format="${format}">${f.nameId}</saml2:NameID><saml2:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml2:SubjectConfirmationData InResponseTo="${f.inResponseTo}" NotOnOrAfter="${subjectUntil}" Recipient="${f.recipient}"/></saml2:SubjectConfirmation></saml2:Subject><saml2:Conditions NotBefore="${notBefore}" NotOnOrAfter="${notOnOrAfter}"><saml2:AudienceRestriction><saml2:Audience>${f.audience}</saml2:Audience></saml2:AudienceRestriction></saml2:Conditions><saml2:AuthnStatement AuthnInstant="${iso(f.nowMs)}" SessionIndex="${assertionId}"><saml2:AuthnContext><saml2:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml2:AuthnContextClassRef></saml2:AuthnContext></saml2:AuthnStatement>${attributes}</saml2:Assertion></saml2p:Response>`;
  }
  const sign = f.sign ?? "assertion";
  if (sign !== "none") {
    const target = sign === "assertion" ? "//*[local-name(.)='Assertion']" : "/*[local-name(.)='Response']";
    const signer = new SignedXml({
      privateKey: f.keys.privatePem,
      signatureAlgorithm: f.signatureAlgorithm ?? "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256",
      canonicalizationAlgorithm: "http://www.w3.org/2001/10/xml-exc-c14n#",
    });
    signer.addReference({ xpath: target, digestAlgorithm: f.digestAlgorithm ?? "http://www.w3.org/2001/04/xmlenc#sha256", transforms: ["http://www.w3.org/2000/09/xmldsig#enveloped-signature", "http://www.w3.org/2001/10/xml-exc-c14n#"] });
    signer.computeSignature(xml, { location: { reference: `${target}/*[local-name(.)='Issuer']`, action: "after" } });
    xml = signer.getSignedXml();
  }
  if (f.after) xml = f.after(xml);
  return Buffer.from(xml).toString("base64");
}

// --- the rig ---

export interface EnterpriseRig extends AgentRig {
  enterprise: InMemoryEnterpriseStore;
  oauth: InMemoryOAuthStore;
  sidecar: FakeSidecar;
  idp: FakeOidcIdp;
  adminKey: string;
}

export async function makeEnterpriseRig(opts: { region?: string; sidecar?: boolean; graceDays?: number; cimdFetch?: CimdFetch } = {}): Promise<EnterpriseRig> {
  const enterprise = new InMemoryEnterpriseStore();
  const oauth = new InMemoryOAuthStore();
  const sidecar = new FakeSidecar();
  const idp = new FakeOidcIdp();
  const agentStore = new InMemoryAgentStore();
  const deps: EnterpriseDeps = {
    store: enterprise, secret: ENTERPRISE_SECRET,
    region: { served: opts.region ?? "us", endpoints: { eu: EU_ENDPOINT, jp: JP_ENDPOINT } },
    ...(opts.sidecar === false ? {} : { sidecar }),
    deleteGraceMs: (opts.graceDays ?? 30) * 86_400_000,
    idpFetch: idp.fetch, resolveHost: () => Promise.resolve(["93.184.216.34"]), oidcCache: new OidcCache(),
  };
  const rig = makeAgentRig({ enterprise: deps, oauth: { store: oauth, secret: OAUTH_SECRET, cache: new CimdCache(), resolveHost: () => Promise.resolve(["93.184.216.34"]), ...(opts.cimdFetch ? { fetch: opts.cimdFetch } : {}) } }, agentStore);
  idp.nowMs = () => rig.now().getTime();
  const adminKey = await insertKey(rig, `prn_${randomUUID()}`, ["admin"]);
  return { ...rig, enterprise, oauth, sidecar, idp, adminKey };
}

/** Insert a live key for a principal (creating the principal), returning the secret. */
export async function insertKey(rig: AgentRig, principalId: string, scopes: Array<"read" | "write" | "run" | "admin">, tenantId = principalId): Promise<string> {
  const { key, prefix } = generateAgentKey("live");
  const now = rig.now().getTime();
  if (!(await rig.store.principal(principalId))) await rig.store.createPrincipal({ principalId, tenantId, createdAt: now });
  await rig.store.insertKey({ keyId: `ak_${randomUUID()}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId, scopes, createdAt: now });
  return key;
}

export function adminCall(rig: EnterpriseRig, path: string, init: { method?: string; body?: unknown; idem?: string } = {}): Promise<Response> {
  return Promise.resolve(rig.app.request(path, {
    method: init.method ?? "GET",
    headers: { authorization: `Bearer ${rig.adminKey}`, "content-type": "application/json", ...(init.idem ? { "idempotency-key": init.idem } : {}) },
    ...(init.body !== undefined ? { body: JSON.stringify(init.body) } : {}),
  }));
}

export async function createTenant(rig: EnterpriseRig, body: Record<string, unknown>): Promise<{ tenantId: string; scimToken: string | null; json: Record<string, unknown> }> {
  const res = await adminCall(rig, "/v1/admin/tenants", { method: "POST", body, idem: `idem-${randomUUID()}` });
  if (res.status !== 201) throw new Error(`tenant creation failed: ${res.status} ${await res.text()}`);
  const json = (await res.json()) as { tenant: { tenant_id: string }; scim_token: string | null };
  return { tenantId: json.tenant.tenant_id, scimToken: json.scim_token, json: json as unknown as Record<string, unknown> };
}

export function scim(rig: EnterpriseRig, token: string, path: string, init: { method?: string; body?: unknown } = {}): Promise<Response> {
  return Promise.resolve(rig.app.request(`/scim/v2${path}`, {
    method: init.method ?? "GET",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/scim+json" },
    ...(init.body !== undefined ? { body: JSON.stringify(init.body) } : {}),
  }));
}

/** Cookies from a response's Set-Cookie headers, as `name=value` pairs (attributes dropped). */
export function cookiesOf(res: Response): Record<string, { value: string; raw: string }> {
  const out: Record<string, { value: string; raw: string }> = {};
  for (const raw of res.headers.getSetCookie()) {
    const [pair = ""] = raw.split(";");
    const i = pair.indexOf("=");
    out[pair.slice(0, i)] = { value: pair.slice(i + 1), raw };
  }
  return out;
}

export { BASE };
