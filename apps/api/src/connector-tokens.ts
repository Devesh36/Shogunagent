/**
 * Connector tokens for the router (docs/implementation-plan.md E48, E50; contract:
 * docs/api/connector-tokens.md). The router's first-party connectors (Gmail, Google Calendar,
 * GitHub, Linear, Slack) act as the CALLING principal's own account; per run the router asks this
 * route for that principal's access token and drops it after the one vendor request.
 *
 *   POST /v1/internal/router/connector-token      [ROUTER_SETTLEMENT_TOKEN]
 *        Authorization: Bearer <router token>; X-Caller-Authorization: Bearer <the caller's key>
 *        { slot, provider_id, run_id? }
 *        → 200 { access_token, token_type: "Bearer", expires_at }   Cache-Control: no-store
 *        → 404 connector_not_connected (no grant, a revoked or expired one, no vault configured)
 *        → 401 / 403 the service token or the caller's key; 400 a malformed body; 503 the vault did
 *          not answer
 *
 * The principal comes from the caller's key, never from the body, so the router cannot ask for
 * another principal's token.
 *
 * Where the token comes from (E50): the principal's sidecar (`shogun-api --multi`), which owns the
 * principal's token store, opens the grant there, refreshes it through the core's single egress
 * under a per-(principal, slot) lock and writes the refreshed tokenset back
 * (`POST /v1/internal/connectors/token`, `crates/shogun-mcp/src/connector_vault.rs`). The gateway
 * reaches it through the same transport as the `/mcp` relay (`SHOGUN_SIDECAR_URL` or the Workers
 * binding, mcp-proxy.ts), presents `SHOGUN_SIDECAR_ADMIN_TOKEN` as its own credential and names
 * the principal with the principal's sidecar bearer (`sgw1` derived, or listed) in
 * `X-Shogun-Principal-Authorization`. The gateway holds no row, no KMS client and no refresh
 * token: the refresh token never leaves the sidecar, and the access token passes through this
 * route's response only.
 *
 * What never happens: the token, the body or the caller's key in a log line, a metric, an error
 * message or a store. Logs carry fixed words only. The route is internal and free (no reservation:
 * it is part of a run the router already reserved).
 */
import type { Context, Hono } from "hono";
import { bodyLimit } from "hono/body-limit";

import { admitPrincipal, admitRefusalResponse, agentError, hasScopes, resolveAgentKey, type AgentKeyLookup, type AgentPrincipal, type AgentVars } from "./agent-keys.js";
import type { LogFn } from "./logging.js";
import { derivedSidecarBearer, shardOf, type FetchFn, type McpRelayConfig } from "./mcp-proxy.js";
import { tokenMatches } from "./payouts-app.js";

export const CONNECTOR_TOKEN_PATH = "/v1/internal/router/connector-token";

/** The fixed provider → slot table (docs/api/connector-tokens.md). A pair outside it is `400`. */
export const CONNECTOR_SLOT_BY_PROVIDER = Object.freeze({
  gmail: "gmail-tokenset",
  calendar: "gcal-tokenset",
  github: "github-tokenset",
  linear: "linear-tokenset",
  slack: "slack-tokenset",
} as const);
export type ConnectorProviderId = keyof typeof CONNECTOR_SLOT_BY_PROVIDER;
export type ConnectorSlot = (typeof CONNECTOR_SLOT_BY_PROVIDER)[ConnectorProviderId];

const MAX_BODY_BYTES = 1024;
const RUN_ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
/** What the router accepts as a vendor token (apps/router/src/transport.ts `TOKEN_RE`). */
const TOKEN_RE = /^[\x21-\x7e]{8,4096}$/;
const BODY_FIELDS = new Set(["slot", "provider_id", "run_id"]);

/** The principal rule of the Rust token store (`token_store::valid_principal`). */
export function validPrincipal(principal: string): boolean {
  return /^[a-z0-9][a-z0-9_-]{0,63}$/.test(principal);
}

// --- the vault: the principal's sidecar (E50) ---

