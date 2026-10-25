/**
 * E50: the hosted connect flow (connector-connect.ts) against a fake principal sidecar. What is
 * proved:
 *
 * - an agent gets a link for a human (202, `next_action: ask_human`) and nothing else: it cannot
 *   start or finish the flow itself (the page needs a CSRF cookie, the callback the flow cookie of
 *   the browser that pressed Continue);
 * - the sidecar is asked as the link's principal, by its derived bearer; the state is sealed and
 *   carries no principal in clear; the code goes to the sidecar once and never into a log line,
 *   a response or a store here;
 * - after the human finishes, the router's connector-token route serves the connected account;
 * - forged, expired, replayed or cross-browser callbacks connect nothing.
 */
import { describe, expect, it } from "vitest";

import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import { CONNECT_CALLBACK_PATH, CONNECT_CSRF_COOKIE, CONNECT_FLOW_COOKIE, openClaims, sealClaims } from "../src/connector-connect.js";
import { CONNECTOR_TOKEN_PATH, sidecarConnectorVault } from "../src/connector-tokens.js";
import { derivedSidecarBearer } from "../src/mcp-proxy.js";
import { BASE, TEST_KEY_ITERATIONS, makeAgentRig } from "./agent-helpers.js";
import { ADMIN_TOKEN, BEARER_SECRET, FakeSidecar, SIDECAR_URL } from "./connector-helpers.js";

const ROUTER_TOKEN = "rst_router_wallet_token_0123456789abcdef";
const CONNECT_SECRET = "connector-connect-secret-0123456789abcdef";
const NOW = Date.parse("2026-10-25T12:00:00Z");

function fixture(opts: { connectSecret?: string | false } = {}) {
  const sidecar = new FakeSidecar();
  const vault = sidecarConnectorVault({ relay: { url: SIDECAR_URL, bearerSecret: BEARER_SECRET }, adminToken: ADMIN_TOKEN }, sidecar.fetch);
  const rig = makeAgentRig({
    routerWallet: { token: ROUTER_TOKEN },
    connectorTokens: { vault, ...(opts.connectSecret === false ? {} : { connectSecret: opts.connectSecret ?? CONNECT_SECRET }) },
  });
  rig.setNow(new Date(NOW));
  const key = async (principalId: string, scopes: AgentScope[] = ["read", "write", "run"]) => {
    const { key: k, prefix } = generateAgentKey("live");
    if (!(await rig.store.principal(principalId))) await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: NOW });
    await rig.store.insertKey({ keyId: `ak_${principalId}_${scopes.join("")}`, prefix, hash: await hashAgentKey(k, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: NOW });
    return k;
  };
  const askLink = (k: string | undefined, provider = "gmail") =>
    rig.app.request(`/v1/connectors/${provider}/connect`, { method: "POST", headers: k ? { authorization: `Bearer ${k}` } : {} });
  return { sidecar, rig, key, askLink };
}

type F = ReturnType<typeof fixture>;

function cookieValue(res: Response, name: string): string | undefined {
  const raw = res.headers.get("set-cookie") ?? "";
  const m = new RegExp(`${name}=([^;]*)`).exec(raw);
  return m?.[1];
}

/** Open the link page, press Continue; returns the redirect and the flow cookie. */
async function continueFlow(f: F, connectUrl: string) {
  const path = new URL(connectUrl).pathname;
  const page = await f.rig.app.request(path);
  expect(page.status).toBe(200);
  const csrfCookie = cookieValue(page, CONNECT_CSRF_COOKIE);
  const csrf = /name="csrf" value="([0-9a-f]+)"/.exec(await page.text())?.[1];
  expect(csrfCookie && csrf).toBeTruthy();
  const res = await f.rig.app.request(path, {
    method: "POST",
    headers: { cookie: `${CONNECT_CSRF_COOKIE}=${csrfCookie}`, origin: BASE, "content-type": "application/x-www-form-urlencoded" },
    body: `csrf=${csrf}`,
  });
  return { res, flow: cookieValue(res, CONNECT_FLOW_COOKIE) };
}

