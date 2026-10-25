/** E15 remote approvals on the gateway: the decision token (cross-checked with the Rust sidecar),
 * the one sidecar client, the server-rendered approval page with its CSRF guard, the Slack app
 * (signature, button press), outbound webhooks (signed, retried, summary only), and the `/mcp`
 * hook that learns of a queued approval from a header without reading the body. */
import { createHmac } from "node:crypto";
import { describe, expect, it } from "vitest";

import { CSRF_COOKIE, csrfFor } from "../src/approvals/app.js";
import { httpSidecarClient, pickSummary, type ApprovalSummary, type SidecarClient } from "../src/approvals/sidecar-client.js";
import { ACTION_APPROVE, ACTION_REJECT, SLACK_POST_MESSAGE, approvalMessage, parseInteraction, verifySlackSignature } from "../src/approvals/slack.js";
import { mintApprovalToken, verifyApprovalToken } from "../src/approvals/token.js";
import {
  InMemoryWebhookStore, SqlWebhookStore, checkWebhookUrl, deliverEvent, signDelivery, verifyDelivery, webhookSecret, type WebhookStore,
} from "../src/approvals/webhooks.js";
import { routeTemplate } from "../src/logging.js";
import { readFileSync } from "node:fs";
import { makeAgentRig, registerAgent, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";

const KEY = "gateway-and-sidecar-shared-key-0123456789";
const WEBHOOK_MASTER = "webhook-master-secret-0123456789abcdef";
const SLACK_SECRET = "slack-signing-secret";
const NOW = Date.parse("2026-10-25T12:00:00Z");

describe("decision token", () => {
  it("matches the Rust sidecar's vector byte for byte", () => {
    // crates/shogun-mcp/src/remote_approvals.rs `verifies_the_gateways_cross_language_vector`.
    const vector = "sat1.eyJhIjo3LCJwIjoiYWxpY2UiLCJlIjo0MTAyNDQ0ODAwMDAwLCJuIjoiMDAxMTIyMzM0NDU1NjY3Nzg4OTlhYWJiY2NkZGVlZmYifQ.MTBMDvcUdsb4kU0IE2BA9v_tuD4t5FxLo4XeYpOYxYk";
    expect(mintApprovalToken("cross-language-vector-key-0123456789", 7, "alice", 4102444800000, "00112233445566778899aabbccddeeff")).toBe(vector);
    const checked = verifyApprovalToken(vector, "cross-language-vector-key-0123456789", NOW);
    expect(checked).toEqual({ ok: true, claims: { approvalId: 7, principal: "alice", expiresMs: 4102444800000, nonce: "00112233445566778899aabbccddeeff" } });
  });

  it("refuses expired, forged and malformed tokens", () => {
    const token = mintApprovalToken(KEY, 3, null, NOW + 1_000);
    expect(verifyApprovalToken(token, KEY, NOW).ok).toBe(true);
    expect(verifyApprovalToken(token, KEY, NOW + 1_000)).toEqual({ ok: false, code: "expired" });
    expect(verifyApprovalToken(token, "another-key-that-is-long-enough-000000", NOW)).toEqual({ ok: false, code: "bad_signature" });
    const [prefix, , mac] = token.split(".");
    const forged = `${prefix}.${Buffer.from(JSON.stringify({ a: 4, p: null, e: NOW + 1_000, n: "x" })).toString("base64url")}.${mac}`;
    expect(verifyApprovalToken(forged, KEY, NOW)).toEqual({ ok: false, code: "bad_signature" });
    expect(verifyApprovalToken("sat1.x", KEY, NOW)).toEqual({ ok: false, code: "malformed" });
  });
});

/** A fake sidecar with the real contract: verifies the token, spends the nonce once. */
class FakeSidecar implements SidecarClient {
  readonly decisions: Array<{ approvalId: number; decision: string; decidedBy: string }> = [];
  private readonly spent = new Set<string>();
  readonly summaries = new Map<number, ApprovalSummary>();

  summary(token: string, approvalId: number): ReturnType<SidecarClient["summary"]> {
    const checked = verifyApprovalToken(token, KEY, NOW);
    if (!checked.ok || checked.claims.approvalId !== approvalId) return Promise.resolve({ ok: false, status: 403, code: "invalid_token" });
    const s = this.summaries.get(approvalId);
    return Promise.resolve(s ? { ok: true, value: s } : { ok: false, status: 404, code: "not_found" });
  }

  decide(token: string, approvalId: number, decision: "approve" | "reject", decidedBy: string, nonce: string): ReturnType<SidecarClient["decide"]> {
    const checked = verifyApprovalToken(token, KEY, NOW);
    if (!checked.ok || checked.claims.approvalId !== approvalId) return Promise.resolve({ ok: false, status: 403, code: "invalid_token" });
    if (checked.claims.nonce !== nonce) return Promise.resolve({ ok: false, status: 403, code: "nonce_mismatch" });
    if (this.spent.has(nonce)) return Promise.resolve({ ok: false, status: 409, code: "token_replayed" });
    this.spent.add(nonce);
    this.decisions.push({ approvalId, decision, decidedBy });
    const s = this.summaries.get(approvalId);
    if (s) this.summaries.set(approvalId, { ...s, status: decision === "approve" ? "approved" : "rejected", decision: { decision, decided_by: decidedBy, decided_at: "2026-10-25T12:00:01.000Z" } });
    return Promise.resolve({ ok: true, value: { approval_id: approvalId, decision, status: decision === "approve" ? "approved" : "rejected", decided_by: decidedBy, decided_at: "2026-10-25T12:00:01.000Z" } });
  }
}

const SUMMARY: ApprovalSummary = {
  approval_id: 9, status: "pending", level: "L3", kind: "Send email", recipient_class: "email_recipient",
  preview: "Forecast attached <script>alert(1)</script>", created_at: "2026-10-25T11:59:00.000Z", expires_at: "2026-10-25T12:09:00.000Z",
};

interface Outbound { url: string; init: RequestInit | undefined }

function rig(overrides: { webhooks?: WebhookStore; deliveryStatuses?: number[]; slack?: boolean } = {}): { rig: AgentRig; sidecar: FakeSidecar; outbound: Outbound[]; sleeps: number[]; webhooks: WebhookStore } {
  const sidecar = new FakeSidecar();
  sidecar.summaries.set(9, { ...SUMMARY });
  const outbound: Outbound[] = [];
  const sleeps: number[] = [];
  const statuses = [...(overrides.deliveryStatuses ?? [])];
  const webhooks = overrides.webhooks ?? new InMemoryWebhookStore();
  const fakeFetch = (url: string, init?: RequestInit): Promise<Response> => {
    outbound.push({ url, init });
    if (url === SLACK_POST_MESSAGE) return Promise.resolve(new Response(JSON.stringify({ ok: true }), { status: 200 }));
    if (url.startsWith("https://hooks.slack.com/")) return Promise.resolve(new Response("ok", { status: 200 }));
    return Promise.resolve(new Response("", { status: statuses.shift() ?? 200 }));
  };
  const agentRig = makeAgentRig({
    // A fake core that answers like the sidecar does when a call queued an approval: the token
    // in `X-Shogun-Approval-Token` (here echoed from a test header), the body untouched.
    mcpUpstream: (req) => {
      const token = req.headers.get("x-test-approval-token");
      return Promise.resolve(new Response(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { resultType: "approval_required" } }), {
        status: 200,
        headers: { "content-type": "application/json", ...(token ? { "x-shogun-approval-token": token } : {}) },
      }));
    },
    approvals: {
      signingKey: KEY, sidecar, webhooks, webhookSecret: WEBHOOK_MASTER, fetch: fakeFetch,
      sleep: (ms: number) => { sleeps.push(ms); return Promise.resolve(); },
      // Deliveries run inline in tests so the assertions see them.
      defer: () => undefined,
      ...(overrides.slack === false ? {} : { slack: { botToken: "xoxb-test", signingSecret: SLACK_SECRET, channel: "C0APPROVE", fetch: fakeFetch } }),
    },
  });
  agentRig.setNow(new Date(NOW));
  return { rig: agentRig, sidecar, outbound, sleeps, webhooks };
}

