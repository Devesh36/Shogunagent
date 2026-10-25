/**
 * The approval decision token (plan E15), the TypeScript half of
 * `crates/shogun-mcp/src/remote_approvals.rs`: `sat1.<base64url(claims)>.<base64url(mac)>`, the
 * MAC being HMAC-SHA256 over `shogun-approval-v1.<base64url(claims)>` keyed with
 * `SHOGUN_APPROVAL_SIGNING_KEY`, which the sidecar and this gateway share and nothing else holds.
 * The claims are `{a: approval id, p: principal | null, e: expiry ms, n: nonce}`.
 *
 * The gateway verifies a token before it shows a page or posts to Slack (so a forged link costs
 * the sidecar nothing), and the sidecar verifies it again and spends the nonce on the decision:
 * the sidecar is the one that refuses a replay.
 */
import { Buffer } from "node:buffer";
import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";

export const TOKEN_PREFIX = "sat1";
const MAC_DOMAIN = "shogun-approval-v1.";
export const MIN_SIGNING_KEY_BYTES = 32;
export const MAX_TOKEN_CHARS = 1024;
/** The header a token rides in, gateway ↔ sidecar. */
export const TOKEN_HEADER = "x-shogun-approval-token";

export interface ApprovalClaims {
  approvalId: number;
  principal: string | null;
  expiresMs: number;
  nonce: string;
}

export type TokenCheck = { ok: true; claims: ApprovalClaims } | { ok: false; code: "malformed" | "bad_signature" | "expired" };

function mac(key: string, payloadB64: string): Buffer {
  return createHmac("sha256", key).update(`${MAC_DOMAIN}${payloadB64}`).digest();
}

/** Whether `key` is long enough to sign approvals. */
export function validSigningKey(key: string | undefined): key is string {
  return typeof key === "string" && Buffer.byteLength(key.trim(), "utf8") >= MIN_SIGNING_KEY_BYTES;
}

/** Verify `token` at `nowMs`: format, signature (constant time), expiry. */
export function verifyApprovalToken(token: string, key: string, nowMs: number): TokenCheck {
  const raw = token.trim();
  if (raw.length === 0 || raw.length > MAX_TOKEN_CHARS) return { ok: false, code: "malformed" };
  const parts = raw.split(".");
  if (parts.length !== 3 || parts[0] !== TOKEN_PREFIX) return { ok: false, code: "malformed" };
  const [, payloadB64, macB64] = parts as [string, string, string];
  if (!/^[A-Za-z0-9_-]+$/.test(payloadB64) || !/^[A-Za-z0-9_-]+$/.test(macB64)) return { ok: false, code: "malformed" };
  const given = Buffer.from(macB64, "base64url");
  const expected = mac(key.trim(), payloadB64);
  if (given.length !== expected.length || !timingSafeEqual(given, expected)) return { ok: false, code: "bad_signature" };
  let payload: unknown;
  try {
    payload = JSON.parse(Buffer.from(payloadB64, "base64url").toString("utf8"));
  } catch {
    return { ok: false, code: "malformed" };
  }
  if (typeof payload !== "object" || payload === null) return { ok: false, code: "malformed" };
  const p = payload as Record<string, unknown>;
  const a = p.a, e = p.e, n = p.n, pr = p.p;
  if (typeof a !== "number" || !Number.isSafeInteger(a) || a < 1) return { ok: false, code: "malformed" };
  if (typeof e !== "number" || !Number.isSafeInteger(e)) return { ok: false, code: "malformed" };
  if (typeof n !== "string" || n.length === 0 || n.length > 64) return { ok: false, code: "malformed" };
  if (pr !== null && pr !== undefined && typeof pr !== "string") return { ok: false, code: "malformed" };
  if (nowMs >= e) return { ok: false, code: "expired" };
  return { ok: true, claims: { approvalId: a, principal: typeof pr === "string" ? pr : null, expiresMs: e, nonce: n } };
}

/** Mint a token (tests and tools; production tokens are minted by the sidecar). */
export function mintApprovalToken(key: string, approvalId: number, principal: string | null, expiresMs: number, nonce = randomBytes(16).toString("hex")): string {
  const payloadB64 = Buffer.from(JSON.stringify({ a: approvalId, p: principal, e: expiresMs, n: nonce }), "utf8").toString("base64url");
  return `${TOKEN_PREFIX}.${payloadB64}.${mac(key.trim(), payloadB64).toString("base64url")}`;
}
