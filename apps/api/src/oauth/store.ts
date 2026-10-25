/**
 * The OAuth store (E3-T2): authorization codes, token families and tokens, all keyed by the
 * SHA-256 of the secret (`hashSecret`) — a code, an access token or a refresh token is never
 * stored, only its hash. In-memory here (tests, the Node development entry); `SqlOAuthStore` on
 * Postgres (sql/agent-oauth.sql) in production.
 *
 * - A **code** is single-use: `consumeCode` flips `used_at` in one conditional update. A second
 *   use answers `reused`, and the caller revokes the family the first use created (RFC 6749
 *   §4.1.2: a replayed code may mean it leaked).
 * - A **family** is everything minted from one consent: its access and refresh tokens. Revoking a
 *   family revokes all of them at once.
 * - A **refresh token** is rotated on every use: `rotateRefresh` marks it used in one conditional
 *   update; presenting it again answers `reused`, and the caller revokes the family (refresh token
 *   reuse detection, OAuth 2.1 §6.1).
 *
 * Content-free: hashes, ids, scopes, URLs the client published, timestamps.
 */
import { createHash } from "node:crypto";

import type { AgentScope } from "../agent-keys.js";
import { scopesFromText, scopesToText } from "../agent-store.js";
import type { Row, SqlClient, SqlExecutor } from "../sql.js";

export function hashSecret(secret: string): string {
  return createHash("sha256").update(secret).digest("hex");
}

export interface OAuthCode {
  codeHash: string;
  clientId: string;
  redirectUri: string;
  codeChallenge: string;
  keyId: string;
  principalId: string;
  scopes: AgentScope[];
  resource: string;
  createdAt: number;
  expiresAt: number;
  usedAt?: number;
}

export interface OAuthFamily {
  familyId: string;
  clientId: string;
  keyId: string;
  principalId: string;
  codeHash: string;
  createdAt: number;
  revokedAt?: number;
}

export interface OAuthToken {
  tokenHash: string;
  kind: "access" | "refresh";
  familyId: string;
  clientId: string;
  keyId: string;
  principalId: string;
  scopes: AgentScope[];
  resource: string;
  createdAt: number;
  expiresAt: number;
  /** A refresh token's rotation instant. */
  usedAt?: number;
}

export type Consumed<T> = { kind: "ok"; value: T } | { kind: "reused"; value: T } | { kind: "unknown" };

export interface OAuthStore {
  insertCode(code: OAuthCode): Promise<void>;
  consumeCode(codeHash: string, nowMs: number): Promise<Consumed<OAuthCode>>;
  createFamily(f: OAuthFamily): Promise<void>;
  insertToken(t: OAuthToken): Promise<void>;
  /** A token and whether its family is revoked. */
  token(tokenHash: string): Promise<{ token: OAuthToken; familyRevoked: boolean } | undefined>;
  rotateRefresh(tokenHash: string, nowMs: number): Promise<Consumed<OAuthToken>>;
  revokeFamily(familyId: string, nowMs: number): Promise<void>;
  /** Revoke whatever family a code produced (a replayed code). */
  revokeFamilyOfCode(codeHash: string, nowMs: number): Promise<void>;
  /** E26: revoke every family of a principal (SCIM deactivation and deletion). */
  revokePrincipal(principalId: string, nowMs: number): Promise<void>;
  /** Retention: expired codes, and tokens a day past their expiry. */
  maintenance(nowMs: number): Promise<void>;
}

const TOKEN_GRACE_MS = 86_400_000;

export class InMemoryOAuthStore implements OAuthStore {
  private readonly codes = new Map<string, OAuthCode>();
  private readonly families = new Map<string, OAuthFamily>();
  private readonly tokens = new Map<string, OAuthToken>();

  insertCode(code: OAuthCode): Promise<void> {
    this.codes.set(code.codeHash, { ...code, scopes: [...code.scopes] });
    return Promise.resolve();
  }

  consumeCode(codeHash: string, nowMs: number): Promise<Consumed<OAuthCode>> {
    const c = this.codes.get(codeHash);
    if (!c) return Promise.resolve({ kind: "unknown" });
    if (c.usedAt !== undefined) return Promise.resolve({ kind: "reused", value: { ...c } });
    c.usedAt = nowMs;
    return Promise.resolve({ kind: "ok", value: { ...c } });
  }

  createFamily(f: OAuthFamily): Promise<void> {
    if (!this.families.has(f.familyId)) this.families.set(f.familyId, { ...f });
    return Promise.resolve();
  }