const BASE_ORIGIN = "https://api.test";

describe("the sidecar client", () => {
  it("keeps only the allowlisted summary fields and maps errors to codes", async () => {
    const calls: string[] = [];
    const client = httpSidecarClient("http://127.0.0.1:7464/", (url, init) => {
      calls.push(`${init?.method} ${url} ${(init?.headers as Record<string, string>)["x-shogun-approval-token"]}`);
      if (url.endsWith("/decide")) return Promise.resolve(new Response(JSON.stringify({ error: "token_replayed" }), { status: 409 }));
      return Promise.resolve(new Response(JSON.stringify({ approval: { ...SUMMARY, full_body: "secret body", recipient: "aiko@example.com" } }), { status: 200 }));
    });
    const s = await client.summary("tok", 9);
    expect(s.ok && s.value).toEqual(SUMMARY);
    expect(JSON.stringify(s)).not.toContain("secret body");
    expect(await client.decide("tok", 9, "approve", "web:approval-page", "n")).toEqual({ ok: false, status: 409, code: "token_replayed" });
    expect(calls).toEqual(["GET http://127.0.0.1:7464/v1/approvals/9 tok", "POST http://127.0.0.1:7464/v1/approvals/9/decide tok"]);
    expect(pickSummary({ status: "x" })).toBeUndefined();
    const down = httpSidecarClient("http://127.0.0.1:1", () => Promise.reject(new Error("refused")));
    expect(await down.summary("tok", 9)).toEqual({ ok: false, status: 503, code: "sidecar_unavailable" });
  });
});

