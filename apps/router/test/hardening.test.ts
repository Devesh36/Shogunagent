/**
 * E48 pre-launch hardening of the router: the wallet's `Retry-After` reaches the agent, the boot
 * log says how the Worker is wired, and the first-party connectors act as the CALLING principal's
 * own account (a token fetched per run from apps/api), never a Worker secret shared by everyone.
 */
import { describe, expect, it } from "vitest";
import type { Json } from "../connectors/types.js";
import { createApp } from "../src/app.js";
import type { LogEvent } from "../src/logging.js";
import { InMemorySettlementOutbox, SettlementDelivery } from "../src/outbox.js";
import { run } from "../src/run.js";
import { connectorSourceFor, createFetchTransport } from "../src/transport.js";
import { ApiWalletClient, boundedRetryAfter, type WalletReserveRequest } from "../src/wallet.js";
import { wiringWords } from "../src/wiring.js";
import { AGENT_KEY, bearer, fullCatalog, makeDeps } from "./helpers.js";

const SEARCH = "monid.tinyfish.web-search";

function walletRig(answer: () => { status: number; body: Json; retryAfter?: number }) {
  const events: LogEvent[] = [];
  const logger = { log: (e: LogEvent) => events.push(e) };
  const outbox = new InMemorySettlementOutbox();
  const delivery = new SettlementDelivery(outbox, async () => ({ status: 200 }), logger);
  const wallet = new ApiWalletClient({ wallet: { reserve: async (_b: WalletReserveRequest) => answer() }, outbox, delivery, logger });
  const deps = makeDeps({ catalog: fullCatalog() });
  (deps as { metering: unknown }).metering = wallet;
  return { deps, outbox, events };
}

describe("Retry-After from the wallet reaches the agent (E48)", () => {
  it("a wallet 503 with Retry-After answers 503 with the same retry_after, in the body and the header", async () => {
    const { deps } = walletRig(() => ({ status: 503, body: null, retryAfter: 7 }));
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "w503" }, bearer: bearer() });
    expect(out.status).toBe(503);
    if (out.body.resultType !== "error") throw new Error("expected an error");
    expect(out.body.retry_after).toBe(7);
    expect(out.body.error.next_action).toBe("retry_after");
    expect(deps.calls).toHaveLength(0);

    const app = createApp(deps);
    const res = await app.request("/v1/router/run", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${AGENT_KEY}` }, body: JSON.stringify({ tool_id: SEARCH, input: { query: "x" }, idempotency_key: "w503-rest" }) });
    expect(res.status).toBe(503);
    expect(res.headers.get("retry-after")).toBe("7");
    const mcp = await app.request("/mcp", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${AGENT_KEY}` }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "router.run", arguments: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "w503-mcp" } } }) });
    expect(mcp.headers.get("retry-after")).toBe("7");
  });

  it("a wallet 429 answers 429 rate_limited with its Retry-After; the draft is discarded and nothing is called", async () => {
    const { deps, outbox, events } = walletRig(() => ({ status: 429, body: { error: { code: "rate_limited" } }, retryAfter: 3 }));
    const out = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "w429" }, bearer: bearer() });
    expect(out.status).toBe(429);
    if (out.body.resultType !== "error") throw new Error("expected an error");
    expect(out.body.error.code).toBe("rate_limited");
    expect(out.body.retry_after).toBe(3);
    expect(deps.calls).toHaveLength(0);
    expect(outbox.rows.size).toBe(0);
    expect(events.some((e) => e.outcome === "wallet_rate_limited")).toBe(true);
    // The released key can be retried after the wait.
    const again = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "w429" }, bearer: bearer() });
    expect(again.status).toBe(429);
  });

  it("a missing or absurd Retry-After falls back or is bounded", () => {
    expect(boundedRetryAfter(undefined, 5)).toBe(5);
    expect(boundedRetryAfter(0, 5)).toBe(5);
    expect(boundedRetryAfter(Number.NaN, 5)).toBe(5);
    expect(boundedRetryAfter(2.2, 5)).toBe(3);
    expect(boundedRetryAfter(86_400, 5)).toBe(3600);
  });
});

