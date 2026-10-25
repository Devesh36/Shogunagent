/**
 * The enterprise directory (docs/implementation-plan.md E26): tenants, tenant admins, tenant
 * membership of principals, SCIM users and groups, and the SAML assertion replay cache. In-memory
 * here (tests, the Node development entry); `SqlEnterpriseStore` on Postgres
 * (sql/enterprise.sql) in production.
 *
 * What it holds is identity metadata, never memory content: ids, a tenant's name and region, the
 * SSO configuration (the OIDC client secret only sealed, `crypto.ts`), the SCIM bearer only as a
 * SHA-256 hash, admin e-mail addresses and the SCIM user attributes the customer's IdP pushes
 * (userName, name, e-mails, active). A deleted SCIM user keeps a tombstone (ids and times; the
 * attributes are cleared) so a later re-provision cannot resurrect it by id.
 */
import type { Row, SqlClient, SqlExecutor } from "../sql.js";

export const TENANT_PLANS = ["self-serve", "enterprise"] as const;
export type TenantPlan = (typeof TENANT_PLANS)[number];

export interface OidcConfig {
  /** The IdP's issuer (`iss`), exactly as its discovery document names it. */
  issuer: string;
  clientId: string;
  /** The client secret, sealed (`crypto.ts` `seal`). Absent = a public client (PKCE only). */
  clientSecretSealed?: string;
}

export interface SamlConfig {
  /** The IdP's entity id: the `Issuer` of its assertions. */
  idpEntityId: string;
  /** The IdP's SSO endpoint (HTTP-Redirect binding). */
  ssoUrl: string;
  /** PEM signing certificates (or public keys) the IdP signs assertions with; more than one
   * during a rotation. */
  certificates: string[];
}

export interface SsoConfig {
  protocol: "oidc" | "saml";
  /** Enforced: the tenant's principals cannot consent to OAuth clients by pasting a key. */
  enforced: boolean;
  oidc?: OidcConfig;
  saml?: SamlConfig;
}

export interface TenantRecord {
  tenantId: string;
  name: string;
  region: string;
  plan: TenantPlan;
  sso?: SsoConfig;
  /** SHA-256 (hex) of the SCIM bearer; the bearer itself is shown once. */
  scimTokenHash?: string;
  /** The operator's Idempotency-Key the tenant was created with. */
  idempotencyKey?: string;
  createdAt: number;
  updatedAt: number;
}

export interface TenantAdmin {
  adminId: string;
  tenantId: string;
  /** Lower-cased. */
  email: string;
  /** The IdP subject the admin signed in with first (`oidc:<iss>|<sub>` or `saml:<issuer>|<NameID>`). */
  subject?: string;
  createdAt: number;
  removedAt?: number;
}

export interface ScimEmail {
  value: string;
  type?: string;
  primary?: boolean;
}

export interface ScimUser {
  /** The SCIM `id`: the principal id the user provisions. */
  userId: string;
  tenantId: string;
  userName: string;
  externalId?: string;
  displayName?: string;
  givenName?: string;
  familyName?: string;
  emails: ScimEmail[];
  active: boolean;
  createdAt: number;
  updatedAt: number;
  /** Tombstone: the user was deleted at this instant (SCIM DELETE). */
  deletedAt?: number;
  /** The principal store is purged at or after this instant (the grace period). */
  deleteAfter?: number;
  /** The purge ran. */
  purgedAt?: number;
}

export interface ScimGroup {
  groupId: string;
  tenantId: string;
  displayName: string;
  externalId?: string;
  /** Member principal ids (SCIM user ids). */
  members: string[];
  createdAt: number;
  updatedAt: number;
  deletedAt?: number;
}

/** A SCIM list filter the store evaluates (`userName eq`, `externalId eq`, `displayName eq`). */
export interface EqFilter {
  attribute: "userName" | "externalId" | "displayName";
  value: string;
}

export interface Page<T> {
  total: number;
  rows: T[];
}

export interface EnterpriseStore {
  createTenant(t: TenantRecord): Promise<{ created: true; tenant: TenantRecord } | { created: false; tenant: TenantRecord }>;
  tenant(tenantId: string): Promise<TenantRecord | undefined>;
  tenantByScimHash(hash: string): Promise<TenantRecord | undefined>;
  updateTenant(tenantId: string, patch: { name?: string; plan?: TenantPlan; sso?: SsoConfig | null }, at: number): Promise<TenantRecord | undefined>;
  setScimTokenHash(tenantId: string, hash: string, at: number): Promise<boolean>;

  /** A principal belongs to at most one tenant. */
  attachPrincipal(tenantId: string, principalId: string, at: number): Promise<"attached" | "already" | "other_tenant">;
  /** The tenant a principal belongs to. */
  membership(principalId: string): Promise<string | undefined>;
  members(tenantId: string): Promise<string[]>;
  detachPrincipal(principalId: string): Promise<void>;

