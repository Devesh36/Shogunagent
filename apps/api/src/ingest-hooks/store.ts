/**
 * Ingest hook registrations (E44): which principal a provider delivery belongs to, and what its
 * signature is checked against. One row per hook (`apps/api/sql/ingest-hooks.sql`). No body, no
 * delivery content and no plaintext secret ever lands here: derived secrets are a generation
 * number, provider-issued ones a sealed envelope (`secrets.ts`), a Gmail mailbox an HMAC.
 */
import type { SqlClient, Row } from "../sql.js";
import { HOOK_PROVIDERS, type HookProvider } from "./verify.js";

export type HookStatus = "active" | "awaiting_verification";

export interface HookRecord {
  hookId: string;
  principalId: string;
  tenantId: string;
  /** The agent key that created it (audit). */
  keyId: string;
  provider: HookProvider;
  status: HookStatus;
  /** The derived secret's generation (GitHub, Google Calendar). */
  generation: number;
  /** The sealed provider secret (Linear, Slack) or Notion's verification token. */
  sealedSecret?: string;
  /** The previous secret, accepted until `previousValidUntil` after a rotation. */
  previousGeneration?: number;
  previousSealedSecret?: string;
  previousValidUntil?: number;
  /** Gmail: `HookSecrets.mailboxKey` of the mailbox. */
  routeKey?: string;
  /** SHA-256 of the creating request's `Idempotency-Key` (hex), for a replayed create. */
  createKeyHash?: string;
  createdAt: number;
  rotatedAt?: number;
  lastDeliveryAt?: number;
}

export interface HookStore {
  /** Insert; `false` when the principal already has a hook under `createKeyHash`, or the Gmail
   * route key is taken. */
  insert(r: HookRecord): Promise<"ok" | "route_taken" | "duplicate_create">;
  get(hookId: string): Promise<HookRecord | undefined>;
  byCreateKey(principalId: string, createKeyHash: string): Promise<HookRecord | undefined>;
  byRouteKey(provider: HookProvider, routeKey: string): Promise<HookRecord | undefined>;
  listByPrincipal(principalId: string): Promise<HookRecord[]>;
  /** Replace the secret fields and status of a live hook the principal owns. */
  update(r: HookRecord): Promise<boolean>;
  touch(hookId: string, at: number): Promise<void>;
  remove(hookId: string, principalId: string, at: number): Promise<boolean>;
  checkSchema(): Promise<void>;
}

export class InMemoryHookStore implements HookStore {
  private readonly rows = new Map<string, HookRecord & { deletedAt?: number }>();

  insert(r: HookRecord): Promise<"ok" | "route_taken" | "duplicate_create"> {
    const live = [...this.rows.values()].filter((x) => x.deletedAt === undefined);
    if (r.createKeyHash && live.some((x) => x.principalId === r.principalId && x.createKeyHash === r.createKeyHash)) return Promise.resolve("duplicate_create");
    if (r.routeKey && live.some((x) => x.provider === r.provider && x.routeKey === r.routeKey)) return Promise.resolve("route_taken");
    this.rows.set(r.hookId, { ...r });
    return Promise.resolve("ok");
  }

  get(hookId: string): Promise<HookRecord | undefined> {
    const r = this.rows.get(hookId);
    return Promise.resolve(r && r.deletedAt === undefined ? strip(r) : undefined);
  }

  byCreateKey(principalId: string, createKeyHash: string): Promise<HookRecord | undefined> {
    const r = [...this.rows.values()].find((x) => x.deletedAt === undefined && x.principalId === principalId && x.createKeyHash === createKeyHash);
    return Promise.resolve(r ? strip(r) : undefined);
  }

  byRouteKey(provider: HookProvider, routeKey: string): Promise<HookRecord | undefined> {
    const r = [...this.rows.values()].find((x) => x.deletedAt === undefined && x.provider === provider && x.routeKey === routeKey);
    return Promise.resolve(r ? strip(r) : undefined);
  }

