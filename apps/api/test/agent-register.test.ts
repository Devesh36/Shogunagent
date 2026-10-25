/** E3-T3: self-registration — a scoped key bound to a fresh principal with the $5 allowance,
 * behind a proof of work and a per-IP limit. */
import { describe, expect, it } from "vitest";

import { DEFAULT_CAPS } from "../src/caps.js";
import { CHALLENGE_TTL_MS, leadingZeroBits, mintChallenge, powDigest, solvePow, verifyPow } from "../src/pow.js";
import { FREE_ALLOWANCE_MICROS } from "../src/pricing.js";
import { WindowLimiter } from "../src/ratelimit.js";
import { POW_SECRET, TEST_POW_BITS, authed, makeAgentRig, register, registerAgent, solvedPowHeader } from "./agent-helpers.js";

describe("proof of work", () => {
  const NOW = Date.parse("2026-10-25T12:00:00Z");

  it("counts leading zero bits of a hex digest", () => {
    expect(leadingZeroBits("ffff")).toBe(0);
    expect(leadingZeroBits("0fff")).toBe(4);
    expect(leadingZeroBits("00ff")).toBe(8);
    expect(leadingZeroBits("007f")).toBe(9);
    expect(leadingZeroBits("0001")).toBe(15);
  });

  it("mints a MAC'd challenge that verifies with a solving nonce and refuses everything else", () => {
    const { challenge, difficulty_bits, expires_at } = mintChallenge(POW_SECRET, 8, NOW);
    expect(difficulty_bits).toBe(8);
    expect(Date.parse(expires_at)).toBe(NOW + CHALLENGE_TTL_MS);
    const nonce = solvePow(challenge, 8)!;
    expect(leadingZeroBits(powDigest(challenge, nonce))).toBeGreaterThanOrEqual(8);
    expect(verifyPow(`${challenge}:${nonce}`, POW_SECRET, 8, NOW)).toEqual({ ok: true, challenge, expiresAt: NOW + CHALLENGE_TTL_MS });
    expect(verifyPow(undefined, POW_SECRET, 8, NOW)).toEqual({ ok: false, code: "pow_missing" });
    expect(verifyPow("garbage", POW_SECRET, 8, NOW)).toEqual({ ok: false, code: "pow_malformed" });
    expect(verifyPow(`${challenge}:${nonce}`, "other-secret", 8, NOW)).toEqual({ ok: false, code: "pow_invalid" });
    expect(verifyPow(`${challenge}:${nonce}`, POW_SECRET, 8, NOW + CHALLENGE_TTL_MS)).toEqual({ ok: false, code: "pow_expired" });
    expect(verifyPow(`${challenge}:${nonce}`, POW_SECRET, 32, NOW)).toEqual({ ok: false, code: "pow_insufficient" });
    // A forged expiry does not verify: the MAC covers it.
    const forged = challenge.replace(/^\d+/, String(NOW + 10 * CHALLENGE_TTL_MS));
    expect(verifyPow(`${forged}:${nonce}`, POW_SECRET, 8, NOW).ok).toBe(false);
  });
});

