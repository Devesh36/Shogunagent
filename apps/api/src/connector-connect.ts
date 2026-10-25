/**
 * The hosted connect flow (docs/implementation-plan.md E50; contract: docs/api/connector-tokens.md
 * §"Connecting an account"). A principal's first-party connector (Gmail, Google Calendar, Slack,
 * Linear) acts as the principal's own vendor account; this is how a human connects that account.
 *
 * Connecting an account is a human's act, never an agent's (the L3 rule: an agent cannot grant
 * itself a vendor credential). An agent can only ask for a link:
 *
 *   POST /v1/connectors/{provider}/connect          [agent key, `run`]
 *        → 202 { status: "pending_human", provider, slot, connect_url, expires_at, next_action: "ask_human" }
 *   GET  /v1/connectors/connect/{link}             the human's page: what will be connected, for whom
 *   POST /v1/connectors/connect/{link}             the human's "Continue" (CSRF-bound form)
 *        → 303 to the provider's consent page
 *   GET  /v1/connectors/callback?state&code         the provider's redirect
 *        → the code goes to the principal's sidecar, which exchanges it and stores the tokenset
 *
 * Nothing persists here. The link and the OAuth `state` are sealed (AES-256-GCM under keys derived
 * from `CONNECTOR_CONNECT_SECRET`) and carry the principal, the key and the provider; the PKCE
 * verifier lives in the sidecar's memory; the code passes through to the sidecar in one request
 * and is never logged, stored or echoed; the tokenset is written by the sidecar into the
 * principal's own token store (invariants 1, 2, 7). The callback must arrive in the browser that
 * pressed Continue (a flow cookie whose hash is sealed in the state), so a consent URL forwarded to
 * someone else cannot attach their account to this principal.
 *
 * Logs carry the route template and fixed words only (`routeTemplate` never logs a query string).
 */
