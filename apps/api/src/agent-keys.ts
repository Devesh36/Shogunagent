/**
 * Agent API keys (docs/implementation-plan.md E3-T1): the second credential of the gateway,
 * next to the Ed25519 licence token (`auth.ts`, which stays untouched — E3-T4).
 *
 *     sa_live_<43 base64url chars>    (32 random bytes)
 *     sa_test_<43 base64url chars>
 *
 * **What is stored.** Never the key. The row holds a short lookup `prefix` (the first
 * `PREFIX_LENGTH` characters of the random part — enough to find the candidate rows, not enough
 * to guess the rest) and a salted PBKDF2-SHA256 hash of the whole key (`hashAgentKey`). On a
 * request the prefix selects the candidates and the hash of the presented key is compared to
 * each in constant time (`verifyAgentKey`). A database leak therefore yields prefixes and hashes
 * of 256-bit random secrets: nothing that logs in.
 *
 * **Why PBKDF2 and not Argon2id (the plan's wording).** The gateway runs on Cloudflare Workers
 * under `nodejs_compat`, where `crypto.subtle` offers PBKDF2 natively and Argon2 only as a WASM
 * bundle that would have to ship inside the Worker. A memory-hard KDF exists to protect
 * *low-entropy* secrets (passwords); these keys carry 256 bits of entropy from the platform CSPRNG,
 * so the KDF is defence in depth against a leaked table, not the thing standing between an
 * attacker and a key. PBKDF2-SHA256 with a per-key 16-byte salt and `DEFAULT_PBKDF2_ITERATIONS`
 * rounds (a Workers-friendly count; the runtime caps PBKDF2 at 100,000) is sufficient for that
 * and keeps the auth path a few milliseconds. The stored string names its algorithm and its
 * iteration count (`pbkdf2-sha256$<iterations>$<salt>$<hash>`), so a later switch to Argon2id
 * verifies old rows and writes new ones without a migration.
 *
 * **Keys never reach a log line.** The log middleware (`logging.ts`) sees no headers; the only
 * key-derived value a handler may put in the log context is the OPAQUE key id, which the
 * logger hashes (`logId`) — never the secret (test/agent-keys.test.ts asserts both).
 */
import { randomBytes, timingSafeEqual } from "node:crypto";
import type { Context, MiddlewareHandler } from "hono";

import type { LogVars } from "./logging.js";

export const AGENT_SCOPES = ["read", "write", "run", "admin"] as const;
export type AgentScope = (typeof AGENT_SCOPES)[number];

export type AgentKeyMode = "live" | "test";

/** Random part: 32 bytes as unpadded base64url = 43 characters. */
export const KEY_RANDOM_BYTES = 32;
const KEY_RANDOM_CHARS = 43;
/** Lookup prefix length (characters of the random part). 10 base64url characters ≈ 60 bits:
 * unique in practice, useless for guessing the remaining 196 bits. */
export const PREFIX_LENGTH = 10;

export const DEFAULT_PBKDF2_ITERATIONS = 20_000;
/** The Workers runtime refuses PBKDF2 above this many iterations. */
export const MAX_PBKDF2_ITERATIONS = 100_000;
const SALT_BYTES = 16;
const HASH_BITS = 256;

const KEY_RE = new RegExp(`^sa_(live|test)_([A-Za-z0-9_-]{${KEY_RANDOM_CHARS}})$`);

/** A parsed key: its mode and lookup prefix. The secret itself is only ever passed on to the
 * hasher; nothing else reads it. */
export interface ParsedAgentKey {
  mode: AgentKeyMode;
  prefix: string;
}

/** Parse a presented key, or `undefined` for anything that is not one. Never throws; a malformed
 * key is the same 401 as a wrong one. */
export function parseAgentKey(key: string): ParsedAgentKey | undefined {
  const m = KEY_RE.exec(key);
  if (!m || !m[1] || !m[2]) return undefined;
  return { mode: m[1] as AgentKeyMode, prefix: m[2].slice(0, PREFIX_LENGTH) };
}

