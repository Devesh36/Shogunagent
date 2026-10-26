/**
 * OAuth 2.1 for MCP clients with Client ID Metadata Documents (docs/implementation-plan.md E3-T2;
 * docs/api/oauth.md). The gateway is both the protected resource (`/mcp` and the REST routes) and
 * its authorization server. There is no Dynamic Client Registration endpoint.
 *
 *   GET  /.well-known/oauth-protected-resource[/mcp]   RFC 9728 Protected Resource Metadata
 *   GET  /.well-known/oauth-authorization-server       RFC 8414 Authorization Server Metadata
 *   GET  /oauth/authorize                               validate the request, render the consent page
 *   POST /oauth/authorize                               the human's decision → 302 with a code
 *   POST /oauth/token                                   authorization_code (PKCE S256) | refresh_token
 *   POST /oauth/revoke                                  RFC 7009: revoke a token's whole family
 *
 * **Who consents.** A principal exists because an agent registered it (POST /v1/agents); its
 * credential is an `sa_` key. The consent page asks the human for that key, verifies it, and binds
 * the code to the key's principal with the requested scopes intersected with the key's (`admin`
 * is never grantable). Access tokens are then the key's delegate: same principal, same caps (they
 * book on the key's counters), at most its scopes, dead when the key is revoked or expires. Account
 * sign-in (passkeys) replaces the key field when accounts exist.
 *
 * **Tokens.** Access tokens `sat_…` live `ACCESS_TOKEN_TTL_SECS`; refresh tokens `sar_…` live
 * `REFRESH_TOKEN_TTL_SECS` and rotate on every use. Codes live `CODE_TTL_SECS`, are single-use and
 * PKCE-bound (S256 only). Everything is stored as a SHA-256 hash (oauth/store.ts). A replayed code
 * or a reused refresh token revokes the whole family.
 *
 * Nothing here logs a client id, a redirect URI, a code, a token or a key: the log line carries a
 * fixed event word (logging.ts).
 */
import { createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import { Hono, type Context } from "hono";
import { bodyLimit } from "hono/body-limit";

import { resolveAgentKey, type AgentKeyLookup, type AgentPrincipal, type AgentScope, type AgentVars } from "../agent-keys.js";
import type { AgentStore } from "../agent-store.js";
import { DISPLAY_NAME, DOCS_URL, mcpUrl, origin, type WellknownContext } from "../wellknown/identity.js";
import { CimdError, clientMetadata, redirectUriAllowed, type CimdCache, type CimdFetch, type ClientMetadata, type ResolveHost } from "./cimd.js";
import { hashSecret, type OAuthStore, type OAuthToken } from "./store.js";

export const ACCESS_TOKEN_TTL_SECS = 900;
export const REFRESH_TOKEN_TTL_SECS = 30 * 86_400;
export const CODE_TTL_SECS = 60;
export const CONSENT_TTL_SECS = 600;
/** Scopes an OAuth client may ask for. `admin` is never delegated. */
export const OAUTH_SCOPES: readonly AgentScope[] = Object.freeze(["read", "write", "run"]);
const VERIFIER_RE = /^[A-Za-z0-9._~-]{43,128}$/;
const CHALLENGE_RE = /^[A-Za-z0-9_-]{43}$/;
const MAX_STATE = 512;
const MAX_FORM_BYTES = 16 * 1024;

export interface OAuthDeps {
  store: OAuthStore;
  /** HMAC key of the consent form token (`AGENT_OAUTH_SECRET`, ≥ 32 characters). */
  secret: string;
  fetch?: CimdFetch;
  resolveHost?: ResolveHost;
  cache?: CimdCache;
}

export interface OAuthAppDeps extends OAuthDeps, WellknownContext {
  agentStore: AgentStore;
  now: () => Date;
  /** E26: whether a principal may consent by pasting a key. `sso_required` (an SSO-enforced
   * enterprise tenant) refuses it. Absent = always allowed. */
  consentPolicy?: (principalId: string) => Promise<"allow" | "sso_required">;
}

const b64url = (b: Buffer): string => b.toString("base64url");
const randomToken = (prefix: string): string => `${prefix}${b64url(randomBytes(32))}`;

export function s256(verifier: string): string {
  return b64url(Buffer.from(hashSecret(verifier), "hex"));
}

/** Parse a space-separated scope parameter: default `read`; `undefined` for anything unknown. */
export function parseScopes(raw: string | undefined): AgentScope[] | undefined {
  const words = (raw ?? "").split(/\s+/).filter((w) => w.length > 0);
  if (words.length === 0) return ["read"];
  const out: AgentScope[] = [];
  for (const w of words) {
    if (!(OAUTH_SCOPES as readonly string[]).includes(w)) return undefined;
    if (!out.includes(w as AgentScope)) out.push(w as AgentScope);
  }
  return out;
}

/** The scopes a grant may carry: requested ∩ the key's, `admin` expanded and never passed on. */
export function grantableScopes(requested: readonly AgentScope[], keyScopes: readonly AgentScope[]): AgentScope[] {
  const has = (s: AgentScope): boolean => keyScopes.includes("admin") || keyScopes.includes(s);
  return requested.filter((s) => s !== "admin" && has(s));
}

/** The AgentKeyLookup that also accepts OAuth access tokens: the agent store for `sa_` keys, the
 * OAuth store for `sat_` tokens, resolved to the consenting key's principal. */
export function oauthKeyLookup(agentStore: AgentStore, oauthStore: OAuthStore): AgentKeyLookup {
  return {
    keysByPrefix: (prefix) => agentStore.keysByPrefix(prefix),
    async accessToken(token, nowMs) {
      const found = await oauthStore.token(hashSecret(token));
      if (!found || found.token.kind !== "access") return { ok: false, code: "invalid_key" };
      if (found.familyRevoked) return { ok: false, code: "key_revoked" };
      if (found.token.expiresAt <= nowMs) return { ok: false, code: "key_expired" };
      const key = await agentStore.keyById(found.token.keyId);
      if (!key || key.principalId !== found.token.principalId) return { ok: false, code: "invalid_key" };
      if (key.revokedAt !== undefined && key.revokedAt <= nowMs) return { ok: false, code: "key_revoked" };
      if (key.expiresAt !== undefined && key.expiresAt <= nowMs) return { ok: false, code: "key_expired" };
      const principal: AgentPrincipal = {
        keyId: key.keyId, mode: key.mode, principalId: key.principalId, tenantId: key.tenantId,
        scopes: grantableScopes(found.token.scopes, key.scopes),
        ...(key.expiresAt !== undefined ? { expiresAt: Math.min(key.expiresAt, found.token.expiresAt) } : { expiresAt: found.token.expiresAt }),
        ...(key.ref !== undefined ? { ref: key.ref } : {}),
        ...(key.capDayMicros !== undefined ? { capDayMicros: key.capDayMicros } : {}),
        ...(key.capMonthMicros !== undefined ? { capMonthMicros: key.capMonthMicros } : {}),
        via: "oauth", clientId: found.token.clientId,
      };
      return { ok: true, principal };
    },
  };
}

// --- metadata documents ---

export function protectedResourceMetadata(ctx: WellknownContext): Record<string, unknown> {
  return {
    resource: mcpUrl(ctx),
    authorization_servers: [origin(ctx)],
    scopes_supported: [...OAUTH_SCOPES],
    bearer_methods_supported: ["header"],
    resource_name: DISPLAY_NAME,
    resource_documentation: DOCS_URL,
  };
}

export function authorizationServerMetadata(ctx: WellknownContext): Record<string, unknown> {
  const o = origin(ctx);
  return {
    issuer: o,
    authorization_endpoint: `${o}/oauth/authorize`,
    token_endpoint: `${o}/oauth/token`,
    revocation_endpoint: `${o}/oauth/revoke`,
    scopes_supported: [...OAUTH_SCOPES],
    response_types_supported: ["code"],
    response_modes_supported: ["query"],
    grant_types_supported: ["authorization_code", "refresh_token"],
    code_challenge_methods_supported: ["S256"],
    token_endpoint_auth_methods_supported: ["none"],
    revocation_endpoint_auth_methods_supported: ["none"],
    client_id_metadata_document_supported: true,
    authorization_response_iss_parameter_supported: true,
    service_documentation: DOCS_URL,
  };
}

// --- consent token (binds the form to the validated request) ---

interface AuthorizeParams {
  clientId: string;
  redirectUri: string;
  codeChallenge: string;
  scopes: AgentScope[];
  state?: string;
  resource: string;
}

function consentPayload(p: AuthorizeParams, exp: number): string {
  return JSON.stringify(["v1", p.clientId, p.redirectUri, p.codeChallenge, p.scopes.join(" "), p.state ?? null, p.resource, exp]);
}

export function signConsent(secret: string, p: AuthorizeParams, nowMs: number): string {
  const exp = Math.floor(nowMs / 1000) + CONSENT_TTL_SECS;
  return `${exp}.${b64url(createHmac("sha256", secret).update(consentPayload(p, exp)).digest())}`;
}

export function verifyConsent(secret: string, token: string | undefined, p: AuthorizeParams, nowMs: number): boolean {
  if (!token) return false;
  const [expText, mac] = token.split(".", 2);
  if (!expText || !mac || !/^\d+$/.test(expText)) return false;
  const exp = Number(expText);
  if (exp < Math.floor(nowMs / 1000)) return false;
  const expected = createHmac("sha256", secret).update(consentPayload(p, exp)).digest();
  const given = Buffer.from(mac, "base64url");
  return given.length === expected.length && timingSafeEqual(given, expected);
}

// --- HTML ---

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch] ?? ch);
}