  addAdmin(a: TenantAdmin): Promise<"created" | "duplicate">;
  admins(tenantId: string): Promise<TenantAdmin[]>;
  admin(adminId: string): Promise<TenantAdmin | undefined>;
  /** Bind the IdP subject on first sign-in. False when another subject is already bound. */
  bindAdminSubject(adminId: string, subject: string): Promise<boolean>;
  removeAdmin(adminId: string, at: number): Promise<boolean>;

  insertUser(u: ScimUser): Promise<"created" | "conflict">;
  user(tenantId: string, userId: string): Promise<ScimUser | undefined>;
  listUsers(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimUser>>;
  /** Replace a live user's attributes. `conflict` when the new userName is another live user's. */
  updateUser(u: ScimUser): Promise<"ok" | "conflict" | "not_found">;
  /** Tombstone: clear the attributes, set `deletedAt` and `deleteAfter`. */
  deleteUser(tenantId: string, userId: string, at: number, deleteAfter: number): Promise<boolean>;
  /** Deleted users whose grace period has passed and whose store is not purged yet. */
  duePurges(nowMs: number, limit: number): Promise<ScimUser[]>;
  markPurged(userId: string, at: number): Promise<void>;

  insertGroup(g: ScimGroup): Promise<"created" | "conflict">;
  group(tenantId: string, groupId: string): Promise<ScimGroup | undefined>;
  listGroups(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimGroup>>;
  updateGroup(g: ScimGroup): Promise<"ok" | "conflict" | "not_found">;
  deleteGroup(tenantId: string, groupId: string, at: number): Promise<boolean>;
  /** Live groups of a tenant that list `principalId`. */
  groupsOf(tenantId: string, principalId: string): Promise<ScimGroup[]>;

  /** Spend a SAML assertion id until `expiresAt`. False when it was already spent (a replay). */
  spendAssertion(key: string, expiresAt: number, nowMs: number): Promise<boolean>;
  maintenance(nowMs: number): Promise<void>;
}

export class EnterpriseStoreUnavailable extends Error {}

const lc = (s: string): string => s.toLowerCase();
const clone = <T>(v: T): T => structuredClone(v);

function matches(filter: EqFilter | undefined, row: { userName?: string; externalId?: string; displayName?: string }): boolean {
  if (!filter) return true;
  if (filter.attribute === "userName") return row.userName !== undefined && lc(row.userName) === lc(filter.value);
  if (filter.attribute === "externalId") return row.externalId === filter.value;
  return row.displayName !== undefined && row.displayName === filter.value;
}

export class InMemoryEnterpriseStore implements EnterpriseStore {
  private readonly tenants = new Map<string, TenantRecord>();
  private readonly memberships = new Map<string, string>();
  private readonly adminRows = new Map<string, TenantAdmin>();
  private readonly users = new Map<string, ScimUser>();
  private readonly groups = new Map<string, ScimGroup>();
  private readonly assertions = new Map<string, number>();

  createTenant(t: TenantRecord): Promise<{ created: true; tenant: TenantRecord } | { created: false; tenant: TenantRecord }> {
    const existing = t.idempotencyKey ? [...this.tenants.values()].find((x) => x.idempotencyKey === t.idempotencyKey) : undefined;
    if (existing) return Promise.resolve({ created: false, tenant: clone(existing) });
    this.tenants.set(t.tenantId, clone(t));
    return Promise.resolve({ created: true, tenant: clone(t) });
  }

  tenant(tenantId: string): Promise<TenantRecord | undefined> {
    const t = this.tenants.get(tenantId);
    return Promise.resolve(t ? clone(t) : undefined);
  }

  tenantByScimHash(hash: string): Promise<TenantRecord | undefined> {
    const t = [...this.tenants.values()].find((x) => x.scimTokenHash === hash);
    return Promise.resolve(t ? clone(t) : undefined);
  }

  updateTenant(tenantId: string, patch: { name?: string; plan?: TenantPlan; sso?: SsoConfig | null }, at: number): Promise<TenantRecord | undefined> {
    const t = this.tenants.get(tenantId);
    if (!t) return Promise.resolve(undefined);
    if (patch.name !== undefined) t.name = patch.name;
    if (patch.plan !== undefined) t.plan = patch.plan;
    if (patch.sso === null) delete t.sso;
    else if (patch.sso !== undefined) t.sso = clone(patch.sso);
    t.updatedAt = at;
    return Promise.resolve(clone(t));
  }

  setScimTokenHash(tenantId: string, hash: string, at: number): Promise<boolean> {
    const t = this.tenants.get(tenantId);
    if (!t) return Promise.resolve(false);
    t.scimTokenHash = hash;
    t.updatedAt = at;
    return Promise.resolve(true);
  }

  attachPrincipal(tenantId: string, principalId: string): Promise<"attached" | "already" | "other_tenant"> {
    const current = this.memberships.get(principalId);
    if (current === tenantId) return Promise.resolve("already");
    if (current !== undefined) return Promise.resolve("other_tenant");
    this.memberships.set(principalId, tenantId);
    return Promise.resolve("attached");
  }

  membership(principalId: string): Promise<string | undefined> {
    return Promise.resolve(this.memberships.get(principalId));
  }

  members(tenantId: string): Promise<string[]> {
    return Promise.resolve([...this.memberships.entries()].filter(([, t]) => t === tenantId).map(([p]) => p).sort());
  }

  detachPrincipal(principalId: string): Promise<void> {
    this.memberships.delete(principalId);
    return Promise.resolve();
  }

  addAdmin(a: TenantAdmin): Promise<"created" | "duplicate"> {
    const dup = [...this.adminRows.values()].some((x) => x.tenantId === a.tenantId && x.removedAt === undefined && x.email === a.email);
    if (dup || this.adminRows.has(a.adminId)) return Promise.resolve("duplicate");
    this.adminRows.set(a.adminId, clone(a));
    return Promise.resolve("created");
  }

  admins(tenantId: string): Promise<TenantAdmin[]> {
    return Promise.resolve([...this.adminRows.values()].filter((a) => a.tenantId === tenantId && a.removedAt === undefined).map(clone));
  }

  admin(adminId: string): Promise<TenantAdmin | undefined> {
    const a = this.adminRows.get(adminId);
    return Promise.resolve(a ? clone(a) : undefined);
  }

  bindAdminSubject(adminId: string, subject: string): Promise<boolean> {
    const a = this.adminRows.get(adminId);
    if (!a || (a.subject !== undefined && a.subject !== subject)) return Promise.resolve(false);
    a.subject = subject;
    return Promise.resolve(true);
  }

  removeAdmin(adminId: string, at: number): Promise<boolean> {
    const a = this.adminRows.get(adminId);
    if (!a || a.removedAt !== undefined) return Promise.resolve(false);
    a.removedAt = at;
    return Promise.resolve(true);
  }

  private liveUserName(tenantId: string, userName: string, except?: string): boolean {
    return [...this.users.values()].some((u) => u.tenantId === tenantId && u.deletedAt === undefined && u.userId !== except && lc(u.userName) === lc(userName));
  }

  insertUser(u: ScimUser): Promise<"created" | "conflict"> {
    if (this.users.has(u.userId) || this.liveUserName(u.tenantId, u.userName)) return Promise.resolve("conflict");
    this.users.set(u.userId, clone(u));
    return Promise.resolve("created");
  }

  user(tenantId: string, userId: string): Promise<ScimUser | undefined> {
    const u = this.users.get(userId);
    return Promise.resolve(u && u.tenantId === tenantId ? clone(u) : undefined);
  }

  listUsers(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimUser>> {
    const all = [...this.users.values()].filter((u) => u.tenantId === tenantId && u.deletedAt === undefined && matches(filter, u))
      .sort((a, b) => a.createdAt - b.createdAt || a.userId.localeCompare(b.userId));
    return Promise.resolve({ total: all.length, rows: all.slice(offset, offset + limit).map(clone) });
  }

  updateUser(u: ScimUser): Promise<"ok" | "conflict" | "not_found"> {
    const cur = this.users.get(u.userId);
    if (!cur || cur.tenantId !== u.tenantId || cur.deletedAt !== undefined) return Promise.resolve("not_found");
    if (this.liveUserName(u.tenantId, u.userName, u.userId)) return Promise.resolve("conflict");
    this.users.set(u.userId, clone(u));
    return Promise.resolve("ok");
  }

  deleteUser(tenantId: string, userId: string, at: number, deleteAfter: number): Promise<boolean> {
    const u = this.users.get(userId);
    if (!u || u.tenantId !== tenantId || u.deletedAt !== undefined) return Promise.resolve(false);
    this.users.set(userId, { userId, tenantId, userName: "", emails: [], active: false, createdAt: u.createdAt, updatedAt: at, deletedAt: at, deleteAfter });
    return Promise.resolve(true);
  }

  duePurges(nowMs: number, limit: number): Promise<ScimUser[]> {
    return Promise.resolve([...this.users.values()].filter((u) => u.deleteAfter !== undefined && u.deleteAfter <= nowMs && u.purgedAt === undefined)
      .sort((a, b) => (a.deleteAfter ?? 0) - (b.deleteAfter ?? 0)).slice(0, limit).map(clone));
  }

  markPurged(userId: string, at: number): Promise<void> {
    const u = this.users.get(userId);
    if (u && u.purgedAt === undefined) u.purgedAt = at;
    return Promise.resolve();
  }

  private liveGroupName(tenantId: string, name: string, except?: string): boolean {
    return [...this.groups.values()].some((g) => g.tenantId === tenantId && g.deletedAt === undefined && g.groupId !== except && g.displayName === name);
  }

  insertGroup(g: ScimGroup): Promise<"created" | "conflict"> {
    if (this.groups.has(g.groupId) || this.liveGroupName(g.tenantId, g.displayName)) return Promise.resolve("conflict");
    this.groups.set(g.groupId, clone(g));
    return Promise.resolve("created");
  }

  group(tenantId: string, groupId: string): Promise<ScimGroup | undefined> {
    const g = this.groups.get(groupId);
    return Promise.resolve(g && g.tenantId === tenantId && g.deletedAt === undefined ? clone(g) : undefined);
  }

  listGroups(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimGroup>> {
    const all = [...this.groups.values()].filter((g) => g.tenantId === tenantId && g.deletedAt === undefined && matches(filter, g))
      .sort((a, b) => a.createdAt - b.createdAt || a.groupId.localeCompare(b.groupId));
    return Promise.resolve({ total: all.length, rows: all.slice(offset, offset + limit).map(clone) });
  }

  updateGroup(g: ScimGroup): Promise<"ok" | "conflict" | "not_found"> {
    const cur = this.groups.get(g.groupId);
    if (!cur || cur.tenantId !== g.tenantId || cur.deletedAt !== undefined) return Promise.resolve("not_found");
    if (this.liveGroupName(g.tenantId, g.displayName, g.groupId)) return Promise.resolve("conflict");
    this.groups.set(g.groupId, clone(g));
    return Promise.resolve("ok");
  }

  deleteGroup(tenantId: string, groupId: string, at: number): Promise<boolean> {
    const g = this.groups.get(groupId);
    if (!g || g.tenantId !== tenantId || g.deletedAt !== undefined) return Promise.resolve(false);
    g.deletedAt = at;
    g.members = [];
    return Promise.resolve(true);
  }

  groupsOf(tenantId: string, principalId: string): Promise<ScimGroup[]> {
    return Promise.resolve([...this.groups.values()].filter((g) => g.tenantId === tenantId && g.deletedAt === undefined && g.members.includes(principalId)).map(clone));
  }

  spendAssertion(key: string, expiresAt: number, nowMs: number): Promise<boolean> {
    const until = this.assertions.get(key);
    if (until !== undefined && until > nowMs) return Promise.resolve(false);
    this.assertions.set(key, expiresAt);
    return Promise.resolve(true);
  }

  maintenance(nowMs: number): Promise<void> {
    for (const [k, until] of this.assertions) if (until <= nowMs) this.assertions.delete(k);
    return Promise.resolve();
  }
}

// --- Postgres ---

export const ENTERPRISE_TABLES = ["enterprise_tenants", "enterprise_members", "enterprise_admins", "enterprise_scim_users", "enterprise_scim_groups", "enterprise_saml_replay"] as const;

const num = (v: unknown): number => Number(v ?? 0);
const opt = (v: unknown): number | undefined => (v === null || v === undefined ? undefined : Number(v));
const optText = (v: unknown): string | undefined => (v === null || v === undefined || v === "" ? undefined : String(v));

function parseSso(text: unknown): SsoConfig | undefined {
  if (typeof text !== "string" || text === "") return undefined;
  try {
    const v = JSON.parse(text) as SsoConfig;
    return typeof v === "object" && v !== null && (v.protocol === "oidc" || v.protocol === "saml") ? v : undefined;
  } catch {
    return undefined;
  }
}

const TENANT_COLUMNS = "tenant_id,name,region,plan,sso_json,scim_token_hash,idempotency_key,created_at,updated_at";

function tenantFromRow(r: Row): TenantRecord {
  const sso = parseSso(r.sso_json);
  return {
    tenantId: String(r.tenant_id), name: String(r.name), region: String(r.region), plan: r.plan === "enterprise" ? "enterprise" : "self-serve",
    ...(sso ? { sso } : {}),
    ...(optText(r.scim_token_hash) ? { scimTokenHash: String(r.scim_token_hash) } : {}),
    ...(optText(r.idempotency_key) ? { idempotencyKey: String(r.idempotency_key) } : {}),
    createdAt: num(r.created_at), updatedAt: num(r.updated_at),
  };
}

const ADMIN_COLUMNS = "admin_id,tenant_id,email,subject,created_at,removed_at";

function adminFromRow(r: Row): TenantAdmin {
  return {
    adminId: String(r.admin_id), tenantId: String(r.tenant_id), email: String(r.email),
    ...(optText(r.subject) ? { subject: String(r.subject) } : {}), createdAt: num(r.created_at),
    ...(opt(r.removed_at) !== undefined ? { removedAt: num(r.removed_at) } : {}),
  };
}

const USER_COLUMNS = "user_id,tenant_id,user_name,external_id,display_name,given_name,family_name,emails_json,active,created_at,updated_at,deleted_at,delete_after,purged_at";

function userFromRow(r: Row): ScimUser {
  let emails: ScimEmail[] = [];
  try {
    const parsed: unknown = JSON.parse(String(r.emails_json ?? "[]"));
    if (Array.isArray(parsed)) emails = parsed as ScimEmail[];
  } catch {
    emails = [];
  }
  return {
    userId: String(r.user_id), tenantId: String(r.tenant_id), userName: String(r.user_name ?? ""),
    ...(optText(r.external_id) ? { externalId: String(r.external_id) } : {}),
    ...(optText(r.display_name) ? { displayName: String(r.display_name) } : {}),
    ...(optText(r.given_name) ? { givenName: String(r.given_name) } : {}),
    ...(optText(r.family_name) ? { familyName: String(r.family_name) } : {}),
    emails, active: num(r.active) === 1, createdAt: num(r.created_at), updatedAt: num(r.updated_at),
    ...(opt(r.deleted_at) !== undefined ? { deletedAt: num(r.deleted_at) } : {}),
    ...(opt(r.delete_after) !== undefined ? { deleteAfter: num(r.delete_after) } : {}),
    ...(opt(r.purged_at) !== undefined ? { purgedAt: num(r.purged_at) } : {}),
  };
}

const GROUP_COLUMNS = "group_id,tenant_id,display_name,external_id,members,created_at,updated_at,deleted_at";

function groupFromRow(r: Row): ScimGroup {
  return {
    groupId: String(r.group_id), tenantId: String(r.tenant_id), displayName: String(r.display_name),
    ...(optText(r.external_id) ? { externalId: String(r.external_id) } : {}),
    members: String(r.members ?? "").split(",").filter((m) => m.length > 0),
    createdAt: num(r.created_at), updatedAt: num(r.updated_at),
    ...(opt(r.deleted_at) !== undefined ? { deletedAt: num(r.deleted_at) } : {}),
  };
}

function userFilterSql(filter: EqFilter | undefined, base: number): { sql: string; params: unknown[] } {
  if (!filter) return { sql: "", params: [] };
  if (filter.attribute === "userName") return { sql: ` AND user_name_lc=$${base}::text`, params: [lc(filter.value)] };
  if (filter.attribute === "externalId") return { sql: ` AND external_id=$${base}::text`, params: [filter.value] };
  return { sql: ` AND display_name=$${base}::text`, params: [filter.value] };
}

/** Postgres's unique-violation and the SQLite double's constraint error both mean "taken". */
function isUniqueViolation(e: unknown): boolean {
  const text = String((e as { code?: unknown })?.code ?? "") + " " + String((e as Error)?.message ?? "");
  return text.includes("23505") || /UNIQUE constraint failed/i.test(text);
}

class Taken extends Error {}

export class SqlEnterpriseStore implements EnterpriseStore {
  constructor(private readonly db: SqlClient) {}

  async checkSchema(): Promise<void> {
    for (const table of ENTERPRISE_TABLES) {
      try {
        await this.db.query(`SELECT 1 FROM ${table} LIMIT 1`);
      } catch {
        throw new EnterpriseStoreUnavailable(`enterprise store table missing or unreadable: ${table}`);
      }
    }
  }

  private async tx<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    try {
      return await this.db.transaction(fn);
    } catch (e) {
      if (e instanceof Taken) throw e;
      throw new EnterpriseStoreUnavailable("enterprise store unavailable");
    }
  }

  private async read(text: string, params: unknown[]): Promise<Row[]> {
    try {
      return await this.db.query(text, params);
    } catch {
      throw new EnterpriseStoreUnavailable("enterprise store unavailable");
    }
  }

  /** Run an insert/update whose unique index may refuse it; a refusal is `Taken`. */
  private async unique(tx: SqlExecutor, text: string, params: unknown[]): Promise<Row[]> {
    try {
      return await tx.query(text, params);
    } catch (e) {
      if (isUniqueViolation(e)) throw new Taken();
      throw e;
    }
  }

  async createTenant(t: TenantRecord): Promise<{ created: true; tenant: TenantRecord } | { created: false; tenant: TenantRecord }> {
    return this.tx(async (tx) => {
      if (t.idempotencyKey) {
        const existing = (await tx.query(`SELECT ${TENANT_COLUMNS} FROM enterprise_tenants WHERE idempotency_key=$1::text`, [t.idempotencyKey]))[0];
        if (existing) return { created: false as const, tenant: tenantFromRow(existing) };
      }
      await tx.query(`INSERT INTO enterprise_tenants(${TENANT_COLUMNS}) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::bigint,$9::bigint)`,
        [t.tenantId, t.name, t.region, t.plan, t.sso ? JSON.stringify(t.sso) : null, t.scimTokenHash ?? null, t.idempotencyKey ?? null, t.createdAt, t.updatedAt]);
      return { created: true as const, tenant: t };
    });
  }

  async tenant(tenantId: string): Promise<TenantRecord | undefined> {
    const r = (await this.read(`SELECT ${TENANT_COLUMNS} FROM enterprise_tenants WHERE tenant_id=$1::text`, [tenantId]))[0];
    return r ? tenantFromRow(r) : undefined;
  }

  async tenantByScimHash(hash: string): Promise<TenantRecord | undefined> {
    const r = (await this.read(`SELECT ${TENANT_COLUMNS} FROM enterprise_tenants WHERE scim_token_hash=$1::text`, [hash]))[0];
    return r ? tenantFromRow(r) : undefined;
  }

  updateTenant(tenantId: string, patch: { name?: string; plan?: TenantPlan; sso?: SsoConfig | null }, at: number): Promise<TenantRecord | undefined> {
    return this.tx(async (tx) => {
      const r = (await tx.query(`SELECT ${TENANT_COLUMNS} FROM enterprise_tenants WHERE tenant_id=$1::text FOR UPDATE`, [tenantId]))[0];
      if (!r) return undefined;
      const cur = tenantFromRow(r);
      const next: TenantRecord = {
        ...cur, ...(patch.name !== undefined ? { name: patch.name } : {}), ...(patch.plan !== undefined ? { plan: patch.plan } : {}), updatedAt: at,
      };
      if (patch.sso === null) delete next.sso;
      else if (patch.sso !== undefined) next.sso = patch.sso;
      await tx.query("UPDATE enterprise_tenants SET name=$2::text,plan=$3::text,sso_json=$4::text,updated_at=$5::bigint WHERE tenant_id=$1::text",
        [tenantId, next.name, next.plan, next.sso ? JSON.stringify(next.sso) : null, at]);
      return next;
    });
  }

  setScimTokenHash(tenantId: string, hash: string, at: number): Promise<boolean> {
    return this.tx(async (tx) => (await tx.query("UPDATE enterprise_tenants SET scim_token_hash=$2::text,updated_at=$3::bigint WHERE tenant_id=$1::text RETURNING tenant_id", [tenantId, hash, at])).length > 0);
  }

  attachPrincipal(tenantId: string, principalId: string, at: number): Promise<"attached" | "already" | "other_tenant"> {
    return this.tx(async (tx) => {
      const inserted = await tx.query(`INSERT INTO enterprise_members(principal_id,tenant_id,created_at) VALUES($1::text,$2::text,$3::bigint)
        ON CONFLICT(principal_id) DO NOTHING RETURNING principal_id`, [principalId, tenantId, at]);
      if (inserted.length > 0) return "attached" as const;
      const r = (await tx.query("SELECT tenant_id FROM enterprise_members WHERE principal_id=$1::text", [principalId]))[0];
      return r && String(r.tenant_id) === tenantId ? ("already" as const) : ("other_tenant" as const);
    });
  }

  async membership(principalId: string): Promise<string | undefined> {
    const r = (await this.read("SELECT tenant_id FROM enterprise_members WHERE principal_id=$1::text", [principalId]))[0];
    return r ? String(r.tenant_id) : undefined;
  }

  async members(tenantId: string): Promise<string[]> {
    return (await this.read("SELECT principal_id FROM enterprise_members WHERE tenant_id=$1::text ORDER BY principal_id", [tenantId])).map((r) => String(r.principal_id));
  }

  detachPrincipal(principalId: string): Promise<void> {
    return this.tx(async (tx) => { await tx.query("DELETE FROM enterprise_members WHERE principal_id=$1::text", [principalId]); });
  }

  async addAdmin(a: TenantAdmin): Promise<"created" | "duplicate"> {
    try {
      return await this.tx(async (tx) => {
        const dup = await tx.query("SELECT admin_id FROM enterprise_admins WHERE tenant_id=$1::text AND email=$2::text AND removed_at IS NULL", [a.tenantId, a.email]);
        if (dup.length > 0) return "duplicate" as const;
        await this.unique(tx, `INSERT INTO enterprise_admins(${ADMIN_COLUMNS}) VALUES($1::text,$2::text,$3::text,$4::text,$5::bigint,NULL)`, [a.adminId, a.tenantId, a.email, a.subject ?? null, a.createdAt]);
        return "created" as const;
      });
    } catch (e) {
      if (e instanceof Taken) return "duplicate";
      throw e;
    }
  }

  async admins(tenantId: string): Promise<TenantAdmin[]> {
    return (await this.read(`SELECT ${ADMIN_COLUMNS} FROM enterprise_admins WHERE tenant_id=$1::text AND removed_at IS NULL ORDER BY created_at, admin_id`, [tenantId])).map(adminFromRow);
  }

  async admin(adminId: string): Promise<TenantAdmin | undefined> {
    const r = (await this.read(`SELECT ${ADMIN_COLUMNS} FROM enterprise_admins WHERE admin_id=$1::text`, [adminId]))[0];
    return r ? adminFromRow(r) : undefined;
  }

  bindAdminSubject(adminId: string, subject: string): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query("UPDATE enterprise_admins SET subject=$2::text WHERE admin_id=$1::text AND (subject IS NULL OR subject=$2::text) RETURNING admin_id", [adminId, subject]);
      return rows.length > 0;
    });
  }

