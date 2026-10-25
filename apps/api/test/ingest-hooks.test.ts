/** E44: provider webhooks into memory (ingest-hooks/*). Every provider's signature scheme, the
 * hook registrations, the relay to the core as `memory.ingest_webhook`, the billing (ingest per
 * event; handshakes and bad signatures free), the failure budget, and that nothing from a body
 * persists or is logged here. Payloads and secrets are synthetic. */
import { createSign, generateKeyPairSync, type KeyObject } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import type { AgentPrincipal } from "../src/agent-keys.js";
import { InMemoryAgentStore } from "../src/agent-store.js";
import { BootError, ingestHooksConfigFromEnv } from "../src/boot.js";
import { FAILURES_PER_WINDOW, MAX_EVENTS_PER_DELIVERY, ROTATION_GRACE_MS, WEBHOOK_TOOL, deliveryUrl, newHookId, type IngestHookDeps } from "../src/ingest-hooks/app.js";
import { verifyGoogleOidc, type JwksSource, type RsaJwk } from "../src/ingest-hooks/pubsub-jwt.js";
import { HookSecrets } from "../src/ingest-hooks/secrets.js";
import { InMemoryHookStore, SqlHookStore, type HookRecord, type HookStore } from "../src/ingest-hooks/store.js";
import { sign, tokenMatches, verifyGitHub, verifyLinear, verifyNotion, verifySlack } from "../src/ingest-hooks/verify.js";
import { routeTemplate } from "../src/logging.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS, classifyCall } from "../src/pricing.js";
import { BASE, authed, makeAgentRig, registerAgent, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";

const MASTER = "ingest-hooks-test-master-secret-0123456789";
const HOOK_SQL = readFileSync(new URL("../sql/ingest-hooks.sql", import.meta.url), "utf8");
const enc = (s: string): Uint8Array => new TextEncoder().encode(s);

// --- a fake Google: one RSA key, tokens signed the way Pub/Sub signs them ---
const google = generateKeyPairSync("rsa", { modulusLength: 2048 });
const KID = "test-kid-1";
const jwk = { ...(google.publicKey.export({ format: "jwk" }) as { n: string; e: string; kty: string }), kid: KID } as RsaJwk;
const fixedJwks: JwksSource = { keys: () => Promise.resolve([jwk]) };
const AUDIENCE = "https://api.test/v1/ingest-hooks/gmail";
const PUSH_SA = "push@shogun-test.iam.gserviceaccount.com";

function googleToken(claims: Record<string, unknown>, key: KeyObject = google.privateKey, kid = KID): string {
  const b64 = (v: unknown): string => Buffer.from(JSON.stringify(v)).toString("base64url");
  const head = b64({ alg: "RS256", kid, typ: "JWT" });
  const body = b64(claims);
  const sig = createSign("RSA-SHA256").update(`${head}.${body}`).sign(key).toString("base64url");
  return `${head}.${body}.${sig}`;
}

function pushClaims(nowMs: number, over: Record<string, unknown> = {}): Record<string, unknown> {
  return { iss: "https://accounts.google.com", aud: AUDIENCE, email: PUSH_SA, email_verified: true, iat: Math.floor(nowMs / 1000) - 5, exp: Math.floor(nowMs / 1000) + 3600, ...over };
}

function pubsubBody(email: string, historyId: number, messageId = "1234567890"): string {
  const data = Buffer.from(JSON.stringify({ emailAddress: email, historyId })).toString("base64");
  return JSON.stringify({ message: { data, messageId, publishTime: "2026-10-25T12:00:00Z" }, subscription: "projects/p/subscriptions/s" });
}

describe("signature schemes (verify.ts)", () => {
  const body = enc('{"action":"opened"}');
  it("GitHub: sha256=<hex> over the body, any current secret, nothing else", () => {
    expect(verifyGitHub(["s1"], body, sign.github("s1", body))).toBe(true);
    expect(verifyGitHub(["old", "s1"], body, sign.github("s1", body))).toBe(true);
    expect(verifyGitHub(["s2"], body, sign.github("s1", body))).toBe(false);
    expect(verifyGitHub(["s1"], enc('{"action":"closed"}'), sign.github("s1", body))).toBe(false);
    for (const bad of [undefined, "", "sha1=abc", sign.github("s1", body).slice(0, -2), sign.linear("s1", body)]) expect(verifyGitHub(["s1"], body, bad)).toBe(false);
    expect(verifyGitHub([], body, sign.github("s1", body))).toBe(false);
  });
  it("Linear and Notion", () => {
    expect(verifyLinear(["l"], body, sign.linear("l", body))).toBe(true);
    expect(verifyLinear(["l"], body, sign.github("l", body))).toBe(false);
    expect(verifyNotion(["secret_x"], body, sign.notion("secret_x", body))).toBe(true);
    expect(verifyNotion(["secret_x"], body, sign.notion("secret_y", body))).toBe(false);
  });
  it("Slack: v0 over ts and body, within five minutes", () => {
    const ts = 1_761_393_600;
    expect(verifySlack(["ss"], body, String(ts), sign.slack("ss", body, ts), ts + 10)).toBe(true);
    expect(verifySlack(["ss"], body, String(ts), sign.slack("ss", body, ts), ts + 301)).toBe(false);
    expect(verifySlack(["ss"], body, String(ts + 1), sign.slack("ss", body, ts), ts)).toBe(false);
    expect(verifySlack(["ss"], body, "nan", sign.slack("ss", body, ts), ts)).toBe(false);
  });
  it("tokens compare in constant time over digests", () => {
    expect(tokenMatches(["tok-a"], "tok-a")).toBe(true);
    expect(tokenMatches(["tok-a", "tok-b"], "tok-b")).toBe(true);
    expect(tokenMatches(["tok-a"], "tok-a ")).toBe(false);
    expect(tokenMatches(["tok-a"], undefined)).toBe(false);
    expect(tokenMatches([""], "")).toBe(false);
  });
});

describe("hook secrets (secrets.ts)", () => {
  const s = new HookSecrets(MASTER);
  it("derives per hook and generation; seals bound to the hook", () => {
    const a = newHookId();
    const b = newHookId();
    expect(a).toMatch(/^hk_[A-Za-z0-9_-]{22}$/);
    expect(s.derived(a, 1)).toBe(new HookSecrets(MASTER).derived(a, 1));
    expect(s.derived(a, 1)).not.toBe(s.derived(a, 2));
    expect(s.derived(a, 1)).not.toBe(s.derived(b, 1));
    expect(s.derived(a, 1)).not.toBe(new HookSecrets(MASTER + "x").derived(a, 1));
    const sealed = s.seal("linear-signing-secret", a);
    expect(sealed).not.toContain("linear-signing-secret");
    expect(s.open(sealed, a)).toBe("linear-signing-secret");
    expect(s.open(sealed, b)).toBeUndefined();
    expect(new HookSecrets(MASTER + "x").open(sealed, a)).toBeUndefined();
    expect(s.open("v1.garbage", a)).toBeUndefined();
    expect(s.mailboxKey(" Ana@Example.com ")).toBe(s.mailboxKey("ana@example.com"));
    expect(s.mailboxKey("ana@example.com")).not.toContain("ana");
    expect(() => new HookSecrets("short")).toThrow();
  });
});

describe("Google OIDC push tokens (pubsub-jwt.ts)", () => {
  const nowMs = Date.parse("2026-10-25T12:00:00Z");
  const expect_ = { audience: AUDIENCE, serviceAccount: PUSH_SA };
  it("accepts Google's token for the configured audience and account only", async () => {
    expect(await verifyGoogleOidc(`Bearer ${googleToken(pushClaims(nowMs))}`, expect_, fixedJwks, nowMs)).toEqual({ ok: true });
    for (const over of [{ aud: "https://other" }, { email: "x@y.iam.gserviceaccount.com" }, { email_verified: false }, { iss: "https://evil" }, { exp: Math.floor(nowMs / 1000) - 120 }, { iat: Math.floor(nowMs / 1000) + 600 }]) {
      expect(await verifyGoogleOidc(`Bearer ${googleToken(pushClaims(nowMs, over))}`, expect_, fixedJwks, nowMs)).toEqual({ ok: false, reason: "claims" });
    }
    const other = generateKeyPairSync("rsa", { modulusLength: 2048 });
    expect(await verifyGoogleOidc(`Bearer ${googleToken(pushClaims(nowMs), other.privateKey)}`, expect_, fixedJwks, nowMs)).toEqual({ ok: false, reason: "bad_signature" });
    expect(await verifyGoogleOidc(`Bearer ${googleToken(pushClaims(nowMs), google.privateKey, "nope")}`, expect_, fixedJwks, nowMs)).toEqual({ ok: false, reason: "unknown_key" });
    expect(await verifyGoogleOidc(undefined, expect_, fixedJwks, nowMs)).toEqual({ ok: false, reason: "missing" });
    expect(await verifyGoogleOidc("Bearer a.b", expect_, fixedJwks, nowMs)).toEqual({ ok: false, reason: "malformed" });
    const down: JwksSource = { keys: () => Promise.reject(new Error("down")) };
    expect(await verifyGoogleOidc(`Bearer ${googleToken(pushClaims(nowMs))}`, expect_, down, nowMs)).toEqual({ ok: false, reason: "keys_unavailable" });
  });
});

describe.each<[string, () => HookStore]>([
  ["memory", () => new InMemoryHookStore()],
  ["sql (sqlite double)", () => new SqlHookStore(sqliteDouble(HOOK_SQL))],
])("hook store: %s", (_name, make) => {
  const row = (over: Partial<HookRecord> = {}): HookRecord => ({
    hookId: newHookId(), principalId: "p1", tenantId: "t1", keyId: "ak_1", provider: "github", status: "active", generation: 1, createdAt: 1000, ...over,
  });
  it("inserts, finds, routes, updates and removes", async () => {
    const store = make();
    await store.checkSchema();
    const a = row({ createKeyHash: "k1" });
    expect(await store.insert(a)).toBe("ok");
    expect(await store.insert(row({ createKeyHash: "k1" }))).toBe("duplicate_create");
    expect((await store.byCreateKey("p1", "k1"))?.hookId).toBe(a.hookId);
    const g = row({ provider: "gmail", routeKey: "rk" });
    expect(await store.insert(g)).toBe("ok");
    expect(await store.insert(row({ provider: "gmail", routeKey: "rk", principalId: "p2" }))).toBe("route_taken");
    expect((await store.byRouteKey("gmail", "rk"))?.hookId).toBe(g.hookId);
    expect((await store.get(a.hookId))?.provider).toBe("github");
    expect((await store.listByPrincipal("p1")).map((h) => h.hookId)).toEqual([a.hookId, g.hookId]);
    expect(await store.update({ ...a, generation: 2, previousGeneration: 1, previousValidUntil: 5000, rotatedAt: 4000 })).toBe(true);
    expect(await store.update({ ...a, principalId: "p2" })).toBe(false);
    const updated = await store.get(a.hookId);
    expect([updated?.generation, updated?.previousGeneration, updated?.previousValidUntil]).toEqual([2, 1, 5000]);
    await store.touch(a.hookId, 7000);
    expect((await store.get(a.hookId))?.lastDeliveryAt).toBe(7000);
    const sealed = row({ provider: "linear", sealedSecret: "v1.envelope" });
    await store.insert(sealed);
    expect(await store.remove(sealed.hookId, "p2", 9000)).toBe(false);
    expect(await store.remove(sealed.hookId, "p1", 9000)).toBe(true);
    expect(await store.get(sealed.hookId)).toBeUndefined();
    expect(await store.remove(g.hookId, "p1", 9000)).toBe(true);
    // The route is free again once its hook is gone.
    expect(await store.insert(row({ provider: "gmail", routeKey: "rk", principalId: "p2" }))).toBe("ok");
  });
  it("a deleted hook keeps no envelope (sql)", async () => {
    const db = sqliteDouble(HOOK_SQL);
    const store = new SqlHookStore(db);
    const r = row({ provider: "slack", sealedSecret: "v1.secret-envelope", previousSealedSecret: "v1.prev" });
    await store.insert(r);
    await store.remove(r.hookId, "p1", 5);
    const left = await db.query("SELECT sealed_secret, previous_sealed_secret, route_key, deleted_at FROM ingest_hooks WHERE hook_id=$1", [r.hookId]);
    expect(left[0]).toEqual({ sealed_secret: null, previous_sealed_secret: null, route_key: null, deleted_at: 5 });
  });
});

// --- the routes, end to end through the agent app ---

interface Seen { url: string; headers: Record<string, string>; body: string; principal: AgentPrincipal }

interface HookRig extends AgentRig {
  seen: Seen[];
  hooks: InMemoryHookStore;
  core: { status: number; quantity: string | null; errorCode: string | null; body: string };
}

function makeHookRig(over: Partial<IngestHookDeps> = {}): HookRig {
  const seen: Seen[] = [];
  const core = { status: 200, quantity: "1" as string | null, errorCode: null as string | null, body: '{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\\"event_ids\\":[4242]}"}]}}' };
  const hooks = new InMemoryHookStore();
  const store = new InMemoryAgentStore();
  const rig = makeAgentRig({
    mcpUpstream: async (req, principal) => {
      seen.push({ url: req.url, headers: Object.fromEntries(req.headers.entries()), body: await req.text(), principal });
      const headers: Record<string, string> = { "content-type": "application/json" };
      if (core.quantity !== null) headers["mcp-quantity"] = core.quantity;
      if (core.errorCode !== null) headers["mcp-error-code"] = core.errorCode;
      return new Response(core.body, { status: core.status, headers });
    },
    ingestHooks: { store: hooks, secrets: new HookSecrets(MASTER), gmail: { audience: AUDIENCE, serviceAccount: PUSH_SA, jwks: fixedJwks }, ...over },
  }, store);
  return { ...rig, seen, hooks, core };
}

async function createHook(rig: AgentRig, key: string, body: Record<string, unknown>, headers: Record<string, string> = {}): Promise<Record<string, unknown> & { hook_id: string; url: string }> {
  const res = await authed(rig, key, "/v1/ingest-hooks", { method: "POST", body, headers });
  if (res.status !== 201 && res.status !== 200) throw new Error(`create failed ${res.status} ${await res.text()}`);
  return (await res.json()) as Record<string, unknown> & { hook_id: string; url: string };
}

const post = (rig: AgentRig, path: string, body: string, headers: Record<string, string> = {}, ip = "198.51.100.7"): Promise<Response> =>
  Promise.resolve(rig.app.request(path, { method: "POST", headers: { "content-type": "application/json", "cf-connecting-ip": ip, ...headers }, body }));

const pathOf = (url: string): string => new URL(url).pathname;

const ISSUE_TEXT = "Customer ACME asked for the renewal quote by Friday";
const githubBody = JSON.stringify({ action: "opened", issue: { number: 7, title: "Renewal", body: ISSUE_TEXT, html_url: "https://github.com/acme/web/issues/7" }, repository: { full_name: "acme/web" }, sender: { login: "octo" } });

async function wallet(rig: AgentRig, key: string): Promise<number> {
  const w = (await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number };
  return w.balance_micro_usd;
}

describe("ingest hooks: GitHub end to end", () => {
  it("creates a hook (secret shown once), relays a signed delivery as memory.ingest_webhook and bills the stored events", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    expect(hook.url).toBe(`${BASE}/v1/ingest-hooks/github/${hook.hook_id}`);
    expect(typeof hook.secret).toBe("string");
    const listed = (await (await authed(rig, key, "/v1/ingest-hooks")).json()) as { hooks: Array<Record<string, unknown>> };
    expect(listed.hooks).toHaveLength(1);
    expect(JSON.stringify(listed)).not.toContain(String(hook.secret));
    const one = (await (await authed(rig, key, `/v1/ingest-hooks/${hook.hook_id}`)).json()) as Record<string, unknown>;
    expect(JSON.stringify(one)).not.toContain(String(hook.secret));

    rig.core.quantity = "1";
    const res = await post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-github-delivery": "72d3162e-cc78-11e3-81ab-4c9367dc0958", "x-hub-signature-256": sign.github(String(hook.secret), githubBody) });
    expect(res.status).toBe(200);
    const reply = (await res.json()) as Record<string, unknown>;
    // The provider sees a fixed acknowledgement, never the core's reply (event ids, receipt).
    expect(reply).toEqual({ received: true, stored_events: 1 });
    expect(res.headers.get("x-usage-unit")).toBe("ingest");
    expect(res.headers.get("x-usage-quantity")).toBe("1");
    expect(rig.seen).toHaveLength(1);
    const call = rig.seen[0]!;
    expect(pathOf(call.url)).toBe("/mcp");
    expect(call.headers["mcp-method"]).toBe("tools/call");
    expect(call.headers["mcp-name"]).toBe(WEBHOOK_TOOL);
    expect(call.headers["idempotency-key"]).toBe("hook:github:72d3162e-cc78-11e3-81ab-4c9367dc0958");
    expect(call.principal).toMatchObject({ principalId, keyId: hook.hook_id, scopes: ["write"] });
    const rpc = JSON.parse(call.body) as { method: string; params: { name: string; arguments: Record<string, unknown> } };
    expect(rpc.method).toBe("tools/call");
    expect(rpc.params.name).toBe(WEBHOOK_TOOL);
    expect(rpc.params.arguments).toMatchObject({ provider: "github", event: "issues", hook_id: hook.hook_id, delivery_id: "72d3162e-cc78-11e3-81ab-4c9367dc0958", idempotency_key: "hook:github:72d3162e-cc78-11e3-81ab-4c9367dc0958" });
    // The verified bytes travel encoded and untouched.
    expect(Buffer.from(String(rpc.params.arguments.payload_b64), "base64").toString("utf8")).toBe(githubBody);
    expect(await wallet(rig, key)).toBe(FREE_ALLOWANCE_MICROS - RATE_MICROS.ingest);
    const receipts = await rig.store.receipts(principalId, 0, 10);
    expect(receipts).toHaveLength(1);
    expect(receipts[0]).toMatchObject({ keyId: hook.hook_id, name: WEBHOOK_TOOL, unit: "ingest", quantity: MAX_EVENTS_PER_DELIVERY, settledQuantity: 1 });
    expect((await rig.hooks.get(hook.hook_id))?.lastDeliveryAt).toBeDefined();
  });

  it("nothing from a body or a secret persists in the gateway's stores or log lines", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    await post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-github-delivery": "d-1", "x-hub-signature-256": sign.github(String(hook.secret), githubBody) });
    await post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-hub-signature-256": "sha256=" + "0".repeat(64) });
    const persisted = JSON.stringify([await rig.hooks.listByPrincipal(principalId), await rig.store.receipts(principalId, 0, 10), rig.logLines]);
    for (const needle of [ISSUE_TEXT, "Renewal", "acme/web", String(hook.secret), "4242", "d-1"]) expect(persisted).not.toContain(needle);
    expect(rig.logLines.some((l) => l.includes("/v1/ingest-hooks/github/:hook") && l.includes("event=ingest_hook_relayed"))).toBe(true);
    expect(rig.logLines.some((l) => l.includes(hook.hook_id))).toBe(false);
  });

  it("a bad signature or an unknown hook costs nothing, reaches no core, and is rate limited", async () => {
    const rig = makeHookRig();
    const { key } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    const before = await wallet(rig, key);
    const bad = await post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-hub-signature-256": sign.github("wrong", githubBody) });
    expect(bad.status).toBe(401);
    expect(((await bad.json()) as { error: { code: string } }).error.code).toBe("invalid_signature");
    expect((await post(rig, `/v1/ingest-hooks/github/${newHookId()}`, githubBody, { "x-hub-signature-256": sign.github(String(hook.secret), githubBody) })).status).toBe(401);
    expect((await post(rig, "/v1/ingest-hooks/github/not-a-hook", githubBody)).status).toBe(401);
    // A hook id under the wrong provider is unknown there.
    expect((await post(rig, `/v1/ingest-hooks/linear/${hook.hook_id}`, githubBody, { "linear-signature": sign.linear(String(hook.secret), githubBody) })).status).toBe(401);
    expect(rig.seen).toHaveLength(0);
    expect(await wallet(rig, key)).toBe(before);
    let last: Response | undefined;
    for (let i = 0; i < FAILURES_PER_WINDOW + 2; i++) last = await post(rig, pathOf(hook.url), githubBody, { "x-hub-signature-256": "sha256=00" }, "192.0.2.99");
    expect(last?.status).toBe(429);
    expect(last?.headers.get("retry-after")).toMatch(/^\d+$/);
    // The hook's own budget is spent too: a correctly signed delivery from elsewhere waits.
    const blocked = await post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-hub-signature-256": sign.github(String(hook.secret), githubBody) }, "203.0.113.50");
    expect(blocked.status).toBe(429);
    expect(rig.seen).toHaveLength(0);
  });

  it("ping is a free handshake; a body over 1 MiB is refused", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    const ping = '{"zen":"Design for failure."}';
    const res = await post(rig, pathOf(hook.url), ping, { "x-github-event": "ping", "x-hub-signature-256": sign.github(String(hook.secret), ping) });
    expect(res.status).toBe(200);
    expect(rig.seen).toHaveLength(0);
    expect(await rig.store.receipts(principalId, 0, 10)).toHaveLength(0);
    const huge = "x".repeat(1024 * 1024 + 1);
    expect((await post(rig, pathOf(hook.url), huge, { "x-hub-signature-256": sign.github(String(hook.secret), huge) })).status).toBe(413);
  });

  it("the core's answer decides retry vs final; neither is charged", async () => {
    const rig = makeHookRig();
    const { key } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    const headers = { "x-github-event": "issues", "x-hub-signature-256": sign.github(String(hook.secret), githubBody) };
    rig.core.quantity = "0";
    rig.core.errorCode = "-32000";
    const down = await post(rig, pathOf(hook.url), githubBody, headers);
    expect(down.status).toBe(503);
    expect(down.headers.get("retry-after")).toBe("30");
    rig.core.errorCode = "-32602";
    const refused = await post(rig, pathOf(hook.url), githubBody, headers);
    expect(refused.status).toBe(200);
    expect(await refused.json()).toEqual({ received: true, stored_events: 0, refused: true });
    rig.core.errorCode = null;
    rig.core.status = 502;
    expect((await post(rig, pathOf(hook.url), githubBody, headers)).status).toBe(503);
    expect(await wallet(rig, key)).toBe(FREE_ALLOWANCE_MICROS);
  });

  it("rotation: the old secret works for the grace period, then only the new one", async () => {
    const rig = makeHookRig();
    const { key } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "github" });
    const rotated = (await (await authed(rig, key, `/v1/ingest-hooks/${hook.hook_id}/rotate`, { method: "POST", body: {} })).json()) as Record<string, unknown>;
    expect(rotated.secret).not.toBe(hook.secret);
    const send = (secret: string): Promise<Response> => post(rig, pathOf(hook.url), githubBody, { "x-github-event": "issues", "x-hub-signature-256": sign.github(secret, githubBody) });
    expect((await send(String(hook.secret))).status).toBe(200);
    expect((await send(String(rotated.secret))).status).toBe(200);
    rig.setNow(new Date(rig.now().getTime() + ROTATION_GRACE_MS + 1000));
    expect((await send(String(hook.secret))).status).toBe(401);
    expect((await send(String(rotated.secret))).status).toBe(200);
  });

  it("create is idempotent under Idempotency-Key; delete stops deliveries; other principals see nothing", async () => {
    const rig = makeHookRig();
    const { key } = await registerAgent(rig);
    const first = await createHook(rig, key, { provider: "github" }, { "idempotency-key": "create-1" });
    const again = await createHook(rig, key, { provider: "github" }, { "idempotency-key": "create-1" });
    expect(again.hook_id).toBe(first.hook_id);
    expect(again.secret).toBe(first.secret);
    expect(again.replayed).toBe(true);
    const other = await registerAgent(rig, {}, "203.0.113.9");
    expect((await authed(rig, other.key, `/v1/ingest-hooks/${first.hook_id}`)).status).toBe(404);
    expect((await authed(rig, other.key, `/v1/ingest-hooks/${first.hook_id}`, { method: "DELETE" })).status).toBe(404);
    expect((await authed(rig, key, `/v1/ingest-hooks/${first.hook_id}`, { method: "DELETE" })).status).toBe(204);
    expect((await post(rig, pathOf(first.url), githubBody, { "x-hub-signature-256": sign.github(String(first.secret), githubBody) })).status).toBe(401);
  });

  it("validates registrations", async () => {
    const rig = makeHookRig();
    const { key } = await registerAgent(rig);
    const code = async (body: unknown): Promise<string> => ((await (await authed(rig, key, "/v1/ingest-hooks", { method: "POST", body })).json()) as { error: { code: string } }).error.code;
    expect(await code({ provider: "jira" })).toBe("invalid_provider");
    expect(await code({ provider: "linear" })).toBe("invalid_signing_secret");
    expect(await code({ provider: "slack", signing_secret: "short" })).toBe("invalid_signing_secret");
    expect(await code({ provider: "gmail" })).toBe("invalid_mailbox");
  });
});

