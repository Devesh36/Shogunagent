/**
 * The enterprise lane (docs/implementation-plan.md E26): tenants, tenant admins with SSO, SCIM
 * provisioning, region separation and the tenant audit export. Mounted by the agent app when
 * `ENTERPRISE_ENABLED=1`.
 *
 * Operator routes (an `admin` agent key, never an OAuth token):
 *   POST   /v1/admin/tenants                       create a tenant (Idempotency-Key) → SCIM token once
 *   GET    /v1/admin/tenants/{id}                  the tenant, its admins and member count
 *   PATCH  /v1/admin/tenants/{id}                  name, plan, SSO configuration (region is fixed)
 *   POST   /v1/admin/tenants/{id}/principals       attach an existing principal (at most one tenant)
 *   POST   /v1/admin/tenants/{id}/admins           add a tenant admin by e-mail
 *   DELETE /v1/admin/tenants/{id}/admins/{admin}   remove one (its sessions end at the next request)
 *   POST   /v1/admin/tenants/{id}/scim-token       rotate the SCIM token → shown once
 *
 * SSO for tenant admins (no agent key; a browser):
 *   GET  /sso/{tenant}/login                 start OIDC (code + PKCE) or SAML (SP-initiated)
 *   GET  /sso/{tenant}/oidc/callback         the IdP's redirect
 *   POST /sso/{tenant}/saml/acs              the IdP's POST (HTTP-POST binding)
 *   GET  /sso/{tenant}/saml/metadata         this SP's metadata for the IdP
 *
 * Tenant admin routes (the admin session cookie):
 *   GET  /v1/tenant/me                       the signed-in admin and the tenant
 *   GET  /v1/tenant/members                  member principals with their SCIM state
 *   POST /v1/tenant/members/{id}/keys        issue an agent key for a member (fills the key slot);
 *                                            optional Idempotency-Key: a retry answers the same key
 *                                            reference (never the secret again)
 *   GET  /v1/tenant/audit/export             every member's sidecar audit export, concatenated
 *   POST /v1/tenant/logout                   end the session
 *
 * SCIM 2.0 under /scim/v2 (`scim.ts`).
 *
 * **Sessions.** An admin session is a signed cookie (`crypto.ts`), `HttpOnly; Secure;
 * SameSite=Strict; Path=/`, valid `SESSION_TTL_SECS` and re-checked on every request against the
 * admin row (a removed admin is out at once). The SSO flow itself rides a separate, sealed,
 * ten-minute cookie (`SameSite=None`, because the SAML IdP returns with a cross-site POST) that
 * binds the state, nonce and PKCE verifier (OIDC) or the request id (SAML) to the browser that
 * started the login.
 *
 * Nothing here logs a token, a code, an assertion, a claim, an e-mail address or a SCIM body: the
 * log line carries a fixed event word (logging.ts).
 */
