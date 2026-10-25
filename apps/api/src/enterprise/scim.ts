/**
 * SCIM 2.0 provisioning (RFC 7643 / 7644) for enterprise tenants (E26):
 *
 *   GET    /scim/v2/ServiceProviderConfig | /Schemas[/{id}] | /ResourceTypes[/{id}]
 *   GET    /scim/v2/Users?filter=userName eq "…"&startIndex=&count=
 *   POST   /scim/v2/Users                   provision a principal (no key yet: the key slot is open)
 *   GET    /scim/v2/Users/{id}
 *   PUT    /scim/v2/Users/{id}              replace
 *   PATCH  /scim/v2/Users/{id}              Okta and Entra ID operation shapes
 *   DELETE /scim/v2/Users/{id}              revoke now; purge the store after the grace period
 *   GET/POST /scim/v2/Groups, GET/PUT/PATCH/DELETE /scim/v2/Groups/{id}   Groups = Teams (E18)
 *
 * **Auth.** `Authorization: Bearer sct_…`, one token per tenant, stored only as its SHA-256. The
 * tenant must be on the enterprise plan and homed in this deployment's region.
 *
 * **Users → principals.** A SCIM user's `id` is the principal id it provisions. Creating one
 * creates the principal under the tenant with no key: a tenant admin issues the key
 * (`POST /v1/tenant/members/{id}/keys`). `active: false` revokes every key of the principal and
 * every OAuth grant minted from them at once (`event=scim_user_deactivated`); re-activation does
 * not resurrect them. DELETE does the same, removes the user from its groups, leaves a tombstone
 * (ids and times; attributes cleared) and schedules the principal-store purge after
 * `SCIM_DELETE_GRACE_DAYS`.
 *
 * **Groups → Teams.** A group's members can read each other's world state (E18 `principal`
 * grants, both directions) and nothing else. Each change is pushed to the sidecar before it is
 * stored (`sidecar-ops.ts`); a grant between two people is revoked only when they share no other
 * group. At most `MAX_GROUP_MEMBERS` members per group (each pair is one grant call).
 *
 * Bodies are JSON the protocol defines; they are parsed for their attributes and never logged or
 * kept beyond the attributes stored (`store.ts`).
 */
import { randomUUID } from "node:crypto";
import { Hono, type Context, type MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import type { AgentVars } from "../agent-keys.js";
import type { AgentStore } from "../agent-store.js";
import type { OAuthStore } from "../oauth/store.js";
import { sha256Hex } from "./crypto.js";
import { regionRefusal, type RegionConfig } from "./region.js";
import type { EnterpriseSidecar } from "./sidecar-ops.js";
import type { EnterpriseStore, EqFilter, ScimEmail, ScimGroup, ScimUser, TenantRecord } from "./store.js";

export const SCIM_BASE = "/scim/v2";
export const SCIM_CONTENT_TYPE = "application/scim+json";
export const MAX_SCIM_BODY_BYTES = 64 * 1024;
export const DEFAULT_SCIM_COUNT = 100;
export const MAX_SCIM_COUNT = 200;
export const MAX_GROUP_MEMBERS = 30;
const MAX_STRING = 256;
const MAX_EMAILS = 10;
const SCIM_TOKEN_RE = /^sct_[A-Za-z0-9_-]{43}$/;

export const SCHEMA = {
  user: "urn:ietf:params:scim:schemas:core:2.0:User",
  group: "urn:ietf:params:scim:schemas:core:2.0:Group",
  principal: "urn:shogunagent:params:scim:schemas:extension:principal:2.0:User",
  list: "urn:ietf:params:scim:api:messages:2.0:ListResponse",
  patch: "urn:ietf:params:scim:api:messages:2.0:PatchOp",
  error: "urn:ietf:params:scim:api:messages:2.0:Error",
  spc: "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig",
  resourceType: "urn:ietf:params:scim:schemas:core:2.0:ResourceType",
  schema: "urn:ietf:params:scim:schemas:core:2.0:Schema",
} as const;

export interface ScimDeps {
  store: EnterpriseStore;
  agentStore: AgentStore;
  oauthStore?: OAuthStore;
  sidecar?: EnterpriseSidecar;
  region: RegionConfig;
  publicBaseUrl: string;
  deleteGraceMs: number;
  now: () => Date;
}

type ScimVars = AgentVars & { scimTenant?: TenantRecord };
type C = Context<{ Variables: ScimVars }>;

export function scimError(c: Context, status: 400 | 401 | 403 | 404 | 409 | 413 | 421 | 500 | 503, detail: string, scimType?: string, extra: Record<string, string> = {}): Response {
  return c.body(JSON.stringify({ schemas: [SCHEMA.error], status: String(status), ...(scimType ? { scimType } : {}), detail, ...extra }), status, { "content-type": SCIM_CONTENT_TYPE, "cache-control": "no-store" });
}

function scimJson(c: Context, body: unknown, status: 200 | 201 = 200, headers: Record<string, string> = {}): Response {
  return c.body(JSON.stringify(body), status, { "content-type": SCIM_CONTENT_TYPE, "cache-control": "no-store", ...headers });
}

// --- parsing ---

/** `attr eq "value"` for userName, externalId or displayName; anything else is unsupported. */
export function parseFilter(raw: string | undefined, allowed: readonly EqFilter["attribute"][]): EqFilter | undefined | "invalid" {
  if (raw === undefined || raw.trim() === "") return undefined;
  const m = /^\s*([A-Za-z0-9.:_-]+)\s+eq\s+"((?:[^"\\]|\\.)*)"\s*$/i.exec(raw);
  if (!m || !m[1]) return "invalid";
  const attr = allowed.find((a) => a.toLowerCase() === m[1]!.toLowerCase().replace(/^urn:ietf:params:scim:schemas:core:2\.0:(user|group):/i, ""));
  if (!attr) return "invalid";
  let value: string;
  try {
    value = JSON.parse(`"${m[2] ?? ""}"`) as string;
  } catch {
    return "invalid";
  }
  return { attribute: attr, value };
}

