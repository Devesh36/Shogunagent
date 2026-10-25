/**
 * The transport (E8-T9; invariant 7): the router's ONLY egress and the only module that reads the
 * credential environment map. The engine hands it a `RunRequest` built by the connector (no
 * credential field exists on that type) and a `ProviderRef` naming an environment variable; the
 * transport resolves the value, injects it into the outgoing request and returns status + body.
 * Response headers are not returned, so nothing the vendor echoes back can leak into the engine.
 *
 * `fetch` is called here and nowhere else in src/ (the ADR's TypeScript analogue of invariant 9).
 *
 * Per-caller connector credentials (E48): a `caller_connector` provider (the first-party Gmail,
 * Calendar, GitHub, Linear and Slack connectors) acts as the CALLING principal's own account. In
 * production the transport asks apps/api for that principal's access token at run time
 * (`POST /v1/internal/router/connector-token`, docs/api/connector-tokens.md), injects it into the
 * one outgoing request and drops it: it is never cached, returned, logged or stored. The Worker
 * env is read for these providers only in development (`ROUTER_DEV_SHARED_CREDENTIALS=1` with no
 * `ROUTER_WALLET_URL`), where one shared account is acceptable.
 */
import { createHash, timingSafeEqual } from "node:crypto";

import type { ConnectorSlot, Json, ProviderAuth, RunRequest } from "../connectors/types.js";
import type { Secret } from "./secret.js";
import type { RunSettlement } from "./settlement.js";
import type { WalletTransport } from "./wallet.js";
import type { SqlConnector, SqlRow } from "./outbox.js";
import postgres from "postgres";

export interface ProviderRef {
  id: string;
  auth: ProviderAuth;
  baseUrl: string;
  baseUrlEnv?: string;
}

export interface TransportCall {
  provider: ProviderRef;
  request: RunRequest;
  /** The agent's own bearer, forwarded only for `caller_bearer` providers, and presented to apps/api
   * (never to the vendor) to fetch the principal's token for `caller_connector` providers. */
  caller?: { bearer?: Secret };
  /** The run's id, sent with a connector-token request for correlation (not a secret). */
  runId?: string;
}

export interface TransportResult {
  status: number;
  body: Json;
}

export interface Transport {
  execute(call: TransportCall): Promise<TransportResult>;
}

export type TransportErrorCode =
  | "credential_missing"
  | "network"
  | "timeout"
  | "invalid_response"
  | "caller_bearer_missing"
  /** E48: the calling principal has no account connected for this connector (apps/api 404). */
  | "connector_not_connected"
  /** E48: apps/api refused the caller's key for a connector token (401/403). */
  | "caller_unauthorized"
  /** E48: apps/api's connector-token route did not answer usably (5xx, network, timeout). */
  | "connector_token_unavailable"
  /** E48: this router has no source of per-caller connector credentials (no `ROUTER_WALLET_URL`,
   * and the development flag is off). */
  | "connector_unconfigured";

export class TransportError extends Error {
  constructor(
    readonly code: TransportErrorCode,
    message: string,
  ) {
    super(message);
  }
}

export type CredentialEnv = Readonly<Record<string, string | undefined>>;

/**
 * Where `caller_connector` tokens come from (E48):
 * - `per_caller` (production): apps/api at `baseUrl`, per run, for the caller's principal.
 * - `dev_shared` (development only): the Worker env variable the provider names. One account for
 *   every caller, so src/worker.ts allows it only without `ROUTER_WALLET_URL`.
 * - `none`: refused with `connector_unconfigured`.
 */
export type ConnectorCredentialSource = { mode: "per_caller"; baseUrl: string; timeoutMs?: number } | { mode: "dev_shared" } | { mode: "none" };

export interface FetchTransportOptions {
  env: CredentialEnv;
  fetch?: typeof fetch;
  defaultTimeoutMs?: number;
  /** Default `none`. */
  connectors?: ConnectorCredentialSource;
}

/** Decide the connector credential source from the Worker env (src/worker.ts logs the result).
 * `ROUTER_WALLET_URL` set means a production deployment: always per caller, and the development
 * flag is ignored (reported as `ignored_flag` so the boot log says so). */
export function connectorSourceFor(env: CredentialEnv): { source: ConnectorCredentialSource; ignoredFlag: boolean } {
  const devFlag = env["ROUTER_DEV_SHARED_CREDENTIALS"] === "1";
  const wallet = env["ROUTER_WALLET_URL"];
  if (wallet) return { source: { mode: "per_caller", baseUrl: wallet }, ignoredFlag: devFlag };
  return { source: devFlag ? { mode: "dev_shared" } : { mode: "none" }, ignoredFlag: false };
}

