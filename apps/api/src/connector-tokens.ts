/**
 * Connector tokens for the router (docs/implementation-plan.md E48; contract:
 * docs/api/connector-tokens.md). The router's first-party connectors (Gmail, Google Calendar,
 * GitHub, Linear, Slack) act as the CALLING principal's own account; per run the router asks this
 * route for that principal's access token and drops it after the one vendor request.
 *
 *   POST /v1/internal/router/connector-token      [ROUTER_SETTLEMENT_TOKEN]
 *        Authorization: Bearer <router token>; X-Caller-Authorization: Bearer <the caller's key>
 *        { slot, provider_id, run_id? }
 *        → 200 { access_token, token_type: "Bearer", expires_at }   Cache-Control: no-store
 *        → 404 connector_not_connected (no row, a row that does not open, an expired grant)
 *        → 401 / 403 the service token or the caller's key; 400 a malformed body; 503 the store or
 *          the KMS did not answer
 *
 * The principal comes from the caller's key, never from the body, so the router cannot ask for
 * another principal's token. The stored value is the connector's tokenset sealed exactly as
 * `shogun_integrations::token_store::KmsSealer` seals it (`connector_tokens`, one row per
 * principal × slot): a per-row data key wrapped by the KMS under the context
 * `shogun/token/v1/<principal>/<slot>`, the value under ChaCha20-Poly1305 with the associated data
 * `shogun-token-kms/v1|<principal>|<slot>`, the row `[1][u16 wrapped len][wrapped][nonce][ct‖tag]`.
 * `KmsSealer` here is that format; the KMS itself is a `KeyWrapper` the runtime supplies. No
 * runtime ships one yet (the AWS / GCP / Vault clients live in the Rust core, behind its single
 * egress), so a deployment without it answers every request `404 connector_not_connected` and logs
 * `connector_vault_unconfigured`: the same answer an unconnected principal gets, never a token.
 *
 * Refresh: a token expiring within `REFRESH_SKEW_MS` is refreshed through a `ConnectorRefresher`
 * (the OAuth broker's refresh path, #1101 D6) and written back sealed before answering. No
 * refresher is wired in any runtime yet: an expiring token that is still valid is returned as is
 * (`expires_at` tells the router), an expired one is `404` (the user reconnects).
 *
 * What never happens: the token, the tokenset, the body or the caller's key in a log line, a metric,
 * an error message or a stored row other than the sealed one. Logs carry fixed words only. The
 * route is internal and free (no reservation: it is part of a run the router already reserved).
 */
import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";

import type { Context, Hono } from "hono";
import { bodyLimit } from "hono/body-limit";

import { admitPrincipal, admitRefusalResponse, agentError, hasScopes, resolveAgentKey, type AgentKeyLookup, type AgentVars } from "./agent-keys.js";
import type { LogFn } from "./logging.js";
import { tokenMatches } from "./payouts-app.js";
import type { Row, SqlClient } from "./sql.js";

export const CONNECTOR_TOKEN_PATH = "/v1/internal/router/connector-token";

/** The fixed provider → slot table (docs/api/connector-tokens.md). A pair outside it is `400`. */
export const CONNECTOR_SLOT_BY_PROVIDER = Object.freeze({
  gmail: "gmail-tokenset",
  calendar: "gcal-tokenset",
  github: "github-tokenset",
  linear: "linear-tokenset",
  slack: "slack-tokenset",
} as const);
export type ConnectorProviderId = keyof typeof CONNECTOR_SLOT_BY_PROVIDER;
export type ConnectorSlot = (typeof CONNECTOR_SLOT_BY_PROVIDER)[ConnectorProviderId];

/** A token expiring within this window is refreshed before it is handed out. */
export const REFRESH_SKEW_MS = 60_000;
const MAX_BODY_BYTES = 1024;
const RUN_ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
/** What the router accepts as a vendor token (apps/router/src/transport.ts `TOKEN_RE`). */
const TOKEN_RE = /^[\x21-\x7e]{8,4096}$/;
const BODY_FIELDS = new Set(["slot", "provider_id", "run_id"]);

