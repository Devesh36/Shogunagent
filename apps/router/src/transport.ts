/**
 * The transport (E8-T9; invariant 7): the router's ONLY egress and the only module that reads the
 * credential environment map. The engine hands it a `RunRequest` built by the connector (no
 * credential field exists on that type) and a `ProviderRef` naming an environment variable; the
 * transport resolves the value, injects it into the outgoing request and returns status + body.
 * Response headers are not returned, so nothing the vendor echoes back can leak into the engine.
 *
 * `fetch` is called here and nowhere else in src/ (the ADR's TypeScript analogue of invariant 9).
 */
import type { Json, ProviderAuth, RunRequest } from "../connectors/types.js";
import type { Secret } from "./secret.js";

export interface ProviderRef {
  id: string;
  auth: ProviderAuth;
  baseUrl: string;
  baseUrlEnv?: string;
}

export interface TransportCall {
  provider: ProviderRef;
  request: RunRequest;
  /** The agent's own bearer, forwarded only for `caller_bearer` providers. */
  caller?: { bearer?: Secret };
}

export interface TransportResult {
  status: number;
  body: Json;
}

export interface Transport {
  execute(call: TransportCall): Promise<TransportResult>;
}

export type TransportErrorCode = "credential_missing" | "network" | "timeout" | "invalid_response" | "caller_bearer_missing";

export class TransportError extends Error {
  constructor(
    readonly code: TransportErrorCode,
    message: string,
  ) {
    super(message);
  }
}

export type CredentialEnv = Readonly<Record<string, string | undefined>>;

export interface FetchTransportOptions {
  env: CredentialEnv;
  fetch?: typeof fetch;
  defaultTimeoutMs?: number;
}

function joinUrl(base: string, path: string, query?: Record<string, string>): URL {
  const url = new URL(path.replace(/^\//, ""), base.endsWith("/") ? base : `${base}/`);
  for (const [k, v] of Object.entries(query ?? {})) url.searchParams.set(k, v);
  return url;
}

/** Applies the provider's auth spec. Returns the headers/query additions; never logs. */
function inject(auth: ProviderAuth, env: CredentialEnv, caller: TransportCall["caller"], url: URL, headers: Headers): void {
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
  }
}

export function createFetchTransport(opts: FetchTransportOptions): Transport {
  const doFetch = opts.fetch ?? fetch;
  const defaultTimeout = opts.defaultTimeoutMs ?? 30_000;
  return {
    async execute(call) {
      const { provider, request } = call;
      const base = (provider.baseUrlEnv && opts.env[provider.baseUrlEnv]) || provider.baseUrl;
      const url = joinUrl(base, request.path, request.query);
      const headers = new Headers(request.headers ?? {});
      headers.set("accept", "application/json");
      inject(provider.auth, opts.env, call.caller, url, headers);
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

/** A transport that answers from a function — fixtures in tests, no network. */
export function createReplayTransport(responder: (call: TransportCall) => TransportResult | Promise<TransportResult>): Transport {
  return { execute: (call) => Promise.resolve(responder(call)) };
}