import { createPublicKey, randomUUID } from "node:crypto";
import { Hono, type Context, type MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import { DEFAULT_PBKDF2_ITERATIONS, agentError, agentPrincipalOf, generateAgentKey, hashAgentKey, type AgentKeyRecord, type AgentPrincipal, type AgentScope, type AgentVars } from "../agent-keys.js";
import type { AgentStore } from "../agent-store.js";
import type { LogFn } from "../logging.js";
import type { ResolveHost } from "../oauth/cimd.js";
import { escapeHtml } from "../oauth/app.js";
import type { OAuthStore } from "../oauth/store.js";
import { enterpriseKeys, pkceChallenge, randomId, randomToken, safeEqual, seal, sha256Hex, signClaims, unseal, verifyClaims, type EnterpriseKeys } from "./crypto.js";
import { IdpFetchError, idpUrl, type IdpFetch } from "./fetch-guard.js";
import { DEFAULT_CLOCK_SKEW_SECS, DEFAULT_OIDC_CACHE, OidcError, authorizationUrl, discover, exchangeCode, verifyIdToken, type OidcCache } from "./oidc.js";
import { REGION_RE, knownRegion, regionRefusal, type RegionConfig } from "./region.js";
import { SamlError, authnRequestXml, newRequestId, redirectBindingUrl, spMetadataXml, validateSamlResponse } from "./saml.js";
import { SCIM_BASE, createScimApp } from "./scim.js";
import type { EnterpriseSidecar } from "./sidecar-ops.js";
import { TENANT_PLANS, type EnterpriseStore, type SsoConfig, type TenantAdmin, type TenantPlan, type TenantRecord } from "./store.js";

export const SESSION_COOKIE = "__Host-shogun_tenant_admin";
export const FLOW_COOKIE = "__Host-shogun_sso_flow";
export const SESSION_TTL_SECS = 30 * 60;
export const FLOW_TTL_SECS = 10 * 60;
export const DEFAULT_DELETE_GRACE_DAYS = 30;
export const MEMBER_KEY_TTL_MS = 365 * 86_400_000;
const MAX_SMALL_BODY = 16 * 1024;
const MAX_ACS_BODY = 384 * 1024;
const MAX_NAME = 200;
const MAX_CERTS = 4;
const MAX_CERT_PEM = 8 * 1024;
const EMAIL_RE = /^[^\s@]{1,128}@[^\s@]{1,253}$/;
const MEMBER_SCOPES: readonly AgentScope[] = Object.freeze(["read", "write", "run"]);

export interface EnterpriseDeps {
  store: EnterpriseStore;
  /** `ENTERPRISE_SECRET` (≥ 32 characters). */
  secret: string;
  region: RegionConfig;
  /** Operator calls to the sidecar (teams, grants, audit export, purge). Absent = those answer 503. */
  sidecar?: EnterpriseSidecar;
  deleteGraceMs?: number;
  skewSecs?: number;
  /** IdP fetches (tests); default the global fetch under the guard. */
  idpFetch?: IdpFetch;
  resolveHost?: ResolveHost;
  oidcCache?: OidcCache;
}

export interface EnterpriseAppDeps extends EnterpriseDeps {
  agentStore: AgentStore;
  oauthStore?: OAuthStore;
  publicBaseUrl: string;
  now: () => Date;
  log: LogFn;
  keyIterations?: number;
  requireKey: (scopes: readonly AgentScope[]) => MiddlewareHandler<{ Variables: AgentVars }>;
}

type Vars = AgentVars & { tenantAdmin?: TenantAdmin; adminTenant?: TenantRecord };
type C = Context<{ Variables: Vars }>;

/** The OAuth consent policy (oauth/app.ts): a principal of an SSO-enforced tenant may not consent
 * by pasting a key. */
export function ssoConsentPolicy(store: Pick<EnterpriseStore, "membership" | "tenant">): (principalId: string) => Promise<"allow" | "sso_required"> {
  return async (principalId) => {
    const tenantId = await store.membership(principalId);
    const tenant = tenantId ? await store.tenant(tenantId) : undefined;
    return tenant?.plan === "enterprise" && tenant.sso?.enforced ? "sso_required" : "allow";
  };
}

// --- cookies ---

function cookieOf(header: string | undefined, name: string): string | undefined {
  for (const part of (header ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && part.slice(0, i).trim() === name) return part.slice(i + 1).trim();
  }
  return undefined;
}

const sessionCookie = (value: string, maxAge: number): string => `${SESSION_COOKIE}=${value}; Max-Age=${maxAge}; Path=/; HttpOnly; Secure; SameSite=Strict`;
const flowCookie = (value: string, maxAge: number): string => `${FLOW_COOKIE}=${value}; Max-Age=${maxAge}; Path=/; HttpOnly; Secure; SameSite=None`;

// --- pages ---

const PAGE_HEADERS = {
  "cache-control": "no-store", "referrer-policy": "no-referrer", "x-frame-options": "DENY", "x-content-type-options": "nosniff",
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
};

function page(c: Context, status: 200 | 400 | 403 | 421 | 503, title: string, body: string, extra: Record<string, string> = {}): Response {
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="referrer" content="no-referrer"><title>${escapeHtml(title)}</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:34rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}@media (prefers-color-scheme:dark){body{color:#eee;background:#111}a{color:#8ab4ff}}code{word-break:break-all}</style></head><body>${body}</body></html>`;
  return c.html(html, status, { ...PAGE_HEADERS, ...extra });
}

const SSO_ERROR_TEXT: Record<string, string> = {
  sso_not_configured: "Single sign-on is not set up for this organization.",
  flow_missing: "The sign-in took too long or was started in another browser. Start again.",
  state_mismatch: "The sign-in response does not match this browser's request. Start again.",
  not_an_admin: "You signed in, but you are not an administrator of this organization.",
  saml_replay: "This sign-in response was already used. Start again.",
  idp_error: "The identity provider refused or cancelled the sign-in.",
};

function ssoError(c: C, code: string, status: 400 | 403 | 503 = 400): Response {
  c.set("relayLogEvent", code === "saml_replay" ? "saml_replay" : "sso_login_refused");
  return page(c, status, "Sign-in failed", `<h1>Sign-in failed</h1><p>${escapeHtml(SSO_ERROR_TEXT[code] ?? "The sign-in response could not be verified. Start again.")}</p><p>Error: <code>${escapeHtml(code)}</code></p>`, { "set-cookie": flowCookie("", 0) });
}

// --- input ---

async function readJson(c: Context): Promise<Record<string, unknown> | undefined> {
  try {
    const v: unknown = await c.req.json();
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

class BadInput extends Error {
  constructor(readonly code: string, readonly detail: string) {
    super(code);
  }
}

const PEM_RE = /^-----BEGIN (CERTIFICATE|PUBLIC KEY)-----\r?\n[A-Za-z0-9+/=\r\n]+-----END \1-----\s*$/;

/** Parse an SSO configuration from an operator body; seals the OIDC client secret. */
async function parseSso(raw: unknown, keys: EnterpriseKeys, current: SsoConfig | undefined): Promise<SsoConfig> {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) throw new BadInput("invalid_sso", "sso must be an object: {protocol: \"oidc\"|\"saml\", enforced, oidc|saml}.");
  const s = raw as Record<string, unknown>;
  const protocol = s.protocol;
  if (protocol !== "oidc" && protocol !== "saml") throw new BadInput("invalid_sso", "sso.protocol must be oidc or saml.");
  const enforced = s.enforced === undefined ? false : s.enforced;
  if (typeof enforced !== "boolean") throw new BadInput("invalid_sso", "sso.enforced must be a boolean.");
  const url = (v: unknown, field: string): string => {
    if (typeof v !== "string") throw new BadInput("invalid_sso", `${field} must be an https URL.`);
    try {
      // Validated, then kept exactly as given: an OIDC issuer is compared to `iss` byte for byte.
      idpUrl(v);
      return v;
    } catch (e) {
      throw new BadInput(e instanceof IdpFetchError && e.code === "idp_host_refused" ? "idp_host_refused" : "invalid_sso", `${field} must be an https URL on a public host.`);
    }
  };
  if (protocol === "oidc") {
    const o = (typeof s.oidc === "object" && s.oidc !== null ? s.oidc : {}) as Record<string, unknown>;
    const issuer = url(o.issuer, "sso.oidc.issuer");
    const clientId = typeof o.client_id === "string" && o.client_id.length > 0 && o.client_id.length <= 512 ? o.client_id : undefined;
    if (!clientId) throw new BadInput("invalid_sso", "sso.oidc.client_id is required.");
    let clientSecretSealed: string | undefined;
    if (typeof o.client_secret === "string" && o.client_secret.length > 0) {
      if (o.client_secret.length > 1024) throw new BadInput("invalid_sso", "sso.oidc.client_secret is too long.");
      clientSecretSealed = await seal(keys.config, "oidc-client-secret", o.client_secret);
    } else if (o.client_secret === undefined && current?.oidc?.clientId === clientId && current.oidc.issuer === issuer) {
      clientSecretSealed = current.oidc.clientSecretSealed;
    }
    return { protocol, enforced, oidc: { issuer, clientId, ...(clientSecretSealed ? { clientSecretSealed } : {}) } };
  }
  const m = (typeof s.saml === "object" && s.saml !== null ? s.saml : {}) as Record<string, unknown>;
  const idpEntityId = typeof m.idp_entity_id === "string" && m.idp_entity_id.length > 0 && m.idp_entity_id.length <= 1024 ? m.idp_entity_id : undefined;
  if (!idpEntityId) throw new BadInput("invalid_sso", "sso.saml.idp_entity_id is required.");
  const ssoUrl = url(m.sso_url, "sso.saml.sso_url");
  const certs = m.certificates;
  const parses = (p: string): boolean => {
    try {
      // An X.509 certificate or an SPKI public key, RSA (the accepted signature algorithms).
      return createPublicKey(p).asymmetricKeyType === "rsa";
    } catch {
      return false;
    }
  };
  if (!Array.isArray(certs) || certs.length === 0 || certs.length > MAX_CERTS || !certs.every((p) => typeof p === "string" && p.length <= MAX_CERT_PEM && PEM_RE.test(p.trim() + "\n") && parses(p))) {
    throw new BadInput("invalid_sso", `sso.saml.certificates must be 1 to ${MAX_CERTS} PEM RSA certificates (or public keys) of the IdP.`);
  }
  return { protocol, enforced, saml: { idpEntityId, ssoUrl, certificates: certs.map((p) => `${(p as string).trim()}\n`) } };
}

// --- the app ---

export function createEnterpriseApp(deps: EnterpriseAppDeps): Hono<{ Variables: Vars }> {
  const app = new Hono<{ Variables: Vars }>();
  const base = deps.publicBaseUrl.replace(/\/+$/, "");
  const baseOrigin = new URL(base).origin;
  const keys = enterpriseKeys(deps.secret);
  const nowMs = (): number => deps.now().getTime();
  const skewSecs = deps.skewSecs ?? DEFAULT_CLOCK_SKEW_SECS;
  const graceMs = deps.deleteGraceMs ?? DEFAULT_DELETE_GRACE_DAYS * 86_400_000;
  const idp = { ...(deps.idpFetch ? { fetch: deps.idpFetch } : {}), ...(deps.resolveHost ? { resolveHost: deps.resolveHost } : {}) };
  const oidcCache = deps.oidcCache ?? DEFAULT_OIDC_CACHE;
  const small = bodyLimit({ maxSize: MAX_SMALL_BODY, onError: (c) => agentError(c, 413, "body_too_large", "The body is too large for this route.") });
  const storeFailure = (c: Context): Response => agentError(c, 503, "tenant_directory_unavailable", "The tenant directory is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
  const spEntityId = (tenantId: string): string => `${base}/sso/${tenantId}/saml/metadata`;
  const acsUrl = (tenantId: string): string => `${base}/sso/${tenantId}/saml/acs`;
  const oidcRedirect = (tenantId: string): string => `${base}/sso/${tenantId}/oidc/callback`;

  function renderTenant(t: TenantRecord): Record<string, unknown> {
    const sso = t.sso
      ? {
        protocol: t.sso.protocol, enforced: t.sso.enforced, login_url: `${base}/sso/${t.tenantId}/login`,
        ...(t.sso.oidc ? { oidc: { issuer: t.sso.oidc.issuer, client_id: t.sso.oidc.clientId, client_secret_set: t.sso.oidc.clientSecretSealed !== undefined, redirect_uri: oidcRedirect(t.tenantId) } } : {}),
        ...(t.sso.saml ? { saml: { idp_entity_id: t.sso.saml.idpEntityId, sso_url: t.sso.saml.ssoUrl, certificates: t.sso.saml.certificates.length, sp_entity_id: spEntityId(t.tenantId), acs_url: acsUrl(t.tenantId), metadata_url: spEntityId(t.tenantId) } } : {}),
      }
      : null;
    return {
      tenant_id: t.tenantId, name: t.name, region: t.region, plan: t.plan, served_here: t.region === deps.region.served,
      ...(t.region !== deps.region.served && deps.region.endpoints[t.region] ? { region_endpoint: deps.region.endpoints[t.region] } : {}),
      sso, scim: t.plan === "enterprise" ? { base_url: `${base}${SCIM_BASE}`, token_set: t.scimTokenHash !== undefined } : null,
      created_at: new Date(t.createdAt).toISOString(), updated_at: new Date(t.updatedAt).toISOString(),
    };
  }

  const renderAdmin = (a: TenantAdmin): Record<string, unknown> => ({ admin_id: a.adminId, email: a.email, sso_bound: a.subject !== undefined, created_at: new Date(a.createdAt).toISOString() });

  // ======================= operator routes =======================

  const operator: MiddlewareHandler<{ Variables: Vars }> = async (c, next) => {
    const p: AgentPrincipal | undefined = agentPrincipalOf(c);
    if (!p) throw new Error("operator route without requireKey");
    if (p.via === "oauth") return agentError(c, 403, "oauth_not_allowed", "Operator routes take an admin agent key, not an OAuth access token.");
    await next();
  };
  const admin = [deps.requireKey(["admin"]), operator] as const;

  app.post("/v1/admin/tenants", ...admin, small, async (c) => {
    const idem = c.req.header("idempotency-key")?.trim();
    if (!idem || idem.length > 255) return agentError(c, 400, "idempotency_key_required", "Send an Idempotency-Key header (1 to 255 characters); a retry with the same key returns the same tenant.");
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"name\": \"Acme\", \"region\": \"eu\", \"plan\": \"enterprise\"}.");
    const name = typeof body.name === "string" ? body.name.trim() : "";
    if (name.length === 0 || name.length > MAX_NAME) return agentError(c, 400, "invalid_name", `name is required, at most ${MAX_NAME} characters.`);
    const region = typeof body.region === "string" ? body.region : deps.region.served;
    if (!REGION_RE.test(region) || !knownRegion(deps.region, region)) {
      return agentError(c, 400, "unknown_region", `region must be ${[deps.region.served, ...Object.keys(deps.region.endpoints)].filter((r, i, a) => a.indexOf(r) === i).join(", ")}.`);
    }
    const plan = (body.plan ?? "self-serve") as TenantPlan;
    if (!(TENANT_PLANS as readonly unknown[]).includes(plan)) return agentError(c, 400, "invalid_plan", "plan must be self-serve or enterprise.");
    let sso: SsoConfig | undefined;
    try {
      if (body.sso !== undefined && body.sso !== null) {
        if (plan !== "enterprise") return agentError(c, 400, "sso_requires_enterprise", "SSO is part of the enterprise plan.");
        sso = await parseSso(body.sso, keys, undefined);
      }
    } catch (e) {
      if (e instanceof BadInput) return agentError(c, 400, e.code, e.detail);
      throw e;
    }
    const at = nowMs();
    const scimToken = plan === "enterprise" ? randomToken("sct_") : undefined;
    const record: TenantRecord = {
      tenantId: `tnt_${randomUUID()}`, name, region, plan, ...(sso ? { sso } : {}),
      ...(scimToken ? { scimTokenHash: sha256Hex(scimToken) } : {}), idempotencyKey: idem, createdAt: at, updatedAt: at,
    };
    let out: Awaited<ReturnType<EnterpriseStore["createTenant"]>>;
    try {
      out = await deps.store.createTenant(record);
    } catch {
      return storeFailure(c);
    }
    c.set("relayLogEvent", "tenant_created");
    if (!out.created) return c.json({ tenant: renderTenant(out.tenant), scim_token: null, created: false, note: "Same Idempotency-Key: the existing tenant. Its SCIM token is not shown again; rotate it with POST /v1/admin/tenants/{id}/scim-token." }, 200, { "cache-control": "no-store" });
    return c.json({ tenant: renderTenant(out.tenant), scim_token: scimToken ?? null, created: true }, 201, { "cache-control": "no-store" });
  });

  const loadTenant = async (c: C): Promise<TenantRecord | Response> => {
    try {
      const t = await deps.store.tenant(c.req.param("id") ?? "");
      return t ?? agentError(c, 404, "tenant_not_found", "No such tenant.");
    } catch {
      return storeFailure(c);
    }
  };

  app.get("/v1/admin/tenants/:id", ...admin, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    try {
      const [admins, members] = await Promise.all([deps.store.admins(t.tenantId), deps.store.members(t.tenantId)]);
      return c.json({ tenant: renderTenant(t), admins: admins.map(renderAdmin), member_count: members.length }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.patch("/v1/admin/tenants/:id", ...admin, small, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be JSON.");
    if (body.region !== undefined && body.region !== t.region) {
      return agentError(c, 400, "region_immutable", "A tenant's region cannot change in place: its data lives in that region. Create the tenant in the new region and move members with export and import.");
    }
    const patch: { name?: string; plan?: TenantPlan; sso?: SsoConfig | null } = {};
    if (body.name !== undefined) {
      if (typeof body.name !== "string" || body.name.trim().length === 0 || body.name.length > MAX_NAME) return agentError(c, 400, "invalid_name", `name must be 1 to ${MAX_NAME} characters.`);
      patch.name = body.name.trim();
    }
    if (body.plan !== undefined) {
      if (!(TENANT_PLANS as readonly unknown[]).includes(body.plan)) return agentError(c, 400, "invalid_plan", "plan must be self-serve or enterprise.");
      patch.plan = body.plan as TenantPlan;
    }
    const plan = patch.plan ?? t.plan;
    try {
      if (body.sso === null) patch.sso = null;
      else if (body.sso !== undefined) {
        if (plan !== "enterprise") return agentError(c, 400, "sso_requires_enterprise", "SSO is part of the enterprise plan.");
        patch.sso = await parseSso(body.sso, keys, t.sso);
      }
      if (plan !== "enterprise" && t.sso && patch.sso === undefined) patch.sso = null;
    } catch (e) {
      if (e instanceof BadInput) return agentError(c, 400, e.code, e.detail);
      throw e;
    }
    try {
      const updated = await deps.store.updateTenant(t.tenantId, patch, nowMs());
      if (!updated) return agentError(c, 404, "tenant_not_found", "No such tenant.");
      c.set("relayLogEvent", "tenant_updated");
      return c.json({ tenant: renderTenant(updated) }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/admin/tenants/:id/principals", ...admin, small, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    const body = await readJson(c);
    const principalId = typeof body?.principal_id === "string" ? body.principal_id : "";
    if (!/^prn_[A-Za-z0-9-]{1,64}$/.test(principalId)) return agentError(c, 400, "invalid_principal", "principal_id must be a principal id (prn_…).");
    try {
      if (!(await deps.agentStore.principal(principalId))) return agentError(c, 404, "principal_not_found", "No such principal.");
      const out = await deps.store.attachPrincipal(t.tenantId, principalId, nowMs());
      if (out === "other_tenant") return agentError(c, 409, "principal_in_other_tenant", "This principal already belongs to another tenant. A principal belongs to at most one tenant.");
      await deps.agentStore.assignTenant(principalId, t.tenantId);
      c.set("relayLogEvent", "principal_attached");
      return c.json({ tenant_id: t.tenantId, principal_id: principalId, attached: out === "attached" }, out === "attached" ? 201 : 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/admin/tenants/:id/admins", ...admin, small, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    const body = await readJson(c);
    const email = typeof body?.email === "string" ? body.email.trim().toLowerCase() : "";
    if (!EMAIL_RE.test(email)) return agentError(c, 400, "invalid_email", "email must be the admin's address at the tenant's IdP.");
    const a: TenantAdmin = { adminId: `tad_${randomUUID()}`, tenantId: t.tenantId, email, createdAt: nowMs() };
    try {
      if ((await deps.store.addAdmin(a)) === "duplicate") return agentError(c, 409, "admin_exists", "This address is already an admin of the tenant.");
    } catch {
      return storeFailure(c);
    }
    c.set("relayLogEvent", "tenant_admin_added");
    return c.json({ admin: renderAdmin(a), login_url: t.sso ? `${base}/sso/${t.tenantId}/login` : null }, 201, { "cache-control": "no-store" });
  });

  app.delete("/v1/admin/tenants/:id/admins/:admin", ...admin, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    try {
      const a = await deps.store.admin(c.req.param("admin"));
      if (!a || a.tenantId !== t.tenantId || a.removedAt !== undefined) return agentError(c, 404, "admin_not_found", "No such admin.");
      await deps.store.removeAdmin(a.adminId, nowMs());
      return c.json({ admin_id: a.adminId, removed: true }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/admin/tenants/:id/scim-token", ...admin, async (c) => {
    const t = await loadTenant(c);
    if (t instanceof Response) return t;
    if (t.plan !== "enterprise") return agentError(c, 400, "scim_requires_enterprise", "SCIM provisioning is part of the enterprise plan.");
    const token = randomToken("sct_");
    try {
      await deps.store.setScimTokenHash(t.tenantId, sha256Hex(token), nowMs());
    } catch {
      return storeFailure(c);
    }
    c.set("relayLogEvent", "scim_token_rotated");
    return c.json({ tenant_id: t.tenantId, scim_token: token, scim_base_url: `${base}${SCIM_BASE}`, note: "Shown once. The previous token stopped working now." }, 201, { "cache-control": "no-store" });
  });

  // ======================= SSO =======================

  const ssoTenant = async (c: C): Promise<TenantRecord | Response> => {
    let t: TenantRecord | undefined;
    try {
      t = await deps.store.tenant(c.req.param("tenant") ?? "");
    } catch {
      return page(c, 503, "Unavailable", "<h1>Try again shortly</h1><p>The organization directory is unavailable.</p>");
    }
    if (!t || t.plan !== "enterprise" || !t.sso) return ssoError(c, "sso_not_configured", 400);
    if (t.region !== deps.region.served) {
      const r = regionRefusal(deps.region, t.region);
      c.set("relayLogEvent", "region_refused");
      return c.json({ error: { code: r.code, message: r.message, next_action: r.nextAction, ...(r.extra ?? {}) } }, 421, { "cache-control": "no-store" });
    }
    return t;
  };

  interface Flow { k: "oidc" | "saml"; t: string; s: string; n?: string; v?: string; r?: string; exp: number }

  const readFlow = async (c: C, tenantId: string, kind: Flow["k"]): Promise<Flow | undefined> => {
    const text = await unseal(keys.flow, "sso-flow", cookieOf(c.req.header("cookie"), FLOW_COOKIE));
    if (!text) return undefined;
    try {
      const f = JSON.parse(text) as Flow;
      return f.k === kind && f.t === tenantId && typeof f.exp === "number" && f.exp > nowMs() && typeof f.s === "string" ? f : undefined;
    } catch {
      return undefined;
    }
  };

  /** Sign the admin in: match by bound subject, else bind on a verified e-mail. */
  const signIn = async (c: C, t: TenantRecord, subject: string, email: string | undefined, method: "oidc" | "saml"): Promise<Response> => {
    let admin: TenantAdmin | undefined;
    try {
      const admins = await deps.store.admins(t.tenantId);
      admin = admins.find((a) => a.subject === subject);
      if (!admin && email) {
        const byEmail = admins.find((a) => a.email === email && a.subject === undefined);
        if (byEmail && (await deps.store.bindAdminSubject(byEmail.adminId, subject))) admin = { ...byEmail, subject };
      }
    } catch {
      return page(c, 503, "Unavailable", "<h1>Try again shortly</h1><p>The organization directory is unavailable.</p>", { "set-cookie": flowCookie("", 0) });
    }
    if (!admin) return ssoError(c, "not_an_admin", 403);
    const iat = Math.floor(nowMs() / 1000);
    const token = signClaims(keys.session, { a: admin.adminId, t: t.tenantId, m: method, iat, exp: iat + SESSION_TTL_SECS, sid: randomId(12) });
    c.set("relayLogEvent", "sso_login_succeeded");
    // A same-origin link (not a redirect from the IdP's cross-site navigation) so the
    // SameSite=Strict session cookie is sent on the next request.
    const res = page(c, 200, "Signed in", `<h1>Signed in</h1><p>You are signed in as an administrator of ${escapeHtml(t.name)} for ${SESSION_TTL_SECS / 60} minutes.</p><p><a href="/v1/tenant/me">Continue</a></p>`);
    res.headers.append("set-cookie", sessionCookie(token, SESSION_TTL_SECS));
    res.headers.append("set-cookie", flowCookie("", 0));
    return res;
  };

  app.get("/sso/:tenant/login", async (c) => {
    const t = await ssoTenant(c);
    if (t instanceof Response) return t;
    const sso = t.sso as SsoConfig;
    const exp = nowMs() + FLOW_TTL_SECS * 1000;
    const state = randomId(24);
    try {
      if (sso.protocol === "oidc" && sso.oidc) {
        const d = await discover(sso.oidc.issuer, idp, oidcCache, nowMs());
        const nonce = randomId(24);
        const verifier = randomId(48);
        const flow: Flow = { k: "oidc", t: t.tenantId, s: state, n: nonce, v: verifier, exp };
        const loginHint = c.req.query("login_hint");
        const location = authorizationUrl(d, { clientId: sso.oidc.clientId, redirectUri: oidcRedirect(t.tenantId), state, nonce, codeChallenge: pkceChallenge(verifier), ...(loginHint && loginHint.length <= 256 ? { loginHint } : {}) });
        c.set("relayLogEvent", "sso_login_started");
        return c.body(null, 302, { location, "cache-control": "no-store", "referrer-policy": "no-referrer", "set-cookie": flowCookie(await seal(keys.flow, "sso-flow", JSON.stringify(flow)), FLOW_TTL_SECS) });
      }
      if (sso.protocol === "saml" && sso.saml) {
        const requestId = newRequestId();
        const xml = authnRequestXml({ id: requestId, issueInstant: new Date(nowMs()).toISOString(), destination: sso.saml.ssoUrl, acsUrl: acsUrl(t.tenantId), spEntityId: spEntityId(t.tenantId) });
        const flow: Flow = { k: "saml", t: t.tenantId, s: state, r: requestId, exp };
        c.set("relayLogEvent", "sso_login_started");
        return c.body(null, 302, { location: await redirectBindingUrl(sso.saml.ssoUrl, xml, state), "cache-control": "no-store", "referrer-policy": "no-referrer", "set-cookie": flowCookie(await seal(keys.flow, "sso-flow", JSON.stringify(flow)), FLOW_TTL_SECS) });
      }
    } catch (e) {
      return ssoError(c, e instanceof OidcError ? e.code : "idp_error", 503);
    }
    return ssoError(c, "sso_not_configured");
  });

  app.get("/sso/:tenant/oidc/callback", async (c) => {
    const t = await ssoTenant(c);
    if (t instanceof Response) return t;
    const oidc = t.sso?.protocol === "oidc" ? t.sso.oidc : undefined;
    if (!oidc) return ssoError(c, "sso_not_configured");
    const flow = await readFlow(c, t.tenantId, "oidc");
    if (!flow || !flow.n || !flow.v) return ssoError(c, "flow_missing");
    if (!safeEqual(c.req.query("state") ?? "", flow.s)) return ssoError(c, "state_mismatch");
    if (c.req.query("error") !== undefined) return ssoError(c, "idp_error", 403);
    const code = c.req.query("code");
    if (!code || code.length > 2048) return ssoError(c, "idp_error");
    try {
      const d = await discover(oidc.issuer, idp, oidcCache, nowMs());
      const clientSecret = oidc.clientSecretSealed ? await unseal(keys.config, "oidc-client-secret", oidc.clientSecretSealed) : undefined;
      if (oidc.clientSecretSealed && clientSecret === undefined) return ssoError(c, "client_secret_unreadable", 503);
      const idToken = await exchangeCode(d, { code, redirectUri: oidcRedirect(t.tenantId), verifier: flow.v, clientId: oidc.clientId, ...(clientSecret ? { clientSecret } : {}) }, idp);
      const claims = await verifyIdToken(idToken, { issuer: d.issuer, clientId: oidc.clientId, nonce: flow.n, jwksUri: d.jwksUri, nowMs: nowMs(), skewSecs }, idp, oidcCache);
      return await signIn(c, t, `oidc:${claims.iss}|${claims.sub}`, claims.emailVerified ? claims.email : undefined, "oidc");
    } catch (e) {
      return ssoError(c, e instanceof OidcError ? e.code : "idp_error", e instanceof OidcError && (e.code === "discovery_failed" || e.code === "jwks_failed" || e.code === "token_exchange_failed") ? 503 : 403);
    }
  });

  app.post("/sso/:tenant/saml/acs", bodyLimit({ maxSize: MAX_ACS_BODY, onError: (c) => c.text("Too large", 413) }), async (c) => {
    const t = await ssoTenant(c);
    if (t instanceof Response) return t;
    const saml = t.sso?.protocol === "saml" ? t.sso.saml : undefined;
    if (!saml) return ssoError(c, "sso_not_configured");
    let form: Record<string, unknown>;
    try {
      form = await c.req.parseBody();
    } catch {
      return ssoError(c, "saml_malformed");
    }
    const flow = await readFlow(c, t.tenantId, "saml");
    if (!flow || !flow.r) return ssoError(c, "flow_missing");
    if (typeof form.RelayState !== "string" || !safeEqual(form.RelayState, flow.s)) return ssoError(c, "state_mismatch");
    if (typeof form.SAMLResponse !== "string") return ssoError(c, "saml_malformed");
    try {
      const a = validateSamlResponse(form.SAMLResponse, { spEntityId: spEntityId(t.tenantId), acsUrl: acsUrl(t.tenantId), idpEntityId: saml.idpEntityId, certificates: saml.certificates, requestId: flow.r, nowMs: nowMs(), skewSecs });
      let fresh: boolean;
      try {
        fresh = await deps.store.spendAssertion(sha256Hex(`${t.tenantId}|${a.assertionId}`), a.expiresAt, nowMs());
      } catch {
        return page(c, 503, "Unavailable", "<h1>Try again shortly</h1><p>The organization directory is unavailable.</p>");
      }
      if (!fresh) return ssoError(c, "saml_replay", 403);
      return await signIn(c, t, `saml:${a.issuer}|${a.nameId}`, a.email, "saml");
    } catch (e) {
      return ssoError(c, e instanceof SamlError ? e.code : "saml_malformed", 403);
    }
  });

  app.get("/sso/:tenant/saml/metadata", async (c) => {
    let t: TenantRecord | undefined;
    try {
      t = await deps.store.tenant(c.req.param("tenant"));
    } catch {
      return storeFailure(c);
    }
    if (!t || t.plan !== "enterprise") return agentError(c, 404, "tenant_not_found", "No such tenant.");
    return c.body(spMetadataXml({ spEntityId: spEntityId(t.tenantId), acsUrl: acsUrl(t.tenantId) }), 200, { "content-type": "application/samlmetadata+xml; charset=utf-8", "cache-control": "public, max-age=3600" });
  });

  // ======================= tenant admin session =======================

  const session: MiddlewareHandler<{ Variables: Vars }> = async (c, next) => {
    const refuse = (msg: string): Response => {
      c.set("relayLogEvent", "admin_session_rejected");
      return agentError(c, 401, "admin_session_required", msg, "sign_in", { "set-cookie": sessionCookie("", 0) });
    };
    if (c.req.method !== "GET" && c.req.method !== "HEAD") {
      // SameSite=Strict already keeps the cookie off cross-site requests; refuse a foreign Origin too.
      const o = c.req.header("origin");
      if (o !== undefined && o !== baseOrigin) return agentError(c, 403, "cross_origin_refused", "This route accepts requests from the gateway's own origin only.");
    }
    const claims = verifyClaims(keys.session, cookieOf(c.req.header("cookie"), SESSION_COOKIE));
    const now = Math.floor(nowMs() / 1000);
    if (!claims || typeof claims.a !== "string" || typeof claims.t !== "string" || typeof claims.exp !== "number" || claims.exp <= now) {
      return refuse("Sign in with your organization's single sign-on: GET /sso/{tenant}/login.");
    }
    let a: TenantAdmin | undefined;
    let t: TenantRecord | undefined;
    try {
      [a, t] = await Promise.all([deps.store.admin(claims.a), deps.store.tenant(claims.t)]);
    } catch {
      return storeFailure(c);
    }
    if (!a || a.removedAt !== undefined || a.tenantId !== claims.t || !t) return refuse("This session is no longer valid. Sign in again.");
    if (t.region !== deps.region.served) {
      const r = regionRefusal(deps.region, t.region);
      return c.json({ error: { code: r.code, message: r.message, next_action: r.nextAction, ...(r.extra ?? {}) } }, 421, { "cache-control": "no-store" });
    }
    c.set("tenantAdmin", a);
    c.set("adminTenant", t);
    await next();
  };
  const who = (c: C): { admin: TenantAdmin; tenant: TenantRecord } => {
    const admin = c.get("tenantAdmin");
    const tenant = c.get("adminTenant");
    if (!admin || !tenant) throw new Error("tenant route without session");
    return { admin, tenant };
  };

  app.get("/v1/tenant/me", session, (c) => {
    const { admin, tenant } = who(c);
    return c.json({ admin: renderAdmin(admin), tenant: renderTenant(tenant) }, 200, { "cache-control": "no-store" });
  });

  app.post("/v1/tenant/logout", session, (c) => c.json({ signed_out: true }, 200, { "cache-control": "no-store", "set-cookie": sessionCookie("", 0) }));

  app.get("/v1/tenant/members", session, async (c) => {
    const { tenant } = who(c);
    try {
      const ids = await deps.store.members(tenant.tenantId);
      const rows = await Promise.all(ids.map(async (id) => {
        const u = await deps.store.user(tenant.tenantId, id);
        return { principal_id: id, scim: u ? { user_name: u.userName || null, active: u.active, deleted: u.deletedAt !== undefined } : null };
      }));
      return c.json({ tenant_id: tenant.tenantId, members: rows }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/tenant/members/:id/keys", session, small, async (c) => {
    const { tenant } = who(c);
    const principalId = c.req.param("id");
    const body = (c.req.header("content-length") === "0" ? {} : await readJson(c)) ?? {};
    const requested = Array.isArray(body.scopes) ? body.scopes : [...MEMBER_SCOPES];
    const idem = c.req.header("idempotency-key")?.trim();
    if (idem !== undefined && (idem.length === 0 || idem.length > 255)) return agentError(c, 400, "invalid_idempotency_key", "Idempotency-Key must be 1 to 255 characters.");
    // E51: ["ingest"] alone is the write-only browser-extension key (memory.ingest only).
    const ingestOnly = requested.length === 1 && requested[0] === "ingest";
    if (!ingestOnly && (requested.length === 0 || !requested.every((s): s is AgentScope => typeof s === "string" && (MEMBER_SCOPES as readonly string[]).includes(s)))) {
      return agentError(c, 400, "invalid_scopes", "scopes must be a non-empty subset of read, write, run, or [\"ingest\"] alone for a write-only browser-extension key (admin is never issued here).");
    }
    try {
      if ((await deps.store.membership(principalId)) !== tenant.tenantId) return agentError(c, 404, "member_not_found", "No such member of this tenant.");
      const u = await deps.store.user(tenant.tenantId, principalId);
      if (u && (!u.active || u.deletedAt !== undefined)) return agentError(c, 409, "member_inactive", "This member is deactivated in your identity provider. Reactivate it there first.");
      const at = nowMs();
      const scopes = [...new Set(requested)] as AgentScope[];
      // E39: with an Idempotency-Key the key id derives from (tenant, member, key), so a retried
      // request finds the key it already issued instead of minting a second. The secret is shown
      // only by the call that created the key; a replay answers its reference, never the secret.
      const keyId = idem !== undefined ? memberKeyId(tenant.tenantId, principalId, idem) : `ak_${randomUUID()}`;
      const replay = async (existing: AgentKeyRecord): Promise<Response> => {
        if ([...existing.scopes].sort().join(",") !== [...scopes].sort().join(",")) {
          return agentError(c, 409, "idempotency_key_reused", "This Idempotency-Key already issued a key with other scopes. Use a new key for a different request.", "fix_input");
        }
        c.set("relayLogAgentKey", keyId);
        c.set("relayLogEvent", "member_key_replayed");
        return c.json({
          key: null, key_id: keyId, principal_id: principalId, tenant_id: tenant.tenantId, scopes: existing.scopes,
          expires_at: existing.expiresAt !== undefined ? new Date(existing.expiresAt).toISOString() : null, created: false,
          revoked: existing.revokedAt !== undefined,
          note: "Same Idempotency-Key: the key issued before. Its secret is shown only once; if it was lost, revoke it with POST /v1/keys/{id}/revoke and issue a new one with a new Idempotency-Key.",
        }, 200, { "cache-control": "no-store" });
      };
      if (idem !== undefined) {
        const existing = await deps.agentStore.keyById(keyId);
        if (existing) return replay(existing);
      }
      const { key, prefix } = generateAgentKey("live");
      const inserted = await deps.agentStore.insertKeyOnce({
        keyId, prefix, hash: await hashAgentKey(key, deps.keyIterations ?? DEFAULT_PBKDF2_ITERATIONS), mode: "live",
        principalId, tenantId: tenant.tenantId, scopes, createdAt: at, expiresAt: at + MEMBER_KEY_TTL_MS,
      });
      if (!inserted) {
        // A concurrent retry with the same Idempotency-Key won: answer its key, not this secret.
        const existing = await deps.agentStore.keyById(keyId);
        if (!existing) return storeFailure(c);
        return replay(existing);
      }
      c.set("relayLogAgentKey", keyId);
      c.set("relayLogEvent", "member_key_issued");
      return c.json({ key, key_id: keyId, principal_id: principalId, tenant_id: tenant.tenantId, scopes, expires_at: new Date(at + MEMBER_KEY_TTL_MS).toISOString(), created: true, note: "Shown once. Hand it to the member's agent; revoke it with POST /v1/keys/{id}/revoke." }, 201, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.get("/v1/tenant/audit/export", session, async (c) => {
    const { tenant } = who(c);
    const range: { sinceMs?: number; untilMs?: number } = {};
    for (const [param, field] of [["since_ms", "sinceMs"], ["until_ms", "untilMs"]] as const) {
      const raw = c.req.query(param);
      if (raw === undefined) continue;
      if (!/^\d{1,15}$/.test(raw)) return agentError(c, 400, "invalid_range", `${param} must be milliseconds since the epoch.`);
      range[field] = Number(raw);
    }
    const sidecar = deps.sidecar;
    if (!sidecar) return agentError(c, 503, "sidecar_unavailable", "The audit export needs the sidecar (SHOGUN_SIDECAR_URL and SHOGUN_SIDECAR_ADMIN_TOKEN).", "retry_after", { "retry-after": "60" });
    let members: string[];
    try {
      members = await deps.store.members(tenant.tenantId);
    } catch {
      return storeFailure(c);
    }
    c.set("relayLogEvent", "audit_export_streamed");
    return new Response(auditExportStream(sidecar, tenant.tenantId, members, range, nowMs), {
      status: 200, headers: { "content-type": "application/x-ndjson; charset=utf-8", "cache-control": "no-store", "content-disposition": `attachment; filename="audit-${tenant.tenantId}.ndjson"` },
    });
  });

  // ======================= SCIM =======================
  app.route("/", createScimApp({
    store: deps.store, agentStore: deps.agentStore, ...(deps.oauthStore ? { oauthStore: deps.oauthStore } : {}), ...(deps.sidecar ? { sidecar: deps.sidecar } : {}),
    region: deps.region, publicBaseUrl: deps.publicBaseUrl, deleteGraceMs: graceMs, now: deps.now,
  }));

  return app;
}

/**
 * The tenant audit export as one NDJSON stream: for each member principal, a header line
 * `{"type":"principal","principal_id":…,"status":200}` followed by the sidecar's export body
 * **byte for byte** (never parsed: the gateway reads no record, only frames the stream), a newline
 * if the body did not end with one, and at the end `{"type":"end",…}` with the counts. A member
 * whose export failed gets its header with the status and error code and no body. Members are
 * fetched one after another, as the reader pulls.
 */
export function auditExportStream(sidecar: EnterpriseSidecar, tenantId: string, members: readonly string[], range: { sinceMs?: number; untilMs?: number }, nowMs: () => number): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  const line = (v: Record<string, unknown>): Uint8Array => enc.encode(`${JSON.stringify(v)}\n`);
  let i = 0;
  let failed = 0;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  let lastByte = 10;
  let started = false;
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (!started) {
        started = true;
        controller.enqueue(line({ type: "export", tenant_id: tenantId, generated_at: new Date(nowMs()).toISOString(), principals: members.length, format: "shogunagent.tenant-audit-export.v1" }));
        return;
      }
      for (;;) {
        if (reader) {
          const { done, value } = await reader.read();
          if (!done) {
            if (value.byteLength > 0) {
              lastByte = value[value.byteLength - 1] ?? lastByte;
              controller.enqueue(value);
            }
            return;
          }
          reader = undefined;
          if (lastByte !== 10) controller.enqueue(enc.encode("\n"));
          lastByte = 10;
          continue;
        }
        if (i >= members.length) {
          controller.enqueue(line({ type: "end", principals: members.length, failed }));
          controller.close();
          return;
        }
        const principalId = members[i] ?? "";
        i += 1;
        const out = await sidecar.auditExport(principalId, range).catch(() => ({ ok: false as const, status: 503, code: "sidecar_unavailable" }));
        if (!out.ok) {
          failed += 1;
          controller.enqueue(line({ type: "principal", principal_id: principalId, status: out.status, error: out.code }));
          return;
        }
        controller.enqueue(line({ type: "principal", principal_id: principalId, status: 200 }));
        if (out.body) reader = out.body.getReader();
        return;
      }
    },
    async cancel() {
      await reader?.cancel().catch(() => undefined);
    },
  });
}

/** E39: the key id a member key issued under `idem` gets: `ak_` and a UUID-shaped SHA-256 of
 * (tenant, member, Idempotency-Key), so the same request always names the same key and two
 * tenants or members never share one. */
export function memberKeyId(tenantId: string, principalId: string, idem: string): string {
  const h = sha256Hex(`shogun/member-key/v1\n${tenantId}\n${principalId}\n${idem}`);
  return `ak_${h.slice(0, 8)}-${h.slice(8, 12)}-5${h.slice(13, 16)}-8${h.slice(17, 20)}-${h.slice(20, 32)}`;
}

/** The sweep's enterprise jobs: expire SAML replay rows, and purge the stores of SCIM-deleted
 * users whose grace period has passed. Each failure is one fixed line; a purge the sidecar did
 * not accept stays due and is retried on the next sweep. */
export async function enterpriseJobs(store: EnterpriseStore, sidecar: EnterpriseSidecar | undefined, nowMs: number, log: LogFn): Promise<{ purged: number; failed: number }> {
  await store.maintenance(nowMs).catch(() => log("enterprise maintenance unavailable"));
  let purged = 0;
  let failed = 0;
  const due = await store.duePurges(nowMs, 50).catch(() => undefined);
  if (!due) {
    log("enterprise purge listing unavailable");
    return { purged, failed };
  }
  for (const u of due) {
    const out = sidecar ? await sidecar.purgePrincipal(u.userId).catch(() => ({ ok: false as const, status: 503, code: "sidecar_unavailable" })) : { ok: false as const, status: 503, code: "sidecar_unavailable" };
    if (!out.ok) {
      failed += 1;
      continue;
    }
    await store.markPurged(u.userId, nowMs);
    await store.detachPrincipal(u.userId);
    purged += 1;
  }
  if (purged > 0 || failed > 0) log(`enterprise_purges purged=${purged} failed=${failed}`);
  return { purged, failed };
}