/** The principal rule of the Rust token store (`token_store::valid_principal`). */
export function validPrincipal(principal: string): boolean {
  return /^[a-z0-9][a-z0-9_-]{0,63}$/.test(principal);
}

// --- the KMS envelope (byte-compatible with shogun_integrations::token_store::KmsSealer) ---

/** One stored envelope: opaque bytes and the KMS key version that wrapped its data key. */
export interface SealedRow {
  keyId: string;
  ciphertext: Uint8Array;
}

export interface WrappedDataKey {
  keyVersion: string;
  blob: Uint8Array;
}

/** The KMS did not answer (unreachable, throttled): nothing was opened, retry later (503). */
export class KmsUnavailable extends Error {
  constructor() {
    super("the KMS is unavailable");
  }
}

/** A row that does not open: wrong binding, altered bytes, a wrapped key the KMS refuses. Treated
 * as absent (404) and logged `connector_token_unreadable`. */
export class RowUnreadable extends Error {
  constructor() {
    super("the stored connector credential does not open");
  }
}

/**
 * The customer's KMS key as a wrapper of data keys (the TypeScript side of the Rust `KeyWrapper`).
 * `unwrap` throws `KmsUnavailable` for a transient failure; any other throw means the wrapped key
 * does not open here (a wrong context, a disabled key). Returned key bytes are zeroed by the caller.
 */
export interface KeyWrapper {
  readonly provider: string;
  generate(context: string): Promise<{ key: Uint8Array; wrapped: WrappedDataKey }>;
  unwrap(wrapped: WrappedDataKey, context: string): Promise<Uint8Array>;
}

const ROW_VERSION = 1;
const NONCE_LEN = 12;
const TAG_LEN = 16;

/** The KMS context a credential's data key is bound to (Rust `token_context`). */
export function tokenContext(principal: string, account: string): string {
  return `shogun/token/v1/${principal}/${account}`;
}

function rowAad(principal: string, account: string): Buffer {
  return Buffer.from(`shogun-token-kms/v1|${principal}|${account}`, "utf8");
}

/** `KmsEnvelope` over a `KeyWrapper`: one data key per row, bound at the KMS (context) and in the
 * AEAD (associated data). */
export class KmsSealer {
  constructor(private readonly wrapper: KeyWrapper) {}

  get provider(): string {
    return this.wrapper.provider;
  }

  async seal(principal: string, account: string, value: string): Promise<SealedRow> {
    const { key, wrapped } = await this.wrapper.generate(tokenContext(principal, account));
    try {
      if (key.length !== 32 || wrapped.blob.length > 0xffff) throw new RowUnreadable();
      const nonce = randomBytes(NONCE_LEN);
      const plain = Buffer.from(value, "utf8");
      const cipher = createCipheriv("chacha20-poly1305", key, nonce, { authTagLength: TAG_LEN });
      cipher.setAAD(rowAad(principal, account), { plaintextLength: plain.length });
      const sealed = Buffer.concat([cipher.update(plain), cipher.final(), cipher.getAuthTag()]);
      plain.fill(0);
      const out = Buffer.alloc(3 + wrapped.blob.length + NONCE_LEN + sealed.length);
      out[0] = ROW_VERSION;
      out.writeUInt16BE(wrapped.blob.length, 1);
      Buffer.from(wrapped.blob).copy(out, 3);
      nonce.copy(out, 3 + wrapped.blob.length);
      sealed.copy(out, 3 + wrapped.blob.length + NONCE_LEN);
      return { keyId: wrapped.keyVersion, ciphertext: new Uint8Array(out) };
    } finally {
      key.fill(0);
    }
  }

