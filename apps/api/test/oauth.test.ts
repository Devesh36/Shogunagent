/** E3-T2: OAuth 2.1 + Client ID Metadata Documents for MCP clients — the SSRF guard on the CIMD
 * fetch, strict document validation, redirect URI matching, the authorization code flow with
 * PKCE (S256 only), refresh rotation with reuse detection, revocation, and access tokens working
 * wherever `sa_` keys do (same principal, same caps, at most the key's scopes). */
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  CimdCache, CimdError, clientMetadata, expandIPv6, hostAllowed, isPrivateAddress, parseClientId, redirectUriAllowed, validateClientMetadata,
  type CimdFetch,
} from "../src/oauth/cimd.js";
import { grantableScopes, parseScopes, s256 } from "../src/oauth/app.js";
import { InMemoryOAuthStore, SqlOAuthStore, hashSecret, type OAuthStore } from "../src/oauth/store.js";
import { BASE, authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";

const OAUTH_SQL = readFileSync(new URL("../sql/agent-oauth.sql", import.meta.url), "utf8");
const CLIENT_ID = "https://client.example.com/oauth/client.json";
const REDIRECT = "http://127.0.0.1:33418/callback";
const VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE = createHash("sha256").update(VERIFIER).digest("base64url");

function doc(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return { client_id: CLIENT_ID, client_name: "Example MCP Client", logo_uri: "https://client.example.com/logo.png", redirect_uris: [REDIRECT, "https://client.example.com/cb"], token_endpoint_auth_method: "none", grant_types: ["authorization_code", "refresh_token"], response_types: ["code"], ...overrides };
}

function cimdFetch(body: unknown = doc(), init: { status?: number; headers?: Record<string, string> } = {}): { fetch: CimdFetch; calls: string[] } {
  const calls: string[] = [];
  const fetch: CimdFetch = (url) => {
    calls.push(url);
    const text = typeof body === "string" ? body : JSON.stringify(body);
    return Promise.resolve(new Response(text, { status: init.status ?? 200, headers: { "content-type": "application/json", ...(init.headers ?? {}) } }));
  };
  return { fetch, calls };
}

describe("CIMD: client ids and the SSRF guard", () => {
  it("accepts only canonical https URLs with a path on a public host", () => {
    expect(parseClientId(CLIENT_ID).href).toBe(CLIENT_ID);
    for (const bad of [
      "http://client.example.com/c.json", "https://client.example.com", "https://client.example.com/", "https://user:pw@client.example.com/c.json",
      "https://client.example.com/c.json#x", "https://client.example.com/a/../c.json", "https://client.example.com/a/%2e%2e/c.json", "https://CLIENT.example.com/c.json",
      "not a url", "", "https://client.example.com/" + "a".repeat(2100),
    ]) expect(() => parseClientId(bad), bad).toThrow(CimdError);
  });

  it("refuses private, loopback, link-local, metadata and reserved hosts", () => {
    for (const host of [
      "localhost", "127.0.0.1", "10.0.0.5", "172.16.3.4", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "224.0.0.1", "198.18.0.1",
      "[::1]", "[::]", "[fd00::1]", "[fe80::1]", "[::ffff:127.0.0.1]", "[::ffff:a9fe:a9fe]", "[64:ff9b::a00:1]", "[2002:a00:1::]", "[2001:db8::1]",
      "2130706433", "0x7f.1", "intranet", "printer.local", "db.internal", "api.localhost", "router.home.arpa",
    ]) {
      expect(hostAllowed(host), host).toBe(false);
      expect(() => parseClientId(`https://${host}/c.json`), host).toThrow(CimdError);
    }
    for (const host of ["client.example.com", "8.8.8.8", "[2606:4700::1111]"]) expect(hostAllowed(host), host).toBe(true);
  });

  it("classifies resolved addresses", () => {
    expect(isPrivateAddress("93.184.216.34")).toBe(false);
    expect(isPrivateAddress("10.1.2.3")).toBe(true);
    expect(isPrivateAddress("::1")).toBe(true);
    expect(isPrivateAddress("2606:4700:4700::1111")).toBe(false);
    expect(isPrivateAddress("::ffff:192.168.0.1")).toBe(true);
    expect(isPrivateAddress("garbage")).toBe(true);
    expect(expandIPv6("::ffff:1.2.3.4")).toEqual([0, 0, 0, 0, 0, 0xffff, 0x0102, 0x0304]);
  });

  it("validates the document strictly", () => {
    expect(validateClientMetadata(CLIENT_ID, doc())).toEqual({ clientId: CLIENT_ID, clientName: "Example MCP Client", logoUri: "https://client.example.com/logo.png", redirectUris: [REDIRECT, "https://client.example.com/cb"] });
    const refused = (d: Record<string, unknown>): string | undefined => {
      try { validateClientMetadata(CLIENT_ID, d); return undefined; } catch (e) { return (e as CimdError).code; }
    };
    expect(refused(doc({ client_id: "https://evil.example.com/c.json" }))).toBe("client_id_mismatch");
    expect(refused(doc({ redirect_uris: [] }))).toBe("client_document_invalid");
    expect(refused(doc({ redirect_uris: ["javascript:alert(1)"] }))).toBe("client_document_invalid");
    expect(refused(doc({ redirect_uris: ["http://client.example.com/cb"] }))).toBe("client_document_invalid");
    expect(refused(doc({ redirect_uris: ["https://client.example.com/cb#frag"] }))).toBe("client_document_invalid");
    expect(refused(doc({ client_secret: "s" }))).toBe("client_document_invalid");
    expect(refused(doc({ token_endpoint_auth_method: "client_secret_basic" }))).toBe("client_document_invalid");
    expect(refused(doc({ grant_types: ["client_credentials"] }))).toBe("client_document_invalid");
    // An unsafe logo is dropped, not trusted.
    expect(validateClientMetadata(CLIENT_ID, doc({ logo_uri: "http://169.254.169.254/x.png" })).logoUri).toBeUndefined();
  });

  it("matches redirect URIs exactly, with only the RFC 8252 loopback port allowance", () => {
    const registered = [REDIRECT, "https://client.example.com/cb"];
    expect(redirectUriAllowed(registered, "https://client.example.com/cb")).toBe(true);
    expect(redirectUriAllowed(registered, REDIRECT)).toBe(true);
    expect(redirectUriAllowed(registered, "http://127.0.0.1:50000/callback")).toBe(true);
    expect(redirectUriAllowed(registered, "http://127.0.0.1:50000/callback/x")).toBe(false);
    expect(redirectUriAllowed(registered, "http://127.0.0.1:50000/callback?x=1")).toBe(false);
    expect(redirectUriAllowed(registered, "http://localhost:33418/callback")).toBe(false);
    expect(redirectUriAllowed(registered, "https://client.example.com:8443/cb")).toBe(false);
    expect(redirectUriAllowed(registered, "https://client.example.com/cb/")).toBe(false);
    expect(redirectUriAllowed(registered, "https://evil.example.com/cb")).toBe(false);
  });

  it("fetches without following redirects, caps the size, requires JSON, checks resolved addresses, and caches", async () => {
    const now = Date.parse("2026-10-25T12:00:00Z");
    const ok = cimdFetch(doc(), { headers: { "cache-control": "max-age=600" } });
    const cache = new CimdCache();
    expect((await clientMetadata(CLIENT_ID, { fetch: ok.fetch, cache }, now)).clientName).toBe("Example MCP Client");
    await clientMetadata(CLIENT_ID, { fetch: ok.fetch, cache }, now + 500_000);
    expect(ok.calls).toHaveLength(1);
    await clientMetadata(CLIENT_ID, { fetch: ok.fetch, cache }, now + 700_000);
    expect(ok.calls).toHaveLength(2);
    const code = async (deps: Parameters<typeof clientMetadata>[1]): Promise<string> => {
      try { await clientMetadata(CLIENT_ID, { cache: new CimdCache(), ...deps }, now); return "ok"; } catch (e) { return (e as CimdError).code; }
    };
    expect(await code({ fetch: cimdFetch("", { status: 302, headers: { location: "http://169.254.169.254/" } }).fetch })).toBe("client_redirected");
    expect(await code({ fetch: cimdFetch({ ...doc(), pad: "x".repeat(6_000) }).fetch })).toBe("client_document_too_large");
    expect(await code({ fetch: cimdFetch("<html>", { headers: { "content-type": "text/html" } }).fetch })).toBe("client_document_not_json");
    expect(await code({ fetch: cimdFetch(doc(), { status: 404 }).fetch })).toBe("client_fetch_failed");
    expect(await code({ fetch: () => Promise.reject(new Error("boom")) })).toBe("client_fetch_failed");
    // DNS rebinding to a private address is refused before any fetch.
    const never = cimdFetch();
    expect(await code({ fetch: never.fetch, resolveHost: () => Promise.resolve(["93.184.216.34", "10.0.0.1"]) })).toBe("client_host_refused");
    expect(never.calls).toHaveLength(0);
    expect(await code({ fetch: cimdFetch().fetch, resolveHost: () => Promise.resolve(["93.184.216.34"]) })).toBe("ok");
    expect(await code({ fetch: cimdFetch().fetch, resolveHost: () => Promise.reject(new Error("nxdomain")) })).toBe("client_fetch_failed");
  });
});

describe("scopes and PKCE helpers", () => {
  it("parses scopes, never grants admin, and computes S256", () => {
    expect(parseScopes(undefined)).toEqual(["read"]);
    expect(parseScopes("read write read")).toEqual(["read", "write"]);
    expect(parseScopes("read admin")).toBeUndefined();
    expect(grantableScopes(["read", "write", "run"], ["read", "run"])).toEqual(["read", "run"]);
    expect(grantableScopes(["read", "write"], ["admin"])).toEqual(["read", "write"]);
    expect(s256(VERIFIER)).toBe("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
  });
});

// --- the flow ---

interface OAuthRig { rig: AgentRig; store: OAuthStore; fetchCalls: string[] }

function oauthRig(fetch = cimdFetch()): OAuthRig {
  const store = new InMemoryOAuthStore();
  const rig = makeAgentRig({ oauth: { store, secret: "test-oauth-secret-test-oauth-secret", fetch: fetch.fetch, cache: new CimdCache() } });
  return { rig, store, fetchCalls: fetch.calls };
}

function authorizeQuery(overrides: Record<string, string | undefined> = {}): string {
  const p = new URLSearchParams();
  const base: Record<string, string | undefined> = { response_type: "code", client_id: CLIENT_ID, redirect_uri: REDIRECT, code_challenge: CHALLENGE, code_challenge_method: "S256", state: "st-1", scope: "read write", resource: `${BASE}/mcp`, ...overrides };
  for (const [k, v] of Object.entries(base)) if (v !== undefined) p.set(k, v);
  return p.toString();
}

function form(rig: AgentRig, path: string, fields: Record<string, string>): Promise<Response> {
  return Promise.resolve(rig.app.request(path, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(fields).toString() }));
}

/** GET the consent page, then approve with `key`. Returns the code. */
async function authorize(o: OAuthRig, key: string, overrides: Record<string, string | undefined> = {}): Promise<string> {
  const page = await o.rig.app.request(`/oauth/authorize?${authorizeQuery(overrides)}`);
  expect(page.status).toBe(200);
  const html = await page.text();
  const consent = /name="consent" value="([^"]+)"/.exec(html)?.[1];
  const fields = Object.fromEntries(new URLSearchParams(authorizeQuery(overrides)));
  const res = await form(o.rig, "/oauth/authorize", { ...fields, consent: consent ?? "", decision: "approve", api_key: key });
  expect(res.status).toBe(302);
  const location = new URL(res.headers.get("location") ?? "");
  expect(location.searchParams.get("state")).toBe(overrides.state ?? "st-1");
  expect(location.searchParams.get("iss")).toBe(BASE);
  return location.searchParams.get("code") ?? "";
}