function joinUrl(base: string, path: string, query?: Record<string, string>): URL {
  const url = new URL(path.replace(/^\//, ""), base.endsWith("/") ? base : `${base}/`);
  for (const [k, v] of Object.entries(query ?? {})) url.searchParams.set(k, v);
  return url;
}

/** Characters a vendor token may hold: visible ASCII, bounded. Anything else is not sent. */
const TOKEN_RE = /^[\x21-\x7e]{8,4096}$/;

/**
 * Fetch the calling principal's access token for `slot` from apps/api
 * (docs/api/connector-tokens.md). The router presents `ROUTER_SETTLEMENT_TOKEN` and forwards the
 * caller's bearer in `X-Caller-Authorization`; apps/api resolves the principal from that key, so
 * the router never names a principal and cannot ask for anyone else's token. Returns the token
 * value to `inject` only; nothing about it is logged.
 */
async function fetchConnectorToken(opts: { env: CredentialEnv; baseUrl: string; fetch: typeof fetch; timeoutMs: number }, slot: ConnectorSlot, providerId: string, runId: string | undefined, caller: Secret): Promise<string> {
  const service = opts.env["ROUTER_SETTLEMENT_TOKEN"];
  if (!service) throw new TransportError("credential_missing", "ROUTER_SETTLEMENT_TOKEN is not configured");
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), opts.timeoutMs);
  let res: Response;
  try {
    res = await opts.fetch(joinUrl(opts.baseUrl, "/v1/internal/router/connector-token"), {
      method: "POST",
      headers: { authorization: `Bearer ${service}`, "x-caller-authorization": `Bearer ${caller.expose()}`, "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ slot, provider_id: providerId, ...(runId ? { run_id: runId } : {}) }),
      signal: controller.signal,
    });
  } catch {
    throw new TransportError("connector_token_unavailable", "the connector token service is unreachable");
  } finally {
    clearTimeout(timer);
  }
  if (res.status === 404) {
    await res.body?.cancel().catch(() => undefined);
    throw new TransportError("connector_not_connected", "no account is connected for this connector");
  }
  if (res.status === 401 || res.status === 403) {
    await res.body?.cancel().catch(() => undefined);
    throw new TransportError("caller_unauthorized", "the caller's key may not use this connector");
  }
  if (res.status !== 200) {
    await res.body?.cancel().catch(() => undefined);
    throw new TransportError("connector_token_unavailable", "the connector token service did not answer");
  }
  let parsed: unknown;
  try {
    parsed = await res.json();
  } catch {
    throw new TransportError("connector_token_unavailable", "the connector token answer is not JSON");
  }
  const body = (parsed && typeof parsed === "object" ? parsed : {}) as Record<string, unknown>;
  const token = body["access_token"];
  const expiresAt = body["expires_at"];
  if (typeof token !== "string" || !TOKEN_RE.test(token)) throw new TransportError("connector_token_unavailable", "the connector token answer is malformed");
  if (typeof expiresAt === "number" && expiresAt <= Date.now()) throw new TransportError("connector_token_unavailable", "the connector token answer is already expired");
  return token;
}

/** Applies the provider's auth spec. Returns the headers/query additions; never logs. */
async function inject(auth: ProviderAuth, ctx: { env: CredentialEnv; connectors: ConnectorCredentialSource; fetch: typeof fetch; providerId: string; runId: string | undefined }, caller: TransportCall["caller"], url: URL, headers: Headers): Promise<void> {
  const env = ctx.env;
  switch (auth.kind) {
    case "none":
      return;
    case "bearer": {
      const v = env[auth.credential_env];
      if (!v) throw new TransportError("credential_missing", `provider credential ${auth.credential_env} is not configured`);
      headers.set("authorization", `Bearer ${v}`);
      return;
    }
    case "header": {
      const v = env[auth.credential_env];
      if (!v) throw new TransportError("credential_missing", `provider credential ${auth.credential_env} is not configured`);
      headers.set(auth.header, v);
      return;
    }
    case "query": {
      const v = env[auth.credential_env];
      if (!v) throw new TransportError("credential_missing", `provider credential ${auth.credential_env} is not configured`);
      url.searchParams.set(auth.param, v);
      return;
    }
    case "caller_bearer": {
      const bearer = caller?.bearer;
      if (!bearer) throw new TransportError("caller_bearer_missing", "this provider needs the caller's bearer");
      headers.set("authorization", `Bearer ${bearer.expose()}`);
      return;
    }
    case "caller_connector": {
      const source = ctx.connectors;
      if (source.mode === "dev_shared") {
        const v = env[auth.credential_env];
        if (!v) throw new TransportError("credential_missing", `provider credential ${auth.credential_env} is not configured`);
        headers.set("authorization", `Bearer ${v}`);
        return;
      }
      if (source.mode === "none") throw new TransportError("connector_unconfigured", "no per-caller connector credentials are configured on this router");
      const bearer = caller?.bearer;
      if (!bearer) throw new TransportError("caller_bearer_missing", "this provider needs the caller's bearer");
      const token = await fetchConnectorToken({ env, baseUrl: source.baseUrl, fetch: ctx.fetch, timeoutMs: source.timeoutMs ?? 5_000 }, auth.slot, ctx.providerId, ctx.runId, bearer);
      headers.set("authorization", `Bearer ${token}`);
      return;
    }
  }
}

export function createFetchTransport(opts: FetchTransportOptions): Transport {
  const doFetch = opts.fetch ?? fetch;
  const defaultTimeout = opts.defaultTimeoutMs ?? 30_000;
  const connectors = opts.connectors ?? { mode: "none" };
  return {
    async execute(call) {
      const { provider, request } = call;
      const base = (provider.baseUrlEnv && opts.env[provider.baseUrlEnv]) || provider.baseUrl;
      const url = joinUrl(base, request.path, request.query);
      const headers = new Headers(request.headers ?? {});
      headers.set("accept", "application/json");
      await inject(provider.auth, { env: opts.env, connectors, fetch: doFetch, providerId: provider.id, runId: call.runId }, call.caller, url, headers);
      const init: RequestInit = { method: request.method, headers };
      if (request.body !== undefined && request.method !== "GET") {
        headers.set("content-type", "application/json");
        init.body = JSON.stringify(request.body);
      }
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), request.timeout_ms ?? defaultTimeout);
      init.signal = controller.signal;
      let res: Response;
      try {
        res = await doFetch(url, init);
      } catch (e) {
        clearTimeout(timer);
        if (controller.signal.aborted) throw new TransportError("timeout", "provider did not answer in time");
        throw new TransportError("network", "provider unreachable");
      }
      clearTimeout(timer);
      const text = await res.text();
      let body: Json;
      try {
        body = text ? (JSON.parse(text) as Json) : null;
      } catch {
        body = text;
      }
      return { status: res.status, body };
    },
  };
}