/** The sidecar's connector routes (`shogun_mcp::connector_vault`). */
export const SIDECAR_CONNECTOR_TOKEN_PATH = "/v1/internal/connectors/token";
export const SIDECAR_CONNECT_START_PATH = "/v1/internal/connectors/connect/start";
export const SIDECAR_CONNECT_FINISH_PATH = "/v1/internal/connectors/connect/finish";
/** One poll of a device flow (GitHub): the human pressed "Check" (E50 follow-up). */
export const SIDECAR_CONNECT_POLL_PATH = "/v1/internal/connectors/connect/poll";
/** The principal's connections (a free read) and removing one (a narrowing action). */
export const SIDECAR_STATUS_PATH = "/v1/internal/connectors/status";
export const SIDECAR_DISCONNECT_PATH = "/v1/internal/connectors/disconnect";
/** The only page a device flow sends a human to. */
export const DEVICE_VERIFICATION_URI = "https://github.com/login/device";
/** The header naming the principal to the sidecar, by the principal's own sidecar bearer. */
export const SIDECAR_PRINCIPAL_HEADER = "x-shogun-principal-authorization";
/** The router gives this route 5 s; the sidecar's answer (a refresh included) must fit inside. */
export const VAULT_TOKEN_TIMEOUT_MS = 4_500;
/** A code exchange is one vendor round trip; a human is waiting on the callback page. */
export const VAULT_CONNECT_TIMEOUT_MS = 15_000;

/** Why the vault did not answer with a value. `gateway_rejected` = the sidecar refused the
 * gateway's own credential (an operator fault), `transport` = nothing came back, `upstream` =
 * the sidecar answered an error. */
export type VaultUnavailable = { kind: "unavailable"; reason: "gateway_rejected" | "transport" | "upstream" | "not_provisioned" };

export type VaultTokenAnswer =
  | { kind: "token"; accessToken: string; expiresAt: number }
  | { kind: "not_connected" }
  /** The core refuses this principal (purged): a 403 to the router. */
  | { kind: "refused" }
  | VaultUnavailable;

/** What a human does for a device flow: open `verificationUri` and type `userCode`. */
export interface DeviceStep {
  userCode: string;
  verificationUri: string;
  intervalMs: number;
}

export type ConnectStartAnswer =
  | { kind: "started"; authorizeUrl: string; expiresAt: number; device?: DeviceStep }
  | { kind: "not_configured" }
  | { kind: "refused" }
  | VaultUnavailable;

export type ConnectFinishAnswer =
  | { kind: "connected"; provider: string; slot: string }
  /** The flow is unknown, used, expired or another principal's. */
  | { kind: "flow_unknown" }
  /** The provider refused the code or did not grant the scopes the connector needs. */
  | { kind: "rejected" }
  | { kind: "refused" }
  | VaultUnavailable;

export type ConnectPollAnswer =
  | { kind: "pending"; device: DeviceStep; expiresAt: number }
  | { kind: "connected"; provider: string; slot: string }
  | { kind: "flow_unknown" }
  /** The human denied it at the provider, or the code expired. */
  | { kind: "rejected" }
  | { kind: "refused" }
  | VaultUnavailable;

/** One connection of the principal, as the sidecar reports it (never a token or an identity). */
export interface ConnectorStatusRow {
  service: string;
  provider: string | null;
  state: "connected" | "needs_reauth" | "disconnected" | "coming_soon";
  connected: boolean;
  connectable: boolean;
  expires_at: number | null;
}

export type ConnectorStatusAnswer = { kind: "status"; connectors: ConnectorStatusRow[] } | { kind: "refused" } | VaultUnavailable;

export type DisconnectAnswer =
  | { kind: "disconnected"; provider: string; slot: string; removed: boolean; vendorRevoke: string }
  | { kind: "unknown" }
  | { kind: "refused" }
  | VaultUnavailable;

/** The principal as the sidecar needs it: its id and the key whose derived bearer names it. */
export type VaultPrincipal = Pick<AgentPrincipal, "principalId" | "keyId">;

/** Where connector credentials live, seen from the gateway. The default (and only production)
 * implementation is `sidecarConnectorVault`; tests inject fakes. */
export interface ConnectorVault {
  accessToken(principal: VaultPrincipal, slot: ConnectorSlot): Promise<VaultTokenAnswer>;
  connectStart(principal: VaultPrincipal, provider: string, state: string): Promise<ConnectStartAnswer>;
  connectFinish(principal: VaultPrincipal, state: string, code: string): Promise<ConnectFinishAnswer>;
  /** One poll of a device flow (GitHub). Optional: a vault without it serves no device flow. */
  connectPoll?(principal: VaultPrincipal, state: string): Promise<ConnectPollAnswer>;
  /** The principal's connections. Optional: absent = the status route answers 503. */
  status?(principal: VaultPrincipal): Promise<ConnectorStatusAnswer>;
  /** Remove one connection (revoked at the vendor where it can be). */
  disconnect?(principal: VaultPrincipal, provider: string): Promise<DisconnectAnswer>;
}

const USER_CODE_RE = /^[A-Za-z0-9]{4}-[A-Za-z0-9]{4}$/;
const STATES = new Set(["connected", "needs_reauth", "disconnected", "coming_soon"]);

