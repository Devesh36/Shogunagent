/** Single-user, loopback-only Gmail test relay. No project key reaches the desktop. */
import { timingSafeEqual } from "node:crypto";
import { Hono } from "hono";
import { bodyLimit } from "hono/body-limit";
import { TokenBucketLimiter } from "./ratelimit.js";

export const GMAIL_PORT = 4319;
const MAX_RESPONSE = 2 * 1024 * 1024;
const ID = /^[A-Za-z0-9_-]{1,160}$/;
type ObjectValue = Record<string, unknown>;
export interface GmailState {
  toolVersion: string | null;
  userId: string;
  authConfigId: string;
  connectedAccountId: string | null;
  consentGrantedAt: string | null;
  lastVerifiedAt: string | null;
  lastReadLogId?: string | null;
}
export interface GmailStore {
  load(): GmailState;
  save(state: GmailState): void;
}
export interface GmailProvider {
  call(method: string, path: string, body?: ObjectValue): Promise<ObjectValue>;
}
export async function verifyGmailConfiguration(api: GmailProvider, state: GmailState): Promise<string> {
  const auth = await api.call("GET", `/auth_configs/${encodeURIComponent(state.authConfigId)}`);
  if (auth.id !== state.authConfigId || object(auth.toolkit).slug !== "gmail" || auth.auth_scheme !== "OAUTH2" ||
      auth.status !== "ENABLED" || auth.is_composio_managed !== true) throw new GmailError("invalid_gmail_auth_config", 400);
  let version = state.toolVersion;
  for (const tool of ["GMAIL_FETCH_EMAILS", "GMAIL_FETCH_MESSAGE_BY_THREAD_ID", "GMAIL_LIST_LABELS"]) {
    const metadata = await api.call("GET", `/tools/${tool}${version ? `?version=${version}` : ""}`);
    if (metadata.slug !== tool || object(metadata.toolkit).slug !== "gmail" || metadata.is_deprecated === true ||
        typeof metadata.version !== "string" || !/^\d{8}_\d{2}$/.test(metadata.version)) throw new GmailError("invalid_gmail_tool_metadata", 400);
    if (version && metadata.version !== version) throw new GmailError("gmail_tool_version_mismatch", 400);
    version = metadata.version;
  }
  if (!version) throw new GmailError("invalid_gmail_tool_version", 400);
  return version;
}
export class GmailError extends Error {
  constructor(readonly code: string, readonly status: 400 | 401 | 403 | 409 | 429 | 502 | 503 | 504) {
    super(code);
  }
}
export function object(value: unknown): ObjectValue {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new GmailError("invalid_input", 400);
  return value as ObjectValue;
}
function exact(value: ObjectValue, keys: string[]): void {
  if (Object.keys(value).some((key) => !keys.includes(key))) throw new GmailError("invalid_input", 400);
}
export function connectUrl(value: unknown): string {
  if (typeof value !== "string") throw new GmailError("invalid_provider_link", 502);
  let url: URL;
  try { url = new URL(value); } catch { throw new GmailError("invalid_provider_link", 502); }
  if (url.origin !== "https://connect.composio.dev" || url.username || url.password || !url.pathname.startsWith("/link/")) {
    throw new GmailError("invalid_provider_link", 502);
  }
  return url.href;
}
export class FetchGmailProvider implements GmailProvider {
  constructor(private readonly apiKey: string, private readonly fetcher: typeof fetch = fetch) {}
  async call(method: string, path: string, body?: ObjectValue): Promise<ObjectValue> {
    // Fixed upstream, no redirects, bounded deadline/response. Never echo provider errors.
    try {
      const response = await this.fetcher(`https://backend.composio.dev/api/v3${path}`, {
        method, headers: { "x-api-key": this.apiKey, "content-type": "application/json" },
        ...(body ? { body: JSON.stringify(body) } : {}),
        redirect: "error", signal: AbortSignal.timeout(10_000),
      });
      if (!response.ok) {
        await response.body?.cancel();
        if (response.status === 404 && method === "GET" && /^\/connected_accounts\/ca_[A-Za-z0-9_-]+$/.test(path)) {
          throw new GmailError("connection_missing", 409);
        }
        throw new GmailError(response.status === 429 ? "provider_rate_limited" : "provider_request_failed", response.status === 429 ? 429 : 502);
      }
      const reader = response.body?.getReader();
      if (!reader) throw new GmailError("invalid_provider_response", 502);
      let size = 0;
      const chunks: Uint8Array[] = [];
      while (true) {
        const part = await reader.read();
        if (part.done) break;
        size += part.value.byteLength;
        if (size > MAX_RESPONSE) {
          await reader.cancel();
          throw new GmailError("provider_response_too_large", 502);
        }
        chunks.push(part.value);
      }
      return object(JSON.parse(Buffer.concat(chunks).toString("utf8")));
    } catch (error) {
      if (error instanceof GmailError) throw error;
      throw new GmailError("provider_unavailable", 502);
    }
  }
}