describe("ingest hooks: Linear, Slack, Notion, Calendar, Gmail", () => {
  it("Linear: the supplied signing secret is sealed; Linear-Delivery is the idempotency key", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const secret = "lin_wh_0123456789abcdef0123";
    const hook = await createHook(rig, key, { provider: "linear", signing_secret: secret });
    expect(JSON.stringify(hook)).not.toContain(secret);
    expect(JSON.stringify(await rig.hooks.listByPrincipal(principalId))).not.toContain(secret);
    const body = JSON.stringify({ action: "create", type: "Issue", data: { identifier: "ENG-1", title: "x" }, url: "https://linear.app/a/issue/ENG-1" });
    const res = await post(rig, pathOf(hook.url), body, { "linear-signature": sign.linear(secret, body), "linear-delivery": "b1c2d3e4-0000-4000-8000-000000000001", "linear-event": "Issue" });
    expect(res.status).toBe(200);
    expect(rig.seen[0]?.headers["idempotency-key"]).toBe("hook:linear:b1c2d3e4-0000-4000-8000-000000000001");
    expect(JSON.parse(rig.seen[0]!.body).params.arguments.event).toBe("Issue");
    expect((await post(rig, pathOf(hook.url), body, { "linear-signature": sign.linear("lin_wh_wrong_secret_000000", body) })).status).toBe(401);
  });

  it("Slack: url_verification is echoed for free after the signature; events relay with a body-digest delivery id", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const secret = "8f742231b10e8888abcd99yyyzzz85a5";
    const hook = await createHook(rig, key, { provider: "slack", signing_secret: secret });
    const ts = Math.floor(rig.now().getTime() / 1000);
    const challenge = JSON.stringify({ token: "legacy", challenge: "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P", type: "url_verification" });
    const hs = await post(rig, pathOf(hook.url), challenge, { "x-slack-request-timestamp": String(ts), "x-slack-signature": sign.slack(secret, challenge, ts) });
    expect(hs.status).toBe(200);
    expect(await hs.json()).toEqual({ challenge: "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P" });
    expect(rig.seen).toHaveLength(0);
    expect(await rig.store.receipts(principalId, 0, 10)).toHaveLength(0);
    // An unsigned challenge is not answered.
    expect((await post(rig, pathOf(hook.url), challenge, { "x-slack-request-timestamp": String(ts), "x-slack-signature": "v0=00" })).status).toBe(401);
    const event = JSON.stringify({ type: "event_callback", team_id: "T1", event_id: "Ev1", event: { type: "message", user: "U1", channel: "C1", text: "hello", ts: "1.2" } });
    const stale = await post(rig, pathOf(hook.url), event, { "x-slack-request-timestamp": String(ts - 600), "x-slack-signature": sign.slack(secret, event, ts - 600) });
    expect(stale.status).toBe(401);
    const ok = await post(rig, pathOf(hook.url), event, { "x-slack-request-timestamp": String(ts), "x-slack-signature": sign.slack(secret, event, ts) });
    expect(ok.status).toBe(200);
    expect(rig.seen[0]?.headers["idempotency-key"]).toMatch(/^hook:slack:sha256-[0-9a-f]{64}$/);
  });

  it("Notion: the one-time verification token is sealed, shown to the owner, then signs events", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "notion" });
    expect(hook.status).toBe("awaiting_verification");
    const token = "secret_tYjK3l2wPq8N0aBcDeFgHiJkLmNoPqRs";
    const verification = JSON.stringify({ verification_token: token });
    expect((await post(rig, pathOf(hook.url), verification)).status).toBe(200);
    expect(rig.seen).toHaveLength(0);
    expect(JSON.stringify(await rig.hooks.listByPrincipal(principalId))).not.toContain(token);
    const shown = (await (await authed(rig, key, `/v1/ingest-hooks/${hook.hook_id}`)).json()) as Record<string, unknown>;
    expect(shown.verification_token).toBe(token);
    // A second token cannot replace the first.
    expect((await post(rig, pathOf(hook.url), JSON.stringify({ verification_token: "secret_attackerAttackerAttacker0000" }))).status).toBe(401);
    const event = JSON.stringify({ id: "evt-1", type: "page.content_updated", entity: { id: "1a2b", type: "page" } });
    const res = await post(rig, pathOf(hook.url), event, { "x-notion-signature": sign.notion(token, event) });
    expect(res.status).toBe(200);
    expect((await rig.hooks.get(hook.hook_id))?.status).toBe("active");
    const after = (await (await authed(rig, key, `/v1/ingest-hooks/${hook.hook_id}`)).json()) as Record<string, unknown>;
    expect(after.verification_token).toBeUndefined();
  });

  it("Google Calendar: channel id and token from headers, sync is free, a change relays without a payload", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "google_calendar" });
    expect(hook.channel_id).toBe(hook.hook_id);
    const goog = (state: string, token = String(hook.channel_token), id = hook.hook_id): Record<string, string> => ({
      "x-goog-channel-id": id, "x-goog-channel-token": token, "x-goog-resource-state": state, "x-goog-message-number": "2", "x-goog-resource-id": "ret08u3rv24htgh289g",
      "x-goog-resource-uri": "https://www.googleapis.com/calendar/v3/calendars/primary/events?alt=json",
    });
    expect((await post(rig, pathOf(hook.url), "", goog("sync"))).status).toBe(200);
    expect(await rig.store.receipts(principalId, 0, 10)).toHaveLength(0);
    expect((await post(rig, pathOf(hook.url), "", goog("exists", "wrong-token"))).status).toBe(401);
    expect((await post(rig, pathOf(hook.url), "", goog("exists", String(hook.channel_token), newHookId()))).status).toBe(401);
    rig.core.quantity = "0";
    const res = await post(rig, pathOf(hook.url), "", goog("exists"));
    expect(res.status).toBe(200);
    const args = JSON.parse(rig.seen[0]!.body).params.arguments as Record<string, unknown>;
    expect(args.payload_b64).toBeUndefined();
    expect(args.changed).toEqual({ channel_id: hook.hook_id, resource_id: "ret08u3rv24htgh289g", resource_state: "exists", message_number: "2", resource_uri: "https://www.googleapis.com/calendar/v3/calendars/primary/events?alt=json" });
    expect(rig.seen[0]?.headers["idempotency-key"]).toBe(`hook:google_calendar:${hook.hook_id}:2`);
    expect(await wallet(rig, key)).toBe(FREE_ALLOWANCE_MICROS);
  });

  it("Gmail: Google's OIDC token, routed by mailbox; only the history and message ids are relayed", async () => {
    const rig = makeHookRig();
    const { key, principalId } = await registerAgent(rig);
    const hook = await createHook(rig, key, { provider: "gmail", mailbox: "Ana@Example.com" });
    expect(hook.url).toBe(`${BASE}/v1/ingest-hooks/gmail`);
    expect(JSON.stringify(await rig.hooks.listByPrincipal(principalId)).toLowerCase()).not.toContain("ana@example.com");
    const other = await registerAgent(rig, {}, "203.0.113.9");
    const taken = await authed(rig, other.key, "/v1/ingest-hooks", { method: "POST", body: { provider: "gmail", mailbox: "ana@example.com" } });
    expect(taken.status).toBe(409);
    const nowMs = rig.now().getTime();
    const auth = { authorization: `Bearer ${googleToken(pushClaims(nowMs))}` };
    rig.core.quantity = "0";
    const res = await post(rig, "/v1/ingest-hooks/gmail", pubsubBody("ana@example.com", 98765), auth);
    expect(res.status).toBe(200);
    expect(rig.seen).toHaveLength(1);
    const args = JSON.parse(rig.seen[0]!.body).params.arguments as Record<string, unknown>;
    expect(args).toMatchObject({ provider: "gmail", hook_id: hook.hook_id, delivery_id: "1234567890", changed: { history_id: "98765", message_id: "1234567890" } });
    expect(JSON.stringify(args)).not.toContain("ana@example.com");
    expect(rig.seen[0]?.principal.principalId).toBe(principalId);
    const unrouted = await post(rig, "/v1/ingest-hooks/gmail", pubsubBody("nobody@example.com", 1), auth);
    expect(await unrouted.json()).toEqual({ received: true, routed: false });
    expect(rig.seen).toHaveLength(1);
    const forged = await post(rig, "/v1/ingest-hooks/gmail", pubsubBody("ana@example.com", 2), { authorization: `Bearer ${googleToken(pushClaims(nowMs, { aud: "https://elsewhere" }))}` });
    expect(forged.status).toBe(401);
    expect((await post(rig, "/v1/ingest-hooks/gmail", pubsubBody("ana@example.com", 2))).status).toBe(401);
    expect(rig.seen).toHaveLength(1);
  });

  it("Gmail push without configuration answers 503 and no Gmail hook can be made", async () => {
    const rig = makeHookRig({ gmail: undefined } as unknown as Partial<IngestHookDeps>);
    const { key } = await registerAgent(rig);
    expect((await post(rig, "/v1/ingest-hooks/gmail", pubsubBody("a@b.c", 1))).status).toBe(503);
    expect((await authed(rig, key, "/v1/ingest-hooks", { method: "POST", body: { provider: "gmail", mailbox: "a@b.c" } })).status).toBe(503);
  });
});