  /** Open a row, or throw `RowUnreadable` / `KmsUnavailable`. */
  async open(principal: string, account: string, row: SealedRow): Promise<string> {
    const bytes = Buffer.from(row.ciphertext);
    if (bytes.length < 3 || bytes[0] !== ROW_VERSION) throw new RowUnreadable();
    const len = bytes.readUInt16BE(1);
    const rest = bytes.subarray(3);
    if (rest.length <= len + NONCE_LEN + TAG_LEN - 1) throw new RowUnreadable();
    const wrapped: WrappedDataKey = { keyVersion: row.keyId, blob: new Uint8Array(rest.subarray(0, len)) };
    const body = rest.subarray(len);
    let key: Uint8Array;
    try {
      key = await this.wrapper.unwrap(wrapped, tokenContext(principal, account));
    } catch (e) {
      if (e instanceof KmsUnavailable) throw e;
      throw new RowUnreadable();
    }
    try {
      if (key.length !== 32) throw new RowUnreadable();
      const nonce = body.subarray(0, NONCE_LEN);
      const sealed = body.subarray(NONCE_LEN, body.length - TAG_LEN);
      const tag = body.subarray(body.length - TAG_LEN);
      const decipher = createDecipheriv("chacha20-poly1305", key, nonce, { authTagLength: TAG_LEN });
      decipher.setAAD(rowAad(principal, account), { plaintextLength: sealed.length });
      decipher.setAuthTag(tag);
      let plain: Buffer;
      try {
        plain = Buffer.concat([decipher.update(sealed), decipher.final()]);
      } catch {
        throw new RowUnreadable();
      }
      const text = new TextDecoder("utf-8", { fatal: true }).decode(plain);
      plain.fill(0);
      return text;
    } catch (e) {
      if (e instanceof RowUnreadable) throw e;
      throw new RowUnreadable();
    } finally {
      key.fill(0);
    }
  }
}

// --- the rows (connector_tokens, apps/api/sql/connector-tokens.sql) ---

/** The row storage: principal, slot and the sealed row, never a value. Throws on a store error. */
export interface ConnectorRowStore {
  fetch(principal: string, account: string): Promise<SealedRow | undefined>;
  upsert(principal: string, account: string, row: SealedRow, at: number): Promise<void>;
}

export class InMemoryConnectorRows implements ConnectorRowStore {
  readonly rows = new Map<string, SealedRow & { updatedAt: number }>();

  fetch(principal: string, account: string): Promise<SealedRow | undefined> {
    const r = this.rows.get(`${principal}\u0000${account}`);
    return Promise.resolve(r ? { keyId: r.keyId, ciphertext: new Uint8Array(r.ciphertext) } : undefined);
  }

  upsert(principal: string, account: string, row: SealedRow, at: number): Promise<void> {
    this.rows.set(`${principal}\u0000${account}`, { keyId: row.keyId, ciphertext: new Uint8Array(row.ciphertext), updatedAt: at });
    return Promise.resolve();
  }
}

function bytesOf(v: unknown): Uint8Array | undefined {
  if (v instanceof Uint8Array) return new Uint8Array(v);
  return undefined;
}

/** `connector_tokens` over Postgres. Errors carry fixed words only (a driver error can quote
 * parameters). */
export class SqlConnectorRows implements ConnectorRowStore {
  constructor(private readonly db: SqlClient) {}

  async checkSchema(): Promise<void> {
    try {
      await this.db.query("SELECT principal_id,account,key_id,ciphertext,updated_at FROM connector_tokens LIMIT 1");
    } catch {
      throw new Error("connector token table missing or unreadable: connector_tokens (apply sql/connector-tokens.sql)");
    }
  }

  async fetch(principal: string, account: string): Promise<SealedRow | undefined> {
    let rows: Row[];
    try {
      rows = await this.db.query("SELECT key_id,ciphertext FROM connector_tokens WHERE principal_id=$1::text AND account=$2::text", [principal, account]);
    } catch {
      throw new Error("connector token store unavailable");
    }
    const r = rows[0];
    if (!r) return undefined;
    const ciphertext = bytesOf(r.ciphertext);
    // A row the driver did not hand back as bytes cannot be an envelope: treat it as unreadable.
    return { keyId: String(r.key_id), ciphertext: ciphertext ?? new Uint8Array() };
  }

  async upsert(principal: string, account: string, row: SealedRow, at: number): Promise<void> {
    try {
      await this.db.query(`INSERT INTO connector_tokens(principal_id,account,key_id,ciphertext,updated_at) VALUES($1::text,$2::text,$3::text,$4::bytea,$5::bigint)
        ON CONFLICT(principal_id,account) DO UPDATE SET key_id=excluded.key_id,ciphertext=excluded.ciphertext,updated_at=excluded.updated_at`,
      [principal, account, row.keyId, Buffer.from(row.ciphertext), at]);
    } catch {
      throw new Error("connector token store unavailable");
    }
  }
}