import { createCipheriv, createDecipheriv, createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";

import type { Context, Hono, MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import { agentError, agentPrincipalOf, type AgentScope, type AgentVars } from "./agent-keys.js";
import { CONNECTOR_SLOT_BY_PROVIDER, type ConnectorVault, type VaultPrincipal } from "./connector-tokens.js";

/** The providers the hosted flow connects: the ones the core has a web-capable OAuth flow for.
 * GitHub is connected with the device flow in the core and has no hosted callback yet. */
export const CONNECTABLE_PROVIDERS = Object.freeze(["gmail", "calendar", "slack", "linear"] as const);
export type ConnectableProvider = (typeof CONNECTABLE_PROVIDERS)[number];

export const CONNECT_LINK_PATH = "/v1/connectors/connect";
export const CONNECT_CALLBACK_PATH = "/v1/connectors/callback";
/** How long the human has to open the link and press Continue. */
export const CONNECT_LINK_TTL_MS = 15 * 60_000;
/** How long the provider's consent may take (the sidecar keeps the verifier 10 minutes). */
export const CONNECT_STATE_TTL_MS = 10 * 60_000;
export const CONNECT_CSRF_COOKIE = "shogun_connect_csrf";
export const CONNECT_FLOW_COOKIE = "shogun_connect_flow";
const MAX_FORM_BYTES = 4 * 1024;

const PROVIDER_NAME: Record<ConnectableProvider, string> = { gmail: "Gmail", calendar: "Google Calendar", slack: "Slack", linear: "Linear" };

const PAGE_HEADERS: Record<string, string> = {
  "content-type": "text/html; charset=utf-8",
  "cache-control": "no-store",
  // The link and the provider's code are in the URL: never send them on as a referrer.
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
};

export interface ConnectorConnectDeps {
  /** `CONNECTOR_CONNECT_SECRET` (≥ 32 characters): seals links and OAuth states. */
  secret: string;
  vault: ConnectorVault;
  /** The gateway's public origin; the callback is `<publicBaseUrl>/v1/connectors/callback`, the
   * same URL the sidecar's `SHOGUN_CONNECT_REDIRECT_URI` names. */
  publicBaseUrl: string;
  requireKey: (scopes: readonly AgentScope[]) => MiddlewareHandler<{ Variables: AgentVars }>;
  now: () => Date;
}

// --- sealing (stateless: nothing about a flow is stored on the gateway) ---

type Purpose = "link" | "state";

interface LinkClaims {
  v: 1;
  p: string;
  k: string;
  prov: ConnectableProvider;
  exp: number;
  n: string;
}

interface StateClaims extends LinkClaims {
  /** SHA-256 (hex) of the flow cookie set in the browser that pressed Continue. */
  f: string;
}

function keyFor(secret: string, purpose: Purpose): Buffer {
  return createHmac("sha256", secret).update(`shogun/connector-connect/v1/${purpose}`).digest();
}

export function sealClaims(secret: string, purpose: Purpose, claims: object): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", keyFor(secret, purpose), iv);
  cipher.setAAD(Buffer.from(purpose));
  const ct = Buffer.concat([cipher.update(JSON.stringify(claims), "utf8"), cipher.final()]);
  return Buffer.concat([iv, ct, cipher.getAuthTag()]).toString("base64url");
}

export function openClaims(secret: string, purpose: Purpose, sealed: string): Record<string, unknown> | undefined {
  if (!/^[A-Za-z0-9_-]{40,1024}$/.test(sealed)) return undefined;
  const raw = Buffer.from(sealed, "base64url");
  if (raw.length < 12 + 16 + 2) return undefined;
  try {
    const decipher = createDecipheriv("aes-256-gcm", keyFor(secret, purpose), raw.subarray(0, 12));
    decipher.setAAD(Buffer.from(purpose));
    decipher.setAuthTag(raw.subarray(raw.length - 16));
    const plain = Buffer.concat([decipher.update(raw.subarray(12, raw.length - 16)), decipher.final()]).toString("utf8");
    const parsed: unknown = JSON.parse(plain);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

function isProvider(p: unknown): p is ConnectableProvider {
  return typeof p === "string" && (CONNECTABLE_PROVIDERS as readonly string[]).includes(p);
}

function linkClaims(o: Record<string, unknown> | undefined): LinkClaims | undefined {
  if (!o || o.v !== 1 || typeof o.p !== "string" || typeof o.k !== "string" || !isProvider(o.prov) || typeof o.exp !== "number" || typeof o.n !== "string") return undefined;
  return { v: 1, p: o.p, k: o.k, prov: o.prov, exp: o.exp, n: o.n };
}

function stateClaims(o: Record<string, unknown> | undefined): StateClaims | undefined {
  const base = linkClaims(o);
  if (!base || typeof o?.f !== "string") return undefined;
  return { ...base, f: o.f };
}

const sha256Hex = (s: string): string => createHash("sha256").update(s).digest("hex");

function sameHex(a: string, b: string): boolean {
  const x = Buffer.from(a);
  const y = Buffer.from(b);
  return x.length === y.length && timingSafeEqual(x, y);
}

// --- the human's pages ---

const escapeHtml = (s: string): string =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");

function page(title: string, body: string): string {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex"><title>${escapeHtml(title)}</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}dl{display:grid;grid-template-columns:auto 1fr;gap:.25rem 1rem}dt{color:#555}button{font:inherit;padding:.5rem 1.25rem}@media (prefers-color-scheme:dark){body{color:#eee;background:#111}dt{color:#aaa}}</style></head><body>${body}</body></html>`;
}

function cookieOf(header: string | undefined, name: string): string | undefined {
  for (const part of (header ?? "").split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === name) return v.join("=");
  }
  return undefined;
}

function csrfFor(secret: string, cookie: string, link: string): string {
  return createHmac("sha256", secret).update(`connect-csrf.${cookie}.${link}`).digest("hex");
}

export function registerConnectorConnectRoutes(app: Hono<{ Variables: AgentVars }>, deps: ConnectorConnectDeps): void {
  const origin = new URL(deps.publicBaseUrl).origin;
  const secure = origin.startsWith("https://");
  const html = (c: Context<{ Variables: AgentVars }>, status: number, title: string, body: string, extra: Record<string, string> = {}): Response =>
    c.html(page(title, `<h1>${escapeHtml(title)}</h1>${body}`), status as 200, { ...PAGE_HEADERS, ...extra });
  const principalOfClaims = (claims: LinkClaims): VaultPrincipal => ({ principalId: claims.p, keyId: claims.k });

  // --- the agent asks for a link (it cannot connect anything itself) ---
  app.post("/v1/connectors/:provider/connect", deps.requireKey(["run"]), (c) => {
    const provider = c.req.param("provider");
    if (!isProvider(provider)) {
      return agentError(c, 404, "unknown_connector", `This connector cannot be connected here. Connectable: ${CONNECTABLE_PROVIDERS.join(", ")}.`, "fix_input");
    }
    const principal = agentPrincipalOf(c);
    if (!principal) return agentError(c, 401, "missing_key", "Send your agent key as the bearer.", "register");
    const exp = deps.now().getTime() + CONNECT_LINK_TTL_MS;
    const link = sealClaims(deps.secret, "link", { v: 1, p: principal.principalId, k: principal.keyId, prov: provider, exp, n: randomBytes(9).toString("base64url") } satisfies LinkClaims);
    c.set("relayLogEvent", "connector_connect_link_created");
    return c.json({
      status: "pending_human",
      provider,
      slot: CONNECTOR_SLOT_BY_PROVIDER[provider],
      connect_url: `${deps.publicBaseUrl.replace(/\/+$/, "")}${CONNECT_LINK_PATH}/${link}`,
      expires_at: new Date(exp).toISOString(),
      next_action: "ask_human",
      message: `A human must open connect_url and sign in to ${PROVIDER_NAME[provider]}; an agent cannot connect an account. Then retry the run.`,
    }, 202, { "cache-control": "no-store" });
  });

  const openLink = (link: string): LinkClaims | "expired" | undefined => {
    const claims = linkClaims(openClaims(deps.secret, "link", link));
    if (!claims) return undefined;
    return claims.exp <= deps.now().getTime() ? "expired" : claims;
  };
  const badLink = (c: Context<{ Variables: AgentVars }>, claims: "expired" | undefined): Response => {
    c.set("relayLogEvent", "connector_connect_refused");
    return claims === "expired"
      ? html(c, 410, "Link expired", "<p>This connect link has expired. Ask the agent for a new one.</p>")
      : html(c, 400, "Link not valid", "<p>This connect link is not valid.</p>");
  };

  // --- the human's page ---
  app.get(`${CONNECT_LINK_PATH}/:link`, (c) => {
    const link = c.req.param("link");
    const claims = openLink(link);
    if (claims === undefined || claims === "expired") return badLink(c, claims);
    c.set("relayLogEvent", "connector_connect_page");
    const cookie = cookieOf(c.req.header("cookie"), CONNECT_CSRF_COOKIE) ?? randomBytes(16).toString("hex");
    const name = escapeHtml(PROVIDER_NAME[claims.prov]);
    const form = `<form method="post" action="${CONNECT_LINK_PATH}/${escapeHtml(link)}"><input type="hidden" name="csrf" value="${csrfFor(deps.secret, cookie, link)}"><button type="submit">Continue to ${name}</button></form>`;
    const setCookie = `${CONNECT_CSRF_COOKIE}=${cookie}; Path=${CONNECT_LINK_PATH}; HttpOnly; SameSite=Strict; Max-Age=900${secure ? "; Secure" : ""}`;
    return html(c, 200, `Connect ${PROVIDER_NAME[claims.prov]}?`,
      `<p>An agent asked to use your ${name} account through ShogunAgent. If you continue, you sign in to ${name} and its tools act as you for this principal's agents. Sends and posts still need your approval each time. Nothing is connected unless you continue and consent.</p><dl><dt>Principal</dt><dd>${escapeHtml(claims.p)}</dd><dt>Account</dt><dd>${name}</dd></dl>${form}<p>If you did not expect this, close the page.</p>`,
      { "set-cookie": setCookie });
  });

  // --- Continue: start the flow in the principal's sidecar, send the browser to the provider ---
  app.post(`${CONNECT_LINK_PATH}/:link`, bodyLimit({ maxSize: MAX_FORM_BYTES, onError: (c) => c.text("Too large", 413) }), async (c) => {
    const link = c.req.param("link");
    const sentOrigin = c.req.header("origin");
    if (sentOrigin !== undefined && sentOrigin !== origin) {
      c.set("relayLogEvent", "connector_connect_refused");
      return html(c, 403, "Not connected", "<p>This form must be sent from the connect page.</p>");
    }
    const claims = openLink(link);
    if (claims === undefined || claims === "expired") return badLink(c, claims);
    const form = await c.req.parseBody().catch(() => ({} as Record<string, unknown>));
    const cookie = cookieOf(c.req.header("cookie"), CONNECT_CSRF_COOKIE);
    const csrf = typeof form.csrf === "string" ? form.csrf : "";
    if (!cookie || !sameHex(csrf, csrfFor(deps.secret, cookie, link))) {
      c.set("relayLogEvent", "connector_connect_refused");
      return html(c, 403, "Not connected", "<p>The form expired or was not sent from the connect page. Open the link again.</p>");
    }
    const flow = randomBytes(24).toString("base64url");
    const exp = Math.min(claims.exp + CONNECT_STATE_TTL_MS, deps.now().getTime() + CONNECT_STATE_TTL_MS);
    const state = sealClaims(deps.secret, "state", { ...claims, exp, f: sha256Hex(flow) } satisfies StateClaims);
    let started;
    try {
      started = await deps.vault.connectStart(principalOfClaims(claims), claims.prov, state);
    } catch {
      started = { kind: "unavailable" as const, reason: "transport" as const };
    }
    if (started.kind !== "started") {
      c.set("relayLogEvent", "connector_connect_refused");
      return started.kind === "not_configured"
        ? html(c, 503, "Not available", `<p>${escapeHtml(PROVIDER_NAME[claims.prov])} cannot be connected on this deployment yet. Ask the operator.</p>`)
        : html(c, 503, "Not connected", "<p>The connection could not be started. Try again in a moment.</p>");
    }
    c.set("relayLogEvent", "connector_connect_started");
    const flowCookie = `${CONNECT_FLOW_COOKIE}=${flow}; Path=${CONNECT_CALLBACK_PATH}; HttpOnly; SameSite=Lax; Max-Age=${Math.floor(CONNECT_STATE_TTL_MS / 1000)}${secure ? "; Secure" : ""}`;
    return new Response(null, { status: 303, headers: { location: started.authorizeUrl, "cache-control": "no-store", "referrer-policy": "no-referrer", "set-cookie": flowCookie } });
  });

  // --- the provider's redirect: hand the code to the principal's sidecar, keep nothing ---
  app.get(CONNECT_CALLBACK_PATH, async (c) => {
    const clearFlow = `${CONNECT_FLOW_COOKIE}=; Path=${CONNECT_CALLBACK_PATH}; HttpOnly; SameSite=Lax; Max-Age=0${secure ? "; Secure" : ""}`;
    const refuse = (status: number, title: string, message: string): Response => {
      c.set("relayLogEvent", "connector_connect_refused");
      return html(c, status, title, `<p>${escapeHtml(message)}</p>`, { "set-cookie": clearFlow });
    };
    const claims = stateClaims(openClaims(deps.secret, "state", c.req.query("state") ?? ""));
    if (!claims) return refuse(400, "Not connected", "This sign-in did not come from a ShogunAgent connect link.");
    if (claims.exp <= deps.now().getTime()) return refuse(410, "Not connected", "This sign-in took too long. Ask the agent for a new link.");
    const flow = cookieOf(c.req.header("cookie"), CONNECT_FLOW_COOKIE);
    if (!flow || !sameHex(sha256Hex(flow), claims.f)) {
      return refuse(403, "Not connected", "This sign-in was started in another browser. Open the connect link in this browser and continue from there.");
    }
    const name = PROVIDER_NAME[claims.prov];
    if (c.req.query("error") !== undefined) return refuse(400, "Not connected", `You did not grant access at ${name}. Nothing was connected.`);
    const code = c.req.query("code");
    if (!code || code.length > 4096) return refuse(400, "Not connected", `${name} did not send a sign-in code. Start again from the link.`);
    let done;
    try {
      done = await deps.vault.connectFinish(principalOfClaims(claims), c.req.query("state") ?? "", code);
    } catch {
      done = { kind: "unavailable" as const, reason: "transport" as const };
    }
    switch (done.kind) {
      case "connected":
        c.set("relayLogEvent", "connector_connected");
        return html(c, 200, `${name} connected`, `<p>${escapeHtml(name)} is connected for principal ${escapeHtml(claims.p)}. The agent can retry its run. You can close this page.</p>`, { "set-cookie": clearFlow });
      case "flow_unknown":
        return refuse(410, "Not connected", "This sign-in was already used or has expired. Ask the agent for a new link.");
      case "rejected":
        return refuse(400, "Not connected", `${name} refused the sign-in or did not grant every permission the connector needs. Start again from a new link and grant them.`);
      default:
        return refuse(503, "Not connected", "The connection could not be completed. Ask the agent for a new link and try again in a moment.");
    }
  });
}