function callback(f: F, query: Record<string, string>, flow?: string) {
  return f.rig.app.request(`${CONNECT_CALLBACK_PATH}?${new URLSearchParams(query)}`, { headers: flow ? { cookie: `${CONNECT_FLOW_COOKIE}=${flow}` } : {} });
}

describe("POST /v1/connectors/{provider}/connect (the agent's side)", () => {
  it("hands the agent a link for a human, never a connection", async () => {
    const f = fixture();
    const k = await f.key("prn_a");
    const res = await f.askLink(k);
    expect(res.status).toBe(202);
    const body = (await res.json()) as Record<string, string>;
    expect(body).toMatchObject({ status: "pending_human", provider: "gmail", slot: "gmail-tokenset", next_action: "ask_human" });
    expect(body.connect_url).toMatch(/^https:\/\/api\.test\/v1\/connectors\/connect\/[A-Za-z0-9_-]+$/);
    expect(body.connect_url).not.toContain("prn_a");
    expect(body.expires_at).toBe(new Date(NOW + 15 * 60_000).toISOString());
    // Nothing reached the sidecar: asking for a link starts nothing.
    expect(f.sidecar.calls).toEqual([]);
  });

  it("needs a key with the run scope and a connectable provider", async () => {
    const f = fixture();
    expect((await f.askLink(undefined)).status).toBe(401);
    expect((await f.askLink(await f.key("prn_r", ["read"]))).status).toBe(403);
    const k = await f.key("prn_a");
    for (const provider of ["github", "notion", "monid"]) expect((await f.askLink(k, provider)).status).toBe(404);
    for (const provider of ["calendar", "slack", "linear"]) expect((await f.askLink(k, provider)).status).toBe(202);
  });

  it("is not served without CONNECTOR_CONNECT_SECRET", async () => {
    const f = fixture({ connectSecret: false });
    expect((await f.askLink(await f.key("prn_a"))).status).toBe(404);
    expect((await f.rig.app.request(`${CONNECT_CALLBACK_PATH}?state=x&code=y`)).status).toBe(404);
  });
});

