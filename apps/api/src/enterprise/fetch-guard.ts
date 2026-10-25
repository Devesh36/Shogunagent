/**
 * Every outbound request to a customer's identity provider (E26: the OIDC discovery document, its
 * JWKS and its token endpoint) goes through `idpFetch`. A tenant's IdP URL is operator-entered but
 * customer-chosen, so it gets the same server-side request forgery guard as a CIMD client id
 * (oauth/cimd.ts, whose host and address checks are reused here):
 *
 * - `https:` only, no userinfo, no fragment, at most 2048 characters;
 * - the host passes `hostAllowed` (no localhost, single labels, `.local`/`.internal`/…, private,
 *   loopback, link-local, CGNAT, metadata or otherwise reserved IP literals);
 * - when a resolver is wired (the Node entry), every resolved address must be public; on Workers
 *   the platform's `fetch` cannot reach private networks;
 * - redirects are not followed (a 3xx is a refusal), the body is capped, the request times out.
 *
 * Errors carry a fixed code and never the URL, a header or a body.
 */
import { hostAllowed, isPrivateAddress, type ResolveHost } from "../oauth/cimd.js";

export const IDP_TIMEOUT_MS = 5_000;
export const IDP_MAX_BYTES = 64 * 1024;
const MAX_URL = 2_048;

export type IdpFetch = (url: string, init: { method: "GET" | "POST"; headers: Record<string, string>; body?: string; redirect: "manual"; signal?: AbortSignal }) => Promise<Response>;

export interface IdpFetchDeps {
  fetch?: IdpFetch;
  resolveHost?: ResolveHost;
}

export type IdpFetchErrorCode = "idp_url_invalid" | "idp_host_refused" | "idp_unreachable" | "idp_redirected" | "idp_response_too_large";

export class IdpFetchError extends Error {
  constructor(readonly code: IdpFetchErrorCode) {
    super(code);
    this.name = "IdpFetchError";
  }
}

/** Validate an IdP URL without fetching it (also used for the SAML SSO URL the browser is sent to). */
export function idpUrl(raw: string | undefined): URL {
  if (!raw || raw.length > MAX_URL) throw new IdpFetchError("idp_url_invalid");
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw new IdpFetchError("idp_url_invalid");
  }
  if (url.protocol !== "https:" || url.username || url.password || url.hash || raw.includes("#")) throw new IdpFetchError("idp_url_invalid");
  if (!hostAllowed(url.hostname)) throw new IdpFetchError("idp_host_refused");
  return url;
}

async function readCapped(res: Response, cap: number): Promise<string> {
  const length = Number(res.headers.get("content-length") ?? "NaN");
  if (Number.isFinite(length) && length > cap) throw new IdpFetchError("idp_response_too_large");
  if (!res.body) return "";
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > cap) {
      await reader.cancel().catch(() => undefined);
      throw new IdpFetchError("idp_response_too_large");
    }
    chunks.push(value);
  }
  return new TextDecoder("utf-8", { fatal: false }).decode(Buffer.concat(chunks));
}

/** Fetch an IdP URL under the guard. Returns the status and the (capped) body text. */
export async function idpFetch(raw: string, init: { method: "GET" | "POST"; headers?: Record<string, string>; body?: string }, deps: IdpFetchDeps, maxBytes = IDP_MAX_BYTES): Promise<{ status: number; text: string; contentType: string }> {
  const url = idpUrl(raw);
  if (deps.resolveHost) {
    let addresses: string[];
    try {
      addresses = await deps.resolveHost(url.hostname.replace(/^\[|\]$/g, ""));
    } catch {
      throw new IdpFetchError("idp_unreachable");
    }
    if (addresses.length === 0 || addresses.some(isPrivateAddress)) throw new IdpFetchError("idp_host_refused");
  }
  const fetchFn = deps.fetch ?? (globalThis.fetch as unknown as IdpFetch);
  let res: Response;
  try {
    res = await fetchFn(url.href, {
      method: init.method, headers: { accept: "application/json", ...(init.headers ?? {}) }, ...(init.body !== undefined ? { body: init.body } : {}),
      redirect: "manual", signal: AbortSignal.timeout(IDP_TIMEOUT_MS),
    });
  } catch {
    throw new IdpFetchError("idp_unreachable");
  }
  if (res.status >= 300 && res.status < 400) throw new IdpFetchError("idp_redirected");
  const text = await readCapped(res, maxBytes);
  return { status: res.status, text, contentType: (res.headers.get("content-type") ?? "").toLowerCase() };
}