export function pageParams(startIndexRaw: string | undefined, countRaw: string | undefined): { offset: number; limit: number; startIndex: number } {
  const si = startIndexRaw !== undefined && /^-?\d+$/.test(startIndexRaw) ? Number(startIndexRaw) : 1;
  const startIndex = Math.max(1, si);
  const cnt = countRaw !== undefined && /^-?\d+$/.test(countRaw) ? Number(countRaw) : DEFAULT_SCIM_COUNT;
  return { startIndex, offset: startIndex - 1, limit: Math.min(MAX_SCIM_COUNT, Math.max(0, cnt)) };
}

/** Entra ID sends booleans as "True"/"False" strings. */
function bool(v: unknown): boolean | undefined {
  if (typeof v === "boolean") return v;
  if (typeof v === "string" && /^(true|false)$/i.test(v.trim())) return v.trim().toLowerCase() === "true";
  return undefined;
}

class Invalid extends Error {
  constructor(readonly detail: string, readonly scimType = "invalidValue") {
    super(detail);
  }
}

function optString(v: unknown, field: string): string | undefined {
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "string" || v.length > MAX_STRING) throw new Invalid(`${field} must be a string of at most ${MAX_STRING} characters.`);
  return v.trim() === "" ? undefined : v;
}

function emailsOf(v: unknown): ScimEmail[] {
  if (v === undefined || v === null) return [];
  if (!Array.isArray(v) || v.length > MAX_EMAILS) throw new Invalid(`emails must be an array of at most ${MAX_EMAILS} entries.`);
  return v.map((e) => {
    if (typeof e !== "object" || e === null) throw new Invalid("each email must be an object with a value.");
    const r = e as Record<string, unknown>;
    const value = optString(r.value, "emails.value");
    if (!value) throw new Invalid("each email must have a value.");
    const type = optString(r.type, "emails.type");
    const primary = bool(r.primary);
    return { value, ...(type ? { type } : {}), ...(primary !== undefined ? { primary } : {}) };
  });
}

/** The attributes of a User resource body (POST / PUT). */
function userAttributes(body: Record<string, unknown>): Omit<ScimUser, "userId" | "tenantId" | "createdAt" | "updatedAt"> {
  const userName = optString(body.userName, "userName");
  if (!userName) throw new Invalid("userName is required.", "invalidValue");
  const name = typeof body.name === "object" && body.name !== null ? (body.name as Record<string, unknown>) : {};
  const active = body.active === undefined ? true : bool(body.active);
  if (active === undefined) throw new Invalid("active must be a boolean.");
  const externalId = optString(body.externalId, "externalId");
  const displayName = optString(body.displayName, "displayName");
  const givenName = optString(name.givenName, "name.givenName");
  const familyName = optString(name.familyName, "name.familyName");
  return {
    userName, emails: emailsOf(body.emails), active,
    ...(externalId ? { externalId } : {}), ...(displayName ? { displayName } : {}), ...(givenName ? { givenName } : {}), ...(familyName ? { familyName } : {}),
  };
}

interface PatchOp {
  op: "add" | "replace" | "remove";
  path?: string;
  value?: unknown;
}

function patchOps(body: Record<string, unknown>): PatchOp[] {
  const ops = body.Operations ?? body.operations;
  if (!Array.isArray(ops) || ops.length === 0 || ops.length > 100) throw new Invalid("Operations must be a non-empty array.", "invalidSyntax");
  return ops.map((raw) => {
    if (typeof raw !== "object" || raw === null) throw new Invalid("Each operation must be an object.", "invalidSyntax");
    const o = raw as Record<string, unknown>;
    const op = typeof o.op === "string" ? o.op.toLowerCase() : "";
    if (op !== "add" && op !== "replace" && op !== "remove") throw new Invalid("op must be add, replace or remove.", "invalidSyntax");
    if (o.path !== undefined && typeof o.path !== "string") throw new Invalid("path must be a string.", "invalidPath");
    return { op, ...(typeof o.path === "string" ? { path: o.path.trim() } : {}), ...(o.value !== undefined ? { value: o.value } : {}) };
  });
}

