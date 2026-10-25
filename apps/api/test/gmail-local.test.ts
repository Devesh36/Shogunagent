import { mkdtempSync, chmodSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createLocalGmailApp, FetchGmailProvider, GmailError, verifyGmailConfiguration, type GmailState } from "../src/gmail-local.js";
import { serve } from "@hono/node-server";
import { request as httpRequest } from "node:http";
import { FileGmailStore } from "../src/gmail-local-store.js";

const token = "a".repeat(64);
const active: GmailState = { toolVersion: "20261001_00", userId: "shogun-local-test", authConfigId: "ac_test", connectedAccountId: "ca_test", consentGrantedAt: "2026-10-07T00:00:00Z", lastVerifiedAt: null };
function fixture(initial: GmailState = active) {
  let state = { ...initial };
  let time = 0;
  const provider = { call: vi.fn(async (_method: string, path: string): Promise<Record<string, unknown>> => {
    if (path === "/connected_accounts/link") return { connected_account_id: "ca_test", redirect_url: "https://connect.composio.dev/link/test", link_token: "do-not-return" };
    if (path.startsWith("/tools/")) return { successful: true, error: null, data: { messages: [] }, state: { val: { access_token: "do-not-return" } } };
    return { id: "ca_test", user_id: "shogun-local-test", toolkit: { slug: "gmail" }, auth_config: { id: "ac_test" }, status: "ACTIVE" };
  }) };
  const app = createLocalGmailApp({ bearer: token, provider, toolVersion: "20261001_00", now: () => time,
    store: { load: () => ({ ...state }), save: (s) => { state = { ...s }; } } });
  const request = (path: string, method = "GET", body?: unknown, extra?: Record<string, string>) => app.request(`http://127.0.0.1:4319${path}`, {
    method, headers: { host: "127.0.0.1:4319", authorization: `Bearer ${token}`, "content-type": "application/json", ...extra },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  return { app, provider, request, state: () => state, advance: () => { time += 60_000; } };
}
const read = { tool: "GMAIL_FETCH_EMAILS", arguments: { user_id: "me", max_results: 1, include_payload: true } };

describe("local Gmail security and lifecycle", () => {
  it("serves authenticated native requests over a real loopback socket", async () => {
    const f = fixture();
    const server = serve({ fetch: f.app.fetch, hostname: "127.0.0.1", port: 0 });
    try {
      await new Promise<void>((resolve) => server.once("listening", resolve));
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("test_listener_unavailable");
      const nativeRequest = (authorized: boolean) => new Promise<{ status: number | undefined; body: string; cache: string | undefined }>((resolve, reject) => {
        const request = httpRequest({ hostname: "127.0.0.1", port: address.port, path: "/healthz",
          headers: { host: "127.0.0.1:4319", ...(authorized ? { authorization: `Bearer ${token}` } : {}) } }, (response) => {
          let body = "";
          response.on("data", (chunk: Buffer) => { body += chunk.toString(); });
          response.on("end", () => resolve({ status: response.statusCode, body, cache: response.headers["cache-control"] }));
          response.on("error", reject);
        });
        request.on("error", reject);
        request.end();
      });
      expect((await nativeRequest(false)).status).toBe(401);
      const ok = await nativeRequest(true);
      expect(JSON.parse(ok.body)).toEqual({ ok: true, mode: "local-single-user", gmail: "read-only" });
      expect(ok.cache).toBe("no-store");
    } finally { await new Promise<void>((resolve) => server.close(() => resolve())); }
  });
  it("requires bearer and rejects browser origins and DNS rebinding before egress", async () => {
    const f = fixture();
    for (const [headers, status] of [[{ authorization: "" }, 401], [{ authorization: "Bearer wrong" }, 401], [{ origin: "https://attacker.example" }, 403], [{ host: "attacker.example:4319" }, 403]] as const) {
      expect((await f.request("/v1/gmail/execute", "POST", read, headers)).status).toBe(status);
    }
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("rejects forged callback status and never publishes secrets", async () => {
    const f = fixture({ ...active, connectedAccountId: null, consentGrantedAt: null });
    const response = await f.request("/callback?status=success&connected_account_id=ca_foreign", "GET", undefined, { authorization: "" });
    expect(response.status).toBe(200);
    expect(f.state().connectedAccountId).toBeNull();
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("requires explicit consent to start Connect", async () => {
    const f = fixture();
    expect((await f.request("/v1/gmail/connect", "POST", { consent: false })).status).toBe(403);
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("persists only the backend-issued account and stable server identity", async () => {
    const f = fixture({ ...active, connectedAccountId: null, consentGrantedAt: null });
    const response = await f.request("/v1/gmail/connect", "POST", { consent: true });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ status: "INITIATED", redirect_url: "https://connect.composio.dev/link/test" });
    expect(f.state().connectedAccountId).toBe("ca_test");
    expect(f.state().lastVerifiedAt).toBeNull();
    expect(f.provider.call).toHaveBeenCalledWith("POST", "/connected_accounts/link", expect.objectContaining({ user_id: active.userId, auth_config_id: active.authConfigId }));
  });
  it("rejects malicious provider links before storing a connection", async () => {
    const f = fixture({ ...active, connectedAccountId: null });
    f.provider.call.mockResolvedValueOnce({ connected_account_id: "ca_test", redirect_url: "https://connect.composio.dev.attacker.example/link/test" });
    expect((await f.request("/v1/gmail/connect", "POST", { consent: true })).status).toBe(502);
    expect(f.state().connectedAccountId).toBeNull();
  });
  it.each(["user", "toolkit", "auth-config", "account", "shared", "disabled"])("refuses %s ownership mismatch", async (kind) => {
    const f = fixture();
    f.provider.call.mockResolvedValueOnce({ id: kind === "account" ? "ca_other" : "ca_test", user_id: kind === "user" ? "other-user" : active.userId,
      toolkit: { slug: kind === "toolkit" ? "slack" : "gmail" }, auth_config: { id: kind === "auth-config" ? "ac_other" : "ac_test" },
      status: "ACTIVE", ...(kind === "shared" ? { experimental: { account_type: "SHARED" } } : {}), is_disabled: kind === "disabled" });
    expect((await f.request("/v1/gmail/execute", "POST", read)).status).toBe(403);
    expect(f.provider.call).toHaveBeenCalledTimes(1);
  });
  it("pins identity, connected account, version and mailbox while projecting read data", async () => {
    const f = fixture();
    const response = await f.request("/v1/gmail/execute", "POST", read);
    expect(await response.json()).toEqual({ successful: true, data: { messages: [] }, error: null });
    expect(f.provider.call).toHaveBeenLastCalledWith("POST", "/tools/execute/GMAIL_FETCH_EMAILS", {
      user_id: active.userId, connected_account_id: "ca_test", version: "20261001_00", arguments: read.arguments,
    });
    expect(f.state().lastVerifiedAt).not.toBeNull();
  });
  it.each(["GMAIL_SEND_EMAIL", "GMAIL_CREATE_EMAIL_DRAFT", "GMAIL_CREATE_LABEL", "GMAIL_MODIFY_THREAD_LABELS", "SLACK_SEND_MESSAGE"])("blocks %s before egress", async (tool) => {
    const f = fixture();
    expect((await f.request("/v1/gmail/execute", "POST", { ...read, tool })).status).toBe(403);
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("retains only a bounded execution log ID for real-read evidence", async () => {
    const f = fixture();
    const provider = f.provider.call.getMockImplementation();
    if (!provider) throw new Error("missing_test_provider");
    f.provider.call.mockImplementation(async (method, path) => ({ ...await provider(method, path), log_id: "log_real_read_123" }));
    const response = await f.request("/v1/gmail/execute", "POST", read);
    expect(await response.json()).toEqual({ successful: true, data: { messages: [] }, error: null, log_id: "log_real_read_123" });
    expect(f.state().lastReadLogId).toBe("log_real_read_123");
    expect((await f.request("/v1/gmail/connection", "DELETE")).status).toBe(200);
    expect(f.state().lastReadLogId).toBeNull();
  });
  it("does not reflect or persist provider content disguised as a log ID", async () => {
    const f = fixture();
    const provider = f.provider.call.getMockImplementation();
    if (!provider) throw new Error("missing_test_provider");
    f.provider.call.mockImplementation(async (method, path) => ({ ...await provider(method, path), log_id: "log_mail body / secret" }));
    const response = await f.request("/v1/gmail/execute", "POST", read);
    expect(await response.json()).toEqual({ successful: true, data: { messages: [] }, error: null });
    expect(f.state().lastReadLogId).toBeNull();
  });
  it.each([
    { ...read, user_id: "other" }, { ...read, connected_account_id: "ca_other" },
    { ...read, arguments: { ...read.arguments, user_id: "other" } },
    { ...read, arguments: { ...read.arguments, max_results: 16 } },
    { ...read, arguments: { ...read.arguments, max_results: 1.5 } },
    { ...read, arguments: { ...read.arguments, arbitrary_url: "https://attacker.example" } },
  ])("rejects untrusted identity and invalid arguments", async (body) => {
    const f = fixture();
    expect((await f.request("/v1/gmail/execute", "POST", body)).status).toBe(400);
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("refuses expired connection and revoked consent", async () => {
    const expired = fixture();
    expired.provider.call.mockResolvedValueOnce({ id: "ca_test", user_id: active.userId, toolkit: { slug: "gmail" }, auth_config: { id: active.authConfigId }, status: "EXPIRED" });
    expect((await expired.request("/v1/gmail/execute", "POST", read)).status).toBe(409);
    const revoked = fixture({ ...active, consentGrantedAt: null });
    expect((await revoked.request("/v1/gmail/execute", "POST", read)).status).toBe(403);
    expect(revoked.provider.call).not.toHaveBeenCalled();
  });
  it("durably blocks reads before a failed disconnect and permits retry", async () => {
    const f = fixture();
    f.provider.call.mockResolvedValueOnce({ id: "ca_test", user_id: active.userId, toolkit: { slug: "gmail" }, auth_config: { id: active.authConfigId }, status: "ACTIVE" });
    f.provider.call.mockRejectedValueOnce(new Error("secret provider detail"));
    const response = await f.request("/v1/gmail/connection", "DELETE");
    expect(response.status).toBe(503);
    expect(await response.text()).not.toContain("secret");
    expect(f.state().consentGrantedAt).toBeNull();
    expect(f.state().connectedAccountId).toBe("ca_test");
    expect((await f.request("/v1/gmail/execute", "POST", read)).status).toBe(403);
    expect((await f.request("/v1/gmail/connection", "DELETE")).status).toBe(200);
    expect(f.state().connectedAccountId).toBeNull();
  });
  it("recovers after the account is removed in Composio and never deletes a foreign account", async () => {
    const removed = fixture();
    removed.provider.call.mockRejectedValueOnce(new GmailError("connection_missing", 409));
    expect((await removed.request("/v1/gmail/connection", "DELETE")).status).toBe(200);
    expect(removed.state().connectedAccountId).toBeNull();
    expect(removed.provider.call).toHaveBeenCalledTimes(1);
    const foreign = fixture();
    foreign.provider.call.mockResolvedValueOnce({ id: "ca_test", user_id: "someone-else", toolkit: { slug: "gmail" }, auth_config: { id: "ac_test" } });
    expect((await foreign.request("/v1/gmail/connection", "DELETE")).status).toBe(403);
    expect(foreign.provider.call).toHaveBeenCalledTimes(1);
    expect(foreign.state().consentGrantedAt).toBeNull();
  });
  it("bounds request size and request rate", async () => {
    const f = fixture();
    expect((await f.request("/v1/gmail/execute", "POST", { ...read, padding: "x".repeat(9000) })).status).toBe(413);
    for (let i = 0; i < 11; i++) expect((await f.request("/healthz")).status).toBe(200);
    expect((await f.request("/healthz")).status).toBe(429);
    f.advance();
    expect((await f.request("/healthz")).status).toBe(200);
    expect(f.provider.call).not.toHaveBeenCalled();
  });
  it("bounds in-flight work and does not race connect/disconnect", async () => {
    const f = fixture();
    let finish!: (value: Record<string, unknown>) => void;
    f.provider.call.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const pending = f.request("/v1/gmail/status");
    await vi.waitFor(() => expect(f.provider.call).toHaveBeenCalledTimes(1));
    expect((await f.request("/v1/gmail/connection", "DELETE")).status).toBe(409);
    finish({ id: "ca_test", user_id: active.userId, toolkit: { slug: "gmail" }, auth_config: { id: active.authConfigId }, status: "ACTIVE" });
    expect((await pending).status).toBe(200);
  });
  it("returns a content-free error on a failed provider tool", async () => {
    const f = fixture();
    f.provider.call.mockResolvedValueOnce({ id: "ca_test", user_id: active.userId, toolkit: { slug: "gmail" }, auth_config: { id: active.authConfigId }, status: "ACTIVE" });
    f.provider.call.mockResolvedValueOnce({ successful: false, error: "mail-body secret", data: {} });
    expect(await (await f.request("/v1/gmail/execute", "POST", read)).text()).toBe('{"error":"gmail_read_failed"}');
    expect(f.state().lastVerifiedAt).toBeNull();
  });
});

describe("configuration verification", () => {
  function provider() {
    return { call: vi.fn(async (_method: string, path: string): Promise<Record<string, unknown>> => {
      if (path.startsWith("/auth_configs/")) return { id: "ac_test", toolkit: { slug: "gmail" }, auth_scheme: "OAUTH2", status: "ENABLED", is_composio_managed: true };
      return { slug: path.split("/").at(-1)?.split("?")[0], toolkit: { slug: "gmail" }, version: "20261001_00", is_deprecated: false };
    }) };
  }
  it("resolves one pinned version using metadata only", async () => {
    const p = provider();
    expect(await verifyGmailConfiguration(p, { ...active, toolVersion: null })).toBe("20261001_00");
    expect(p.call).toHaveBeenCalledTimes(4);
    expect(p.call.mock.calls.every(([method, path]) => method === "GET" && !path.includes("connected_accounts") && !path.includes("execute"))).toBe(true);
  });
  it("refuses a disabled/non-Gmail auth config", async () => {
    const p = provider();
    p.call.mockResolvedValueOnce({ id: "ac_test", toolkit: { slug: "slack" }, status: "DISABLED" });
    await expect(verifyGmailConfiguration(p, active)).rejects.toMatchObject({ code: "invalid_gmail_auth_config" });
    expect(p.call).toHaveBeenCalledTimes(1);
  });
  it("refuses changed tool versions instead of silently executing latest", async () => {
    const p = provider();
    await expect(verifyGmailConfiguration(p, { ...active, toolVersion: "20250901_00" })).rejects.toMatchObject({ code: "gmail_tool_version_mismatch" });
  });
});

describe("provider transport", () => {
  it("uses fixed HTTPS upstream, disables redirects and supplies a deadline", async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(new Response('{"successful":true}'));
    const api = new FetchGmailProvider("secret", fetcher);
    await api.call("POST", "/tools/execute/GMAIL_LIST_LABELS", { arguments: {} });
    expect(fetcher).toHaveBeenCalledWith("https://backend.composio.dev/api/v3/tools/execute/GMAIL_LIST_LABELS", expect.objectContaining({ redirect: "error", signal: expect.any(AbortSignal) }));
  });
  it("never returns provider error bodies or exception messages", async () => {
    const api = new FetchGmailProvider("secret", vi.fn<typeof fetch>().mockResolvedValue(new Response("secret-key mail-body", { status: 500 })));
    await expect(api.call("GET", "/connected_accounts/ca_test")).rejects.toMatchObject({ code: "provider_request_failed", status: 502 });
    const offline = new FetchGmailProvider("secret", vi.fn<typeof fetch>().mockRejectedValue(new Error("secret-key")));
    await expect(offline.call("GET", "/connected_accounts/ca_test")).rejects.toEqual(new GmailError("provider_unavailable", 502));
  });
  it("bounds upstream response size", async () => {
    const api = new FetchGmailProvider("secret", vi.fn<typeof fetch>().mockResolvedValue(new Response("x".repeat(2 * 1024 * 1024 + 1))));
    await expect(api.call("GET", "/connected_accounts/ca_test")).rejects.toMatchObject({ code: "provider_response_too_large" });
  });
});

describe("connection metadata persistence", () => {
  const directories: string[] = [];
  afterEach(() => { for (const dir of directories.splice(0)) rmSync(dir, { recursive: true, force: true }); });
  function path() {
    const dir = mkdtempSync(join(tmpdir(), "shogun-gmail-test-"));
    directories.push(dir);
    chmodSync(dir, 0o700);
    return join(dir, "connection.json");
  }
  it("preserves identity and successful-read metadata across restart without provider credentials", () => {
    const file = path();
    writeFileSync(file, JSON.stringify(active), { mode: 0o600 });
    new FileGmailStore(file).save({ ...active, lastVerifiedAt: "2026-10-07T04:00:00Z", lastReadLogId: "log_verified_read" });
    expect(new FileGmailStore(file).load()).toEqual({ ...active, lastVerifiedAt: "2026-10-07T04:00:00Z", lastReadLogId: "log_verified_read" });
    expect(readFileSync(file, "utf8")).not.toContain("token");
  });
  it("refuses exposed permissions and symlinks", () => {
    const file = path();
    writeFileSync(file, JSON.stringify(active), { mode: 0o644 });
    expect(() => new FileGmailStore(file).load()).toThrow("unsafe_gmail_state_file");
    const link = path();
    symlinkSync(file, link);
    expect(() => new FileGmailStore(link).load()).toThrow();
  });
});
