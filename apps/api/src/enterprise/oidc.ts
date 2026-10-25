/**
 * OpenID Connect for tenant admins (E26): authorization code + PKCE (S256) against the customer's
 * IdP. This module is the protocol: discovery, JWKS, the token exchange and ID token validation.
 * The routes are in `app.ts`.
 *
 * ID token validation (OIDC Core §3.1.3.7), all of it:
 * - compact JWS, `alg` one of RS256/RS384/RS512/PS256/PS384/PS512/ES256/ES384 — never `none`,
 *   never an HMAC (the client secret must not become a verification key);
 * - the key comes from the IdP's JWKS by `kid` (one refetch on an unknown `kid`, for rotation);
 *   the signature is verified with WebCrypto (Workers and Node alike);
 * - `iss` equals the discovered issuer exactly; `aud` contains the client id, and when `aud` holds
 *   more than one value `azp` must be the client id;
 * - `exp` in the future and `iat` / `nbf` not in the future, each within the clock skew;
 * - `nonce` equals the one sealed into the flow cookie; `sub` present.
 *
 * Every fetch goes through `fetch-guard.ts`. Nothing here logs a claim, a token or a URL.
 */
import { idpFetch, IdpFetchError, type IdpFetchDeps } from "./fetch-guard.js";

export const DEFAULT_CLOCK_SKEW_SECS = 120;
const DISCOVERY_TTL_MS = 3_600_000;
const JWKS_TTL_MS = 600_000;
const MAX_ID_TOKEN = 16 * 1024;

export type OidcErrorCode =
  | "discovery_failed" | "discovery_issuer_mismatch" | "token_exchange_failed" | "id_token_missing" | "id_token_malformed"
  | "id_token_alg_refused" | "id_token_key_unknown" | "id_token_signature_invalid" | "id_token_issuer_mismatch"
  | "id_token_audience_mismatch" | "id_token_expired" | "id_token_not_yet_valid" | "id_token_nonce_mismatch" | "id_token_subject_missing"
  | "jwks_failed";

export class OidcError extends Error {
  constructor(readonly code: OidcErrorCode) {
    super(code);
    this.name = "OidcError";
  }
}

export interface Discovery {
  issuer: string;
  authorizationEndpoint: string;
  tokenEndpoint: string;
  jwksUri: string;
}

export interface Jwk {
  kty: string;
  kid?: string;
  alg?: string;
  use?: string;
  [k: string]: unknown;
}

/** Per-isolate caches of discovery documents and key sets (by URL). */
export class OidcCache {
  readonly discovery = new Map<string, { value: Discovery; until: number }>();
  readonly jwks = new Map<string, { value: Jwk[]; until: number }>();
}

export const DEFAULT_OIDC_CACHE = new OidcCache();

const str = (v: unknown): string | undefined => (typeof v === "string" && v.length > 0 ? v : undefined);