describe("POST /v1/agents", () => {
  it("returns a live key with read,write,run scopes, a fresh principal, the $5 allowance and the default caps", async () => {
    const rig = makeAgentRig();
    const reg = await registerAgent(rig, { ref: "dev_alice" });
    expect(reg.key).toMatch(/^sa_live_[A-Za-z0-9_-]{43}$/);
    expect(reg.body).toEqual(expect.objectContaining({
      key_id: expect.stringMatching(/^ak_/), principal_id: expect.stringMatching(/^prn_/), scopes: ["read", "write", "run"], ref: "dev_alice",
      wallet: { balance_micro_usd: FREE_ALLOWANCE_MICROS, reserved_micro_usd: 0 },
      caps: { key: { day_micro_usd: DEFAULT_CAPS.key.day, month_micro_usd: DEFAULT_CAPS.key.month }, principal: { day_micro_usd: DEFAULT_CAPS.principal.day, month_micro_usd: DEFAULT_CAPS.principal.month } },
    }));
    expect(reg.body.tenant_id).toBe(reg.principalId);
    expect(Date.parse(String(reg.body.expires_at))).toBeGreaterThan(rig.now().getTime());
    // The key logs in, and the wallet shows the allowance.
    const wallet = await authed(rig, reg.key, "/v1/wallet");
    expect(wallet.status).toBe(200);
    expect(((await wallet.json()) as { available_micro_usd: number }).available_micro_usd).toBe(FREE_ALLOWANCE_MICROS);
    const me = await authed(rig, reg.key, "/v1/agents/me");
    expect(((await me.json()) as { principal_id: string; ref: string }).ref).toBe("dev_alice");
    // Only a hash of the key is stored.
    const stored = await rig.store.keyById(reg.keyId);
    expect(stored?.hash.startsWith("pbkdf2-sha256$")).toBe(true);
    expect(stored?.hash).not.toContain(reg.key.slice(8));
    expect(stored?.ref).toBe("dev_alice");
  });

  it("works with an empty body and refuses a malformed ref or caps above the defaults", async () => {
    const rig = makeAgentRig();
    const empty = await rig.app.request("/v1/agents", { method: "POST", headers: { "cf-connecting-ip": "203.0.113.9", "x-agent-pow": await solvedPowHeader(rig) } });
    expect(empty.status).toBe(201);
    expect((await register(rig, { ref: "has spaces" })).status).toBe(400);
    expect((await register(rig, { caps: { day_micro_usd: DEFAULT_CAPS.key.day + 1 } })).status).toBe(400);
    expect((await register(rig, { caps: { day_micro_usd: "1" } })).status).toBe(400);
    // Lowering is allowed and the response shows the lowered cap.
    const lowered = await register(rig, { caps: { day_micro_usd: 250_000 } });
    expect(lowered.status).toBe(201);
    expect(((await lowered.json()) as { caps: { key: { day_micro_usd: number } } }).caps.key.day_micro_usd).toBe(250_000);
  });

  it("refuses a missing, wrong, insufficient or reused proof of work with 401 and names the fix", async () => {
    const rig = makeAgentRig();
    const missing = await rig.app.request("/v1/agents", { method: "POST", headers: { "cf-connecting-ip": "203.0.113.2" } });
    expect(missing.status).toBe(401);
    const body = (await missing.json()) as { error: { code: string; message: string; next_action: string } };
    expect(body.error.code).toBe("pow_missing");
    expect(body.error.next_action).toBe("solve_challenge");
    expect(body.error.message).toContain("/v1/agents/challenge");
    const challenge = ((await (await rig.app.request("/v1/agents/challenge")).json()) as { challenge: string }).challenge;
    let weak = 0;
    while (leadingZeroBits(powDigest(challenge, String(weak))) >= TEST_POW_BITS) weak += 1;
    const insufficient = await register(rig, {}, "203.0.113.2", `${challenge}:${weak}`);
    expect(insufficient.status).toBe(401);
    expect(((await insufficient.json()) as { error: { code: string } }).error.code).toBe("pow_insufficient");
    // A solved challenge registers once; the same solution again is refused.
    const solved = await solvedPowHeader(rig);
    expect((await register(rig, {}, "203.0.113.2", solved)).status).toBe(201);
    const reused = await register(rig, {}, "203.0.113.2", solved);
    expect(reused.status).toBe(401);
    expect(((await reused.json()) as { error: { code: string } }).error.code).toBe("pow_reused");
    expect(rig.logLines.filter((l) => l.includes("event=pow_rejected"))).toHaveLength(3);
    // An expired challenge (the clock moved past its TTL) is refused too.
    const late = await solvedPowHeader(rig);
    rig.setNow(new Date(rig.now().getTime() + CHALLENGE_TTL_MS + 1));
    expect(((await (await register(rig, {}, "203.0.113.2", late)).json()) as { error: { code: string } }).error.code).toBe("pow_expired");
  });

  it("limits registrations per IP, before the proof of work is even checked", async () => {
    const rig = makeAgentRig({ registerLimiter: new WindowLimiter(2, 3_600_000) });
    expect((await register(rig, {}, "198.51.100.1")).status).toBe(201);
    expect((await register(rig, {}, "198.51.100.1")).status).toBe(201);
    const third = await rig.app.request("/v1/agents", { method: "POST", headers: { "cf-connecting-ip": "198.51.100.1" } });
    expect(third.status).toBe(429);
    expect(Number(third.headers.get("retry-after"))).toBeGreaterThan(0);
    expect(((await third.json()) as { error: { code: string } }).error.code).toBe("rate_limited");
    // Another address is unaffected; X-Forwarded-For is honoured without CF-Connecting-IP.
    expect((await register(rig, {}, "198.51.100.2")).status).toBe(201);
    const viaXff = await rig.app.request("/v1/agents", { method: "POST", headers: { "x-forwarded-for": "198.51.100.1, 10.0.0.1", "x-agent-pow": await solvedPowHeader(rig) } });
    expect(viaXff.status).toBe(429);
    expect(rig.logLines.filter((l) => l.includes("event=register_rate_limited"))).toHaveLength(2);
  });

  it("answers 503 and holds nothing when the store cannot be written", async () => {
    const rig = makeAgentRig();
    const original = rig.store.insertKey.bind(rig.store);
    rig.store.insertKey = () => Promise.reject(new Error("down"));
    const res = await register(rig);
    expect(res.status).toBe(503);
    rig.store.insertKey = original;
    expect((await register(rig)).status).toBe(201);
  });
});

describe("key management", () => {
  it("revokes its own key (and not someone else's), lowers caps itself, needs admin to raise them", async () => {
    const rig = makeAgentRig();
    const a = await registerAgent(rig, {}, "203.0.113.10");
    const b = await registerAgent(rig, {}, "203.0.113.11");
    expect((await authed(rig, a.key, `/v1/keys/${b.keyId}/revoke`, { method: "POST" })).status).toBe(404);
    const lowered = await authed(rig, a.key, `/v1/keys/${a.keyId}`, { method: "PATCH", body: { day_micro_usd: 100_000 } });
    expect(lowered.status).toBe(200);
    expect(((await lowered.json()) as { caps: { key: { day_micro_usd: number; month_micro_usd: number } } }).caps.key).toEqual({ day_micro_usd: 100_000, month_micro_usd: DEFAULT_CAPS.key.month });
    const raised = await authed(rig, a.key, `/v1/keys/${a.keyId}`, { method: "PATCH", body: { day_micro_usd: DEFAULT_CAPS.key.day * 2 } });
    expect(raised.status).toBe(403);
    expect(((await raised.json()) as { error: { code: string } }).error.code).toBe("caps_above_default");
    expect((await authed(rig, a.key, `/v1/keys/${a.keyId}`, { method: "PATCH", body: { day_micro_usd: -1 } })).status).toBe(400);
    expect((await authed(rig, a.key, `/v1/keys/${a.keyId}/revoke`, { method: "POST" })).status).toBe(200);
    const after = await authed(rig, a.key, "/v1/wallet");
    expect(after.status).toBe(401);
    expect(((await after.json()) as { error: { code: string } }).error.code).toBe("key_revoked");
    expect((await authed(rig, b.key, "/v1/wallet")).status).toBe(200);
  });
});