describe("the approval page", () => {
  const token = mintApprovalToken(KEY, 9, "prn_alice", NOW + 600_000, "nonce-9");

  async function open(r: AgentRig): Promise<{ cookie: string; csrf: string; html: string; res: Response }> {
    const res = await r.app.request(`/approve/${token}`);
    const html = await res.text();
    const cookie = /shogun_approve_csrf=([0-9a-f]+)/.exec(res.headers.get("set-cookie") ?? "")?.[1] ?? "";
    const csrf = /name="csrf" value="([0-9a-f]+)"/.exec(html)?.[1] ?? "";
    return { cookie, csrf, html, res };
  }

  const post = (r: AgentRig, form: Record<string, string>, headers: Record<string, string> = {}) =>
    r.app.request(`/approve/${token}`, { method: "POST", body: new URLSearchParams(form).toString(), headers: { "content-type": "application/x-www-form-urlencoded", ...headers } });

  it("renders the redacted summary with no script, escapes it, and sets a CSRF cookie", async () => {
    const { rig: r } = rig();
    const { res, html, cookie, csrf } = await open(r);
    expect(res.status).toBe(200);
    expect(res.headers.get("referrer-policy")).toBe("no-referrer");
    expect(res.headers.get("content-security-policy")).toContain("default-src 'none'");
    expect(html).toContain("Send email");
    expect(html).toContain("email_recipient");
    expect(html).toContain("&lt;script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain('value="approve"');
    expect(cookie).toMatch(/^[0-9a-f]{32}$/);
    expect(csrf).toBe(csrfFor(KEY, cookie, token));
    expect(res.headers.get("set-cookie")).toContain("SameSite=Strict");
    expect(res.headers.get("set-cookie")).toContain("Secure");
    // The token never reaches a log line: the route is collapsed to its template.
    expect(r.logLines.join("\n")).not.toContain(token);
    expect(r.logLines.some((l) => l.startsWith("GET /approve/:token 200"))).toBe(true);
  });

  it("refuses a POST without the cookie, with a wrong token, or from another origin, and decides once", async () => {
    const { rig: r, sidecar } = rig();
    const { cookie, csrf } = await open(r);
    expect((await post(r, { csrf, decision: "approve" })).status).toBe(403);
    expect((await post(r, { csrf: "0".repeat(64), decision: "approve" }, { cookie: `${CSRF_COOKIE}=${cookie}` })).status).toBe(403);
    expect((await post(r, { csrf, decision: "approve" }, { cookie: `${CSRF_COOKIE}=${cookie}`, origin: "https://evil.example" })).status).toBe(403);
    expect(sidecar.decisions).toEqual([]);
    const ok = await post(r, { csrf, decision: "approve" }, { cookie: `${CSRF_COOKIE}=${cookie}`, origin: BASE_ORIGIN });
    expect(ok.status).toBe(200);
    expect(await ok.text()).toContain("Approved");
    expect(sidecar.decisions).toEqual([{ approvalId: 9, decision: "approve", decidedBy: "web:approval-page" }]);
    // A replay (back button, double submit) is the sidecar's refusal, shown as already decided.
    const again = await post(r, { csrf, decision: "approve" }, { cookie: `${CSRF_COOKIE}=${cookie}` });
    expect(again.status).toBe(409);
    expect(await again.text()).toContain("already decided");
    expect(sidecar.decisions).toHaveLength(1);
  });

  it("refuses an expired or forged link without calling the sidecar", async () => {
    const { rig: r, sidecar } = rig();
    const expired = mintApprovalToken(KEY, 9, "prn_alice", NOW - 1);
    expect((await r.app.request(`/approve/${expired}`)).status).toBe(410);
    const forged = mintApprovalToken("not-the-shared-key-but-long-enough-0000", 9, "prn_alice", NOW + 60_000);
    expect((await r.app.request(`/approve/${forged}`)).status).toBe(403);
    expect(sidecar.decisions).toEqual([]);
  });
});

function slackSigned(body: string, ts = Math.floor(NOW / 1000)): Record<string, string> {
  const sig = `v0=${createHmac("sha256", SLACK_SECRET).update(`v0:${ts}:${body}`).digest("hex")}`;
  return { "content-type": "application/x-www-form-urlencoded", "x-slack-request-timestamp": String(ts), "x-slack-signature": sig };
}

describe("the Slack app", () => {
  const token = mintApprovalToken(KEY, 9, "prn_alice", NOW + 600_000, "nonce-slack");
  const press = (actionId: string) =>
    `payload=${encodeURIComponent(JSON.stringify({ type: "block_actions", user: { id: "U0123" }, response_url: "https://hooks.slack.com/actions/T/1/x", actions: [{ action_id: actionId, value: token }] }))}`;

  it("verifies Slack's signature before anything is parsed", async () => {
    const { rig: r, sidecar } = rig();
    const body = press(ACTION_APPROVE);
    const bad = { ...slackSigned(body), "x-slack-signature": `v0=${"0".repeat(64)}` };
    expect((await r.app.request("/v1/slack/interactions", { method: "POST", body, headers: bad })).status).toBe(401);
    const stale = slackSigned(body, Math.floor(NOW / 1000) - 3_600);
    expect((await r.app.request("/v1/slack/interactions", { method: "POST", body, headers: stale })).status).toBe(401);
    expect(sidecar.decisions).toEqual([]);
    expect(verifySlackSignature(SLACK_SECRET, body, slackSigned(body)["x-slack-request-timestamp"], slackSigned(body)["x-slack-signature"], Math.floor(NOW / 1000))).toBe(true);
  });

  it("decides on a button press as the Slack user and replaces the message", async () => {
    const { rig: r, sidecar, outbound } = rig();
    const body = press(ACTION_REJECT);
    const res = await r.app.request("/v1/slack/interactions", { method: "POST", body, headers: slackSigned(body) });
    expect(res.status).toBe(200);
    expect(sidecar.decisions).toEqual([{ approvalId: 9, decision: "reject", decidedBy: "slack:U0123" }]);
    const replaced = outbound.find((o) => o.url.startsWith("https://hooks.slack.com/"));
    expect(JSON.parse(String(replaced?.init?.body))).toEqual({ replace_original: true, text: "Rejected by <@U0123>." });
    // The second press of the same message is refused by the sidecar (single use).
    await r.app.request("/v1/slack/interactions", { method: "POST", body, headers: slackSigned(body) });
    expect(sidecar.decisions).toHaveLength(1);
  });

  it("parses only block actions with a known button", () => {
    expect(parseInteraction("payload=%7B%7D")).toBeUndefined();
    expect(parseInteraction(`payload=${encodeURIComponent(JSON.stringify({ type: "block_actions", user: { id: "U1" }, actions: [{ action_id: "other", value: "t" }] }))}`)).toBeUndefined();
    const message = approvalMessage("C1", SUMMARY, "tok");
    expect(JSON.stringify(message)).toContain(ACTION_APPROVE);
    expect(JSON.stringify(message)).not.toContain("<script>");
  });
});

describe("webhooks and the approval.created notification", () => {
  it("registers HTTPS endpoints only and shows the derived secret once", async () => {
    const { rig: r } = rig();
    const agent = await registerAgent(r);
    const reg = (body: unknown) => r.app.request("/v1/webhooks", { method: "POST", headers: { authorization: `Bearer ${agent.key}`, "content-type": "application/json" }, body: JSON.stringify(body) });
    expect((await reg({ url: "http://hooks.example/x" })).status).toBe(400);
    expect((await reg({ url: "https://127.0.0.1/x" })).status).toBe(400);
    expect((await reg({ url: "https://user:pw@hooks.example/x" })).status).toBe(400);
    expect((await reg({ url: "https://hooks.example/x", events: ["approval.sent"] })).status).toBe(400);
    const ok = await reg({ url: "https://hooks.example/x", events: ["approval.created"] });
    expect(ok.status).toBe(201);
    const created = (await ok.json()) as { id: string; secret: string };
    expect(created.secret).toBe(webhookSecret(WEBHOOK_MASTER, created.id));
    const listed = (await (await r.app.request("/v1/webhooks", { headers: { authorization: `Bearer ${agent.key}` } })).json()) as { webhooks: unknown[] };
    expect(JSON.stringify(listed)).not.toContain(created.secret);
    expect(listed.webhooks).toHaveLength(1);
    expect((await r.app.request(`/v1/webhooks/${created.id}`, { method: "DELETE", headers: { authorization: `Bearer ${agent.key}` } })).status).toBe(200);
    expect((await r.app.request(`/v1/webhooks/${created.id}`, { method: "DELETE", headers: { authorization: `Bearer ${agent.key}` } })).status).toBe(404);
    expect((await r.app.request("/v1/webhooks", { method: "POST", body: "{}" })).status).toBe(401);
  });

  it("learns of an approval from the core's header, posts to Slack and delivers signed, summary-only webhooks with retries", async () => {
    const { rig: r, outbound, sleeps } = rig({ deliveryStatuses: [500, 200] });
    const agent = await registerAgent(r);
    const reg = await r.app.request("/v1/webhooks", { method: "POST", headers: { authorization: `Bearer ${agent.key}`, "content-type": "application/json" }, body: JSON.stringify({ url: "https://hooks.example/approvals" }) });
    const { secret } = (await reg.json()) as { secret: string };
    // The core answers an `approval_required` call with the token in a header; the body is
    // relayed untouched and the header is not passed on.
    const token = mintApprovalToken(KEY, 9, agent.principalId, NOW + 600_000, "nonce-created");
    const upstreamWithHeader = makeAgentRigUpstreamHeader(r, token);
    const res = await upstreamWithHeader();
    expect(res.status).toBe(200);
    expect(res.headers.get("x-shogun-approval-token")).toBeNull();
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));
    const slack = outbound.find((o) => o.url === SLACK_POST_MESSAGE);
    expect(slack).toBeDefined();
    expect((slack?.init?.headers as Record<string, string>).authorization).toBe("Bearer xoxb-test");
    expect(String(slack?.init?.body)).toContain(token);
    const deliveries = outbound.filter((o) => o.url === "https://hooks.example/approvals");
    expect(deliveries).toHaveLength(2);
    expect(sleeps).toEqual([1_000]);
    const last = deliveries[1]!;
    const headers = last.init?.headers as Record<string, string>;
    const body = String(last.init?.body);
    expect(headers["x-shogun-event"]).toBe("approval.created");
    expect(verifyDelivery(secret, body, headers["x-shogun-signature"]!, Math.floor(NOW / 1000))).toBe(true);
    expect(verifyDelivery(secret, `${body} `, headers["x-shogun-signature"]!, Math.floor(NOW / 1000))).toBe(false);
    const payload = JSON.parse(body) as { event: string; approval: Record<string, unknown>; principal_id: string };
    expect(payload.principal_id).toBe(agent.principalId);
    expect(Object.keys(payload.approval).sort()).toEqual(["approval_id", "created_at", "expires_at", "kind", "level", "preview", "recipient_class", "status"]);
    // The same approval announced again (a retried call, a second notify) posts nothing new.
    const before = outbound.length;
    await r.app.request("/v1/approvals/notify", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ token }) });
    expect(outbound.length).toBe(before);
    // A forged notification is refused.
    const forged = mintApprovalToken("not-the-shared-key-but-long-enough-0000", 9, agent.principalId, NOW + 60_000);
    expect((await r.app.request("/v1/approvals/notify", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ token: forged }) })).status).toBe(403);
  });

  it("stops retrying on a 4xx and gives up after the backoff on 5xx", async () => {
    const store = new InMemoryWebhookStore();
    await store.insert({ webhookId: "wh_1", principalId: "prn_a", url: "https://hooks.example/a", events: ["approval.decided"], createdAt: NOW });
    const statuses = [404];
    const sleeps: number[] = [];
    const fetchFn = () => Promise.resolve(new Response("", { status: statuses.shift() ?? 503 }));
    const deps = { store, masterSecret: WEBHOOK_MASTER, fetch: fetchFn, now: () => new Date(NOW), sleep: (ms: number) => { sleeps.push(ms); return Promise.resolve(); } };
    expect(await deliverEvent(deps, "prn_a", "approval.decided", SUMMARY)).toEqual([{ webhookId: "wh_1", attempts: 1, delivered: false, lastStatus: 404 }]);
    expect(await deliverEvent(deps, "prn_a", "approval.decided", SUMMARY)).toEqual([{ webhookId: "wh_1", attempts: 4, delivered: false, lastStatus: 503 }]);
    expect(sleeps).toEqual([1_000, 5_000, 30_000]);
    // Not subscribed: nothing is sent.
    expect(await deliverEvent(deps, "prn_a", "approval.created", SUMMARY)).toEqual([]);
  });

  it("validates URLs and signs with a timestamp", () => {
    expect(checkWebhookUrl("https://hooks.example/a").ok).toBe(true);
    expect(checkWebhookUrl("http://hooks.example/a", true).ok).toBe(true);
    for (const bad of ["ftp://x/a", "https://localhost/a", "https://10.0.0.1/a", "https://192.168.1.2/a", "https://[::1]/a", "not a url"]) {
      expect(checkWebhookUrl(bad).ok, bad).toBe(false);
    }
    const sig = signDelivery("s", "{}", 100);
    expect(sig).toMatch(/^t=100,v1=[0-9a-f]{64}$/);
    expect(verifyDelivery("s", "{}", sig, 100)).toBe(true);
    expect(verifyDelivery("s", "{}", sig, 1_000)).toBe(false);
  });

  it("stores webhooks in Postgres without secrets", async () => {
    const sql = readFileSync(new URL("../sql/approval-webhooks.sql", import.meta.url), "utf8");
    const store = new SqlWebhookStore(sqliteDouble(sql));
    await store.checkSchema();
    await store.insert({ webhookId: "wh_1", principalId: "prn_a", url: "https://hooks.example/a", events: ["approval.created", "approval.decided"], createdAt: NOW });
    expect(await store.listByPrincipal("prn_a")).toEqual([{ webhookId: "wh_1", principalId: "prn_a", url: "https://hooks.example/a", events: ["approval.created", "approval.decided"], createdAt: NOW }]);
    expect(await store.remove("wh_1", "prn_b", NOW)).toBe(false);
    expect(await store.remove("wh_1", "prn_a", NOW)).toBe(true);
    expect(await store.listByPrincipal("prn_a")).toEqual([]);
    expect(sql).not.toMatch(/secret\s+TEXT/i);
  });

  it("collapses token-bearing paths in the log", () => {
    expect(routeTemplate("/approve/sat1.abc.def")).toBe("/approve/:token");
    expect(routeTemplate("/v1/webhooks/wh_1")).toBe("/v1/webhooks/:id");
    expect(routeTemplate("/v1/slack/interactions")).toBe("/v1/slack/interactions");
  });
});

/** Call `/mcp` through the rig's metered proxy with an upstream that adds the approval header. */
function makeAgentRigUpstreamHeader(r: AgentRig, token: string): () => Promise<Response> {
  return async () => {
    const agent = await registerAgent(r, {}, "203.0.113.9");
    return r.app.request("/mcp", {
      method: "POST",
      headers: {
        authorization: `Bearer ${agent.key}`, "content-type": "application/json",
        "mcp-method": "tools/call", "mcp-name": "memory.search", "x-test-approval-token": token,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "memory.search", arguments: { query: "x" } } }),
    });
  };
}