  removeAdmin(adminId: string, at: number): Promise<boolean> {
    return this.tx(async (tx) => (await tx.query("UPDATE enterprise_admins SET removed_at=$2::bigint WHERE admin_id=$1::text AND removed_at IS NULL RETURNING admin_id", [adminId, at])).length > 0);
  }

  private userParams(u: ScimUser): unknown[] {
    return [u.userId, u.tenantId, u.userName, u.deletedAt === undefined ? lc(u.userName) : null, u.externalId ?? null, u.displayName ?? null, u.givenName ?? null, u.familyName ?? null,
      JSON.stringify(u.emails), u.active ? 1 : 0, u.createdAt, u.updatedAt, u.deletedAt ?? null, u.deleteAfter ?? null, u.purgedAt ?? null];
  }

  async insertUser(u: ScimUser): Promise<"created" | "conflict"> {
    try {
      return await this.tx(async (tx) => {
        const taken = await tx.query("SELECT user_id FROM enterprise_scim_users WHERE user_id=$1::text OR (tenant_id=$2::text AND user_name_lc=$3::text)", [u.userId, u.tenantId, lc(u.userName)]);
        if (taken.length > 0) return "conflict" as const;
        await this.unique(tx, `INSERT INTO enterprise_scim_users(user_id,tenant_id,user_name,user_name_lc,external_id,display_name,given_name,family_name,emails_json,active,created_at,updated_at,deleted_at,delete_after,purged_at)
          VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::text,$9::text,$10::smallint,$11::bigint,$12::bigint,$13::bigint,$14::bigint,$15::bigint)`, this.userParams(u));
        return "created" as const;
      });
    } catch (e) {
      if (e instanceof Taken) return "conflict";
      throw e;
    }
  }