/** Validate the small read-only surface, rather than exposing a generic Composio proxy. */
export function readArguments(tool: unknown, value: unknown): ObjectValue {
  const args = object(value);
  if (args.user_id !== "me") throw new GmailError("invalid_mailbox", 400);
  if (tool === "GMAIL_FETCH_EMAILS") {
    exact(args, ["user_id", "max_results", "include_payload", "query", "page_token"]);
    const max = args.max_results;
    if (typeof max !== "number" || !Number.isInteger(max) || max < 1 || max > 15 || args.include_payload !== true) {
      throw new GmailError("invalid_input", 400);
    }
    for (const key of ["query", "page_token"]) {
      if (args[key] !== undefined && (typeof args[key] !== "string" || (args[key] as string).length > 2048)) throw new GmailError("invalid_input", 400);
    }
  } else if (tool === "GMAIL_FETCH_MESSAGE_BY_THREAD_ID") {
    exact(args, ["user_id", "thread_id"]);
    if (typeof args.thread_id !== "string" || !/^[a-fA-F0-9]{1,64}$/.test(args.thread_id)) throw new GmailError("invalid_input", 400);
  } else if (tool === "GMAIL_LIST_LABELS") {
    exact(args, ["user_id"]);
  } else {
    throw new GmailError("gmail_test_is_read_only", 403);
  }
  return args;
}