/**
 * The settlement poster (E29, E33): POSTs run settlements from the outbox (src/outbox.ts) to
 * apps/api (`POST /v1/internal/router/settlements`), which settles the caller's wallet and the
 * provider's accrual. Lives here because this module is the router's only
 * egress and the only reader of the secret map: `ROUTER_SETTLEMENT_TOKEN` is read at send time
 * and never leaves this function. Settlements are content-free (src/settlement.ts).
 */
export function createSettlementPoster(opts: { env: CredentialEnv; url: string; fetch?: typeof fetch; timeoutMs?: number }): (settlements: RunSettlement[]) => Promise<{ status: number }> {
  const doFetch = opts.fetch ?? fetch;
  return async (settlements) => {
    const token = opts.env["ROUTER_SETTLEMENT_TOKEN"];
    if (!token) throw new TransportError("credential_missing", "ROUTER_SETTLEMENT_TOKEN is not configured");
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), opts.timeoutMs ?? 10_000);
    try {
      const res = await doFetch(opts.url, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json", accept: "application/json" },
        body: JSON.stringify({ settlements }),
        signal: controller.signal,
      });
      return { status: res.status };
    } finally {
      clearTimeout(timer);
    }
  };
}

/**
 * The wallet transport (E32): `POST /v1/internal/router/reserve` on apps/api. The router presents
 * `ROUTER_SETTLEMENT_TOKEN` (read here, at send time) and forwards the caller's own bearer in
 * `X-Caller-Authorization` so apps/api verifies the key and its `run` scope itself. The bearer is
 * exposed here and nowhere else on this path; the body is content-free (ids, a price, a flag).
 * Only the status, the JSON body (a reservation or a 402) and `Retry-After` come back.
 */