/** Mint a fresh key. Returns the secret (shown to the caller exactly once) and its prefix. */
export function generateAgentKey(mode: AgentKeyMode): { key: string; prefix: string } {
  const random = randomBytes(KEY_RANDOM_BYTES).toString("base64url");
  return { key: `sa_${mode}_${random}`, prefix: random.slice(0, PREFIX_LENGTH) };
}

/** PBKDF2-SHA256 over the whole key string with a fresh salt, as the self-describing stored form. */
export async function hashAgentKey(key: string, iterations: number = DEFAULT_PBKDF2_ITERATIONS): Promise<string> {
  const salt = randomBytes(SALT_BYTES);
  const digest = await pbkdf2(key, salt, iterations);
  return `pbkdf2-sha256$${iterations}$${salt.toString("base64url")}$${Buffer.from(digest).toString("base64url")}`;
}

/** Whether `key` is the key `stored` was derived from. Constant-time over the digest; a stored
 * value in an unknown format verifies nothing (false), never throws. */
export async function verifyAgentKey(key: string, stored: string): Promise<boolean> {
  const [alg, itersText, saltB64, hashB64] = stored.split("$");
  if (alg !== "pbkdf2-sha256" || !itersText || !saltB64 || !hashB64) return false;
  const iterations = /^\d+$/.test(itersText) ? Number(itersText) : Number.NaN;
  if (!Number.isSafeInteger(iterations) || iterations < 1 || iterations > MAX_PBKDF2_ITERATIONS) return false;
  const expected = Buffer.from(hashB64, "base64url");
  if (expected.length !== HASH_BITS / 8) return false;
  const actual = Buffer.from(await pbkdf2(key, Buffer.from(saltB64, "base64url"), iterations));
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

async function pbkdf2(key: string, salt: Uint8Array, iterations: number): Promise<ArrayBuffer> {
  const material = await crypto.subtle.importKey("raw", new TextEncoder().encode(key), "PBKDF2", false, ["deriveBits"]);
  return crypto.subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt, iterations }, material, HASH_BITS);
}

/** One stored key row. `hash` is the self-describing PBKDF2 string; the secret is nowhere. */
export interface AgentKeyRecord {
  keyId: string;
  prefix: string;
  hash: string;
  mode: AgentKeyMode;
  principalId: string;
  tenantId: string;
  scopes: AgentScope[];
  /** ms since the epoch. */
  createdAt: number;
  expiresAt?: number;
  revokedAt?: number;
  /** The referrer recorded at registration (another key id or a developer id). Opaque. */
  ref?: string;
  /** Per-key hard caps in micro-USD, when set on the key (else the defaults, `caps.ts`). */
  capDayMicros?: number;
  capMonthMicros?: number;
}

/** What a verified key attaches to the request context. */
export interface AgentPrincipal {
  keyId: string;
  mode: AgentKeyMode;
  principalId: string;
  tenantId: string;
  scopes: readonly AgentScope[];
  expiresAt?: number;
  ref?: string;
  capDayMicros?: number;
  capMonthMicros?: number;
}

/** The metering subject of an agent principal, in the same string space as a licence's
 * (`auth.ts` `meterSubject`): the two credentials share the gateway's ledgers (E3-T4), and a
 * fixed prefix keeps a principal id from ever colliding with a licence id or `account:<acc>`. */
export function agentMeterSubject(principal: Pick<AgentPrincipal, "principalId">): string {
  return `principal:${principal.principalId}`;
}

/** `admin` implies every scope. */
export function hasScopes(granted: readonly AgentScope[], required: readonly AgentScope[]): boolean {
  if (granted.includes("admin")) return true;
  return required.every((s) => granted.includes(s));
}

/** The lookup half of the store the middleware needs: candidates for a prefix. */
export interface AgentKeyLookup {
  keysByPrefix(prefix: string): Promise<AgentKeyRecord[]>;
}

/** Context vars the agent routes use: the log line's, plus the verified principal. */
export interface AgentVars extends LogVars {
  agentPrincipal?: AgentPrincipal;
}

/** The plan's error envelope (docs/implementation-plan.md §7.2): `error.code`, a plain-English
 * `message` that says what to do next, and `next_action` when there is one. */