  insertToken(t: OAuthToken): Promise<void> {
    this.tokens.set(t.tokenHash, { ...t, scopes: [...t.scopes] });
    return Promise.resolve();
  }

  token(tokenHash: string): Promise<{ token: OAuthToken; familyRevoked: boolean } | undefined> {
    const t = this.tokens.get(tokenHash);
    if (!t) return Promise.resolve(undefined);
    return Promise.resolve({ token: { ...t, scopes: [...t.scopes] }, familyRevoked: this.families.get(t.familyId)?.revokedAt !== undefined });
  }

  rotateRefresh(tokenHash: string, nowMs: number): Promise<Consumed<OAuthToken>> {
    const t = this.tokens.get(tokenHash);
    if (!t || t.kind !== "refresh") return Promise.resolve({ kind: "unknown" });
    if (t.usedAt !== undefined) return Promise.resolve({ kind: "reused", value: { ...t } });
    t.usedAt = nowMs;
    return Promise.resolve({ kind: "ok", value: { ...t } });
  }

  revokeFamily(familyId: string, nowMs: number): Promise<void> {
    const f = this.families.get(familyId);
    if (f && f.revokedAt === undefined) f.revokedAt = nowMs;
    return Promise.resolve();
  }

  revokeFamilyOfCode(codeHash: string, nowMs: number): Promise<void> {
    for (const f of this.families.values()) if (f.codeHash === codeHash && f.revokedAt === undefined) f.revokedAt = nowMs;
    return Promise.resolve();
  }

  revokePrincipal(principalId: string, nowMs: number): Promise<void> {
    for (const f of this.families.values()) if (f.principalId === principalId && f.revokedAt === undefined) f.revokedAt = nowMs;
    return Promise.resolve();
  }

  maintenance(nowMs: number): Promise<void> {
    for (const [k, c] of this.codes) if (c.expiresAt + TOKEN_GRACE_MS <= nowMs) this.codes.delete(k);
    for (const [k, t] of this.tokens) if (t.expiresAt + TOKEN_GRACE_MS <= nowMs) this.tokens.delete(k);
    return Promise.resolve();
  }
}

export const OAUTH_TABLES = ["oauth_codes", "oauth_families", "oauth_tokens"] as const;

const num = (v: unknown): number => Number(v ?? 0);
const opt = (v: unknown): number | undefined => (v === null || v === undefined ? undefined : Number(v));

const CODE_COLUMNS = "code_hash,client_id,redirect_uri,code_challenge,key_id,principal_id,scopes,resource,created_at,expires_at,used_at";
const TOKEN_COLUMNS = "token_hash,kind,family_id,client_id,key_id,principal_id,scopes,resource,created_at,expires_at,used_at";

function codeFromRow(r: Row): OAuthCode {
  return {
    codeHash: String(r.code_hash), clientId: String(r.client_id), redirectUri: String(r.redirect_uri), codeChallenge: String(r.code_challenge),
    keyId: String(r.key_id), principalId: String(r.principal_id), scopes: scopesFromText(String(r.scopes)), resource: String(r.resource),
    createdAt: num(r.created_at), expiresAt: num(r.expires_at), ...(opt(r.used_at) !== undefined ? { usedAt: num(r.used_at) } : {}),
  };
}

function tokenFromRow(r: Row): OAuthToken {
  return {
    tokenHash: String(r.token_hash), kind: r.kind === "refresh" ? "refresh" : "access", familyId: String(r.family_id), clientId: String(r.client_id),
    keyId: String(r.key_id), principalId: String(r.principal_id), scopes: scopesFromText(String(r.scopes)), resource: String(r.resource),
    createdAt: num(r.created_at), expiresAt: num(r.expires_at), ...(opt(r.used_at) !== undefined ? { usedAt: num(r.used_at) } : {}),
  };
}

/** Thrown with a fixed message; driver errors can quote parameters and never pass through. */
export class OAuthStoreUnavailable extends Error {}

export class SqlOAuthStore implements OAuthStore {
  constructor(private readonly db: SqlClient) {}

  async checkSchema(): Promise<void> {
    for (const table of OAUTH_TABLES) {
      try {
        await this.db.query(`SELECT 1 FROM ${table} LIMIT 1`);
      } catch {
        throw new OAuthStoreUnavailable(`oauth store table missing or unreadable: ${table}`);
      }
    }
  }