  listByPrincipal(principalId: string): Promise<HookRecord[]> {
    return Promise.resolve([...this.rows.values()].filter((x) => x.deletedAt === undefined && x.principalId === principalId).sort((a, b) => a.createdAt - b.createdAt).map(strip));
  }

  update(r: HookRecord): Promise<boolean> {
    const cur = this.rows.get(r.hookId);
    if (!cur || cur.deletedAt !== undefined || cur.principalId !== r.principalId) return Promise.resolve(false);
    this.rows.set(r.hookId, { ...r });
    return Promise.resolve(true);
  }

  touch(hookId: string, at: number): Promise<void> {
    const cur = this.rows.get(hookId);
    if (cur) cur.lastDeliveryAt = at;
    return Promise.resolve();
  }

  remove(hookId: string, principalId: string, at: number): Promise<boolean> {
    const cur = this.rows.get(hookId);
    if (!cur || cur.deletedAt !== undefined || cur.principalId !== principalId) return Promise.resolve(false);
    // As the SQL store: a deleted hook keeps no envelope and no route.
    const { sealedSecret: _s, previousSealedSecret: _p, routeKey: _r, ...kept } = cur;
    this.rows.set(hookId, { ...kept, deletedAt: at });
    return Promise.resolve(true);
  }

  checkSchema(): Promise<void> {
    return Promise.resolve();
  }
}

function strip(r: HookRecord & { deletedAt?: number }): HookRecord {
  const { deletedAt: _deleted, ...rest } = r;
  return { ...rest };
}

const COLUMNS = "hook_id, principal_id, tenant_id, key_id, provider, status, generation, sealed_secret, previous_generation, previous_sealed_secret, previous_valid_until, route_key, create_key_hash, created_at, rotated_at, last_delivery_at";

const num = (v: unknown): number | undefined => (v === null || v === undefined ? undefined : Number(v));
const str = (v: unknown): string | undefined => (v === null || v === undefined ? undefined : String(v));

function fromRow(r: Row): HookRecord | undefined {
  const provider = String(r.provider);
  if (!(HOOK_PROVIDERS as readonly string[]).includes(provider)) return undefined;
  const status = String(r.status) === "awaiting_verification" ? "awaiting_verification" : "active";
  const out: HookRecord = {
    hookId: String(r.hook_id), principalId: String(r.principal_id), tenantId: String(r.tenant_id), keyId: String(r.key_id),
    provider: provider as HookProvider, status, generation: Number(r.generation), createdAt: Number(r.created_at),
  };
  const opt: Array<[keyof HookRecord, unknown]> = [
    ["sealedSecret", str(r.sealed_secret)], ["previousGeneration", num(r.previous_generation)], ["previousSealedSecret", str(r.previous_sealed_secret)],
    ["previousValidUntil", num(r.previous_valid_until)], ["routeKey", str(r.route_key)], ["createKeyHash", str(r.create_key_hash)],
    ["rotatedAt", num(r.rotated_at)], ["lastDeliveryAt", num(r.last_delivery_at)],
  ];
  for (const [k, v] of opt) if (v !== undefined) (out as unknown as Record<string, unknown>)[k] = v;
  return out;
}

/** The Postgres store (the SQLite double runs the same statements in the unit tests). */
export class SqlHookStore implements HookStore {
  constructor(private readonly db: SqlClient) {}