export function agentError(
  c: Context,
  status: 400 | 401 | 403 | 404 | 409 | 413 | 429 | 503,
  code: string,
  message: string,
  nextAction?: string,
  headers: Record<string, string> = {},
): Response {
  return c.json({ error: { code, message, next_action: nextAction ?? null } }, status, { "cache-control": "no-store", ...headers });
}

/** Resolve a presented `Authorization: Bearer sa_…` header to its principal, or a reason. The
 * reasons are fixed words: nothing about which candidate failed, nothing from the key. */
export async function resolveAgentKey(
  authorization: string | undefined,
  store: AgentKeyLookup,
  nowMs: number,
): Promise<{ ok: true; principal: AgentPrincipal } | { ok: false; code: "missing_key" | "invalid_key" | "key_revoked" | "key_expired" }> {
  if (!authorization || !/^Bearer\s+/i.test(authorization)) return { ok: false, code: "missing_key" };
  const key = authorization.replace(/^Bearer\s+/i, "").trim();
  const parsed = parseAgentKey(key);
  if (!parsed) return { ok: false, code: "invalid_key" };
  const candidates = await store.keysByPrefix(parsed.prefix);
  for (const record of candidates) {
    if (record.mode !== parsed.mode) continue;
    if (!(await verifyAgentKey(key, record.hash))) continue;
    if (record.revokedAt !== undefined && record.revokedAt <= nowMs) return { ok: false, code: "key_revoked" };
    if (record.expiresAt !== undefined && record.expiresAt <= nowMs) return { ok: false, code: "key_expired" };
    return {
      ok: true,
      principal: {
        keyId: record.keyId,
        mode: record.mode,
        principalId: record.principalId,
        tenantId: record.tenantId,
        scopes: record.scopes,
        ...(record.expiresAt !== undefined ? { expiresAt: record.expiresAt } : {}),
        ...(record.ref !== undefined ? { ref: record.ref } : {}),
        ...(record.capDayMicros !== undefined ? { capDayMicros: record.capDayMicros } : {}),
        ...(record.capMonthMicros !== undefined ? { capMonthMicros: record.capMonthMicros } : {}),
      },
    };
  }
  return { ok: false, code: "invalid_key" };
}

const KEY_FAILURE_MESSAGE: Record<"missing_key" | "invalid_key" | "key_revoked" | "key_expired", string> = {
  missing_key: "Send your agent key as `Authorization: Bearer sa_live_…`. Register one with POST /v1/agents.",
  invalid_key: "The agent key is not recognised. Check it, or register a new one with POST /v1/agents.",
  key_revoked: "This agent key was revoked. Register a new one with POST /v1/agents.",
  key_expired: "This agent key has expired. Register a new one with POST /v1/agents.",
};

/**
 * `requireAgentKey(store, scopes)`: verify the bearer, check its scopes and attach the principal
 * to the context (`agentPrincipal`). 401 for a missing/unknown/revoked/expired key, 403 for a
 * key without the scopes. Responses are returned, not thrown, so the middleware behaves the same
 * whether the app it is mounted on has an error handler or not. `now` is injectable for tests.
 */
export function requireAgentKey(
  store: AgentKeyLookup,
  scopes: readonly AgentScope[],
  now: () => Date = () => new Date(),
): MiddlewareHandler<{ Variables: AgentVars }> {
  return async (c, next) => {
    const resolved = await resolveAgentKey(c.req.header("authorization"), store, now().getTime());
    if (!resolved.ok) {
      c.set("relayLogEvent", "agent_key_rejected");
      return agentError(c, 401, resolved.code, KEY_FAILURE_MESSAGE[resolved.code], "register", { "www-authenticate": 'Bearer realm="shogunagent"' });
    }
    // The opaque key id; the logger hashes it for the line (logging.ts `logId`). Never the secret.
    c.set("relayLogAgentKey", resolved.principal.keyId);
    if (!hasScopes(resolved.principal.scopes, scopes)) {
      return agentError(c, 403, "insufficient_scope", `This key lacks the ${scopes.join(", ")} scope.`);
    }
    c.set("agentPrincipal", resolved.principal);
    await next();
  };
}

/** The principal `requireAgentKey` attached. A route registered without it fails closed. */
export function agentPrincipalOf(c: Context<{ Variables: AgentVars }>): AgentPrincipal | undefined {
  return c.get("agentPrincipal");
}
