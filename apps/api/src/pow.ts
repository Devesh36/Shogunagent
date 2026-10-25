/**
 * The proof-of-work gate on self-registration (docs/implementation-plan.md E3-T3).
 *
 * `GET /v1/agents/challenge` hands out a stateless challenge — `<expires_at>.<nonce>.<mac>`,
 * where `mac` is HMAC-SHA256 over the first two parts with the server's `AGENT_POW_SECRET` —
 * and the client sends `X-Agent-Pow: <challenge>:<nonce>` such that SHA-256 of
 * `challenge + nonce` has at least `bits` leading zero bits. Verification needs no storage for
 * the challenge itself (the MAC proves the server minted it and when it expires); the store only
 * remembers SPENT challenges until they expire, so one solution registers once.
 */
import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";

export const DEFAULT_POW_BITS = 18;
export const MAX_POW_BITS = 32;
export const CHALLENGE_TTL_MS = 5 * 60_000;
const MAC_HEX = 32;
const MAX_NONCE_LENGTH = 64;

export interface Challenge {
  challenge: string;
  difficulty_bits: number;
  /** ISO instant after which the challenge is refused. */
  expires_at: string;
}

export function mintChallenge(secret: string, bits: number, nowMs: number): Challenge {
  const expires = nowMs + CHALLENGE_TTL_MS;
  const nonce = randomBytes(16).toString("base64url");
  const head = `${expires}.${nonce}`;
  const mac = createHmac("sha256", secret).update(head).digest("hex").slice(0, MAC_HEX);
  return { challenge: `${head}.${mac}`, difficulty_bits: bits, expires_at: new Date(expires).toISOString() };
}

/** Whether the hex digest has at least `bits` leading zero bits. */
export function leadingZeroBits(digestHex: string): number {
  let bits = 0;
  for (const ch of digestHex) {
    const v = Number.parseInt(ch, 16);
    if (Number.isNaN(v)) return bits;
    if (v === 0) { bits += 4; continue; }
    bits += Math.clz32(v) - 28;
    break;
  }
  return bits;
}

export function powDigest(challenge: string, nonce: string): string {
  return createHash("sha256").update(challenge + nonce).digest("hex");
}

export type PowFailure = "pow_missing" | "pow_malformed" | "pow_expired" | "pow_invalid" | "pow_insufficient";

/** Verify an `X-Agent-Pow` header. Returns the challenge (for spending) or a fixed failure word. */
export function verifyPow(header: string | undefined, secret: string, bits: number, nowMs: number): { ok: true; challenge: string; expiresAt: number } | { ok: false; code: PowFailure } {
  if (!header) return { ok: false, code: "pow_missing" };
  const sep = header.lastIndexOf(":");
  if (sep <= 0) return { ok: false, code: "pow_malformed" };
  const challenge = header.slice(0, sep);
  const nonce = header.slice(sep + 1);
  if (nonce.length === 0 || nonce.length > MAX_NONCE_LENGTH) return { ok: false, code: "pow_malformed" };
  const [expiresText, serverNonce, mac, ...rest] = challenge.split(".");
  if (!expiresText || !serverNonce || !mac || rest.length > 0 || !/^\d+$/.test(expiresText) || mac.length !== MAC_HEX) return { ok: false, code: "pow_malformed" };
  const expected = createHmac("sha256", secret).update(`${expiresText}.${serverNonce}`).digest("hex").slice(0, MAC_HEX);
  const a = Buffer.from(mac, "utf8");
  const b = Buffer.from(expected, "utf8");
  if (a.length !== b.length || !timingSafeEqual(a, b)) return { ok: false, code: "pow_invalid" };
  const expiresAt = Number(expiresText);
  if (!Number.isSafeInteger(expiresAt) || expiresAt <= nowMs) return { ok: false, code: "pow_expired" };
  if (leadingZeroBits(powDigest(challenge, nonce)) < bits) return { ok: false, code: "pow_insufficient" };
  return { ok: true, challenge, expiresAt };
}

/** Solve a challenge (clients, tests, the README example). Bounded by `maxIterations`. */
export function solvePow(challenge: string, bits: number, maxIterations = 50_000_000): string | undefined {
  for (let i = 0; i < maxIterations; i += 1) {
    const nonce = i.toString(36);
    if (leadingZeroBits(powDigest(challenge, nonce)) >= bits) return nonce;
  }
  return undefined;
}

/** The stored form of a spent challenge: its hash, never the challenge (which embeds nothing
 * secret, but the table should not double as a list of issued MACs either). */
export function challengeHash(challenge: string): string {
  return createHash("sha256").update(challenge).digest("hex");
}