  async insert(r: HookRecord): Promise<"ok" | "route_taken" | "duplicate_create"> {
    return this.db.transaction(async (tx) => {
      if (r.createKeyHash) {
        const dup = await tx.query("SELECT hook_id FROM ingest_hooks WHERE principal_id=$1::text AND create_key_hash=$2::text AND deleted_at IS NULL", [r.principalId, r.createKeyHash]);
        if (dup.length > 0) return "duplicate_create" as const;
      }
      if (r.routeKey) {
        const taken = await tx.query("SELECT hook_id FROM ingest_hooks WHERE provider=$1::text AND route_key=$2::text AND deleted_at IS NULL FOR UPDATE", [r.provider, r.routeKey]);
        if (taken.length > 0) return "route_taken" as const;
      }
      await tx.query(
        `INSERT INTO ingest_hooks (${COLUMNS}) VALUES ($1::text, $2::text, $3::text, $4::text, $5::text, $6::text, $7::integer, $8::text, $9::integer, $10::text, $11::bigint, $12::text, $13::text, $14::bigint, $15::bigint, $16::bigint)`,
        [r.hookId, r.principalId, r.tenantId, r.keyId, r.provider, r.status, r.generation, r.sealedSecret ?? null, r.previousGeneration ?? null, r.previousSealedSecret ?? null,
          r.previousValidUntil ?? null, r.routeKey ?? null, r.createKeyHash ?? null, r.createdAt, r.rotatedAt ?? null, r.lastDeliveryAt ?? null],
      );
      return "ok" as const;
    });
  }

  async get(hookId: string): Promise<HookRecord | undefined> {
    const rows = await this.db.query(`SELECT ${COLUMNS} FROM ingest_hooks WHERE hook_id=$1::text AND deleted_at IS NULL`, [hookId]);
    return rows[0] ? fromRow(rows[0]) : undefined;
  }

  async byCreateKey(principalId: string, createKeyHash: string): Promise<HookRecord | undefined> {
    const rows = await this.db.query(`SELECT ${COLUMNS} FROM ingest_hooks WHERE principal_id=$1::text AND create_key_hash=$2::text AND deleted_at IS NULL`, [principalId, createKeyHash]);
    return rows[0] ? fromRow(rows[0]) : undefined;
  }

  async byRouteKey(provider: HookProvider, routeKey: string): Promise<HookRecord | undefined> {
    const rows = await this.db.query(`SELECT ${COLUMNS} FROM ingest_hooks WHERE provider=$1::text AND route_key=$2::text AND deleted_at IS NULL`, [provider, routeKey]);
    return rows[0] ? fromRow(rows[0]) : undefined;
  }

  async listByPrincipal(principalId: string): Promise<HookRecord[]> {
    const rows = await this.db.query(`SELECT ${COLUMNS} FROM ingest_hooks WHERE principal_id=$1::text AND deleted_at IS NULL ORDER BY created_at`, [principalId]);
    return rows.map(fromRow).filter((r): r is HookRecord => r !== undefined);
  }

  async update(r: HookRecord): Promise<boolean> {
    const rows = await this.db.query(
      `UPDATE ingest_hooks SET status=$3::text, generation=$4::integer, sealed_secret=$5::text, previous_generation=$6::integer, previous_sealed_secret=$7::text,
         previous_valid_until=$8::bigint, rotated_at=$9::bigint
       WHERE hook_id=$1::text AND principal_id=$2::text AND deleted_at IS NULL RETURNING hook_id`,
      [r.hookId, r.principalId, r.status, r.generation, r.sealedSecret ?? null, r.previousGeneration ?? null, r.previousSealedSecret ?? null, r.previousValidUntil ?? null, r.rotatedAt ?? null],
    );
    return rows.length > 0;
  }

  async touch(hookId: string, at: number): Promise<void> {
    await this.db.query("UPDATE ingest_hooks SET last_delivery_at=$2::bigint WHERE hook_id=$1::text AND deleted_at IS NULL", [hookId, at]);
  }

  async remove(hookId: string, principalId: string, at: number): Promise<boolean> {
    // The secret material goes with the row's life: a deleted hook keeps no envelope.
    const rows = await this.db.query(
      "UPDATE ingest_hooks SET deleted_at=$3::bigint, sealed_secret=NULL, previous_sealed_secret=NULL, route_key=NULL WHERE hook_id=$1::text AND principal_id=$2::text AND deleted_at IS NULL RETURNING hook_id",
      [hookId, principalId, at],
    );
    return rows.length > 0;
  }

  async checkSchema(): Promise<void> {
    await this.db.query("SELECT hook_id, route_key, create_key_hash, last_delivery_at FROM ingest_hooks LIMIT 1");
  }
}