  private async tx<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    try {
      return await this.db.transaction(fn);
    } catch {
      throw new OAuthStoreUnavailable("oauth store unavailable");
    }
  }

  insertCode(c: OAuthCode): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO oauth_codes(${CODE_COLUMNS}) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::text,$9::bigint,$10::bigint,NULL)`,
        [c.codeHash, c.clientId, c.redirectUri, c.codeChallenge, c.keyId, c.principalId, scopesToText(c.scopes), c.resource, c.createdAt, c.expiresAt]);
    });
  }

  consumeCode(codeHash: string, nowMs: number): Promise<Consumed<OAuthCode>> {
    return this.tx(async (tx) => {
      const flipped = await tx.query(`UPDATE oauth_codes SET used_at=$2::bigint WHERE code_hash=$1::text AND used_at IS NULL RETURNING ${CODE_COLUMNS}`, [codeHash, nowMs]);
      if (flipped[0]) return { kind: "ok", value: codeFromRow(flipped[0]) } as const;
      const r = (await tx.query(`SELECT ${CODE_COLUMNS} FROM oauth_codes WHERE code_hash=$1::text`, [codeHash]))[0];
      return r ? ({ kind: "reused", value: codeFromRow(r) } as const) : ({ kind: "unknown" } as const);
    });
  }

  createFamily(f: OAuthFamily): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO oauth_families(family_id,client_id,key_id,principal_id,code_hash,created_at,revoked_at) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::bigint,NULL)
        ON CONFLICT(family_id) DO NOTHING`, [f.familyId, f.clientId, f.keyId, f.principalId, f.codeHash, f.createdAt]);
    });
  }

  insertToken(t: OAuthToken): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO oauth_tokens(${TOKEN_COLUMNS}) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::text,$9::bigint,$10::bigint,NULL)`,
        [t.tokenHash, t.kind, t.familyId, t.clientId, t.keyId, t.principalId, scopesToText(t.scopes), t.resource, t.createdAt, t.expiresAt]);
    });
  }

  async token(tokenHash: string): Promise<{ token: OAuthToken; familyRevoked: boolean } | undefined> {
    let rows: Row[];
    try {
      rows = await this.db.query(`SELECT t.token_hash,t.kind,t.family_id,t.client_id,t.key_id,t.principal_id,t.scopes,t.resource,t.created_at,t.expires_at,t.used_at,f.revoked_at AS family_revoked
        FROM oauth_tokens t JOIN oauth_families f ON f.family_id=t.family_id WHERE t.token_hash=$1::text`, [tokenHash]);
    } catch {
      throw new OAuthStoreUnavailable("oauth store unavailable");
    }
    const r = rows[0];
    return r ? { token: tokenFromRow(r), familyRevoked: opt(r.family_revoked) !== undefined } : undefined;
  }

  rotateRefresh(tokenHash: string, nowMs: number): Promise<Consumed<OAuthToken>> {
    return this.tx(async (tx) => {
      const flipped = await tx.query(`UPDATE oauth_tokens SET used_at=$2::bigint WHERE token_hash=$1::text AND kind='refresh' AND used_at IS NULL RETURNING ${TOKEN_COLUMNS}`, [tokenHash, nowMs]);
      if (flipped[0]) return { kind: "ok", value: tokenFromRow(flipped[0]) } as const;
      const r = (await tx.query(`SELECT ${TOKEN_COLUMNS} FROM oauth_tokens WHERE token_hash=$1::text AND kind='refresh'`, [tokenHash]))[0];
      return r ? ({ kind: "reused", value: tokenFromRow(r) } as const) : ({ kind: "unknown" } as const);
    });
  }

  revokeFamily(familyId: string, nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE oauth_families SET revoked_at=$2::bigint WHERE family_id=$1::text AND revoked_at IS NULL", [familyId, nowMs]);
    });
  }

  revokeFamilyOfCode(codeHash: string, nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE oauth_families SET revoked_at=$2::bigint WHERE code_hash=$1::text AND revoked_at IS NULL", [codeHash, nowMs]);
    });
  }

  revokePrincipal(principalId: string, nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE oauth_families SET revoked_at=$2::bigint WHERE principal_id=$1::text AND revoked_at IS NULL", [principalId, nowMs]);
    });
  }

  maintenance(nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("DELETE FROM oauth_codes WHERE expires_at<=$1::bigint", [nowMs - TOKEN_GRACE_MS]);
      await tx.query("DELETE FROM oauth_tokens WHERE expires_at<=$1::bigint", [nowMs - TOKEN_GRACE_MS]);
    });
  }
}