export function createLocalGmailApp(options: {
  bearer: string; store: GmailStore; provider: GmailProvider; toolVersion: string;
  now?: () => number;
}) {
  if (!/^[a-f0-9]{64}$/.test(options.bearer)) throw new Error("invalid_local_bearer");
  if (!/^\d{8}_\d{2}$/.test(options.toolVersion)) throw new Error("invalid_gmail_tool_version");
  const app = new Hono();
  const now = options.now ?? Date.now;
  const limiter = new TokenBucketLimiter(12, 60);
  let busy = false;
  let pendingLink: { url: string; expiresAt: number; accountId: string } | null = null;
  app.use("*", async (c, next) => {
    c.header("Cache-Control", "no-store");
    c.header("X-Content-Type-Options", "nosniff");
    c.header("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'");
    c.header("Referrer-Policy", "no-referrer");
    if (c.req.header("host") !== `127.0.0.1:${GMAIL_PORT}`) return c.json({ error: "invalid_host" }, 403);
    if (c.req.path === "/callback" && c.req.method === "GET") {
      // A callback is merely a landing page. Query parameters never change authorization.
      return c.text("Return to ShogunAI. It will verify Gmail authorization with Composio.");
    }
    if (c.req.header("origin")) return c.json({ error: "browser_api_access_denied" }, 403);
    const actual = Buffer.from(c.req.header("authorization") ?? "");
    const expected = Buffer.from(`Bearer ${options.bearer}`);
    if (actual.length !== expected.length || !timingSafeEqual(actual, expected)) return c.json({ error: "unauthorized" }, 401);
    if (!limiter.take("local-user", now())) {
      c.header("Retry-After", String(limiter.retryAfterSecs("local-user", now())));
      return c.json({ error: "rate_limited" }, 429);
    }
    if (busy) return c.json({ error: "request_in_progress" }, 409);
    busy = true;
    try { await next(); } finally { busy = false; }
  });
  app.use("*", bodyLimit({ maxSize: 8192, onError: (c) => c.json({ error: "body_too_large" }, 413) }));
  app.onError((error, c) => {
    const safe = error instanceof GmailError ? error : new GmailError("local_backend_failed", 503);
    return c.json({ error: safe.code }, safe.status);
  });
  app.notFound((c) => c.json({ error: "not_found" }, 404));

  async function account(state: GmailState, allowDisabled = false): Promise<ObjectValue | null> {
    if (!state.connectedAccountId) return null;
    let result: ObjectValue;
    try { result = await options.provider.call("GET", `/connected_accounts/${encodeURIComponent(state.connectedAccountId)}`); }
    catch (error) {
      if (error instanceof GmailError && error.code === "connection_missing") return null;
      throw error;
    }
    if (result.id !== state.connectedAccountId || result.user_id !== state.userId ||
        object(result.toolkit).slug !== "gmail" || object(result.auth_config).id !== state.authConfigId ||
        (!allowDisabled && (result.is_disabled === true || object(result.auth_config).is_disabled === true)) ||
        (result.experimental !== undefined && object(result.experimental).account_type !== "PRIVATE")) {
      throw new GmailError("connection_ownership_mismatch", 403);
    }
    return result;
  }
  async function parseBody(c: { req: { header(name: string): string | undefined; json(): Promise<unknown> } }): Promise<ObjectValue> {
    if (c.req.header("content-type")?.split(";")[0] !== "application/json") throw new GmailError("json_required", 400);
    try { return object(await c.req.json()); } catch { throw new GmailError("invalid_input", 400); }
  }
  app.get("/healthz", (c) => c.json({ ok: true, mode: "local-single-user", gmail: "read-only" }));
  app.post("/v1/gmail/connect", async (c) => {
    const body = await parseBody(c);
    exact(body, ["consent"]);
    if (body.consent !== true) throw new GmailError("consent_required", 403);
    const state = options.store.load();
    const current = await account(state);
    if (current?.status === "ACTIVE") {
      options.store.save({ ...state, consentGrantedAt: new Date(now()).toISOString() });
      return c.json({ status: "ACTIVE", redirect_url: null });
    }
    if (pendingLink && pendingLink.expiresAt > now() && pendingLink.accountId === state.connectedAccountId &&
        ["INITIATED", "INITIALIZING"].includes(String(current?.status))) {
      return c.json({ status: "INITIATED", redirect_url: pendingLink.url });
    }
    const link = await options.provider.call("POST", "/connected_accounts/link", {
      user_id: state.userId, auth_config_id: state.authConfigId,
      callback_url: `http://127.0.0.1:${GMAIL_PORT}/callback`,
    });
    if (typeof link.connected_account_id !== "string" || !ID.test(link.connected_account_id) || !link.connected_account_id.startsWith("ca_")) throw new GmailError("invalid_provider_response", 502);
    const url = connectUrl(link.redirect_url);
    options.store.save({ ...state, connectedAccountId: link.connected_account_id, consentGrantedAt: new Date(now()).toISOString(), lastVerifiedAt: null, lastReadLogId: null });
    pendingLink = { url, expiresAt: now() + 10 * 60_000, accountId: link.connected_account_id };
    return c.json({ status: "INITIATED", redirect_url: url });
  });
  app.get("/v1/gmail/status", async (c) => {
    const state = options.store.load();
    const current = await account(state);
    const status = typeof current?.status === "string" ? current.status : "DISCONNECTED";
    return c.json({ status, last_verified_at: state.lastVerifiedAt });
  });
  app.post("/v1/gmail/execute", async (c) => {
    const body = await parseBody(c);
    exact(body, ["tool", "arguments"]);
    const args = readArguments(body.tool, body.arguments);
    const state = options.store.load();
    if (!state.consentGrantedAt) throw new GmailError("consent_required", 403);
    if ((await account(state))?.status !== "ACTIVE") throw new GmailError("gmail_not_connected", 409);
    const result = await options.provider.call("POST", `/tools/execute/${String(body.tool)}`, {
      user_id: state.userId, connected_account_id: state.connectedAccountId,
      version: options.toolVersion, arguments: args,
    });
    if (result.successful !== true || result.error) throw new GmailError("gmail_read_failed", 502);
    const logId = typeof result.log_id === "string" && /^log_[A-Za-z0-9_-]{1,150}$/.test(result.log_id) ? result.log_id : null;
    options.store.save({ ...state, lastVerifiedAt: new Date(now()).toISOString(), lastReadLogId: logId });
    // Explicit projection: no credential metadata, provider error, or link token returned.
    return c.json({ successful: true, data: result.data, error: null, ...(logId ? { log_id: logId } : {}) });
  });
  app.delete("/v1/gmail/connection", async (c) => {
    const state = options.store.load();
    // Stop access durably BEFORE remote deletion. Retry keeps the ID until cleanup succeeds.
    options.store.save({ ...state, consentGrantedAt: null, lastVerifiedAt: null, lastReadLogId: null });
    if (state.connectedAccountId && await account(state, true)) {
      await options.provider.call("DELETE", `/connected_accounts/${encodeURIComponent(state.connectedAccountId)}`);
    }
    options.store.save({ ...state, connectedAccountId: null, consentGrantedAt: null, lastVerifiedAt: null, lastReadLogId: null });
    pendingLink = null;
    return c.json({ disconnected: true });
  });
  return app;
}