async function exchange(o: OAuthRig, code: string, overrides: Record<string, string> = {}): Promise<Response> {
  return form(o.rig, "/oauth/token", { grant_type: "authorization_code", code, redirect_uri: REDIRECT, client_id: CLIENT_ID, code_verifier: VERIFIER, ...overrides });
}

interface Tokens { access_token: string; refresh_token: string; expires_in: number; scope: string; token_type: string }

describe("OAuth 2.1 authorization code flow", () => {
  it("publishes Protected Resource and Authorization Server metadata, with no registration endpoint", async () => {
    const { rig } = oauthRig();
    const prm = (await (await rig.app.request("/.well-known/oauth-protected-resource/mcp")).json()) as Record<string, unknown>;
    expect(prm).toEqual(expect.objectContaining({ resource: `${BASE}/mcp`, authorization_servers: [BASE], scopes_supported: ["read", "write", "run"] }));
    expect((await rig.app.request("/.well-known/oauth-protected-resource")).status).toBe(200);
    const as = (await (await rig.app.request("/.well-known/oauth-authorization-server")).json()) as Record<string, unknown>;
    expect(as).toEqual(expect.objectContaining({ issuer: BASE, code_challenge_methods_supported: ["S256"], client_id_metadata_document_supported: true, token_endpoint_auth_methods_supported: ["none"] }));
    expect(as.registration_endpoint).toBeUndefined();
    expect((await rig.app.request("/oauth/register", { method: "POST" })).status).toBe(404);
    // A 401 on /mcp points at the resource metadata.
    const unauth = await rig.app.request("/mcp", { method: "POST", headers: { "mcp-method": "tools/call", "mcp-name": "memory.search" } });
    expect(unauth.status).toBe(401);
    expect(unauth.headers.get("www-authenticate")).toBe(`Bearer realm="shogunagent", resource_metadata="${BASE}/.well-known/oauth-protected-resource/mcp"`);
  });

  it("issues a code on consent, exchanges it with PKCE, and the access token works where sa_ keys do", async () => {
    const o = oauthRig();
    const { key, principalId } = await registerAgent(o.rig);
    const page = await o.rig.app.request(`/oauth/authorize?${authorizeQuery()}`);
    expect(page.headers.get("content-security-policy")).toContain("frame-ancestors 'none'");
    expect(page.headers.get("content-security-policy")).toContain("form-action 'self' http://127.0.0.1:33418");
    const html = await page.text();
    expect(html).toContain("Allow Example MCP Client to use ShogunAgent?");
    expect(html).not.toContain(key);
    const code = await authorize(o, key);
    expect(code).toMatch(/^sac_/);
    const res = await exchange(o, code);
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const tokens = (await res.json()) as Tokens;
    expect(tokens).toEqual(expect.objectContaining({ token_type: "Bearer", expires_in: 900, scope: "read write" }));
    expect(tokens.access_token).toMatch(/^sat_[A-Za-z0-9_-]{43}$/);
    expect(tokens.refresh_token).toMatch(/^sar_/);
    // Hashed at rest: the store never holds the token.
    expect(await o.store.token(hashSecret(tokens.access_token))).toBeDefined();
    const me = (await (await authed(o.rig, tokens.access_token, "/v1/agents/me")).json()) as Record<string, unknown>;
    expect(me).toEqual(expect.objectContaining({ principal_id: principalId, scopes: ["read", "write"] }));
    // Metered /mcp: same wallet, same receipts.
    const ingest = await mcp(o.rig, tokens.access_token, { "mcp-method": "tools/call", "mcp-name": "memory.ingest" });
    expect(ingest.status).toBe(200);
    const receipts = (await (await authed(o.rig, key, "/v1/receipts")).json()) as { receipts: Array<{ name: string }> };
    expect(receipts.receipts.map((r) => r.name)).toContain("memory.ingest");
    // Not granted `run`: a run is 403; key management and admin are refused for OAuth tokens.
    expect((await mcp(o.rig, tokens.access_token, { "mcp-method": "tools/call", "mcp-name": "router.run" })).status).toBe(403);
    expect((await authed(o.rig, tokens.access_token, "/v1/keys/whatever/revoke", { method: "POST" })).status).toBe(403);
    // Nothing secret or identifying reaches the log.
    const log = o.rig.logLines.join("\n");
    for (const s of [code, tokens.access_token, tokens.refresh_token, key, CLIENT_ID, VERIFIER]) expect(log).not.toContain(s);
    expect(log).toContain("event=oauth_token_issued");
  });

  it("refuses a wrong verifier, another client, a mismatched redirect_uri, and an expired code", async () => {
    const o = oauthRig();
    const { key } = await registerAgent(o.rig);
    const c1 = await authorize(o, key);
    expect(await (await exchange(o, c1, { code_verifier: "x".repeat(43) })).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    const c2 = await authorize(o, key);
    expect(await (await exchange(o, c2, { client_id: "https://other.example.com/c.json" })).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    const c3 = await authorize(o, key);
    expect(await (await exchange(o, c3, { redirect_uri: "https://client.example.com/cb" })).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    const c4 = await authorize(o, key);
    o.rig.setNow(new Date(o.rig.now().getTime() + 61_000));
    expect(await (await exchange(o, c4)).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    expect((await form(o.rig, "/oauth/token", { grant_type: "client_credentials", client_id: CLIENT_ID })).status).toBe(400);
    expect((await form(o.rig, "/oauth/token", { grant_type: "authorization_code", code: "x", code_verifier: VERIFIER })).status).toBe(401);
  });

  it("a replayed code revokes the tokens the first exchange issued", async () => {
    const o = oauthRig();
    const { key } = await registerAgent(o.rig);
    const code = await authorize(o, key);
    const tokens = (await (await exchange(o, code)).json()) as Tokens;
    expect((await authed(o.rig, tokens.access_token, "/v1/wallet")).status).toBe(200);
    const replay = await exchange(o, code);
    expect(await replay.json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    expect((await authed(o.rig, tokens.access_token, "/v1/wallet")).status).toBe(401);
  });

  it("validates the request before showing consent: bad clients and redirects get a page, never a redirect", async () => {
    const o = oauthRig();
    const mismatch = await o.rig.app.request(`/oauth/authorize?${authorizeQuery({ redirect_uri: "https://evil.example.com/cb" })}`);
    expect(mismatch.status).toBe(400);
    expect(mismatch.headers.get("location")).toBeNull();
    expect(await mismatch.text()).toContain("invalid_redirect_uri");
    const fetchedBefore = [...o.fetchCalls];
    const ssrf = await o.rig.app.request(`/oauth/authorize?${authorizeQuery({ client_id: "https://169.254.169.254/latest/meta-data" })}`);
    expect(ssrf.status).toBe(400);
    expect(await ssrf.text()).toContain("client_host_refused");
    // The metadata address was never fetched.
    expect(o.fetchCalls).toEqual(fetchedBefore);
    expect(o.fetchCalls.some((u) => u.includes("169.254"))).toBe(false);
    // With the client and redirect valid, other errors go back to the client.
    const plain = await o.rig.app.request(`/oauth/authorize?${authorizeQuery({ code_challenge_method: "plain" })}`);
    expect(plain.status).toBe(302);
    expect(new URL(plain.headers.get("location") ?? "").searchParams.get("error")).toBe("invalid_request");
    const scope = await o.rig.app.request(`/oauth/authorize?${authorizeQuery({ scope: "admin" })}`);
    expect(new URL(scope.headers.get("location") ?? "").searchParams.get("error")).toBe("invalid_scope");
    const resource = await o.rig.app.request(`/oauth/authorize?${authorizeQuery({ resource: "https://elsewhere.example.com/mcp" })}`);
    expect(new URL(resource.headers.get("location") ?? "").searchParams.get("error")).toBe("invalid_target");
    // A forged or altered consent token is refused; a deny redirects with access_denied.
    const fields = Object.fromEntries(new URLSearchParams(authorizeQuery()));
    expect((await form(o.rig, "/oauth/authorize", { ...fields, consent: "1.abc", decision: "approve", api_key: "sa_live_x" })).status).toBe(400);
    const page = await (await o.rig.app.request(`/oauth/authorize?${authorizeQuery()}`)).text();
    const consent = /name="consent" value="([^"]+)"/.exec(page)?.[1] ?? "";
    const widened = await form(o.rig, "/oauth/authorize", { ...fields, scope: "read write run", consent, decision: "approve", api_key: "sa_live_x" });
    expect(widened.status).toBe(400);
    const deny = await form(o.rig, "/oauth/authorize", { ...fields, consent, decision: "deny" });
    expect(new URL(deny.headers.get("location") ?? "").searchParams.get("error")).toBe("access_denied");
    const badKey = await form(o.rig, "/oauth/authorize", { ...fields, consent, decision: "approve", api_key: "sa_live_wrong" });
    expect(badKey.status).toBe(401);
  });

  it("rotates refresh tokens and revokes the family when an old one comes back", async () => {
    const o = oauthRig();
    const { key } = await registerAgent(o.rig);
    const first = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    const refresh = (t: string, extra: Record<string, string> = {}): Promise<Response> => form(o.rig, "/oauth/token", { grant_type: "refresh_token", refresh_token: t, client_id: CLIENT_ID, ...extra });
    const second = (await (await refresh(first.refresh_token)).json()) as Tokens;
    expect(second.refresh_token).not.toBe(first.refresh_token);
    expect((await authed(o.rig, second.access_token, "/v1/wallet")).status).toBe(200);
    // Narrowing is allowed, widening is not.
    expect(await (await refresh(second.refresh_token, { scope: "read write run" })).json()).toEqual(expect.objectContaining({ error: "invalid_scope" }));
    const third = (await (await refresh(second.refresh_token, { scope: "read" })).json()) as Tokens;
    expect(third.scope).toBe("read");
    // Reuse: the first refresh token again. The whole family dies, including the newest tokens.
    const reuse = await refresh(first.refresh_token);
    expect(await reuse.json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    expect((await authed(o.rig, third.access_token, "/v1/wallet")).status).toBe(401);
    expect(await (await refresh(third.refresh_token)).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
    expect(o.rig.logLines.some((l) => l.includes("event=oauth_refresh_reuse"))).toBe(true);
    // Another client cannot use the refresh token.
    const other = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    expect(await (await refresh(other.refresh_token, { client_id: "https://other.example.com/c.json" })).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
  });

  it("access tokens expire, die with the key, and are revocable", async () => {
    const o = oauthRig();
    const { key, keyId } = await registerAgent(o.rig);
    const a = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    o.rig.setNow(new Date(o.rig.now().getTime() + 901_000));
    expect((await authed(o.rig, a.access_token, "/v1/wallet")).status).toBe(401);
    const b = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    expect((await form(o.rig, "/oauth/revoke", { token: b.refresh_token, client_id: CLIENT_ID })).status).toBe(200);
    expect((await authed(o.rig, b.access_token, "/v1/wallet")).status).toBe(401);
    expect((await form(o.rig, "/oauth/revoke", { token: "sat_unknown" })).status).toBe(200);
    const c = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    expect((await authed(o.rig, key, `/v1/keys/${keyId}/revoke`, { method: "POST" })).status).toBe(200);
    expect((await authed(o.rig, c.access_token, "/v1/wallet")).status).toBe(401);
    expect(await (await form(o.rig, "/oauth/token", { grant_type: "refresh_token", refresh_token: c.refresh_token, client_id: CLIENT_ID })).json()).toEqual(expect.objectContaining({ error: "invalid_grant" }));
  });

  it("an OAuth access token never consents to another grant", async () => {
    const o = oauthRig();
    const { key } = await registerAgent(o.rig);
    const t = (await (await exchange(o, await authorize(o, key))).json()) as Tokens;
    const page = await (await o.rig.app.request(`/oauth/authorize?${authorizeQuery()}`)).text();
    const consent = /name="consent" value="([^"]+)"/.exec(page)?.[1] ?? "";
    const res = await form(o.rig, "/oauth/authorize", { ...Object.fromEntries(new URLSearchParams(authorizeQuery())), consent, decision: "approve", api_key: t.access_token });
    expect(res.status).toBe(401);
  });
});

describe.each([
  ["in-memory", (): OAuthStore => new InMemoryOAuthStore()],
  ["sql (sqlite double)", (): OAuthStore => new SqlOAuthStore(sqliteDouble(OAUTH_SQL))],
])("OAuthStore on %s", (_name, open) => {
  it("consumes a code once, rotates a refresh token once, and revokes families", async () => {
    const store = open();
    if (store instanceof SqlOAuthStore) await store.checkSchema();
    const base = { clientId: CLIENT_ID, keyId: "ak_1", principalId: "prn_1", scopes: ["read" as const], resource: `${BASE}/mcp`, createdAt: 1 };
    await store.insertCode({ ...base, codeHash: "c1", redirectUri: REDIRECT, codeChallenge: CHALLENGE, expiresAt: 61_000 });
    expect((await store.consumeCode("c1", 2)).kind).toBe("ok");
    expect((await store.consumeCode("c1", 3)).kind).toBe("reused");
    expect((await store.consumeCode("nope", 3)).kind).toBe("unknown");
    await store.createFamily({ familyId: "f1", clientId: CLIENT_ID, keyId: "ak_1", principalId: "prn_1", codeHash: "c1", createdAt: 2 });
    await store.insertToken({ ...base, tokenHash: "a1", kind: "access", familyId: "f1", expiresAt: 900_000 });
    await store.insertToken({ ...base, tokenHash: "r1", kind: "refresh", familyId: "f1", expiresAt: 9_000_000 });
    expect((await store.rotateRefresh("a1", 5)).kind).toBe("unknown");
    expect((await store.rotateRefresh("r1", 5)).kind).toBe("ok");
    expect((await store.rotateRefresh("r1", 6)).kind).toBe("reused");
    expect((await store.token("a1"))?.familyRevoked).toBe(false);
    await store.revokeFamilyOfCode("c1", 7);
    expect((await store.token("a1"))?.familyRevoked).toBe(true);
    expect((await store.token("a1"))?.token.scopes).toEqual(["read"]);
    await store.maintenance(9_000_000 + 86_400_001);
    expect(await store.token("r1")).toBeUndefined();
  });
});