/** A device step the sidecar answered, checked: GitHub's own page and GitHub's code shape. */
function deviceOf(raw: unknown): DeviceStep | undefined {
  if (typeof raw !== "object" || raw === null) return undefined;
  const d = raw as Record<string, unknown>;
  if (d.verification_uri !== DEVICE_VERIFICATION_URI || typeof d.user_code !== "string" || !USER_CODE_RE.test(d.user_code)) return undefined;
  const interval = typeof d.interval_ms === "number" && d.interval_ms > 0 ? d.interval_ms : 5_000;
  return { userCode: d.user_code, verificationUri: DEVICE_VERIFICATION_URI, intervalMs: interval };
}

/** A status row, copied field by field (nothing the sidecar might add passes through). */
function statusRowOf(raw: unknown): ConnectorStatusRow | undefined {
  if (typeof raw !== "object" || raw === null) return undefined;
  const r = raw as Record<string, unknown>;
  if (typeof r.service !== "string" || !/^[a-z]{2,16}$/.test(r.service) || typeof r.state !== "string" || !STATES.has(r.state)) return undefined;
  return {
    service: r.service,
    provider: typeof r.provider === "string" && /^[a-z]{2,16}$/.test(r.provider) ? r.provider : null,
    state: r.state as ConnectorStatusRow["state"],
    connected: r.connected === true,
    connectable: r.connectable === true,
    expires_at: typeof r.expires_at === "number" ? r.expires_at : null,
  };
}

/** The sidecar vault's settings: the `/mcp` relay's transport and bearer source, and the
 * sidecar's admin credential (`SHOGUN_SIDECAR_ADMIN_TOKEN`). */
export interface SidecarVaultConfig {
  relay: McpRelayConfig;
  adminToken: string;
}

interface SidecarReply {
  status: number;
  body: Record<string, unknown>;
}

/** The vault over the principal's sidecar, through the relay's transport (`fetchFn`: global
 * `fetch` to `SHOGUN_SIDECAR_URL`, or the Workers binding's `fetch`). */