  async user(tenantId: string, userId: string): Promise<ScimUser | undefined> {
    const r = (await this.read(`SELECT ${USER_COLUMNS} FROM enterprise_scim_users WHERE user_id=$1::text AND tenant_id=$2::text`, [userId, tenantId]))[0];
    return r ? userFromRow(r) : undefined;
  }

  async listUsers(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimUser>> {
    const f = userFilterSql(filter, 2);
    const where = `tenant_id=$1::text AND deleted_at IS NULL${f.sql}`;
    const total = num((await this.read(`SELECT COUNT(*) AS n FROM enterprise_scim_users WHERE ${where}`, [tenantId, ...f.params]))[0]?.n);
    const n = f.params.length + 2;
    const rows = await this.read(`SELECT ${USER_COLUMNS} FROM enterprise_scim_users WHERE ${where} ORDER BY created_at, user_id LIMIT $${n}::bigint OFFSET $${n + 1}::bigint`, [tenantId, ...f.params, limit, offset]);
    return { total, rows: rows.map(userFromRow) };
  }

  async updateUser(u: ScimUser): Promise<"ok" | "conflict" | "not_found"> {
    try {
      return await this.tx(async (tx) => {
        const cur = (await tx.query("SELECT user_id FROM enterprise_scim_users WHERE user_id=$1::text AND tenant_id=$2::text AND deleted_at IS NULL FOR UPDATE", [u.userId, u.tenantId]))[0];
        if (!cur) return "not_found" as const;
        const taken = await tx.query("SELECT user_id FROM enterprise_scim_users WHERE tenant_id=$1::text AND user_name_lc=$2::text AND user_id<>$3::text", [u.tenantId, lc(u.userName), u.userId]);
        if (taken.length > 0) return "conflict" as const;
        const p = this.userParams(u);
        await this.unique(tx, `UPDATE enterprise_scim_users SET user_name=$3::text,user_name_lc=$4::text,external_id=$5::text,display_name=$6::text,given_name=$7::text,family_name=$8::text,
          emails_json=$9::text,active=$10::smallint,updated_at=$11::bigint WHERE user_id=$1::text AND tenant_id=$2::text`, [...p.slice(0, 10), u.updatedAt]);
        return "ok" as const;
      });
    } catch (e) {
      if (e instanceof Taken) return "conflict";
      throw e;
    }
  }

  deleteUser(tenantId: string, userId: string, at: number, deleteAfter: number): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query(`UPDATE enterprise_scim_users SET user_name='',user_name_lc=NULL,external_id=NULL,display_name=NULL,given_name=NULL,family_name=NULL,emails_json='[]',
        active=0,updated_at=$3::bigint,deleted_at=$3::bigint,delete_after=$4::bigint WHERE user_id=$1::text AND tenant_id=$2::text AND deleted_at IS NULL RETURNING user_id`, [userId, tenantId, at, deleteAfter]);
      return rows.length > 0;
    });
  }

  async duePurges(nowMs: number, limit: number): Promise<ScimUser[]> {
    return (await this.read(`SELECT ${USER_COLUMNS} FROM enterprise_scim_users WHERE delete_after IS NOT NULL AND delete_after<=$1::bigint AND purged_at IS NULL ORDER BY delete_after LIMIT $2::bigint`, [nowMs, limit])).map(userFromRow);
  }

  markPurged(userId: string, at: number): Promise<void> {
    return this.tx(async (tx) => { await tx.query("UPDATE enterprise_scim_users SET purged_at=$2::bigint WHERE user_id=$1::text AND purged_at IS NULL", [userId, at]); });
  }

  private groupParams(g: ScimGroup): unknown[] {
    return [g.groupId, g.tenantId, g.displayName, g.externalId ?? null, g.members.join(","), g.createdAt, g.updatedAt, g.deletedAt ?? null];
  }

  async insertGroup(g: ScimGroup): Promise<"created" | "conflict"> {
    try {
      return await this.tx(async (tx) => {
        const taken = await tx.query("SELECT group_id FROM enterprise_scim_groups WHERE group_id=$1::text OR (tenant_id=$2::text AND display_name=$3::text AND deleted_at IS NULL)", [g.groupId, g.tenantId, g.displayName]);
        if (taken.length > 0) return "conflict" as const;
        await this.unique(tx, `INSERT INTO enterprise_scim_groups(${GROUP_COLUMNS}) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::bigint,$7::bigint,$8::bigint)`, this.groupParams(g));
        return "created" as const;
      });
    } catch (e) {
      if (e instanceof Taken) return "conflict";
      throw e;
    }
  }

  async group(tenantId: string, groupId: string): Promise<ScimGroup | undefined> {
    const r = (await this.read(`SELECT ${GROUP_COLUMNS} FROM enterprise_scim_groups WHERE group_id=$1::text AND tenant_id=$2::text AND deleted_at IS NULL`, [groupId, tenantId]))[0];
    return r ? groupFromRow(r) : undefined;
  }

  async listGroups(tenantId: string, filter: EqFilter | undefined, offset: number, limit: number): Promise<Page<ScimGroup>> {
    const f = filter ? (filter.attribute === "externalId" ? { sql: " AND external_id=$2::text", params: [filter.value] } : filter.attribute === "displayName" ? { sql: " AND display_name=$2::text", params: [filter.value] } : { sql: " AND 1=0", params: [] as unknown[] }) : { sql: "", params: [] as unknown[] };
    const where = `tenant_id=$1::text AND deleted_at IS NULL${f.sql}`;
    const total = num((await this.read(`SELECT COUNT(*) AS n FROM enterprise_scim_groups WHERE ${where}`, [tenantId, ...f.params]))[0]?.n);
    const n = f.params.length + 2;
    const rows = await this.read(`SELECT ${GROUP_COLUMNS} FROM enterprise_scim_groups WHERE ${where} ORDER BY created_at, group_id LIMIT $${n}::bigint OFFSET $${n + 1}::bigint`, [tenantId, ...f.params, limit, offset]);
    return { total, rows: rows.map(groupFromRow) };
  }

  async updateGroup(g: ScimGroup): Promise<"ok" | "conflict" | "not_found"> {
    try {
      return await this.tx(async (tx) => {
        const cur = (await tx.query("SELECT group_id FROM enterprise_scim_groups WHERE group_id=$1::text AND tenant_id=$2::text AND deleted_at IS NULL FOR UPDATE", [g.groupId, g.tenantId]))[0];
        if (!cur) return "not_found" as const;
        const taken = await tx.query("SELECT group_id FROM enterprise_scim_groups WHERE tenant_id=$1::text AND display_name=$2::text AND deleted_at IS NULL AND group_id<>$3::text", [g.tenantId, g.displayName, g.groupId]);
        if (taken.length > 0) return "conflict" as const;
        await this.unique(tx, "UPDATE enterprise_scim_groups SET display_name=$3::text,external_id=$4::text,members=$5::text,updated_at=$6::bigint WHERE group_id=$1::text AND tenant_id=$2::text",
          [...this.groupParams(g).slice(0, 5), g.updatedAt]);
        return "ok" as const;
      });
    } catch (e) {
      if (e instanceof Taken) return "conflict";
      throw e;
    }
  }

  deleteGroup(tenantId: string, groupId: string, at: number): Promise<boolean> {
    return this.tx(async (tx) => (await tx.query("UPDATE enterprise_scim_groups SET deleted_at=$3::bigint,members='',updated_at=$3::bigint WHERE group_id=$1::text AND tenant_id=$2::text AND deleted_at IS NULL RETURNING group_id", [groupId, tenantId, at])).length > 0);
  }

  async groupsOf(tenantId: string, principalId: string): Promise<ScimGroup[]> {
    // Membership is a comma list: match it whole, never as a substring of another id.
    const rows = await this.read(`SELECT ${GROUP_COLUMNS} FROM enterprise_scim_groups WHERE tenant_id=$1::text AND deleted_at IS NULL AND (','||members||',') LIKE $2::text`, [tenantId, `%,${principalId},%`]);
    return rows.map(groupFromRow);
  }

  spendAssertion(key: string, expiresAt: number, nowMs: number): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query(`INSERT INTO enterprise_saml_replay(assertion_key,expires_at) VALUES($1::text,$2::bigint)
        ON CONFLICT(assertion_key) DO UPDATE SET expires_at=excluded.expires_at WHERE enterprise_saml_replay.expires_at<=$3::bigint RETURNING assertion_key`, [key, expiresAt, nowMs]);
      return rows.length > 0;
    });
  }

  maintenance(nowMs: number): Promise<void> {
    return this.tx(async (tx) => { await tx.query("DELETE FROM enterprise_saml_replay WHERE expires_at<=$1::bigint", [nowMs]); });
  }
}
