/**
 * Client ID Metadata Documents (E3-T2; MCP authorization 2026-07-28, which deprecates Dynamic
 * Client Registration in favour of CIMD). An MCP client's `client_id` is an HTTPS URL; the JSON
 * at that URL is its registration (`redirect_uris`, `client_name`, `logo_uri`). There is no
 * registration endpoint here: a client is whoever serves a valid document at its id.
 *
 * Fetching a URL a stranger chose is a server-side request forgery surface, so the fetch is
 * strict:
 *
 * - the id is an `https:` URL with a path, no userinfo, no fragment, no dot segments, already in
 *   canonical form, at most 2048 characters;
 * - the host is not `localhost`, a single label, `.local` / `.internal` / `.localhost` / `.home.arpa`,
 *   or an IP literal in a private, loopback, link-local, CGNAT, multicast, documentation or
 *   otherwise reserved range (IPv4 and IPv6, including IPv4-mapped / NAT64 / 6to4 forms);
 * - when a resolver is wired (the Node entry: `node:dns`), every address the host resolves to is
 *   checked the same way; on Workers the platform's `fetch` cannot reach private networks;
 * - redirects are not followed (`redirect: "manual"`; a 3xx is a refusal), the body is capped at
 *   `MAX_CIMD_BYTES`, the request times out after `CIMD_TIMEOUT_MS`, and the content type must be
 *   JSON;
 * - the document must name itself (`client_id` equal to the URL), list at least one redirect URI,
 *   each `https:` or `http:` on a loopback host, and must not claim a client secret or an auth
 *   method other than `none` (these are public clients; PKCE is their proof).
 *
 * Valid documents are cached per isolate for their `Cache-Control: max-age`, clamped to
 * [`MIN_CACHE_SECS`, `MAX_CACHE_SECS`]. Nothing about a client is logged.
 */

export const MAX_CIMD_BYTES = 5 * 1024;
export const CIMD_TIMEOUT_MS = 5_000;
export const MAX_CLIENT_ID_LENGTH = 2_048;
export const MIN_CACHE_SECS = 60;
export const MAX_CACHE_SECS = 86_400;
export const DEFAULT_CACHE_SECS = 300;
const MAX_REDIRECT_URIS = 20;
const MAX_CLIENT_NAME = 100;

export interface ClientMetadata {
  clientId: string;
  clientName?: string;
  logoUri?: string;
  redirectUris: string[];
}

/** Why a client id or its document was refused. Fixed codes; the HTML error page names them. */
export type CimdErrorCode =
  | "invalid_client_id" | "client_host_refused" | "client_fetch_failed" | "client_redirected" | "client_document_too_large"
  | "client_document_not_json" | "client_document_invalid" | "client_id_mismatch";

export class CimdError extends Error {
  constructor(readonly code: CimdErrorCode) {
    super(code);
    this.name = "CimdError";
  }
}

export type CimdFetch = (url: string, init: { method: "GET"; headers: Record<string, string>; redirect: "manual"; signal?: AbortSignal }) => Promise<Response>;
/** Resolve a hostname to its addresses (IPv4 / IPv6 text). */
export type ResolveHost = (hostname: string) => Promise<string[]>;

// --- addresses ---

function parseIPv4(s: string): number[] | undefined {
  const parts = s.split(".");
  if (parts.length !== 4) return undefined;
  const out: number[] = [];
  for (const p of parts) {
    if (!/^\d{1,3}$/.test(p)) return undefined;
    const n = Number(p);
    if (n > 255) return undefined;
    out.push(n);
  }
  return out;
}

/** Whether an IPv4 address is anything but a public unicast address. */
export function isPrivateIPv4(s: string): boolean {
  const b = parseIPv4(s);
  if (!b) return true;
  const [a = 0, c = 0, d = 0] = b;
  return a === 0 || a === 10 || a === 127 || a >= 224
    || (a === 100 && c >= 64 && c <= 127) // CGNAT 100.64/10
    || (a === 169 && c === 254) // link-local
    || (a === 172 && c >= 16 && c <= 31)
    || (a === 192 && c === 168)
    || (a === 192 && c === 0 && (d === 0 || d === 2)) // 192.0.0/24 IETF, 192.0.2/24 TEST-NET-1
    || (a === 192 && c === 88 && d === 99) // 6to4 relay anycast
    || (a === 198 && (c === 18 || c === 19)) // benchmarking 198.18/15
    || (a === 198 && c === 51 && d === 100) // TEST-NET-2
    || (a === 203 && c === 0 && d === 113); // TEST-NET-3
}