describe("the human's flow", () => {
  it("connects the account into the link's principal, and the router route then serves it", async () => {
    const f = fixture();
    const k = await f.key("prn_a");
    const { connect_url } = (await (await f.askLink(k, "calendar")).json()) as { connect_url: string };
    const page = await f.rig.app.request(new URL(connect_url).pathname);
    const html = await page.text();
    expect(html).toContain("Connect Google Calendar?");
    expect(html).toContain("prn_a");
    expect(page.headers.get("referrer-policy")).toBe("no-referrer");
    expect(page.headers.get("cache-control")).toBe("no-store");

    const { res, flow } = await continueFlow(f, connect_url);
    expect(res.status).toBe(303);
    expect(flow).toBeTruthy();
    const location = new URL(res.headers.get("location") ?? "");
    expect(location.origin).toBe("https://accounts.vendor.test");
    const state = location.searchParams.get("state") ?? "";
    // The sidecar was asked as prn_a (its derived bearer), with the provider and the sealed state.
    const start = f.sidecar.calls.at(-1);
    expect(start).toMatchObject({ path: "/v1/internal/connectors/connect/start", authorization: `Bearer ${ADMIN_TOKEN}`, body: { provider: "calendar", state } });
    expect(start?.principalHeader).toBe(`Bearer ${derivedSidecarBearer(BEARER_SECRET, { principalId: "prn_a", keyId: "ak_prn_a_readwriterun" })}`);
    expect(state).not.toContain("prn_a");

    const done = await callback(f, { state, code: "vendor-code-1" }, flow);
    expect(done.status).toBe(200);
    expect(await done.text()).toContain("Google Calendar connected");
    expect(f.sidecar.calls.at(-1)).toMatchObject({ path: "/v1/internal/connectors/connect/finish", body: { state, code: "vendor-code-1" } });
    // The flow cookie is cleared; the code is in no log line.
    expect(done.headers.get("set-cookie")).toContain("Max-Age=0");
    for (const line of f.rig.logLines) {
      expect(line).not.toContain("vendor-code-1");
      expect(line).not.toContain(state);
      expect(line).not.toContain(new URL(connect_url).pathname.split("/").at(-1) ?? "?");
    }
    expect(f.rig.logLines.some((l) => l.includes("event=connector_connected"))).toBe(true);

    // The router's route now serves the connected account for prn_a.
    const token = await f.rig.app.request(CONNECTOR_TOKEN_PATH, {
      method: "POST",
      headers: { authorization: `Bearer ${ROUTER_TOKEN}`, "x-caller-authorization": `Bearer ${k}`, "content-type": "application/json" },
      body: JSON.stringify({ slot: "gcal-tokenset", provider_id: "calendar" }),
    });
    expect(((await token.json()) as { access_token: string }).access_token).toBe(f.sidecar.connectedToken);

    // Replaying the callback connects nothing more.
    expect((await callback(f, { state, code: "vendor-code-1" }, flow)).status).toBe(410);
  });

  it("refuses a Continue without the page's CSRF cookie or from another origin", async () => {
    const f = fixture();
    const { connect_url } = (await (await f.askLink(await f.key("prn_a"))).json()) as { connect_url: string };
    const path = new URL(connect_url).pathname;
    expect((await f.rig.app.request(path, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: "csrf=00" })).status).toBe(403);
    expect((await f.rig.app.request(path, { method: "POST", headers: { origin: "https://evil.test", "content-type": "application/x-www-form-urlencoded" }, body: "csrf=00" })).status).toBe(403);
    expect(f.sidecar.calls).toEqual([]);
  });

  it("refuses forged, expired and other-browser callbacks, and a declined consent", async () => {
    const f = fixture();
    const { connect_url } = (await (await f.askLink(await f.key("prn_a"), "gmail")).json()) as { connect_url: string };
    const { res, flow } = await continueFlow(f, connect_url);
    const state = new URL(res.headers.get("location") ?? "").searchParams.get("state") ?? "";

    // Another browser (no flow cookie, or a different one): the consent URL was forwarded.
    expect((await callback(f, { state, code: "vendor-code-1" })).status).toBe(403);
    expect((await callback(f, { state, code: "vendor-code-1" }, "someone-elses-flow-cookie")).status).toBe(403);
    // A state this gateway did not seal, or sealed as a link.
    expect((await callback(f, { state: "x".repeat(60), code: "vendor-code-1" }, flow)).status).toBe(400);
    const asLink = sealClaims(CONNECT_SECRET, "link", openClaims(CONNECT_SECRET, "state", state) ?? {});
    expect((await callback(f, { state: asLink, code: "vendor-code-1" }, flow)).status).toBe(400);
    // The human declined at the provider.
    expect((await callback(f, { state, error: "access_denied" }, flow)).status).toBe(400);
    // Nothing reached the sidecar's finish so far.
    expect(f.sidecar.calls.some((c) => c.path.endsWith("/finish"))).toBe(false);
    // A code the vendor refuses: the sidecar says rejected.
    expect((await callback(f, { state, code: "wrong-code" }, flow)).status).toBe(400);
    expect(f.sidecar.grants.get("prn_a")).toBeUndefined();

    // An expired link and an expired state.
    f.rig.setNow(new Date(NOW + 16 * 60_000));
    expect((await f.rig.app.request(new URL(connect_url).pathname)).status).toBe(410);
    expect((await callback(f, { state, code: "vendor-code-1" }, flow)).status).toBe(410);
    expect((await f.rig.app.request("/v1/connectors/connect/not-a-link")).status).toBe(400);
  });

  it("tells the human when the sidecar cannot connect the provider", async () => {
    const f = fixture();
    const { connect_url } = (await (await f.askLink(await f.key("prn_a"), "slack")).json()) as { connect_url: string };
    f.sidecar.forced = { status: 503, body: { error: "connect_not_configured" } };
    const { res } = await continueFlow(f, connect_url);
    expect(res.status).toBe(503);
    expect(await res.text()).toContain("cannot be connected on this deployment");
    f.sidecar.forced = undefined;
    f.sidecar.down = true;
    expect((await continueFlow(f, connect_url)).res.status).toBe(503);
  });
});