const SCOPE_TEXT: Record<AgentScope, string> = {
  read: "Read your state: commitments, open loops, people, projects and memory (free).",
  write: "Write to your memory: ingest events and notes (metered from your wallet).",
  run: "Run tools through the router on your behalf (metered; L3 actions still wait for your approval).",
  admin: "",
  ingest: "",
};

function page(title: string, body: string, formAction: string[] = []): { html: string; csp: string } {
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="referrer" content="no-referrer">
<title>${escapeHtml(title)}</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:34rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}
@media (prefers-color-scheme:dark){body{color:#eee;background:#111}input{background:#222;color:#eee}}
h1{font-size:1.3rem}code{word-break:break-all}.muted{opacity:.75;font-size:.9rem}img{width:48px;height:48px;border-radius:8px}
input[type=password]{width:100%;padding:.5rem;font:inherit;box-sizing:border-box}button{font:inherit;padding:.5rem 1rem;margin:.5rem .5rem 0 0}
.err{color:#b00020}</style></head><body>${body}</body></html>`;
  const csp = `default-src 'none'; img-src https:; style-src 'unsafe-inline'; form-action 'self'${formAction.map((o) => ` ${o}`).join("")}; frame-ancestors 'none'; base-uri 'none'`;
  return { html, csp };
}

function htmlResponse(c: Context, status: 200 | 400 | 401 | 403, rendered: { html: string; csp: string }): Response {
  return c.html(rendered.html, status, {
    "cache-control": "no-store", "content-security-policy": rendered.csp, "x-frame-options": "DENY", "referrer-policy": "no-referrer", "x-content-type-options": "nosniff",
  });
}

const ERROR_TEXT: Record<string, string> = {
  invalid_client_id: "The client_id must be an https URL of the client's metadata document.",
  client_host_refused: "The client's metadata document is on a host this server will not contact.",
  client_fetch_failed: "The client's metadata document could not be fetched.",
  client_redirected: "The client's metadata document URL redirects; it must answer directly.",
  client_document_too_large: "The client's metadata document is too large.",
  client_document_not_json: "The client's metadata document is not JSON.",
  client_document_invalid: "The client's metadata document is not valid.",
  client_id_mismatch: "The client's metadata document does not name the same client_id.",
  invalid_redirect_uri: "redirect_uri is missing or is not one the client's metadata document lists.",
  invalid_consent: "This form expired or was altered. Start the connection again from your app.",
};

function errorPage(c: Context, code: string, status: 400 | 401 = 400): Response {
  return htmlResponse(c, status, page("Cannot connect", `<h1>This connection cannot continue</h1><p>${escapeHtml(ERROR_TEXT[code] ?? "The request is not valid.")}</p><p class="muted">Error: <code>${escapeHtml(code)}</code>. Nothing was granted.</p>`));
}

function consentPage(c: Context, meta: ClientMetadata, p: AuthorizeParams, consent: string, status: 200 | 401 | 403, error?: string): Response {
  const name = meta.clientName ?? new URL(meta.clientId).hostname;
  const redirectOrigin = new URL(p.redirectUri).origin;
  const hidden = (k: string, v: string): string => `<input type="hidden" name="${k}" value="${escapeHtml(v)}">`;
  const scopes = p.scopes.map((s) => `<li><b>${s}</b> — ${escapeHtml(SCOPE_TEXT[s])}</li>`).join("");
  const body = `${meta.logoUri ? `<img src="${escapeHtml(meta.logoUri)}" alt="" referrerpolicy="no-referrer">` : ""}
<h1>Allow ${escapeHtml(name)} to use ${DISPLAY_NAME}?</h1>
<p class="muted">Client: <code>${escapeHtml(meta.clientId)}</code><br>Returns to: <code>${escapeHtml(redirectOrigin)}</code></p>
<p>It asks to:</p><ul>${scopes}</ul>
${error ? `<p class="err">${escapeHtml(error)}</p>` : ""}
<form method="post" action="/oauth/authorize">
${hidden("client_id", p.clientId)}${hidden("redirect_uri", p.redirectUri)}${hidden("code_challenge", p.codeChallenge)}${hidden("code_challenge_method", "S256")}
${hidden("response_type", "code")}${hidden("scope", p.scopes.join(" "))}${p.state !== undefined ? hidden("state", p.state) : ""}${hidden("resource", p.resource)}${hidden("consent", consent)}
<label for="key">Your ${DISPLAY_NAME} agent key (<code>sa_live_…</code>). It proves which account to connect; it is checked once and not shared with the app.</label>
<input id="key" name="api_key" type="password" autocomplete="off" spellcheck="false" required>
<button type="submit" name="decision" value="approve">Allow</button><button type="submit" name="decision" value="deny" formnovalidate>Deny</button>
</form><p class="muted">The app gets a token that expires in ${ACCESS_TOKEN_TTL_SECS / 60} minutes and can be refreshed. Revoke it at any time by revoking the key.</p>`;
  const rendered = page(`Connect ${name}`, body, [redirectOrigin]);
  return htmlResponse(c, status, rendered);
}

function redirectWith(c: Context, redirectUri: string, params: Record<string, string | undefined>): Response {
  const url = new URL(redirectUri);
  for (const [k, v] of Object.entries(params)) if (v !== undefined) url.searchParams.set(k, v);
  return c.body(null, 302, { location: url.toString(), "cache-control": "no-store", "referrer-policy": "no-referrer" });
}

function tokenError(c: Context, error: string, description: string, status: 400 | 401 = 400): Response {
  return c.json({ error, error_description: description }, status, { "cache-control": "no-store", pragma: "no-cache" });
}

// --- the app ---

export function createOAuthApp(deps: OAuthAppDeps): Hono<{ Variables: AgentVars }> {
  const app = new Hono<{ Variables: AgentVars }>();
  const ctx: WellknownContext = { publicBaseUrl: deps.publicBaseUrl, ...(deps.mcpUrl ? { mcpUrl: deps.mcpUrl } : {}) };
  const issuer = origin(ctx);
  const resourceUri = mcpUrl(ctx);
  const cimd = { ...(deps.fetch ? { fetch: deps.fetch } : {}), ...(deps.resolveHost ? { resolveHost: deps.resolveHost } : {}), ...(deps.cache ? { cache: deps.cache } : {}) };
  const nowMs = (): number => deps.now().getTime();
  const formLimit = bodyLimit({ maxSize: MAX_FORM_BYTES, onError: (c) => c.json({ error: "invalid_request", error_description: "The body is too large." }, 413) });

  const prm = (c: Context): Response => c.json(protectedResourceMetadata(ctx), 200, { "cache-control": "public, max-age=3600" });
  app.get("/.well-known/oauth-protected-resource", prm);
  app.get("/.well-known/oauth-protected-resource/mcp", prm);
  app.get("/.well-known/oauth-authorization-server", (c) => c.json(authorizationServerMetadata(ctx), 200, { "cache-control": "public, max-age=3600" }));

  /** Validate the parts that decide whether we may redirect at all: the client and its redirect
   * URI. Until both hold, every failure is an HTML page, never a redirect. */
  async function clientAndRedirect(c: Context<{ Variables: AgentVars }>, clientId: string | undefined, redirectUri: string | undefined): Promise<{ meta: ClientMetadata; redirectUri: string } | Response> {
    let meta: ClientMetadata;
    try {
      meta = await clientMetadata(clientId, cimd, nowMs());
    } catch (e) {
      c.set("relayLogEvent", "oauth_client_rejected");
      return errorPage(c, e instanceof CimdError ? e.code : "client_fetch_failed");
    }
    if (!redirectUri || !redirectUriAllowed(meta.redirectUris, redirectUri)) {
      c.set("relayLogEvent", "oauth_invalid_request");
      return errorPage(c, "invalid_redirect_uri");
    }
    return { meta, redirectUri };
  }

  /** The rest of the request; errors go back to the (validated) redirect URI. */
  function requestParams(get: (k: string) => string | undefined, meta: ClientMetadata, redirectUri: string): AuthorizeParams | { error: string; description: string; state?: string } {
    const rawState = get("state");
    const state = rawState !== undefined && rawState.length <= MAX_STATE ? rawState : undefined;
    const fail = (error: string, description: string) => ({ error, description, ...(state !== undefined ? { state } : {}) });
    if (rawState !== undefined && rawState.length > MAX_STATE) return fail("invalid_request", "state is too long.");
    if (get("response_type") !== "code") return fail("unsupported_response_type", "Only response_type=code is supported.");
    if (get("code_challenge_method") !== "S256") return fail("invalid_request", "PKCE with code_challenge_method=S256 is required.");
    const challenge = get("code_challenge");
    if (!challenge || !CHALLENGE_RE.test(challenge)) return fail("invalid_request", "code_challenge must be the base64url SHA-256 of the verifier (43 characters).");
    const scopes = parseScopes(get("scope"));
    if (!scopes) return fail("invalid_scope", `scope must be a space-separated subset of ${OAUTH_SCOPES.join(", ")}.`);
    const resource = get("resource") ?? resourceUri;
    if (resource !== resourceUri && resource !== issuer) return fail("invalid_target", `resource must be ${resourceUri}.`);
    return { clientId: meta.clientId, redirectUri, codeChallenge: challenge, scopes, ...(state !== undefined ? { state } : {}), resource: resourceUri };
  }

  app.get("/oauth/authorize", async (c) => {
    const q = (k: string): string | undefined => c.req.query(k);
    const head = await clientAndRedirect(c, q("client_id"), q("redirect_uri"));
    if (head instanceof Response) return head;
    const p = requestParams(q, head.meta, head.redirectUri);
    if ("error" in p) {
      c.set("relayLogEvent", "oauth_invalid_request");
      return redirectWith(c, head.redirectUri, { error: p.error, error_description: p.description, state: p.state, iss: issuer });
    }
    c.set("relayLogEvent", "oauth_consent_shown");
    return consentPage(c, head.meta, p, signConsent(deps.secret, p, nowMs()), 200);
  });

  app.post("/oauth/authorize", formLimit, async (c) => {
    let form: Record<string, unknown>;
    try {
      form = await c.req.parseBody();
    } catch {
      return errorPage(c, "invalid_consent");
    }
    const f = (k: string): string | undefined => (typeof form[k] === "string" ? (form[k] as string) : undefined);
    const head = await clientAndRedirect(c, f("client_id"), f("redirect_uri"));
    if (head instanceof Response) return head;
    const p = requestParams(f, head.meta, head.redirectUri);
    if ("error" in p) return redirectWith(c, head.redirectUri, { error: p.error, error_description: p.description, state: p.state, iss: issuer });
    if (!verifyConsent(deps.secret, f("consent"), p, nowMs())) {
      c.set("relayLogEvent", "oauth_invalid_request");
      return errorPage(c, "invalid_consent");
    }
    if (f("decision") !== "approve") {
      c.set("relayLogEvent", "oauth_denied");
      return redirectWith(c, p.redirectUri, { error: "access_denied", error_description: "The user denied the request.", state: p.state, iss: issuer });
    }
    const presented = f("api_key")?.trim() ?? "";
    // Only a real agent key consents: an OAuth access token cannot mint further grants.
    const resolved = presented.startsWith("sa_") ? await resolveAgentKey(`Bearer ${presented}`, deps.agentStore, nowMs()).catch(() => undefined) : undefined;
    if (!resolved?.ok) {
      c.set("relayLogEvent", "oauth_denied");
      return consentPage(c, head.meta, p, signConsent(deps.secret, p, nowMs()), 401, "That key was not accepted. Check it and try again.");
    }
    if (deps.consentPolicy) {
      const policy = await deps.consentPolicy(resolved.principal.principalId).catch(() => "unavailable" as const);
      if (policy !== "allow") {
        c.set("relayLogEvent", "oauth_sso_required");
        return consentPage(c, head.meta, p, signConsent(deps.secret, p, nowMs()), policy === "sso_required" ? 403 : 401,
          policy === "sso_required"
            ? "Your organization requires single sign-on, so its members cannot connect apps by pasting an agent key. Ask your tenant administrator to connect this app."
            : "The organization directory is unavailable. Try again shortly.");
      }
    }
    const scopes = grantableScopes(p.scopes, resolved.principal.scopes);
    if (scopes.length === 0) {
      c.set("relayLogEvent", "oauth_denied");
      return redirectWith(c, p.redirectUri, { error: "invalid_scope", error_description: "The key holds none of the requested scopes.", state: p.state, iss: issuer });
    }
    const code = randomToken("sac_");
    const now = nowMs();
    try {
      await deps.store.insertCode({
        codeHash: hashSecret(code), clientId: p.clientId, redirectUri: p.redirectUri, codeChallenge: p.codeChallenge,
        keyId: resolved.principal.keyId, principalId: resolved.principal.principalId, scopes, resource: p.resource,
        createdAt: now, expiresAt: now + CODE_TTL_SECS * 1000,
      });
    } catch {
      return redirectWith(c, p.redirectUri, { error: "temporarily_unavailable", error_description: "Try again shortly.", state: p.state, iss: issuer });
    }
    c.set("relayLogAgentKey", resolved.principal.keyId);
    c.set("relayLogEvent", "oauth_code_issued");
    return redirectWith(c, p.redirectUri, { code, state: p.state, iss: issuer });
  });

  async function issue(family: { familyId: string; clientId: string; keyId: string; principalId: string }, scopes: AgentScope[], resource: string): Promise<Record<string, unknown>> {
    const now = nowMs();
    const access = randomToken("sat_");
    const refresh = randomToken("sar_");
    const base = { familyId: family.familyId, clientId: family.clientId, keyId: family.keyId, principalId: family.principalId, scopes, resource, createdAt: now };
    await deps.store.insertToken({ ...base, tokenHash: hashSecret(access), kind: "access", expiresAt: now + ACCESS_TOKEN_TTL_SECS * 1000 });
    await deps.store.insertToken({ ...base, tokenHash: hashSecret(refresh), kind: "refresh", expiresAt: now + REFRESH_TOKEN_TTL_SECS * 1000 });
    return { access_token: access, token_type: "Bearer", expires_in: ACCESS_TOKEN_TTL_SECS, refresh_token: refresh, scope: scopes.join(" ") };
  }

  app.post("/oauth/token", formLimit, async (c) => {
    const type = c.req.header("content-type") ?? "";
    if (!type.toLowerCase().startsWith("application/x-www-form-urlencoded")) return tokenError(c, "invalid_request", "Send the token request as application/x-www-form-urlencoded.");
    let form: Record<string, unknown>;
    try {
      form = await c.req.parseBody();
    } catch {
      return tokenError(c, "invalid_request", "The body could not be parsed.");
    }
    const f = (k: string): string | undefined => (typeof form[k] === "string" ? (form[k] as string) : undefined);
    const clientId = f("client_id");
    if (!clientId) return tokenError(c, "invalid_client", "client_id is required (public clients authenticate with PKCE).", 401);
    const now = nowMs();
    try {
      if (f("grant_type") === "authorization_code") {
        const code = f("code");
        const verifier = f("code_verifier");
        if (!code || !verifier) return tokenError(c, "invalid_request", "code and code_verifier are required.");
        if (!VERIFIER_RE.test(verifier)) return tokenError(c, "invalid_grant", "code_verifier must be 43 to 128 unreserved characters.");
        const codeHash = hashSecret(code);
        const consumed = await deps.store.consumeCode(codeHash, now);
        if (consumed.kind === "unknown") return tokenError(c, "invalid_grant", "The code is not valid.");
        if (consumed.kind === "reused") {
          // A replayed code may have leaked: revoke whatever the first exchange produced.
          await deps.store.revokeFamilyOfCode(codeHash, now);
          c.set("relayLogEvent", "oauth_code_reuse");
          return tokenError(c, "invalid_grant", "The code was already used; tokens issued from it are revoked.");
        }
        const rec = consumed.value;
        if (rec.expiresAt <= now) return tokenError(c, "invalid_grant", "The code expired. Start the authorization again.");
        if (rec.clientId !== clientId) return tokenError(c, "invalid_grant", "The code was issued to another client.");
        if (f("redirect_uri") !== rec.redirectUri) return tokenError(c, "invalid_grant", "redirect_uri does not match the authorization request.");
        const challenge = s256(verifier);
        const a = Buffer.from(challenge);
        const b = Buffer.from(rec.codeChallenge);
        if (a.length !== b.length || !timingSafeEqual(a, b)) return tokenError(c, "invalid_grant", "code_verifier does not match the code_challenge.");
        const familyId = `ofam_${randomUUID()}`;
        await deps.store.createFamily({ familyId, clientId: rec.clientId, keyId: rec.keyId, principalId: rec.principalId, codeHash, createdAt: now });
        const body = await issue({ familyId, clientId: rec.clientId, keyId: rec.keyId, principalId: rec.principalId }, rec.scopes, rec.resource);
        c.set("relayLogAgentKey", rec.keyId);
        c.set("relayLogEvent", "oauth_token_issued");
        return c.json(body, 200, { "cache-control": "no-store", pragma: "no-cache" });
      }
      if (f("grant_type") === "refresh_token") {
        const presented = f("refresh_token");
        if (!presented) return tokenError(c, "invalid_request", "refresh_token is required.");
        const tokenHash = hashSecret(presented);
        const current = await deps.store.token(tokenHash);
        if (!current || current.token.kind !== "refresh") return tokenError(c, "invalid_grant", "The refresh token is not valid.");
        if (current.token.clientId !== clientId) return tokenError(c, "invalid_grant", "The refresh token was issued to another client.");
        if (current.familyRevoked) return tokenError(c, "invalid_grant", "The grant was revoked. Authorize again.");
        // Checked before rotation, so a refused narrowing does not spend the refresh token.
        let scopes = current.token.scopes;
        const asked = f("scope");
        if (asked !== undefined) {
          const narrowed = parseScopes(asked);
          if (!narrowed || narrowed.some((s) => !current.token.scopes.includes(s))) return tokenError(c, "invalid_scope", "A refresh may only keep or narrow the granted scopes.");
          scopes = narrowed;
        }
        const rotated = await deps.store.rotateRefresh(tokenHash, now);
        if (rotated.kind === "unknown") return tokenError(c, "invalid_grant", "The refresh token is not valid.");
        if (rotated.kind === "reused") {
          // Reuse detection: a rotated refresh token came back, so one of its holders is not the
          // client. Revoke the whole family; the client authorizes again.
          await deps.store.revokeFamily(rotated.value.familyId, now);
          c.set("relayLogEvent", "oauth_refresh_reuse");
          return tokenError(c, "invalid_grant", "The refresh token was already used; the grant is revoked. Authorize again.");
        }
        const t: OAuthToken = rotated.value;
        if (t.expiresAt <= now) return tokenError(c, "invalid_grant", "The refresh token expired. Authorize again.");
        const key = await deps.agentStore.keyById(t.keyId);
        if (!key || (key.revokedAt !== undefined && key.revokedAt <= now) || (key.expiresAt !== undefined && key.expiresAt <= now)) {
          await deps.store.revokeFamily(t.familyId, now);
          return tokenError(c, "invalid_grant", "The key behind this grant is no longer valid. Authorize again.");
        }
        const body = await issue({ familyId: t.familyId, clientId: t.clientId, keyId: t.keyId, principalId: t.principalId }, scopes, t.resource);
        c.set("relayLogAgentKey", t.keyId);
        c.set("relayLogEvent", "oauth_refresh_rotated");
        return c.json(body, 200, { "cache-control": "no-store", pragma: "no-cache" });
      }
    } catch {
      return c.json({ error: "temporarily_unavailable", error_description: "Try again shortly." }, 503, { "cache-control": "no-store", "retry-after": "1" });
    }
    return tokenError(c, "unsupported_grant_type", "grant_type must be authorization_code or refresh_token.");
  });

  app.post("/oauth/revoke", formLimit, async (c) => {
    let form: Record<string, unknown> = {};
    try {
      form = await c.req.parseBody();
    } catch {
      return tokenError(c, "invalid_request", "The body could not be parsed.");
    }
    const token = typeof form.token === "string" ? form.token : undefined;
    const clientId = typeof form.client_id === "string" ? form.client_id : undefined;
    if (!token) return tokenError(c, "invalid_request", "token is required.");
    try {
      const found = await deps.store.token(hashSecret(token));
      // RFC 7009 §2.2: an unknown token, or one of another client, is answered 200 all the same.
      if (found && (clientId === undefined || found.token.clientId === clientId)) await deps.store.revokeFamily(found.token.familyId, nowMs());
    } catch {
      return c.json({ error: "temporarily_unavailable" }, 503, { "cache-control": "no-store", "retry-after": "1" });
    }
    c.set("relayLogEvent", "oauth_revoked");
    return c.body(null, 200, { "cache-control": "no-store" });
  });

  return app;
}