/** Expand an IPv6 address (no brackets) into eight 16-bit groups, or `undefined`. */
export function expandIPv6(s: string): number[] | undefined {
  let text = s.toLowerCase();
  const zone = text.indexOf("%");
  if (zone >= 0) text = text.slice(0, zone);
  // A trailing dotted IPv4 becomes two groups.
  const v4 = /^(.*:)(\d+\.\d+\.\d+\.\d+)$/.exec(text);
  if (v4) {
    const b = parseIPv4(v4[2] ?? "");
    if (!b) return undefined;
    text = `${v4[1] ?? ""}${(((b[0] ?? 0) << 8) | (b[1] ?? 0)).toString(16)}:${(((b[2] ?? 0) << 8) | (b[3] ?? 0)).toString(16)}`;
  }
  const halves = text.split("::");
  if (halves.length > 2) return undefined;
  const head = halves[0] ? halves[0].split(":") : [];
  const tail = halves.length === 2 && halves[1] ? halves[1].split(":") : [];
  const missing = 8 - head.length - tail.length;
  if (halves.length === 1 ? missing !== 0 : missing < 1) return undefined;
  const groups = [...head, ...Array<string>(halves.length === 2 ? missing : 0).fill("0"), ...tail];
  const out: number[] = [];
  for (const g of groups) {
    if (!/^[0-9a-f]{1,4}$/.test(g)) return undefined;
    out.push(parseInt(g, 16));
  }
  return out.length === 8 ? out : undefined;
}

/** Whether an IPv6 address is anything but a public unicast address. */
export function isPrivateIPv6(s: string): boolean {
  const g = expandIPv6(s);
  if (!g) return true;
  const [g0 = 0, g1 = 0, g2 = 0, g3 = 0, g4 = 0, g5 = 0, g6 = 0, g7 = 0] = g;
  const v4 = `${g6 >> 8}.${g6 & 255}.${g7 >> 8}.${g7 & 255}`;
  if (g0 === 0 && g1 === 0 && g2 === 0 && g3 === 0 && g4 === 0) {
    if (g5 === 0xffff || g5 === 0) return g5 === 0 && g6 === 0 && g7 <= 1 ? true : isPrivateIPv4(v4); // ::, ::1, ::ffff:a.b.c.d, ::a.b.c.d
    return true;
  }
  if (g0 === 0x64 && g1 === 0xff9b) return isPrivateIPv4(v4); // NAT64 64:ff9b::/96
  if (g0 === 0x2002) return isPrivateIPv4(`${g1 >> 8}.${g1 & 255}.${g2 >> 8}.${g2 & 255}`); // 6to4
  if ((g0 & 0xfe00) === 0xfc00) return true; // ULA fc00::/7
  if ((g0 & 0xffc0) === 0xfe80) return true; // link-local fe80::/10
  if ((g0 & 0xff00) === 0xff00) return true; // multicast
  if (g0 === 0x2001 && g1 === 0x0db8) return true; // documentation
  if (g0 === 0x2001 && g1 < 0x0200) return true; // IETF protocol assignments 2001::/23 (Teredo, ORCHID…)
  if (g0 === 0x0100 && g1 === 0 && g2 === 0 && g3 === 0) return true; // discard 100::/64
  return g0 < 0x2000 || g0 > 0x3fff; // outside global unicast 2000::/3
}

export function isPrivateAddress(ip: string): boolean {
  return ip.includes(":") ? isPrivateIPv6(ip) : isPrivateIPv4(ip);
}

const REFUSED_SUFFIXES = [".localhost", ".local", ".internal", ".home.arpa", ".lan", ".intranet", ".corp"];

/** Whether a URL hostname (as `URL.hostname` renders it) may be fetched. */
export function hostAllowed(hostname: string): boolean {
  const h = hostname.toLowerCase().replace(/\.$/, "");
  if (h.startsWith("[") && h.endsWith("]")) return !isPrivateIPv6(h.slice(1, -1));
  if (/^\d+\.\d+\.\d+\.\d+$/.test(h)) return !isPrivateIPv4(h);
  // Other all-numeric forms (`0x7f.1`, `2130706433`) are IPv4 to some parsers; refuse them.
  if (/^(0x[0-9a-f]*|\d+)(\.(0x[0-9a-f]*|\d+)){0,3}$/i.test(h)) return false;
  if (h === "localhost" || !h.includes(".")) return false;
  return !REFUSED_SUFFIXES.some((s) => h.endsWith(s));
}