// --- the stored tokenset (the JSON of shogun_integrations::token::serialize) ---

export interface StoredTokenset {
  accessToken: string;
  refreshToken: string | null;
  expiresAtMs: number;
  accountLabel: string | null;
}

export function parseTokenset(text: string): StoredTokenset | undefined {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return undefined;
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return undefined;
  const o = raw as Record<string, unknown>;
  if (typeof o.access_token !== "string") return undefined;
  if (o.refresh_token !== undefined && o.refresh_token !== null && typeof o.refresh_token !== "string") return undefined;
  if (typeof o.expires_at_ms !== "number" || !Number.isFinite(o.expires_at_ms)) return undefined;
  return {
    accessToken: o.access_token,
    refreshToken: typeof o.refresh_token === "string" ? o.refresh_token : null,
    expiresAtMs: o.expires_at_ms,
    accountLabel: typeof o.account_label === "string" ? o.account_label : null,
  };
}

export function serializeTokenset(t: StoredTokenset): string {
  return JSON.stringify({ access_token: t.accessToken, refresh_token: t.refreshToken, expires_at_ms: t.expiresAtMs, account_label: t.accountLabel });
}

/** The OAuth broker's refresh path (#1101 D6). `revoked` = the grant is gone (the user must
 * reconnect); `unavailable` = try again later. Never logs the tokens it handles. */
export type RefreshOutcome = { kind: "refreshed"; tokenset: StoredTokenset } | { kind: "revoked" } | { kind: "unavailable" };

export interface ConnectorRefresher {
  refresh(slot: ConnectorSlot, current: StoredTokenset, nowMs: number): Promise<RefreshOutcome>;
}

// --- the route ---

export interface ConnectorTokenDeps {
  /** `ROUTER_SETTLEMENT_TOKEN`. */
  token: string;
  /** How the caller's key resolves (the agent store, plus OAuth access tokens when served). */
  keyLookup: AgentKeyLookup;
  /** The sealed rows. Absent, or `sealer` absent = no vault in this runtime: every answer is 404. */
  rows?: ConnectorRowStore;
  sealer?: KmsSealer;
  refresher?: ConnectorRefresher;
  /** Operational lines (fixed words). */
  log: LogFn;
  now: () => Date;
}

type ParsedRequest = { slot: ConnectorSlot; providerId: ConnectorProviderId };

function parseRequest(raw: unknown): ParsedRequest | undefined {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return undefined;
  const o = raw as Record<string, unknown>;
  if (Object.keys(o).some((k) => !BODY_FIELDS.has(k))) return undefined;
  const { slot, provider_id: providerId, run_id: runId } = o;
  if (typeof providerId !== "string" || !Object.hasOwn(CONNECTOR_SLOT_BY_PROVIDER, providerId)) return undefined;
  const expected = CONNECTOR_SLOT_BY_PROVIDER[providerId as ConnectorProviderId];
  if (slot !== expected) return undefined;
  if (runId !== undefined && (typeof runId !== "string" || !RUN_ID_RE.test(runId))) return undefined;
  return { slot: expected, providerId: providerId as ConnectorProviderId };
}