/** Apply one attribute assignment (`path` already stripped of the core schema URN). Unknown
 * attributes are ignored, as most service providers do: Entra ID sends enterprise-extension
 * attributes this server does not keep. */
function assignUser(u: ScimUser, rawPath: string, value: unknown, remove: boolean): void {
  const path = rawPath.replace(/^urn:ietf:params:scim:schemas:core:2\.0:User:/i, "");
  const p = path.toLowerCase();
  const s = (field: string): string | undefined => (remove ? undefined : optString(value, field));
  if (p === "active") {
    if (remove) throw new Invalid("active cannot be removed.", "mutability");
    const b = bool(value);
    if (b === undefined) throw new Invalid("active must be a boolean.");
    u.active = b;
  } else if (p === "username") {
    const v = s("userName");
    if (!v) throw new Invalid("userName is required.", remove ? "mutability" : "invalidValue");
    u.userName = v;
  } else if (p === "displayname") {
    const v = s("displayName");
    if (v) u.displayName = v; else delete u.displayName;
  } else if (p === "externalid") {
    const v = s("externalId");
    if (v) u.externalId = v; else delete u.externalId;
  } else if (p === "name.givenname") {
    const v = s("name.givenName");
    if (v) u.givenName = v; else delete u.givenName;
  } else if (p === "name.familyname") {
    const v = s("name.familyName");
    if (v) u.familyName = v; else delete u.familyName;
  } else if (p === "name") {
    if (remove) { delete u.givenName; delete u.familyName; return; }
    if (typeof value !== "object" || value === null) throw new Invalid("name must be an object.");
    const n = value as Record<string, unknown>;
    if ("givenName" in n) assignUser(u, "name.givenName", n.givenName, false);
    if ("familyName" in n) assignUser(u, "name.familyName", n.familyName, false);
  } else if (p === "emails") {
    u.emails = remove ? [] : emailsOf(Array.isArray(value) ? value : [value]);
  } else {
    const typed = /^emails\[type eq "([^"]+)"\]\.value$/i.exec(path);
    if (typed && typed[1]) {
      const type = typed[1];
      const rest = u.emails.filter((e) => (e.type ?? "").toLowerCase() !== type.toLowerCase());
      const v = remove ? undefined : optString(value, "emails.value");
      u.emails = v ? [...rest, { value: v, type, ...(rest.length === 0 ? { primary: true } : {}) }] : rest;
    }
  }
}

export function applyUserPatch(current: ScimUser, ops: PatchOp[]): ScimUser {
  const u: ScimUser = structuredClone(current);
  for (const op of ops) {
    if (op.path) {
      assignUser(u, op.path, op.value, op.op === "remove");
      continue;
    }
    if (op.op === "remove") throw new Invalid("remove needs a path.", "noTarget");
    if (typeof op.value !== "object" || op.value === null || Array.isArray(op.value)) throw new Invalid("An operation without a path needs an object value.", "invalidSyntax");
    for (const [k, v] of Object.entries(op.value as Record<string, unknown>)) assignUser(u, k, v, false);
  }
  return u;
}

function memberIds(v: unknown): string[] {
  const list = Array.isArray(v) ? v : v === undefined || v === null ? [] : [v];
  return list.map((m) => {
    const id = typeof m === "object" && m !== null ? (m as Record<string, unknown>).value : m;
    if (typeof id !== "string" || id.length === 0 || id.length > MAX_STRING) throw new Invalid("Each member needs a value (a user id).");
    return id;
  });
}