// --- client ids and redirect URIs ---

/** Validate a presented `client_id`. Returns the URL, or throws `CimdError`. */
export function parseClientId(raw: string | undefined): URL {
  if (!raw || raw.length > MAX_CLIENT_ID_LENGTH) throw new CimdError("invalid_client_id");
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw new CimdError("invalid_client_id");
  }
  if (url.protocol !== "https:" || url.username || url.password || url.hash || raw.includes("#")) throw new CimdError("invalid_client_id");
  if (url.pathname === "/" || url.pathname === "") throw new CimdError("invalid_client_id");
  if (/(^|\/)\.{1,2}(\/|$)/.test(raw.replace(/^https:\/\/[^/]+/, "")) || /%2e/i.test(url.pathname)) throw new CimdError("invalid_client_id");
  if (url.href !== raw) throw new CimdError("invalid_client_id");
  if (!hostAllowed(url.hostname)) throw new CimdError("client_host_refused");
  return url;
}

const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "[::1]"]);

/** A redirect URI a document may list: `https:` anywhere, or `http:` on a loopback host (native
 * apps, RFC 8252). No fragment, no userinfo. */
export function validRedirectUri(raw: unknown): raw is string {
  if (typeof raw !== "string" || raw.length === 0 || raw.length > MAX_CLIENT_ID_LENGTH) return false;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return false;
  }
  if (url.hash || raw.includes("#") || url.username || url.password) return false;
  if (url.protocol === "https:") return true;
  return url.protocol === "http:" && LOOPBACK_HOSTS.has(url.hostname);
}

/** Whether `presented` matches a registered redirect URI: exact string equality, with one
 * exception from RFC 8252 §7.3 — for an `http:` loopback URI the port may differ (a native app
 * listens on whatever port it got), while scheme, host, path and query must still be identical. */
export function redirectUriAllowed(registered: readonly string[], presented: string): boolean {
  if (registered.includes(presented)) return true;
  let p: URL;
  try {
    p = new URL(presented);
  } catch {
    return false;
  }
  if (p.protocol !== "http:" || !LOOPBACK_HOSTS.has(p.hostname) || p.hash || presented.includes("#")) return false;
  return registered.some((r) => {
    try {
      const u = new URL(r);
      return u.protocol === "http:" && u.hostname === p.hostname && u.pathname === p.pathname && u.search === p.search;
    } catch {
      return false;
    }
  });
}

/** Validate a fetched document against the id it was fetched from. */
export function validateClientMetadata(clientId: string, doc: unknown): ClientMetadata {
  if (typeof doc !== "object" || doc === null || Array.isArray(doc)) throw new CimdError("client_document_invalid");
  const d = doc as Record<string, unknown>;
  if (d.client_id !== clientId) throw new CimdError("client_id_mismatch");
  const uris = d.redirect_uris;
  if (!Array.isArray(uris) || uris.length === 0 || uris.length > MAX_REDIRECT_URIS || !uris.every(validRedirectUri)) throw new CimdError("client_document_invalid");
  if (d.client_secret !== undefined || d.client_secret_expires_at !== undefined) throw new CimdError("client_document_invalid");
  if (d.token_endpoint_auth_method !== undefined && d.token_endpoint_auth_method !== "none") throw new CimdError("client_document_invalid");
  if (d.grant_types !== undefined && (!Array.isArray(d.grant_types) || !d.grant_types.includes("authorization_code"))) throw new CimdError("client_document_invalid");
  if (d.response_types !== undefined && (!Array.isArray(d.response_types) || !d.response_types.includes("code"))) throw new CimdError("client_document_invalid");
  const name = typeof d.client_name === "string" ? d.client_name.replace(/[\u0000-\u001f\u007f]/g, "").trim().slice(0, MAX_CLIENT_NAME) : undefined;
  let logo: string | undefined;
  if (typeof d.logo_uri === "string") {
    try {
      const u = new URL(d.logo_uri);
      if (u.protocol === "https:" && !u.username && !u.password && hostAllowed(u.hostname)) logo = u.href;
    } catch {
      logo = undefined;
    }
  }
  return { clientId, redirectUris: [...(uris as string[])], ...(name ? { clientName: name } : {}), ...(logo ? { logoUri: logo } : {}) };
}

