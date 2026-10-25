/**
 * Gmail push notifications arrive as Pub/Sub push requests (E44, docs/api/ingest-sources.md).
 * The push subscription is configured with authentication: Pub/Sub sends
 * `Authorization: Bearer <OIDC ID token>` signed by Google for the operator's push service
 * account. This module verifies that token from the header alone (never the body):
 *
 * - RS256 signature against Google's published keys (`GOOGLE_OIDC_CERTS_URL`, cached by their
 *   `Cache-Control: max-age`, at most `MAX_JWKS_TTL_MS`; a `kid` not in the cache refetches once);
 * - `iss` is Google's, `aud` is the configured audience (`INGEST_HOOKS_GMAIL_AUDIENCE`, the value
 *   set on the push subscription), `email` is the configured push service account and verified;
 * - `exp` is in the future and `iat` is not, within `CLOCK_SKEW_MS`.
 *
 * WebCrypto only, so the Worker and Node run the same code.
 */
export const GOOGLE_OIDC_CERTS_URL = "https://www.googleapis.com/oauth2/v3/certs";
export const GOOGLE_ISSUERS: readonly string[] = Object.freeze(["accounts.google.com", "https://accounts.google.com"]);
export const CLOCK_SKEW_MS = 60_000;
export const MAX_JWKS_TTL_MS = 6 * 3_600_000;
const DEFAULT_JWKS_TTL_MS = 3_600_000;
const MAX_TOKEN_CHARS = 4_096;

export interface RsaJwk {
  kid: string;
  kty: string;
  n: string;
  e: string;
  alg?: string;
}

/** Where Google's signing keys come from (tests pass a fixed set). */
export interface JwksSource {
  keys(forceRefresh: boolean, nowMs: number): Promise<readonly RsaJwk[]>;
}

export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

/** Google's certs over HTTPS, cached per isolate / process. */
export class FetchGoogleJwks implements JwksSource {
  private cached: { keys: readonly RsaJwk[]; until: number; fetchedAt: number } | undefined;

  constructor(private readonly fetchFn: FetchLike = (input, init) => fetch(input, init), private readonly url = GOOGLE_OIDC_CERTS_URL) {}

  async keys(forceRefresh: boolean, nowMs: number): Promise<readonly RsaJwk[]> {
    // A forced refresh (an unknown `kid`) is honoured at most once a minute, so tokens naming
    // made-up keys cannot turn into one fetch to Google each.
    const forced = forceRefresh && (!this.cached || nowMs - this.cached.fetchedAt >= 60_000);
    if (this.cached && !forced && nowMs < this.cached.until) return this.cached.keys;
    const res = await this.fetchFn(this.url, { headers: { accept: "application/json" } });
    if (!res.ok) throw new Error("google_certs_unavailable");
    const json = (await res.json()) as { keys?: unknown };
    const keys = (Array.isArray(json.keys) ? json.keys : []).filter((k): k is RsaJwk =>
      typeof k === "object" && k !== null && typeof (k as RsaJwk).kid === "string" && (k as RsaJwk).kty === "RSA" && typeof (k as RsaJwk).n === "string" && typeof (k as RsaJwk).e === "string");
    const maxAge = /max-age=(\d{1,7})/.exec(res.headers.get("cache-control") ?? "")?.[1];
    const ttl = Math.min(MAX_JWKS_TTL_MS, maxAge !== undefined ? Number(maxAge) * 1000 : DEFAULT_JWKS_TTL_MS);
    this.cached = { keys, until: nowMs + ttl, fetchedAt: nowMs };
    return keys;
  }
}

export interface OidcExpectations {
  audience: string;
  serviceAccount: string;
}

export type OidcVerdict = { ok: true } | { ok: false; reason: "missing" | "malformed" | "unknown_key" | "bad_signature" | "claims" | "keys_unavailable" };

function b64urlBytes(s: string): Uint8Array<ArrayBuffer> | undefined {
  if (!/^[A-Za-z0-9_-]*$/.test(s)) return undefined;
  return new Uint8Array(Buffer.from(s, "base64url"));
}

function b64urlJson(s: string): Record<string, unknown> | undefined {
  const bytes = b64urlBytes(s);
  if (!bytes) return undefined;
  try {
    const v: unknown = JSON.parse(new TextDecoder().decode(bytes));
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

/** Verify the push's `Authorization` header. Never throws. */
export async function verifyGoogleOidc(authorization: string | undefined, expect: OidcExpectations, jwks: JwksSource, nowMs: number): Promise<OidcVerdict> {
  if (!authorization || !/^Bearer\s+/i.test(authorization)) return { ok: false, reason: "missing" };
  const token = authorization.replace(/^Bearer\s+/i, "").trim();
  if (token.length > MAX_TOKEN_CHARS) return { ok: false, reason: "malformed" };
  const parts = token.split(".");
  if (parts.length !== 3) return { ok: false, reason: "malformed" };
  const [h = "", p = "", s = ""] = parts;
  const header = b64urlJson(h);
  const claims = b64urlJson(p);
  const signature = b64urlBytes(s);
  if (!header || !claims || !signature || header.alg !== "RS256" || typeof header.kid !== "string") return { ok: false, reason: "malformed" };
  let keys: readonly RsaJwk[];
  try {
    keys = await jwks.keys(false, nowMs);
    if (!keys.some((k) => k.kid === header.kid)) keys = await jwks.keys(true, nowMs);
  } catch {
    return { ok: false, reason: "keys_unavailable" };
  }
  const jwk = keys.find((k) => k.kid === header.kid);
  if (!jwk) return { ok: false, reason: "unknown_key" };
  let valid = false;
  try {
    const key = await crypto.subtle.importKey("jwk", { kty: "RSA", n: jwk.n, e: jwk.e, alg: "RS256", ext: true }, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
    valid = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, signature, new TextEncoder().encode(`${h}.${p}`));
  } catch {
    valid = false;
  }
  if (!valid) return { ok: false, reason: "bad_signature" };
  const exp = typeof claims.exp === "number" ? claims.exp * 1000 : NaN;
  const iat = typeof claims.iat === "number" ? claims.iat * 1000 : NaN;
  const claimsOk = typeof claims.iss === "string" && GOOGLE_ISSUERS.includes(claims.iss)
    && claims.aud === expect.audience
    && claims.email === expect.serviceAccount
    && claims.email_verified === true
    && Number.isFinite(exp) && exp > nowMs - CLOCK_SKEW_MS
    && Number.isFinite(iat) && iat <= nowMs + CLOCK_SKEW_MS;
  return claimsOk ? { ok: true } : { ok: false, reason: "claims" };
}

let shared: FetchGoogleJwks | undefined;

/** One key cache per process / Worker isolate (the Worker builds its app per request). */
export function sharedGoogleJwks(): FetchGoogleJwks {
  shared ??= new FetchGoogleJwks();
  return shared;
}