export function applyGroupPatch(current: ScimGroup, ops: PatchOp[]): ScimGroup {
  const g: ScimGroup = structuredClone(current);
  const setMembers = (ids: string[]): void => { g.members = [...new Set(ids)]; };
  for (const op of ops) {
    const path = (op.path ?? "").replace(/^urn:ietf:params:scim:schemas:core:2\.0:Group:/i, "");
    const lower = path.toLowerCase();
    if (lower === "members") {
      if (op.op === "add") setMembers([...g.members, ...memberIds(op.value)]);
      else if (op.op === "replace") setMembers(memberIds(op.value));
      else if (op.value === undefined) setMembers([]);
      else { const drop = new Set(memberIds(op.value)); setMembers(g.members.filter((m) => !drop.has(m))); }
      continue;
    }
    const filtered = /^members\[value eq "([^"]+)"\]$/i.exec(path);
    if (filtered && filtered[1]) {
      if (op.op !== "remove") throw new Invalid("Only remove may target one member by filter.", "invalidPath");
      const id = filtered[1];
      setMembers(g.members.filter((m) => m !== id));
      continue;
    }
    if (lower === "displayname") {
      if (op.op === "remove") throw new Invalid("displayName cannot be removed.", "mutability");
      const v = optString(op.value, "displayName");
      if (!v) throw new Invalid("displayName is required.");
      g.displayName = v;
      continue;
    }
    if (lower === "externalid") {
      const v = op.op === "remove" ? undefined : optString(op.value, "externalId");
      if (v) g.externalId = v; else delete g.externalId;
      continue;
    }
    if (path === "") {
      if (op.op === "remove") throw new Invalid("remove needs a path.", "noTarget");
      if (typeof op.value !== "object" || op.value === null || Array.isArray(op.value)) throw new Invalid("An operation without a path needs an object value.", "invalidSyntax");
      const v = op.value as Record<string, unknown>;
      // Okta sends {"op":"replace","value":{"id":…,"displayName":…}}: only the known attributes count.
      if (v.displayName !== undefined) applyGroupPatchInto(g, "displayName", v.displayName);
      if (v.externalId !== undefined) applyGroupPatchInto(g, "externalId", v.externalId);
      if (v.members !== undefined) setMembers(op.op === "add" ? [...g.members, ...memberIds(v.members)] : memberIds(v.members));
      continue;
    }
    // Unknown attribute: ignored.
  }
  return g;
}

function applyGroupPatchInto(g: ScimGroup, attr: "displayName" | "externalId", value: unknown): void {
  const v = optString(value, attr);
  if (attr === "displayName") {
    if (!v) throw new Invalid("displayName is required.");
    g.displayName = v;
  } else if (v) g.externalId = v;
  else delete g.externalId;
}

// --- rendering ---

const iso = (ms: number): string => new Date(ms).toISOString();

export function renderUser(u: ScimUser, base: string, region: string): Record<string, unknown> {
  return {
    schemas: [SCHEMA.user, SCHEMA.principal],
    id: u.userId,
    ...(u.externalId ? { externalId: u.externalId } : {}),
    userName: u.userName,
    ...(u.givenName || u.familyName ? { name: { ...(u.givenName ? { givenName: u.givenName } : {}), ...(u.familyName ? { familyName: u.familyName } : {}) } } : {}),
    ...(u.displayName ? { displayName: u.displayName } : {}),
    emails: u.emails,
    active: u.active,
    groups: [],
    [SCHEMA.principal]: { principalId: u.userId, tenantId: u.tenantId, region },
    meta: { resourceType: "User", created: iso(u.createdAt), lastModified: iso(u.updatedAt), location: `${base}${SCIM_BASE}/Users/${u.userId}`, version: `W/"${u.updatedAt}"` },
  };
}

export function renderGroup(g: ScimGroup, base: string, withMembers = true): Record<string, unknown> {
  return {
    schemas: [SCHEMA.group],
    id: g.groupId,
    ...(g.externalId ? { externalId: g.externalId } : {}),
    displayName: g.displayName,
    ...(withMembers ? { members: g.members.map((m) => ({ value: m, $ref: `${base}${SCIM_BASE}/Users/${m}`, type: "User" })) } : {}),
    meta: { resourceType: "Group", created: iso(g.createdAt), lastModified: iso(g.updatedAt), location: `${base}${SCIM_BASE}/Groups/${g.groupId}`, version: `W/"${g.updatedAt}"` },
  };
}

function listResponse(total: number, startIndex: number, resources: unknown[]): Record<string, unknown> {
  return { schemas: [SCHEMA.list], totalResults: total, startIndex, itemsPerPage: resources.length, Resources: resources };
}

export function serviceProviderConfig(base: string): Record<string, unknown> {
  return {
    schemas: [SCHEMA.spc],
    documentationUri: "https://github.com/Torutesu/Shogunagent/blob/main/apps/api/README-agent-api.md",
    patch: { supported: true },
    bulk: { supported: false, maxOperations: 0, maxPayloadSize: 0 },
    filter: { supported: true, maxResults: MAX_SCIM_COUNT },
    changePassword: { supported: false },
    sort: { supported: false },
    etag: { supported: false },
    authenticationSchemes: [{ type: "oauthbearertoken", name: "Tenant SCIM token", description: "Authorization: Bearer sct_… (one per tenant; rotate with POST /v1/admin/tenants/{id}/scim-token).", primary: true }],
    meta: { resourceType: "ServiceProviderConfig", location: `${base}${SCIM_BASE}/ServiceProviderConfig` },
  };
}

const attr = (name: string, extra: Record<string, unknown> = {}): Record<string, unknown> => ({ name, type: "string", multiValued: false, required: false, caseExact: false, mutability: "readWrite", returned: "default", uniqueness: "none", ...extra });