function parseJson(text: string): Record<string, unknown> | undefined {
  try {
    const v: unknown = JSON.parse(text);
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

/** Fetch and validate `<issuer>/.well-known/openid-configuration`. */
export async function discover(issuer: string, deps: IdpFetchDeps, cache: OidcCache, nowMs: number): Promise<Discovery> {
  const hit = cache.discovery.get(issuer);
  if (hit && hit.until > nowMs) return hit.value;
  const url = `${issuer.replace(/\/+$/, "")}/.well-known/openid-configuration`;
  let res: { status: number; text: string };
  try {
    res = await idpFetch(url, { method: "GET" }, deps);
  } catch {
    throw new OidcError("discovery_failed");
  }
  const doc = res.status === 200 ? parseJson(res.text) : undefined;
  if (!doc) throw new OidcError("discovery_failed");
  if (doc.issuer !== issuer) throw new OidcError("discovery_issuer_mismatch");
  const authorizationEndpoint = str(doc.authorization_endpoint);
  const tokenEndpoint = str(doc.token_endpoint);
  const jwksUri = str(doc.jwks_uri);
  if (!authorizationEndpoint || !tokenEndpoint || !jwksUri) throw new OidcError("discovery_failed");
  for (const u of [authorizationEndpoint, tokenEndpoint, jwksUri]) {
    try {
      const parsed = new URL(u);
      if (parsed.protocol !== "https:") throw new Error("scheme");
    } catch {
      throw new OidcError("discovery_failed");
    }
  }
  const value: Discovery = { issuer, authorizationEndpoint, tokenEndpoint, jwksUri };
  cache.discovery.set(issuer, { value, until: nowMs + DISCOVERY_TTL_MS });
  return value;
}

async function fetchJwks(uri: string, deps: IdpFetchDeps, cache: OidcCache, nowMs: number, force: boolean): Promise<Jwk[]> {
  const hit = cache.jwks.get(uri);
  if (!force && hit && hit.until > nowMs) return hit.value;
  let res: { status: number; text: string };
  try {
    res = await idpFetch(uri, { method: "GET" }, deps);
  } catch {
    throw new OidcError("jwks_failed");
  }
  const doc = res.status === 200 ? parseJson(res.text) : undefined;
  const keys = doc && Array.isArray(doc.keys) ? (doc.keys.filter((k) => typeof k === "object" && k !== null && typeof (k as Jwk).kty === "string") as Jwk[]) : undefined;
  if (!keys) throw new OidcError("jwks_failed");
  cache.jwks.set(uri, { value: keys, until: nowMs + JWKS_TTL_MS });
  return keys;
}

/** The authorization request URL (code flow, PKCE S256). */
export function authorizationUrl(d: Discovery, p: { clientId: string; redirectUri: string; state: string; nonce: string; codeChallenge: string; loginHint?: string }): string {
  const url = new URL(d.authorizationEndpoint);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", p.clientId);
  url.searchParams.set("redirect_uri", p.redirectUri);
  url.searchParams.set("scope", "openid email profile");
  url.searchParams.set("state", p.state);
  url.searchParams.set("nonce", p.nonce);
  url.searchParams.set("code_challenge", p.codeChallenge);
  url.searchParams.set("code_challenge_method", "S256");
  if (p.loginHint) url.searchParams.set("login_hint", p.loginHint);
  return url.toString();
}

/** Exchange the code at the token endpoint; returns the ID token (the access token is dropped). */
export async function exchangeCode(d: Discovery, p: { code: string; redirectUri: string; verifier: string; clientId: string; clientSecret?: string }, deps: IdpFetchDeps): Promise<string> {
  const form = new URLSearchParams({ grant_type: "authorization_code", code: p.code, redirect_uri: p.redirectUri, code_verifier: p.verifier, client_id: p.clientId });
  if (p.clientSecret) form.set("client_secret", p.clientSecret);
  let res: { status: number; text: string };
  try {
    res = await idpFetch(d.tokenEndpoint, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: form.toString() }, deps);
  } catch (e) {
    throw e instanceof IdpFetchError ? new OidcError("token_exchange_failed") : e;
  }
  const doc = res.status === 200 ? parseJson(res.text) : undefined;
  if (!doc) throw new OidcError("token_exchange_failed");
  const idToken = str(doc.id_token);
  if (!idToken) throw new OidcError("id_token_missing");
  return idToken;
}

// --- JWS verification ---

const ALGS: Record<string, { import: RsaHashedImportParams | EcKeyImportParams; verify: AlgorithmIdentifier | RsaPssParams | EcdsaParams; kty: "RSA" | "EC" }> = {
  RS256: { import: { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, verify: { name: "RSASSA-PKCS1-v1_5" }, kty: "RSA" },
  RS384: { import: { name: "RSASSA-PKCS1-v1_5", hash: "SHA-384" }, verify: { name: "RSASSA-PKCS1-v1_5" }, kty: "RSA" },
  RS512: { import: { name: "RSASSA-PKCS1-v1_5", hash: "SHA-512" }, verify: { name: "RSASSA-PKCS1-v1_5" }, kty: "RSA" },
  PS256: { import: { name: "RSA-PSS", hash: "SHA-256" }, verify: { name: "RSA-PSS", saltLength: 32 }, kty: "RSA" },
  PS384: { import: { name: "RSA-PSS", hash: "SHA-384" }, verify: { name: "RSA-PSS", saltLength: 48 }, kty: "RSA" },
  PS512: { import: { name: "RSA-PSS", hash: "SHA-512" }, verify: { name: "RSA-PSS", saltLength: 64 }, kty: "RSA" },
  ES256: { import: { name: "ECDSA", namedCurve: "P-256" }, verify: { name: "ECDSA", hash: "SHA-256" }, kty: "EC" },
  ES384: { import: { name: "ECDSA", namedCurve: "P-384" }, verify: { name: "ECDSA", hash: "SHA-384" }, kty: "EC" },
};

export const ACCEPTED_ID_TOKEN_ALGS = Object.freeze(Object.keys(ALGS));

function decodeSegment(seg: string): Record<string, unknown> {
  const v = parseJson(Buffer.from(seg, "base64url").toString("utf8"));
  if (!v) throw new OidcError("id_token_malformed");
  return v;
}

/** The public members of a JWK only — a stray private member never reaches `importKey`. */
function publicJwk(k: Jwk): JsonWebKey {
  const pick = k.kty === "RSA" ? ["kty", "n", "e"] : ["kty", "crv", "x", "y"];
  return Object.fromEntries(pick.filter((p) => typeof k[p] === "string").map((p) => [p, k[p]])) as JsonWebKey;
}

export interface IdTokenClaims {
  iss: string;
  sub: string;
  aud: string[];
  email?: string;
  emailVerified: boolean;
  exp: number;
}

export interface VerifyIdTokenParams {
  issuer: string;
  clientId: string;
  nonce: string;
  jwksUri: string;
  nowMs: number;
  skewSecs?: number;
}

/** Verify an ID token per OIDC Core §3.1.3.7. Throws `OidcError` with a fixed code. */
export async function verifyIdToken(token: string, p: VerifyIdTokenParams, deps: IdpFetchDeps, cache: OidcCache = DEFAULT_OIDC_CACHE): Promise<IdTokenClaims> {
  if (token.length > MAX_ID_TOKEN) throw new OidcError("id_token_malformed");
  const parts = token.split(".");
  if (parts.length !== 3 || parts.some((s) => !/^[A-Za-z0-9_-]+$/.test(s))) throw new OidcError("id_token_malformed");
  const [h = "", b = "", s = ""] = parts;
  const header = decodeSegment(h);
  const alg = typeof header.alg === "string" ? header.alg : "";
  const spec = ALGS[alg];
  if (!spec) throw new OidcError("id_token_alg_refused");
  const kid = typeof header.kid === "string" ? header.kid : undefined;
  const pickKey = (keys: Jwk[]): Jwk | undefined => {
    const fit = keys.filter((k) => k.kty === spec.kty && (k.use === undefined || k.use === "sig") && (k.alg === undefined || k.alg === alg));
    return kid !== undefined ? fit.find((k) => k.kid === kid) : fit.length === 1 ? fit[0] : undefined;
  };
  let jwk = pickKey(await fetchJwks(p.jwksUri, deps, cache, p.nowMs, false));
  if (!jwk) jwk = pickKey(await fetchJwks(p.jwksUri, deps, cache, p.nowMs, true));
  if (!jwk) throw new OidcError("id_token_key_unknown");
  let ok: boolean;
  try {
    const key = await crypto.subtle.importKey("jwk", publicJwk(jwk), spec.import, false, ["verify"]);
    ok = await crypto.subtle.verify(spec.verify, key, new Uint8Array(Buffer.from(s, "base64url")), new TextEncoder().encode(`${h}.${b}`));
  } catch {
    ok = false;
  }
  if (!ok) throw new OidcError("id_token_signature_invalid");
  const claims = decodeSegment(b);
  const skew = p.skewSecs ?? DEFAULT_CLOCK_SKEW_SECS;
  const now = Math.floor(p.nowMs / 1000);
  if (claims.iss !== p.issuer) throw new OidcError("id_token_issuer_mismatch");
  const aud = typeof claims.aud === "string" ? [claims.aud] : Array.isArray(claims.aud) ? claims.aud.filter((a): a is string => typeof a === "string") : [];
  if (!aud.includes(p.clientId)) throw new OidcError("id_token_audience_mismatch");
  if (aud.length > 1 && claims.azp !== p.clientId) throw new OidcError("id_token_audience_mismatch");
  if (claims.azp !== undefined && claims.azp !== p.clientId) throw new OidcError("id_token_audience_mismatch");
  const exp = typeof claims.exp === "number" ? claims.exp : Number.NaN;
  if (!Number.isFinite(exp) || exp + skew <= now) throw new OidcError("id_token_expired");
  if (typeof claims.iat !== "number" || claims.iat - skew > now) throw new OidcError("id_token_not_yet_valid");
  if (typeof claims.nbf === "number" && claims.nbf - skew > now) throw new OidcError("id_token_not_yet_valid");
  if (typeof claims.nonce !== "string" || claims.nonce !== p.nonce) throw new OidcError("id_token_nonce_mismatch");
  const sub = str(claims.sub);
  if (!sub) throw new OidcError("id_token_subject_missing");
  const email = str(claims.email);
  return { iss: p.issuer, sub, aud, ...(email ? { email: email.toLowerCase() } : {}), emailVerified: claims.email_verified === true || claims.email_verified === "true", exp };
}
