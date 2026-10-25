/**
 * The enterprise lane's secrets (E26), all derived from one operator secret, `ENTERPRISE_SECRET`
 * (≥ 32 characters, never stored, never logged):
 *
 * - **Admin session cookie** — `v1.<base64url(JSON claims)>.<base64url(HMAC-SHA256)>`, keyed with
 *   `derive("session")`. Claims are ids and times only; signed, not secret.
 * - **SSO flow cookie** — the OIDC state, nonce and PKCE verifier or the SAML request id, sealed
 *   (AES-256-GCM, WebCrypto) under `derive("sso-flow")`: the verifier is a secret.
 * - **Sealed configuration** — an OIDC client secret at rest, sealed under `derive("config")`.
 *
 * The derivation is HMAC-SHA256(secret, "shogunagent-enterprise/v1/<label>"): a PRF keyed by a
 * high-entropy secret, so each label is an independent key and rotating the operator secret
 * rotates all three (open sessions end; sealed client secrets must be set again).
 */
import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";

const b64url = (b: Uint8Array): string => Buffer.from(b).toString("base64url");
const fromB64url = (s: string): Buffer => Buffer.from(s, "base64url");

export function derive(secret: string, label: string): Buffer {
  return createHmac("sha256", secret).update(`shogunagent-enterprise/v1/${label}`).digest();
}

export function sha256Hex(s: string): string {
  return createHash("sha256").update(s).digest("hex");
}

/** A 32-byte random token with a prefix (`sct_` SCIM bearer). Shown once; stored as `sha256Hex`. */
export function randomToken(prefix: string): string {
  return `${prefix}${b64url(randomBytes(32))}`;
}

export function randomId(bytes = 16): string {
  return b64url(randomBytes(bytes));
}

/** Constant-time string equality (for state values and the like). */
export function safeEqual(a: string, b: string): boolean {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

// --- signed claims (the admin session) ---

export function signClaims(key: Buffer, claims: Record<string, unknown>): string {
  const body = b64url(Buffer.from(JSON.stringify(claims)));
  const mac = b64url(createHmac("sha256", key).update(`v1.${body}`).digest());
  return `v1.${body}.${mac}`;
}

export function verifyClaims(key: Buffer, token: string | undefined): Record<string, unknown> | undefined {
  if (!token || token.length > 2048) return undefined;
  const [v, body, mac] = token.split(".");
  if (v !== "v1" || !body || !mac) return undefined;
  const expected = createHmac("sha256", key).update(`v1.${body}`).digest();
  const given = fromB64url(mac);
  if (given.length !== expected.length || !timingSafeEqual(given, expected)) return undefined;
  try {
    const parsed: unknown = JSON.parse(fromB64url(body).toString("utf8"));
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

// --- sealing (AES-256-GCM via WebCrypto, available in Workers and Node) ---

async function aesKey(raw: Buffer): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", new Uint8Array(raw), "AES-GCM", false, ["encrypt", "decrypt"]);
}

/** Seal `plaintext` under `key`, bound to `label` (additional data). */
export async function seal(key: Buffer, label: string, plaintext: string): Promise<string> {
  const iv = new Uint8Array(randomBytes(12));
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: new TextEncoder().encode(label) }, await aesKey(key), new TextEncoder().encode(plaintext));
  return `s1.${b64url(iv)}.${b64url(new Uint8Array(ct))}`;
}

/** Open a sealed value, or `undefined` when it was altered, sealed under another key or label, or
 * is not a sealed value at all. Never throws. */
export async function unseal(key: Buffer, label: string, sealed: string | undefined): Promise<string | undefined> {
  if (!sealed || sealed.length > 8192) return undefined;
  const [v, ivText, ctText] = sealed.split(".");
  if (v !== "s1" || !ivText || !ctText) return undefined;
  try {
    const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv: new Uint8Array(fromB64url(ivText)), additionalData: new TextEncoder().encode(label) }, await aesKey(key), new Uint8Array(fromB64url(ctText)));
    return new TextDecoder().decode(pt);
  } catch {
    return undefined;
  }
}

/** The three keys of the lane, derived once. */
export interface EnterpriseKeys {
  session: Buffer;
  flow: Buffer;
  config: Buffer;
}

export function enterpriseKeys(secret: string): EnterpriseKeys {
  return { session: derive(secret, "session"), flow: derive(secret, "sso-flow"), config: derive(secret, "config") };
}

export function pkceChallenge(verifier: string): string {
  return createHash("sha256").update(verifier).digest("base64url");
}