export function schemaDocuments(base: string): Array<Record<string, unknown>> {
  const meta = (id: string) => ({ resourceType: "Schema", location: `${base}${SCIM_BASE}/Schemas/${id}` });
  return [
    {
      schemas: [SCHEMA.schema], id: SCHEMA.user, name: "User", description: "A person provisioned as a principal.",
      attributes: [
        attr("userName", { required: true, uniqueness: "server" }), attr("externalId", { caseExact: true }), attr("displayName"),
        { ...attr("name", { type: "complex" }), subAttributes: [attr("givenName"), attr("familyName")] },
        { ...attr("emails", { type: "complex", multiValued: true }), subAttributes: [attr("value"), attr("type"), attr("primary", { type: "boolean" })] },
        attr("active", { type: "boolean" }),
      ],
      meta: meta(SCHEMA.user),
    },
    {
      schemas: [SCHEMA.schema], id: SCHEMA.group, name: "Group", description: "A Team: members read each other's world state.",
      attributes: [
        attr("displayName", { required: true, uniqueness: "server" }), attr("externalId", { caseExact: true }),
        { ...attr("members", { type: "complex", multiValued: true }), subAttributes: [attr("value", { mutability: "immutable" }), attr("$ref", { type: "reference", mutability: "immutable" }), attr("type", { mutability: "immutable" })] },
      ],
      meta: meta(SCHEMA.group),
    },
    {
      schemas: [SCHEMA.schema], id: SCHEMA.principal, name: "Principal", description: "Where the provisioned principal lives.",
      attributes: [attr("principalId", { mutability: "readOnly", caseExact: true }), attr("tenantId", { mutability: "readOnly", caseExact: true }), attr("region", { mutability: "readOnly" })],
      meta: meta(SCHEMA.principal),
    },
  ];
}

export function resourceTypes(base: string): Array<Record<string, unknown>> {
  return [
    { schemas: [SCHEMA.resourceType], id: "User", name: "User", endpoint: "/Users", schema: SCHEMA.user, schemaExtensions: [{ schema: SCHEMA.principal, required: false }], meta: { resourceType: "ResourceType", location: `${base}${SCIM_BASE}/ResourceTypes/User` } },
    { schemas: [SCHEMA.resourceType], id: "Group", name: "Group", endpoint: "/Groups", schema: SCHEMA.group, meta: { resourceType: "ResourceType", location: `${base}${SCIM_BASE}/ResourceTypes/Group` } },
  ];
}

// --- the app ---