// --- fetch and cache ---

export class CimdCache {
  private readonly entries = new Map<string, { meta: ClientMetadata; expiresAt: number }>();
  constructor(private readonly maxEntries = 500) {}

  get(clientId: string, nowMs: number): ClientMetadata | undefined {
    const e = this.entries.get(clientId);
    if (!e) return undefined;
    if (e.expiresAt <= nowMs) {
      this.entries.delete(clientId);
      return undefined;
    }
    return e.meta;
  }

  set(clientId: string, meta: ClientMetadata, ttlSecs: number, nowMs: number): void {
    if (this.entries.size >= this.maxEntries) {
      const oldest = this.entries.keys().next().value;
      if (oldest !== undefined) this.entries.delete(oldest);
    }
    this.entries.set(clientId, { meta, expiresAt: nowMs + ttlSecs * 1000 });
  }
}

/** One cache per isolate unless the caller supplies its own. */
export const DEFAULT_CIMD_CACHE = new CimdCache();

export function cacheTtlSecs(cacheControl: string | null): number {
  if (!cacheControl) return DEFAULT_CACHE_SECS;
  if (/\bno-store\b/i.test(cacheControl)) return MIN_CACHE_SECS;
  const m = /\bmax-age=(\d+)/i.exec(cacheControl);
  const secs = m ? Number(m[1]) : DEFAULT_CACHE_SECS;
  return Math.min(MAX_CACHE_SECS, Math.max(MIN_CACHE_SECS, secs));
}

async function readCapped(res: Response, cap: number): Promise<string> {
  const length = Number(res.headers.get("content-length") ?? "NaN");
  if (Number.isFinite(length) && length > cap) throw new CimdError("client_document_too_large");
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
      throw new CimdError("client_document_too_large");
    }
    chunks.push(value);
  }
  return new TextDecoder("utf-8", { fatal: false }).decode(Buffer.concat(chunks));
}

export interface CimdDeps {
  fetch?: CimdFetch;
  resolveHost?: ResolveHost;
  cache?: CimdCache;
}

/** Fetch, validate and cache the metadata document of `rawClientId`. Throws `CimdError`. */
export async function clientMetadata(rawClientId: string | undefined, deps: CimdDeps, nowMs: number): Promise<ClientMetadata> {
  const url = parseClientId(rawClientId);
  const clientId = url.href;
  const cache = deps.cache ?? DEFAULT_CIMD_CACHE;
  const hit = cache.get(clientId, nowMs);
  if (hit) return hit;
  if (deps.resolveHost) {
    let addresses: string[];
    try {
      addresses = await deps.resolveHost(url.hostname.replace(/^\[|\]$/g, ""));
    } catch {
      throw new CimdError("client_fetch_failed");
    }
    if (addresses.length === 0 || addresses.some(isPrivateAddress)) throw new CimdError("client_host_refused");
  }
  const fetchFn = deps.fetch ?? (globalThis.fetch as unknown as CimdFetch);
  let res: Response;
  try {
    res = await fetchFn(clientId, { method: "GET", headers: { accept: "application/json" }, redirect: "manual", signal: AbortSignal.timeout(CIMD_TIMEOUT_MS) });
  } catch {
    throw new CimdError("client_fetch_failed");
  }
  if (res.status >= 300 && res.status < 400) throw new CimdError("client_redirected");
  if (res.status !== 200) throw new CimdError("client_fetch_failed");
  const type = (res.headers.get("content-type") ?? "").toLowerCase();
  if (!/^application\/([a-z0-9.+-]+\+)?json\b/.test(type)) throw new CimdError("client_document_not_json");
  const text = await readCapped(res, MAX_CIMD_BYTES);
  let doc: unknown;
  try {
    doc = JSON.parse(text);
  } catch {
    throw new CimdError("client_document_not_json");
  }
  const meta = validateClientMetadata(clientId, doc);
  cache.set(clientId, meta, cacheTtlSecs(res.headers.get("cache-control")), nowMs);
  return meta;
}