describe("ingest hooks: wiring", () => {
  it("the webhook tool is metered as ingest; hook routes collapse to templates", () => {
    expect(classifyCall("tools/call", WEBHOOK_TOOL)).toBe("ingest");
    expect(routeTemplate(`/v1/ingest-hooks/github/${newHookId()}`)).toBe("/v1/ingest-hooks/github/:hook");
    expect(routeTemplate(`/v1/ingest-hooks/${newHookId()}/rotate`)).toBe("/v1/ingest-hooks/:id/rotate");
    expect(routeTemplate(`/v1/ingest-hooks/${newHookId()}`)).toBe("/v1/ingest-hooks/:id");
    expect(routeTemplate("/v1/ingest-hooks/gmail")).toBe("/v1/ingest-hooks/gmail");
    expect(deliveryUrl("https://x/", "gmail", "hk_1")).toBe("https://x/v1/ingest-hooks/gmail");
  });

  it("boot: INGEST_HOOKS_SECRET turns the hooks on; Gmail needs both of its settings", () => {
    expect(ingestHooksConfigFromEnv({})).toBeUndefined();
    expect(ingestHooksConfigFromEnv({ INGEST_HOOKS_SECRET: MASTER })).toEqual({ secret: MASTER });
    expect(ingestHooksConfigFromEnv({ INGEST_HOOKS_SECRET: MASTER, INGEST_HOOKS_GMAIL_AUDIENCE: AUDIENCE, INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT: PUSH_SA })).toEqual({ secret: MASTER, gmail: { audience: AUDIENCE, serviceAccount: PUSH_SA } });
    expect(() => ingestHooksConfigFromEnv({ INGEST_HOOKS_SECRET: "short" })).toThrow(BootError);
    expect(() => ingestHooksConfigFromEnv({ INGEST_HOOKS_SECRET: MASTER, INGEST_HOOKS_GMAIL_AUDIENCE: AUDIENCE })).toThrow(BootError);
    expect(() => ingestHooksConfigFromEnv({ INGEST_HOOKS_GMAIL_AUDIENCE: AUDIENCE, INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT: PUSH_SA })).toThrow(BootError);
    expect(() => ingestHooksConfigFromEnv({ INGEST_HOOKS_SECRET: MASTER, INGEST_HOOKS_GMAIL_AUDIENCE: "http://x", INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT: PUSH_SA })).toThrow(BootError);
  });

  it("without INGEST_HOOKS_SECRET the routes do not exist", async () => {
    const rig = makeAgentRig();
    expect((await post(rig, `/v1/ingest-hooks/github/${newHookId()}`, "{}")).status).toBe(404);
  });
});