export function createScimApp(deps: ScimDeps): Hono<{ Variables: ScimVars }> {
  const app = new Hono<{ Variables: ScimVars }>();
  const base = deps.publicBaseUrl.replace(/\/+$/, "");
  const nowMs = (): number => deps.now().getTime();
  const limit = bodyLimit({ maxSize: MAX_SCIM_BODY_BYTES, onError: (c) => scimError(c, 413, "The request body is too large.", "tooMany") });

  const auth: MiddlewareHandler<{ Variables: ScimVars }> = async (c, next) => {
    const header = c.req.header("authorization") ?? "";
    const token = /^Bearer\s+/i.test(header) ? header.replace(/^Bearer\s+/i, "").trim() : "";
    if (!SCIM_TOKEN_RE.test(token)) {
      c.set("relayLogEvent", "scim_rejected");
      return scimError(c, 401, "Send the tenant's SCIM token as Authorization: Bearer sct_….");
    }
    let tenant: TenantRecord | undefined;
    try {
      tenant = await deps.store.tenantByScimHash(sha256Hex(token));
    } catch {
      return scimError(c, 503, "The tenant directory is unavailable. Retry shortly.");
    }
    if (!tenant) {
      c.set("relayLogEvent", "scim_rejected");
      return scimError(c, 401, "The SCIM token is not recognised. Rotate it with the operator.");
    }
    if (tenant.plan !== "enterprise") return scimError(c, 403, "SCIM provisioning is part of the enterprise plan.");
    if (tenant.region !== deps.region.served) {
      const r = regionRefusal(deps.region, tenant.region);
      c.set("relayLogEvent", "region_refused");
      return scimError(c, 421, r.message, undefined, { next_action: "use_region_endpoint", ...(r.extra ?? {}) });
    }
    c.set("scimTenant", tenant);
    await next();
  };
  const tenantOf = (c: C): TenantRecord => {
    const t = c.get("scimTenant");
    if (!t) throw new Error("SCIM route without auth");
    return t;
  };

  const readBody = async (c: C): Promise<Record<string, unknown> | Response> => {
    try {
      const v: unknown = await c.req.json();
      if (typeof v !== "object" || v === null || Array.isArray(v)) return scimError(c, 400, "The body must be a JSON object.", "invalidSyntax");
      return v as Record<string, unknown>;
    } catch {
      return scimError(c, 400, "The body must be JSON.", "invalidSyntax");
    }
  };

  const failure = (c: C, e: unknown): Response => {
    if (e instanceof Invalid) return scimError(c, 400, e.detail, e.scimType);
    c.set("relayLogEvent", "tenant_directory_unavailable");
    return scimError(c, 503, "The tenant directory is unavailable. Retry shortly.");
  };

  /** Revoke every key and OAuth grant of a principal now. */
  const revokeAccess = async (principalId: string): Promise<void> => {
    const at = nowMs();
    await deps.agentStore.revokePrincipalKeys(principalId, at);
    if (deps.oauthStore) await deps.oauthStore.revokePrincipal(principalId, at);
  };

  // --- discovery documents (authenticated, like the rest: they name the tenant's base) ---
  app.use(`${SCIM_BASE}/*`, auth);
  app.get(`${SCIM_BASE}/ServiceProviderConfig`, (c) => scimJson(c, serviceProviderConfig(base)));
  app.get(`${SCIM_BASE}/Schemas`, (c) => { const docs = schemaDocuments(base); return scimJson(c, listResponse(docs.length, 1, docs)); });
  app.get(`${SCIM_BASE}/Schemas/:id`, (c) => {
    const doc = schemaDocuments(base).find((d) => d.id === c.req.param("id"));
    return doc ? scimJson(c, doc) : scimError(c, 404, "No such schema.");
  });
  app.get(`${SCIM_BASE}/ResourceTypes`, (c) => { const docs = resourceTypes(base); return scimJson(c, listResponse(docs.length, 1, docs)); });
  app.get(`${SCIM_BASE}/ResourceTypes/:id`, (c) => {
    const doc = resourceTypes(base).find((d) => d.id === c.req.param("id"));
    return doc ? scimJson(c, doc) : scimError(c, 404, "No such resource type.");
  });

  // --- Users ---
  app.get(`${SCIM_BASE}/Users`, async (c) => {
    const tenant = tenantOf(c);
    const filter = parseFilter(c.req.query("filter"), ["userName", "externalId"]);
    if (filter === "invalid") return scimError(c, 400, "Supported filters: userName eq \"…\" and externalId eq \"…\".", "invalidFilter");
    const page = pageParams(c.req.query("startIndex"), c.req.query("count"));
    try {
      const out = await deps.store.listUsers(tenant.tenantId, filter, page.offset, page.limit);
      return scimJson(c, listResponse(out.total, page.startIndex, out.rows.map((u) => renderUser(u, base, tenant.region))));
    } catch (e) {
      return failure(c, e);
    }
  });

  app.post(`${SCIM_BASE}/Users`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const attrs = userAttributes(body);
      const existing = await deps.store.listUsers(tenant.tenantId, { attribute: "userName", value: attrs.userName }, 0, 1);
      if (existing.total > 0) return scimError(c, 409, "A user with this userName already exists in this tenant.", "uniqueness");
      const at = nowMs();
      const principalId = `prn_${randomUUID()}`;
      await deps.agentStore.createPrincipal({ principalId, tenantId: tenant.tenantId, createdAt: at });
      const attached = await deps.store.attachPrincipal(tenant.tenantId, principalId, at);
      if (attached === "other_tenant") return scimError(c, 409, "The principal belongs to another tenant.", "uniqueness");
      const user: ScimUser = { ...attrs, userId: principalId, tenantId: tenant.tenantId, createdAt: at, updatedAt: at };
      if ((await deps.store.insertUser(user)) === "conflict") {
        await deps.store.detachPrincipal(principalId);
        return scimError(c, 409, "A user with this userName already exists in this tenant.", "uniqueness");
      }
      c.set("relayLogEvent", "scim_user_created");
      return scimJson(c, renderUser(user, base, tenant.region), 201, { location: `${base}${SCIM_BASE}/Users/${principalId}` });
    } catch (e) {
      return failure(c, e);
    }
  });

  app.get(`${SCIM_BASE}/Users/:id`, async (c) => {
    const tenant = tenantOf(c);
    try {
      const u = await deps.store.user(tenant.tenantId, c.req.param("id"));
      return u && u.deletedAt === undefined ? scimJson(c, renderUser(u, base, tenant.region)) : scimError(c, 404, "No such user.");
    } catch (e) {
      return failure(c, e);
    }
  });

  /** Store a changed user, revoking access when it went inactive. */
  const saveUser = async (c: C, tenant: TenantRecord, current: ScimUser, next: ScimUser): Promise<Response> => {
    next.updatedAt = nowMs();
    const out = await deps.store.updateUser(next);
    if (out === "not_found") return scimError(c, 404, "No such user.");
    if (out === "conflict") return scimError(c, 409, "A user with this userName already exists in this tenant.", "uniqueness");
    if (current.active && !next.active) {
      await revokeAccess(next.userId);
      c.set("relayLogEvent", "scim_user_deactivated");
    } else {
      c.set("relayLogEvent", "scim_user_updated");
    }
    return scimJson(c, renderUser(next, base, tenant.region));
  };

  app.put(`${SCIM_BASE}/Users/:id`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const current = await deps.store.user(tenant.tenantId, c.req.param("id"));
      if (!current || current.deletedAt !== undefined) return scimError(c, 404, "No such user.");
      const attrs = userAttributes({ ...body, ...(body.active === undefined ? { active: current.active } : {}) });
      return await saveUser(c, tenant, current, { ...attrs, userId: current.userId, tenantId: current.tenantId, createdAt: current.createdAt, updatedAt: current.updatedAt });
    } catch (e) {
      return failure(c, e);
    }
  });

  app.patch(`${SCIM_BASE}/Users/:id`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const current = await deps.store.user(tenant.tenantId, c.req.param("id"));
      if (!current || current.deletedAt !== undefined) return scimError(c, 404, "No such user.");
      return await saveUser(c, tenant, current, applyUserPatch(current, patchOps(body)));
    } catch (e) {
      return failure(c, e);
    }
  });

  app.delete(`${SCIM_BASE}/Users/:id`, async (c) => {
    const tenant = tenantOf(c);
    const id = c.req.param("id");
    try {
      const current = await deps.store.user(tenant.tenantId, id);
      if (!current || current.deletedAt !== undefined) return scimError(c, 404, "No such user.");
      // Access ends now; the store is purged after the grace period (the sweep, `app.ts`).
      await revokeAccess(id);
      for (const g of await deps.store.groupsOf(tenant.tenantId, id)) {
        const next = { ...g, members: g.members.filter((m) => m !== id), updatedAt: nowMs() };
        const synced = await syncTeam(tenant.tenantId, g, next);
        if (synced) return synced(c);
        await deps.store.updateGroup(next);
      }
      const at = nowMs();
      await deps.store.deleteUser(tenant.tenantId, id, at, at + deps.deleteGraceMs);
      c.set("relayLogEvent", "scim_user_deleted");
      return c.body(null, 204, { "cache-control": "no-store" });
    } catch (e) {
      return failure(c, e);
    }
  });

  // --- Groups ---

  /** Whether two principals share a live group other than `exceptGroupId`. */
  const stillShare = async (tenantId: string, a: string, b: string, exceptGroupId: string): Promise<boolean> =>
    (await deps.store.groupsOf(tenantId, a)).some((g) => g.groupId !== exceptGroupId && g.members.includes(b));

  /** Push a membership change to the sidecar: the team, then grants for new pairs and revocations
   * for pairs that no longer share a group. `undefined` = done; else the refusal to answer with. */
  async function syncTeam(tenantId: string, before: ScimGroup | undefined, after: ScimGroup): Promise<((c: C) => Response) | undefined> {
    if (!deps.sidecar) return (c) => scimError(c, 503, "Group sync needs the sidecar (SHOGUN_SIDECAR_URL and SHOGUN_SIDECAR_ADMIN_TOKEN). Nothing was changed.");
    const sidecar = deps.sidecar;
    const old = new Set(before?.members ?? []);
    const now = new Set(after.members);
    const unavailable = (c: C): Response => {
      c.set("relayLogEvent", "sidecar_unavailable");
      return scimError(c, 503, "The sidecar did not accept the team change. Retry; the change is applied idempotently.");
    };
    const team = await sidecar.setTeam(tenantId, after.groupId, after.members);
    if (!team.ok) return unavailable;
    for (const a of now) {
      for (const b of now) {
        if (a === b || (old.has(a) && old.has(b))) continue;
        if (!(await sidecar.grantWorldRead(a, b)).ok) return unavailable;
      }
    }
    for (const a of old) {
      for (const b of old) {
        if (a === b || (now.has(a) && now.has(b))) continue;
        if (await stillShare(tenantId, a, b, after.groupId)) continue;
        if (!(await sidecar.revokeWorldRead(a, b)).ok) return unavailable;
      }
    }
    return undefined;
  }

  /** Members must be live users of this tenant. */
  const checkMembers = async (tenantId: string, members: readonly string[]): Promise<void> => {
    if (members.length > MAX_GROUP_MEMBERS) throw new Invalid(`A group holds at most ${MAX_GROUP_MEMBERS} members.`, "tooMany");
    for (const m of members) {
      const u = await deps.store.user(tenantId, m);
      if (!u || u.deletedAt !== undefined) throw new Invalid("Every member must be a user of this tenant.");
    }
  };

  app.get(`${SCIM_BASE}/Groups`, async (c) => {
    const tenant = tenantOf(c);
    const filter = parseFilter(c.req.query("filter"), ["displayName", "externalId"]);
    if (filter === "invalid") return scimError(c, 400, "Supported filters: displayName eq \"…\" and externalId eq \"…\".", "invalidFilter");
    const page = pageParams(c.req.query("startIndex"), c.req.query("count"));
    const withMembers = !/\bmembers\b/i.test(c.req.query("excludedAttributes") ?? "");
    try {
      const out = await deps.store.listGroups(tenant.tenantId, filter, page.offset, page.limit);
      return scimJson(c, listResponse(out.total, page.startIndex, out.rows.map((g) => renderGroup(g, base, withMembers))));
    } catch (e) {
      return failure(c, e);
    }
  });

  app.post(`${SCIM_BASE}/Groups`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const displayName = optString(body.displayName, "displayName");
      if (!displayName) throw new Invalid("displayName is required.");
      const externalId = optString(body.externalId, "externalId");
      const members = [...new Set(memberIds(body.members))];
      await checkMembers(tenant.tenantId, members);
      const taken = await deps.store.listGroups(tenant.tenantId, { attribute: "displayName", value: displayName }, 0, 1);
      if (taken.total > 0) return scimError(c, 409, "A group with this displayName already exists in this tenant.", "uniqueness");
      const at = nowMs();
      const group: ScimGroup = { groupId: `grp_${randomUUID()}`, tenantId: tenant.tenantId, displayName, ...(externalId ? { externalId } : {}), members, createdAt: at, updatedAt: at };
      const refused = await syncTeam(tenant.tenantId, undefined, group);
      if (refused) return refused(c);
      if ((await deps.store.insertGroup(group)) === "conflict") return scimError(c, 409, "A group with this displayName already exists in this tenant.", "uniqueness");
      c.set("relayLogEvent", "scim_group_synced");
      return scimJson(c, renderGroup(group, base), 201, { location: `${base}${SCIM_BASE}/Groups/${group.groupId}` });
    } catch (e) {
      return failure(c, e);
    }
  });

  app.get(`${SCIM_BASE}/Groups/:id`, async (c) => {
    const tenant = tenantOf(c);
    try {
      const g = await deps.store.group(tenant.tenantId, c.req.param("id"));
      return g ? scimJson(c, renderGroup(g, base, !/\bmembers\b/i.test(c.req.query("excludedAttributes") ?? ""))) : scimError(c, 404, "No such group.");
    } catch (e) {
      return failure(c, e);
    }
  });

  const saveGroup = async (c: C, tenant: TenantRecord, current: ScimGroup, next: ScimGroup): Promise<Response> => {
    await checkMembers(tenant.tenantId, next.members);
    next.updatedAt = nowMs();
    const refused = await syncTeam(tenant.tenantId, current, next);
    if (refused) return refused(c);
    const out = await deps.store.updateGroup(next);
    if (out === "not_found") return scimError(c, 404, "No such group.");
    if (out === "conflict") return scimError(c, 409, "A group with this displayName already exists in this tenant.", "uniqueness");
    c.set("relayLogEvent", "scim_group_synced");
    return scimJson(c, renderGroup(next, base));
  };

  app.put(`${SCIM_BASE}/Groups/:id`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const current = await deps.store.group(tenant.tenantId, c.req.param("id"));
      if (!current) return scimError(c, 404, "No such group.");
      const displayName = optString(body.displayName, "displayName");
      if (!displayName) throw new Invalid("displayName is required.");
      const externalId = optString(body.externalId, "externalId");
      const next: ScimGroup = { ...current, displayName, members: [...new Set(memberIds(body.members))] };
      if (externalId) next.externalId = externalId; else delete next.externalId;
      return await saveGroup(c, tenant, current, next);
    } catch (e) {
      return failure(c, e);
    }
  });

  app.patch(`${SCIM_BASE}/Groups/:id`, limit, async (c) => {
    const tenant = tenantOf(c);
    const body = await readBody(c);
    if (body instanceof Response) return body;
    try {
      const current = await deps.store.group(tenant.tenantId, c.req.param("id"));
      if (!current) return scimError(c, 404, "No such group.");
      const next = applyGroupPatch(current, patchOps(body));
      const res = await saveGroup(c, tenant, current, next);
      // RFC 7644 §3.5.2: 204 is allowed when no attributes were requested back; Entra ID and Okta
      // both accept 200 with the resource, which is what saveGroup returns.
      return res;
    } catch (e) {
      return failure(c, e);
    }
  });

  app.delete(`${SCIM_BASE}/Groups/:id`, async (c) => {
    const tenant = tenantOf(c);
    try {
      const current = await deps.store.group(tenant.tenantId, c.req.param("id"));
      if (!current) return scimError(c, 404, "No such group.");
      const refused = await syncTeam(tenant.tenantId, current, { ...current, members: [] });
      if (refused) return refused(c);
      await deps.store.deleteGroup(tenant.tenantId, current.groupId, nowMs());
      c.set("relayLogEvent", "scim_group_synced");
      return c.body(null, 204, { "cache-control": "no-store" });
    } catch (e) {
      return failure(c, e);
    }
  });

  app.all(`${SCIM_BASE}/*`, (c) => scimError(c, 404, "No such SCIM endpoint."));
  return app;
}