export function registerConnectorTokenRoute(app: Hono<{ Variables: AgentVars }>, deps: ConnectorTokenDeps): void {
  const notConnected = (c: Context<{ Variables: AgentVars }>, event: NonNullable<AgentVars["relayLogEvent"]>): Response => {
    c.set("relayLogEvent", event);
    return agentError(c, 404, "connector_not_connected", "No account is connected for this connector. Connect it in ShogunAgent, then retry the run.", "authenticate");
  };
  const unavailable = (c: Context<{ Variables: AgentVars }>): Response => {
    c.set("relayLogEvent", "connector_token_unavailable");
    return agentError(c, 503, "connector_token_unavailable", "The connector credential store did not answer. Retry shortly.", "retry_after", { "retry-after": "5" });
  };

  app.post(CONNECTOR_TOKEN_PATH, bodyLimit({ maxSize: MAX_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", "The body is a slot, a provider id and a run id.") }), async (c) => {
    // The service token first, before the body is read.
    if (!tokenMatches(c.req.header("authorization"), deps.token)) {
      c.set("relayLogEvent", "connector_token_rejected");
      return agentError(c, 401, "invalid_token", "This route takes the router's service token.");
    }
    let raw: unknown;
    try {
      raw = await c.req.json();
    } catch {
      raw = undefined;
    }
    const req = parseRequest(raw);
    if (!req) {
      return agentError(c, 400, "invalid_connector_token_request", "The body must be {\"slot\", \"provider_id\", \"run_id\"?} with a slot that belongs to the provider and no other field.", "fix_input");
    }
    const nowMs = deps.now().getTime();
    // The caller's own key, forwarded by the router: verified here, never stored or logged.
    const resolved = await resolveAgentKey(c.req.header("x-caller-authorization"), deps.keyLookup, nowMs);
    if (!resolved.ok) {
      c.set("relayLogEvent", "agent_key_rejected");
      return agentError(c, 401, resolved.code, "The caller's agent key is not valid. Send it as the run's bearer.", "authenticate");
    }
    const principal = resolved.principal;
    c.set("relayLogAgentKey", principal.keyId);
    const admit = await admitPrincipal(deps.keyLookup, principal, nowMs);
    if (admit) return admitRefusalResponse(c, admit);
    if (!hasScopes(principal.scopes, ["run"])) return agentError(c, 403, "insufficient_scope", "This key lacks the run scope.");

    if (!deps.rows || !deps.sealer) return notConnected(c, "connector_vault_unconfigured");
    if (!validPrincipal(principal.principalId)) return notConnected(c, "connector_not_connected");
    const account = req.slot;
    let row: SealedRow | undefined;
    try {
      row = await deps.rows.fetch(principal.principalId, account);
    } catch {
      return unavailable(c);
    }
    if (!row) return notConnected(c, "connector_not_connected");
    let tokenset: StoredTokenset | undefined;
    try {
      tokenset = parseTokenset(await deps.sealer.open(principal.principalId, account, row));
    } catch (e) {
      if (e instanceof KmsUnavailable) return unavailable(c);
      return notConnected(c, "connector_token_unreadable");
    }
    if (!tokenset) return notConnected(c, "connector_token_unreadable");

    let event: NonNullable<AgentVars["relayLogEvent"]> = "connector_token_served";
    if (tokenset.expiresAtMs - nowMs <= REFRESH_SKEW_MS && deps.refresher && tokenset.refreshToken) {
      let outcome: RefreshOutcome;
      try {
        outcome = await deps.refresher.refresh(req.slot, tokenset, nowMs);
      } catch {
        outcome = { kind: "unavailable" };
      }
      if (outcome.kind === "revoked") return notConnected(c, "connector_token_revoked");
      if (outcome.kind === "refreshed") {
        // A refresh response may omit the refresh token: the existing one stays valid.
        const next: StoredTokenset = { ...outcome.tokenset, refreshToken: outcome.tokenset.refreshToken ?? tokenset.refreshToken, accountLabel: outcome.tokenset.accountLabel ?? tokenset.accountLabel };
        try {
          await deps.rows.upsert(principal.principalId, account, await deps.sealer.seal(principal.principalId, account, serializeTokenset(next)), nowMs);
        } catch {
          // The new access token is valid either way; the next run refreshes again.
          deps.log("connector_token_writeback_failed");
        }
        tokenset = next;
        event = "connector_token_refreshed";
      } else if (tokenset.expiresAtMs <= nowMs) {
        return unavailable(c);
      }
    }
    if (tokenset.expiresAtMs <= nowMs) return notConnected(c, "connector_token_expired");
    if (!TOKEN_RE.test(tokenset.accessToken)) return notConnected(c, "connector_token_unreadable");
    c.set("relayLogEvent", event);
    return c.json(
      { access_token: tokenset.accessToken, token_type: "Bearer", expires_at: Math.min(Math.trunc(tokenset.expiresAtMs), Number.MAX_SAFE_INTEGER) },
      200,
      { "cache-control": "no-store", pragma: "no-cache" },
    );
  });
}