describe("boot wiring log (E48)", () => {
  it("names, in fixed words, how the wallet, the outbox, idempotency and connector credentials are wired", () => {
    expect(wiringWords({})).toEqual(["wallet_in_memory_dev", "idempotency_in_memory", "connector_credentials_none"]);
    expect(wiringWords({ ROUTER_DEV_SHARED_CREDENTIALS: "1" })).toEqual(["wallet_in_memory_dev", "idempotency_in_memory", "connector_credentials_dev_shared"]);
    expect(wiringWords({ ROUTER_WALLET_URL: "https://api.test" })).toEqual(["wallet_outbox_missing", "idempotency_in_memory", "connector_credentials_per_caller"]);
    expect(wiringWords({ ROUTER_WALLET_URL: "https://api.test", HYPERDRIVE: { connectionString: "postgres://u:SECRET@h/db" }, ROUTER_DEV_SHARED_CREDENTIALS: "1" })).toEqual([
      "wallet_wired_with_outbox", "idempotency_postgres", "connector_credentials_per_caller", "dev_shared_credentials_ignored",
    ]);
  });
});

describe("per-caller connector credentials (E48)", () => {
  const GMAIL_DRAFT = { to: ["jun@example.com"], subject: "Re: deck", body: "Sending tonight." };
  interface Seen { url: string; headers: Record<string, string>; body: string | null }

  function fetchRig(tokenAnswer: () => Response) {
    const seen: Seen[] = [];
    const doFetch = async (url: string | URL | Request, init?: RequestInit): Promise<Response> => {
      const headers: Record<string, string> = {};
      new Headers(init?.headers).forEach((v, k) => (headers[k] = v));
      seen.push({ url: String(url), headers, body: typeof init?.body === "string" ? init.body : null });
      if (String(url).endsWith("/v1/internal/router/connector-token")) return tokenAnswer();
      return new Response(JSON.stringify({ id: "r-1", message: { id: "m-1", threadId: "t-1" } }), { status: 200 });
    };
    return { seen, doFetch: doFetch as typeof fetch };
  }

  function depsWith(env: Record<string, string>, tokenAnswer: () => Response) {
    const { seen, doFetch } = fetchRig(tokenAnswer);
    const deps = makeDeps();
    deps.transport = createFetchTransport({ env, connectors: connectorSourceFor(env).source, fetch: doFetch });
    return { deps, seen };
  }

  const PROD_ENV = { ROUTER_WALLET_URL: "https://api.test", ROUTER_SETTLEMENT_TOKEN: "rst_service_token_value", GOOGLE_OAUTH_ACCESS_TOKEN: "SHARED-ENV-TOKEN" };
  const confirm = { confirm: true };

  it("fetches the calling principal's token from apps/api per run and injects it into the vendor call only", async () => {
    const { deps, seen } = depsWith(PROD_ENV, () => new Response(JSON.stringify({ access_token: "ya29.PRINCIPAL-TOKEN-SENTINEL", token_type: "Bearer", expires_at: Date.now() + 3_600_000 }), { status: 200 }));
    deps.randomId = () => "run-42";
    const out = await run(deps, { input: { tool_id: "gmail.drafts.create", input: GMAIL_DRAFT, idempotency_key: "g1", inputResponses: confirm }, bearer: bearer() });
    expect(out.status).toBe(200);
    expect(seen).toHaveLength(2);
    const [tokenCall, vendorCall] = seen;
    expect(tokenCall?.url).toBe("https://api.test/v1/internal/router/connector-token");
    expect(tokenCall?.headers["authorization"]).toBe("Bearer rst_service_token_value");
    expect(tokenCall?.headers["x-caller-authorization"]).toBe(`Bearer ${AGENT_KEY}`);
    expect(JSON.parse(tokenCall?.body ?? "{}")).toEqual({ slot: "gmail-tokenset", provider_id: "gmail", run_id: "run-42" });
    expect(vendorCall?.url).toMatch(/^https:\/\/gmail\.googleapis\.com\//);
    expect(vendorCall?.headers["authorization"]).toBe("Bearer ya29.PRINCIPAL-TOKEN-SENTINEL");
    // The shared Worker env token is never used in production, and the caller's key never reaches the vendor.
    expect(JSON.stringify(seen)).not.toContain("SHARED-ENV-TOKEN");
    expect(JSON.stringify(vendorCall)).not.toContain(AGENT_KEY);
    // The token is not in the answer, the run log or a log line.
    const leak = JSON.stringify([out.body, deps.runLog.records, deps.logs]);
    expect(leak).not.toContain("PRINCIPAL-TOKEN-SENTINEL");
  });

  it("refuses with connector_not_connected (next_action authenticate) when the principal has no account connected; nothing is charged", async () => {
    const { deps, seen } = depsWith(PROD_ENV, () => new Response(JSON.stringify({ error: { code: "connector_not_connected" } }), { status: 404 }));
    const out = await run(deps, { input: { tool_id: "calendar.events.list", input: { time_min: "2026-10-27T00:00:00Z" }, idempotency_key: "c1" }, bearer: bearer() });
    expect(out.status).toBe(403);
    if (out.body.resultType !== "error") throw new Error("expected an error");
    expect(out.body.error.code).toBe("connector_not_connected");
    expect(out.body.error.next_action).toBe("authenticate");
    expect(out.body.error.message).toMatch(/connect it/);
    expect(JSON.parse(seen[0]?.body ?? "{}")).toMatchObject({ slot: "gcal-tokenset", provider_id: "calendar" });
    expect(seen).toHaveLength(1);
    expect(deps.metering.balanceOf(AGENT_KEY)).toBe(1_000_000);
  });

  it("maps a refused key to authentication_required and an unavailable token service to a retryable 503", async () => {
    const refused = depsWith(PROD_ENV, () => new Response("{}", { status: 403 }));
    const a = await run(refused.deps, { input: { tool_id: "github.issues.list", input: { owner: "o", repo: "r" }, idempotency_key: "gh" }, bearer: bearer() });
    expect(a.status).toBe(401);
    expect(JSON.parse(refused.seen[0]?.body ?? "{}")).toMatchObject({ slot: "github-tokenset" });

    const down = depsWith(PROD_ENV, () => new Response("oops", { status: 502 }));
    const b = await run(down.deps, { input: { tool_id: "github.issues.list", input: { owner: "o", repo: "r" }, idempotency_key: "gh" }, bearer: bearer() });
    expect(b.status).toBe(503);
    if (b.body.resultType !== "error") throw new Error("expected an error");
    expect(b.body.error.next_action).toBe("retry_after");
    expect(b.body.retry_after).toBe(5);
    expect(down.seen).toHaveLength(1);

    const malformed = depsWith(PROD_ENV, () => new Response(JSON.stringify({ access_token: "bad\r\nInjected: header" }), { status: 200 }));
    const c = await run(malformed.deps, { input: { tool_id: "github.issues.list", input: { owner: "o", repo: "r" }, idempotency_key: "gh" }, bearer: bearer() });
    expect(c.status).toBe(503);
    expect(malformed.seen).toHaveLength(1);
  });

  it("without ROUTER_WALLET_URL and without the development flag, first-party connector runs are refused before any call", async () => {
    const { deps, seen } = depsWith({ GITHUB_TOKEN: "SHARED-ENV-TOKEN" }, () => new Response("{}", { status: 500 }));
    const out = await run(deps, { input: { tool_id: "github.issues.list", input: { owner: "o", repo: "r" }, idempotency_key: "none" }, bearer: bearer() });
    expect(out.status).toBe(503);
    if (out.body.resultType !== "error") throw new Error("expected an error");
    expect(out.body.error.next_action).toBe("try_alternative");
    expect(seen).toHaveLength(0);
  });

  it("the development flag reads the env, and only when no wallet URL is set", async () => {
    expect(connectorSourceFor({ ROUTER_DEV_SHARED_CREDENTIALS: "1" })).toEqual({ source: { mode: "dev_shared" }, ignoredFlag: false });
    expect(connectorSourceFor({ ROUTER_DEV_SHARED_CREDENTIALS: "true" }).source.mode).toBe("none");
    expect(connectorSourceFor({ ROUTER_DEV_SHARED_CREDENTIALS: "1", ROUTER_WALLET_URL: "https://api.test" })).toEqual({ source: { mode: "per_caller", baseUrl: "https://api.test" }, ignoredFlag: true });

    const { deps, seen } = depsWith({ ROUTER_DEV_SHARED_CREDENTIALS: "1", GITHUB_TOKEN: "ghp_DEV-SHARED" }, () => new Response("{}", { status: 500 }));
    await run(deps, { input: { tool_id: "github.issues.list", input: { owner: "o", repo: "r" }, idempotency_key: "dev" }, bearer: bearer() });
    // Development: no token request, the shared env token goes to the vendor.
    expect(seen).toHaveLength(1);
    expect(seen[0]?.url).toMatch(/^https:\/\/api\.github\.com\//);
    expect(seen[0]?.headers["authorization"]).toBe("Bearer ghp_DEV-SHARED");
  });

  it("an agent without a key cannot run a per-caller connector tool", async () => {
    const { deps, seen } = depsWith(PROD_ENV, () => new Response("{}", { status: 200 }));
    const out = await run(deps, { input: { tool_id: "calendar.events.list", input: { time_min: "2026-10-27T00:00:00Z" }, idempotency_key: "anon" } });
    expect(out.status).toBe(401);
    expect(seen).toHaveLength(0);
  });
});