export function sidecarConnectorVault(config: SidecarVaultConfig, fetchFn: FetchFn = (input, init) => fetch(input, init)): ConnectorVault {
  const base = config.relay.url.replace(/\/+$/, "");
  const bearerFor = (p: VaultPrincipal): string | undefined => {
    if (config.relay.shard && shardOf(p.principalId, config.relay.shard.count) !== config.relay.shard.index) return undefined;
    return config.relay.tokens?.get(p.principalId) ?? (config.relay.bearerSecret ? derivedSidecarBearer(config.relay.bearerSecret, p) : undefined);
  };

  const post = async (path: string, principal: VaultPrincipal, payload: Record<string, string>, timeoutMs: number): Promise<SidecarReply | VaultUnavailable> => {
    const bearer = bearerFor(principal);
    if (!bearer) return { kind: "unavailable", reason: "not_provisioned" };
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), timeoutMs);
    try {
      const res = await fetchFn(`${base}${path}`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${config.adminToken}`,
          [SIDECAR_PRINCIPAL_HEADER]: `Bearer ${bearer}`,
          "content-type": "application/json",
          accept: "application/json",
        },
        body: JSON.stringify(payload),
        signal: abort.signal,
        redirect: "manual",
      });
      let body: unknown;
      try {
        body = await res.json();
      } catch {
        body = undefined;
      }
      if (res.status === 401 || res.status === 403) return { kind: "unavailable", reason: "gateway_rejected" };
      return { status: res.status, body: typeof body === "object" && body !== null && !Array.isArray(body) ? (body as Record<string, unknown>) : {} };
    } catch {
      return { kind: "unavailable", reason: "transport" };
    } finally {
      clearTimeout(timer);
    }
  };
  const upstream: VaultUnavailable = { kind: "unavailable", reason: "upstream" };
  const isReply = (r: SidecarReply | VaultUnavailable): r is SidecarReply => !("kind" in r);

  return {
    async accessToken(principal, slot) {
      const r = await post(SIDECAR_CONNECTOR_TOKEN_PATH, principal, { slot }, VAULT_TOKEN_TIMEOUT_MS);
      if (!isReply(r)) return r;
      if (r.status === 200 && typeof r.body.access_token === "string" && typeof r.body.expires_at === "number") {
        return { kind: "token", accessToken: r.body.access_token, expiresAt: r.body.expires_at };
      }
      if (r.status === 404 && r.body.error === "connector_not_connected") return { kind: "not_connected" };
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
    async connectStart(principal, provider, state) {
      const r = await post(SIDECAR_CONNECT_START_PATH, principal, { provider, state }, VAULT_CONNECT_TIMEOUT_MS);
      if (!isReply(r)) return r;
      if (r.status === 200 && typeof r.body.authorize_url === "string" && /^https:\/\//.test(r.body.authorize_url) && typeof r.body.expires_at === "number") {
        if (r.body.device !== undefined) {
          const device = deviceOf(r.body.device);
          if (!device || r.body.authorize_url !== DEVICE_VERIFICATION_URI) return upstream;
          return { kind: "started", authorizeUrl: r.body.authorize_url, expiresAt: r.body.expires_at, device };
        }
        return { kind: "started", authorizeUrl: r.body.authorize_url, expiresAt: r.body.expires_at };
      }
      if (r.body.error === "connect_not_configured" || r.body.error === "unknown_connector") return { kind: "not_configured" };
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
    async connectFinish(principal, state, code) {
      const r = await post(SIDECAR_CONNECT_FINISH_PATH, principal, { state, code }, VAULT_CONNECT_TIMEOUT_MS);
      if (!isReply(r)) return r;
      if (r.status === 200 && r.body.connected === true && typeof r.body.provider === "string" && typeof r.body.slot === "string") {
        return { kind: "connected", provider: r.body.provider, slot: r.body.slot };
      }
      if (r.body.error === "connect_flow_unknown") return { kind: "flow_unknown" };
      if (r.body.error === "connect_rejected" || r.body.error === "invalid_request") return { kind: "rejected" };
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
    async connectPoll(principal, state) {
      const r = await post(SIDECAR_CONNECT_POLL_PATH, principal, { state }, VAULT_CONNECT_TIMEOUT_MS);
      if (!isReply(r)) return r;
      if (r.status === 200 && r.body.connected === true && typeof r.body.provider === "string" && typeof r.body.slot === "string") {
        return { kind: "connected", provider: r.body.provider, slot: r.body.slot };
      }
      if (r.status === 200 && r.body.status === "pending" && typeof r.body.expires_at === "number") {
        const device = deviceOf(r.body.device);
        return device ? { kind: "pending", device, expiresAt: r.body.expires_at } : upstream;
      }
      if (r.body.error === "connect_flow_unknown") return { kind: "flow_unknown" };
      if (r.body.error === "connect_rejected" || r.body.error === "invalid_request") return { kind: "rejected" };
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
    async status(principal) {
      const r = await post(SIDECAR_STATUS_PATH, principal, {}, VAULT_TOKEN_TIMEOUT_MS);
      if (!isReply(r)) return r;
      if (r.status === 200 && Array.isArray(r.body.connectors)) {
        const rows = r.body.connectors.map(statusRowOf);
        return rows.every((x): x is ConnectorStatusRow => x !== undefined) ? { kind: "status", connectors: rows } : upstream;
      }
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
    async disconnect(principal, provider) {
      const r = await post(SIDECAR_DISCONNECT_PATH, principal, { provider }, VAULT_CONNECT_TIMEOUT_MS);
      if (!isReply(r)) return r;
      const b = r.body;
      if (r.status === 200 && b.disconnected === true && typeof b.provider === "string" && typeof b.slot === "string" && typeof b.removed === "boolean" && typeof b.vendor_revoke === "string") {
        return { kind: "disconnected", provider: b.provider, slot: b.slot, removed: b.removed, vendorRevoke: b.vendor_revoke };
      }
      if (b.error === "unknown_connector") return { kind: "unknown" };
      if (r.status === 410) return { kind: "refused" };
      return upstream;
    },
  };
}

// --- the route ---

export interface ConnectorTokenDeps {
  /** `ROUTER_SETTLEMENT_TOKEN`. */
  token: string;
  /** How the caller's key resolves (the agent store, plus OAuth access tokens when served). */
  keyLookup: AgentKeyLookup;
  /** The sidecar vault. Absent = no vault on this gateway: every answer is 404. */
  vault?: ConnectorVault;
  /** Operational lines (fixed words). */
  log: LogFn;
  now: () => Date;
}

type ParsedRequest = { slot: ConnectorSlot; providerId: ConnectorProviderId };

function parseRequest(raw: unknown): ParsedRequest | undefined {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return undefined;
  const o = raw as Record<string, unknown>;
  if (Object.keys(o).some((k) => !BODY_FIELDS.has(k))) return undefined;
  const { slot, provider_id: providerId, run_id: runId } = o;
  if (typeof providerId !== "string" || !Object.hasOwn(CONNECTOR_SLOT_BY_PROVIDER, providerId)) return undefined;
  const expected = CONNECTOR_SLOT_BY_PROVIDER[providerId as ConnectorProviderId];
  if (slot !== expected) return undefined;
  if (runId !== undefined && (typeof runId !== "string" || !RUN_ID_RE.test(runId))) return undefined;
  return { slot: expected, providerId: providerId as ConnectorProviderId };
}

const UNAVAILABLE_EVENT = {
  gateway_rejected: "connector_vault_rejected_gateway",
  transport: "connector_token_unavailable",
  upstream: "connector_token_unavailable",
  not_provisioned: "connector_vault_not_provisioned",
} as const;

export function registerConnectorTokenRoute(app: Hono<{ Variables: AgentVars }>, deps: ConnectorTokenDeps): void {
  const notConnected = (c: Context<{ Variables: AgentVars }>, event: NonNullable<AgentVars["relayLogEvent"]>): Response => {
    c.set("relayLogEvent", event);
    return agentError(c, 404, "connector_not_connected", "No account is connected for this connector. A human connects it with POST /v1/connectors/{provider}/connect, then the run is retried.", "authenticate");
  };
  const unavailable = (c: Context<{ Variables: AgentVars }>, event: NonNullable<AgentVars["relayLogEvent"]>): Response => {
    c.set("relayLogEvent", event);
    return agentError(c, 503, "connector_token_unavailable", "The connector credential store did not answer. Retry shortly.", "retry_after", { "retry-after": "5" });
  };

  app.post(CONNECTOR_TOKEN_PATH, bodyLimit({ maxSize: MAX_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", "The body is a slot, a provider id and a run id.") }), async (c) => {
    // The service token first, before the body is read.
    if (!tokenMatches(c.req.header("authorization"), deps.token)) {
      c.set("relayLogEvent", "connector_token_rejected");
      return agentError(c, 401, "invalid_token", "This route takes the router's service token.");
    }
    let raw: unknown;
    try {
      raw = await c.req.json();
    } catch {
      raw = undefined;
    }
    const req = parseRequest(raw);
    if (!req) {
      return agentError(c, 400, "invalid_connector_token_request", "The body must be {\"slot\", \"provider_id\", \"run_id\"?} with a slot that belongs to the provider and no other field.", "fix_input");
    }
    const nowMs = deps.now().getTime();
    // The caller's own key, forwarded by the router: verified here, never stored or logged.
    const resolved = await resolveAgentKey(c.req.header("x-caller-authorization"), deps.keyLookup, nowMs);
    if (!resolved.ok) {
      c.set("relayLogEvent", "agent_key_rejected");
      return agentError(c, 401, resolved.code, "The caller's agent key is not valid. Send it as the run's bearer.", "authenticate");
    }
    const principal = resolved.principal;
    c.set("relayLogAgentKey", principal.keyId);
    const admit = await admitPrincipal(deps.keyLookup, principal, nowMs);
    if (admit) return admitRefusalResponse(c, admit);
    if (!hasScopes(principal.scopes, ["run"])) return agentError(c, 403, "insufficient_scope", "This key lacks the run scope.");

    if (!deps.vault) return notConnected(c, "connector_vault_unconfigured");
    if (!validPrincipal(principal.principalId)) return notConnected(c, "connector_not_connected");
    let answer: VaultTokenAnswer;
    try {
      answer = await deps.vault.accessToken(principal, req.slot);
    } catch {
      answer = { kind: "unavailable", reason: "transport" };
    }
    switch (answer.kind) {
      case "not_connected":
        return notConnected(c, "connector_not_connected");
      case "refused":
        c.set("relayLogEvent", "connector_principal_refused");
        return agentError(c, 403, "principal_unavailable", "This principal's store is closed on this deployment. Ask the operator.", "contact_operator");
      case "unavailable":
        return unavailable(c, UNAVAILABLE_EVENT[answer.reason]);
      case "token":
        break;
    }
    // The router refuses a token it could not send, and one already expired: never hand one out.
    if (!TOKEN_RE.test(answer.accessToken)) return notConnected(c, "connector_token_unreadable");
    if (answer.expiresAt <= nowMs) return notConnected(c, "connector_token_expired");
    c.set("relayLogEvent", "connector_token_served");
    return c.json(
      { access_token: answer.accessToken, token_type: "Bearer", expires_at: Math.min(Math.trunc(answer.expiresAt), Number.MAX_SAFE_INTEGER) },
      200,
      { "cache-control": "no-store", pragma: "no-cache" },
    );
  });
}