export function createWalletTransport(opts: { env: CredentialEnv; baseUrl: string; fetch?: typeof fetch; timeoutMs?: number }): WalletTransport {
  const doFetch = opts.fetch ?? fetch;
  const url = joinUrl(opts.baseUrl, "/v1/internal/router/reserve");
  return {
    async reserve(body, caller) {
      const token = opts.env["ROUTER_SETTLEMENT_TOKEN"];
      if (!token) throw new TransportError("credential_missing", "ROUTER_SETTLEMENT_TOKEN is not configured");
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), opts.timeoutMs ?? 5_000);
      try {
        const res = await doFetch(url, {
          method: "POST",
          headers: { authorization: `Bearer ${token}`, "x-caller-authorization": `Bearer ${caller.expose()}`, "content-type": "application/json", accept: "application/json" },
          body: JSON.stringify(body),
          signal: controller.signal,
        });
        const text = await res.text();
        let parsed: Json = null;
        try {
          parsed = text ? (JSON.parse(text) as Json) : null;
        } catch {
          parsed = null;
        }
        const retry = Number(res.headers.get("retry-after"));
        return { status: res.status, body: parsed, ...(Number.isFinite(retry) && retry > 0 ? { retryAfter: retry } : {}) };
      } finally {
        clearTimeout(timer);
      }
    },
  };
}

/**
 * The outbox's database connection (E33): Postgres through the Worker's Hyperdrive binding. Workers
 * do not let a socket opened by one request serve another, so each operation opens one connection
 * (`max: 1`; Hyperdrive pools behind it) and closes it after. The driver never prints a notice or a
 * query; rows hold content-free settlements only (apps/router/sql/settlement-outbox.sql).
 */
export function postgresConnector(connectionString: string): SqlConnector {
  return async (fn) => {
    const sql = postgres(connectionString, { max: 1, fetch_types: false, prepare: false, idle_timeout: 5, connect_timeout: 5, onnotice: () => undefined });
    try {
      return await fn({
        query: async (text, params = []) => [...(await sql.unsafe(text, params as postgres.ParameterOrJSON<never>[]))] as SqlRow[],
      });
    } finally {
      await sql.end({ timeout: 1 }).catch(() => undefined);
    }
  };
}

/** Parse `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`, values URL-decoded). Throws a message that
 * names the variable and never its value. */
export function parseOtlpHeaders(raw: string | undefined): Record<string, string> {
  const headers: Record<string, string> = {};
  const text = raw?.trim();
  if (!text) return headers;
  for (const pair of text.split(",")) {
    const i = pair.indexOf("=");
    const key = i > 0 ? pair.slice(0, i).trim().toLowerCase() : "";
    let value: string | undefined;
    try {
      value = i > 0 ? decodeURIComponent(pair.slice(i + 1).trim()) : undefined;
    } catch {
      value = undefined;
    }
    if (!/^[a-z0-9-]+$/.test(key) || value === undefined || /[\r\n]/.test(value)) throw new TransportError("credential_missing", "invalid OTEL_EXPORTER_OTLP_HEADERS: expected k=v,k2=v2");
    headers[key] = value;
  }
  return headers;
}

/**
 * The OTLP span poster (E37): POSTs one OTLP JSON body of body-free spans to the operator's
 * collector. Lives here for the same reason as the settlement poster: `OTEL_EXPORTER_OTLP_HEADERS`
 * (the collector's credential) is read at send time and never leaves this function. Only the
 * collector's status is read back.
 */
export function createOtlpPoster(opts: { env: CredentialEnv; url: string; fetch?: typeof fetch; timeoutMs?: number }): (body: string) => Promise<{ status: number }> {
  const doFetch = opts.fetch ?? fetch;
  return async (body) => {
    const headers = parseOtlpHeaders(opts.env["OTEL_EXPORTER_OTLP_HEADERS"]);
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), opts.timeoutMs ?? 5_000);
    try {
      const res = await doFetch(opts.url, { method: "POST", headers: { ...headers, "content-type": "application/json" }, body, signal: controller.signal });
      await res.body?.cancel().catch(() => undefined);
      return { status: res.status };
    } finally {
      clearTimeout(timer);
    }
  };
}

/**
 * The `/metrics` bearer check (E37): `ROUTER_METRICS_TOKEN` is a secret, so it is read here.
 * `disabled` when unset (the route answers 404), else a constant-time digest comparison.
 */
export function createMetricsAuthorizer(env: CredentialEnv): (authorization: string | undefined) => "ok" | "unauthorized" | "disabled" {
  const token = env["ROUTER_METRICS_TOKEN"];
  const expected = token ? createHash("sha256").update(token).digest() : undefined;
  return (authorization) => {
    if (!expected) return "disabled";
    const presented = (authorization ?? "").replace(/^Bearer\s+/i, "");
    return timingSafeEqual(createHash("sha256").update(presented).digest(), expected) ? "ok" : "unauthorized";
  };
}

/** A transport that answers from a function — fixtures in tests, no network. */
export function createReplayTransport(responder: (call: TransportCall) => TransportResult | Promise<TransportResult>): Transport {
  return { execute: (call) => Promise.resolve(responder(call)) };
}
