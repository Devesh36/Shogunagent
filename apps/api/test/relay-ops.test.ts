/** Issue #787: auth before body buffering, error mapping on transport failures, the lifecycle
 * log event, and the boot checks (numeric env, server timeouts). */
import { request as httpRequest, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { serve } from "@hono/node-server";
import { afterEach, describe, expect, it, vi } from "vitest";

import { verifyLicense } from "../src/auth.js";
import { BootError, SERVER_TIMEOUTS, intFromEnv } from "../src/boot.js";
import { FetchAnthropicGateway, NO_RESPONSE, UpstreamError } from "../src/gateway.js";
import { TokenBucketLimiter } from "../src/ratelimit.js";
import { InMemoryUsageStore, LedgerUnavailable } from "../src/usage.js";
import { makeKeys, makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";

afterEach(() => vi.unstubAllGlobals());

/** A request body that counts every pull. `highWaterMark: 0` means nothing is pulled until a
 * reader asks, so `pulls() === 0` proves nobody read the body. */
function countingBody(chunks = 16, chunkBytes = 1024 * 1024): { body: ReadableStream<Uint8Array>; pulls: () => number } {
  let pulled = 0;
  const body = new ReadableStream<Uint8Array>({
    pull(controller) {
      pulled += 1;
      controller.enqueue(new Uint8Array(chunkBytes).fill(0x78));
      if (pulled >= chunks) controller.close();
    },
  }, { highWaterMark: 0 });
  return { body, pulls: () => pulled };
}

function streamedPost(path: string, body: ReadableStream<Uint8Array>, headers: Record<string, string>): RequestInit & { duplex: "half" } {
  return { method: "POST", headers: { "content-type": "application/json", ...headers }, body, duplex: "half" };
}

describe("authentication runs before the body is read (#787 item 1)", () => {
  it("answers an unauthenticated chunked body 401 without pulling a byte of it", async () => {
    const rig = await makeRig();
    for (const path of ["/v1/batch", "/v1/batch/recover"]) {
      const { body, pulls } = countingBody();
      const res = await rig.app.request(path, streamedPost(path, body, {}));
      expect(res.status).toBe(401);
      expect(pulls()).toBe(0);
    }
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("answers a forged bearer with a huge Content-Length 401, not 413 — auth comes first", async () => {
    const rig = await makeRig();
    const stranger = await makeKeys();
    const forged = await signLicense(stranger.privateKey);
    const { body, pulls } = countingBody();
    const res = await rig.app.request("/v1/batch", streamedPost("/v1/batch", body, {
      authorization: `Bearer ${forged}`,
      "content-length": String(64 * 1024 * 1024),
    }));
    expect(res.status).toBe(401);
    expect(pulls()).toBe(0);
  });

  it("answers a plan outside the lane 402 and a rate-limited licence 429 before reading the body", async () => {
    const rig = await makeRig({ rateLimit: new TokenBucketLimiter(1, 1), now: () => new Date("2026-10-01T00:00:00Z") });
    const free = await signLicense(rig.keys.privateKey, { plan: "free", iat: Math.floor(Date.parse("2026-10-01T00:00:00Z") / 1000) });
    const refused = countingBody();
    expect((await rig.app.request("/v1/batch", streamedPost("/v1/batch", refused.body, { authorization: `Bearer ${free}` }))).status).toBe(402);
    expect(refused.pulls()).toBe(0);

    expect((await postBatch(rig, sampleBody)).status).toBe(202); // spends the only slot
    const limited = countingBody();
    const res = await rig.app.request("/v1/batch", streamedPost("/v1/batch", limited.body, { authorization: `Bearer ${rig.token}` }));
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).not.toBeNull();
    expect(limited.pulls()).toBe(0);
  });

  it("still bounds an authenticated body with 413", async () => {
    const rig = await makeRig();
    const { body } = countingBody(9); // 9 MiB, chunked: no Content-Length to reject up front
    const res = await rig.app.request("/v1/batch", streamedPost("/v1/batch", body, { authorization: `Bearer ${rig.token}` }));
    expect(res.status).toBe(413);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("over a real socket, answers 401 while an unauthenticated upload is still open", async () => {
    const rig = await makeRig();
    const server = await new Promise<Server>((resolve) => {
      const s = serve({ fetch: rig.app.fetch, port: 0, hostname: "127.0.0.1", serverOptions: { ...SERVER_TIMEOUTS } }, () => resolve(s as Server));
    });
    try {
      const { port } = server.address() as AddressInfo;
      const status = await new Promise<number>((resolve, reject) => {
        const req = httpRequest({ host: "127.0.0.1", port, method: "POST", path: "/v1/batch", headers: { "content-type": "application/json", "transfer-encoding": "chunked" } }, (res) => {
          res.resume();
          resolve(res.statusCode ?? 0);
          req.destroy();
        });
        req.on("error", (e) => { if (!req.destroyed) reject(e); });
        // One chunk, and the request is never ended: a handler that waited for the body would
        // never answer, and the test would time out.
        req.write("{\"purpose\":\"consolidation\",\"items\":[");
      });
      expect(status).toBe(401);
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  });
});

describe("malformed bearer segments are 401, never 500 (#787 item 2)", () => {
  /** Re-shape one segment of an otherwise valid token. */
  async function withSegment(i: 1 | 2, f: (seg: string) => string): Promise<{ rig: Awaited<ReturnType<typeof makeRig>>; token: string }> {
    const rig = await makeRig();
    const parts = rig.token.split(".");
    parts[i] = f(parts[i] ?? "");
    return { rig, token: parts.join(".") };
  }

  const cases: [string, 1 | 2, (s: string) => string][] = [
    ["padded payload", 1, (s) => `${s}==`],
    ["single pad on payload", 1, (s) => `${s}=`],
    ["padded signature", 2, (s) => `${s}==`],
    ["standard-alphabet characters in the payload", 1, (s) => s.replace(/-/g, "+").replace(/_/g, "/") + "+/"],
    ["an illegal character in the payload", 1, (s) => `${s.slice(0, 4)}!${s.slice(4)}`],
    ["an illegal character in the signature", 2, (s) => `${s.slice(0, 4)}*${s.slice(4)}`],
    ["whitespace inside the payload", 1, (s) => `${s.slice(0, 4)} ${s.slice(4)}`],
    ["non-canonical trailing bits in the signature", 2, (s) => s.slice(0, -1) + (s.endsWith("A") ? "B" : "A")],
    ["an empty signature", 2, () => ""],
  ];

  for (const [label, i, f] of cases) {
    it(`rejects ${label} with 401`, async () => {
      const { rig, token } = await withSegment(i, f);
      const res = await postBatch(rig, sampleBody, token);
      expect(res.status).toBe(401);
      expect(rig.gateway.createCalls).toHaveLength(0);
      expect(() => verifyLicense(`Bearer ${token}`, rig.keys.publicKey)).toThrowError(/invalid or expired|missing/);
    });
  }

  it("still accepts the canonical token", async () => {
    const rig = await makeRig();
    expect(verifyLicense(`Bearer ${rig.token}`, rig.keys.publicKey).licenseId).toBe("lic_test_1");
  });
});

describe("upstream transport failures map to 502 (#787 item 3)", () => {
  it("wraps a network failure and a timeout as UpstreamError without a definite rejection", async () => {
    for (const failure of [new TypeError("fetch failed"), new DOMException("The operation was aborted due to timeout", "TimeoutError")]) {
      vi.stubGlobal("fetch", vi.fn(async () => { throw failure; }));
      const gateway = new FetchAnthropicGateway("provider-key");
      for (const call of [() => gateway.createBatch([]), () => gateway.getBatch("msgbatch_1")]) {
        const err = await call().then(() => undefined, (e: unknown) => e);
        expect(err).toBeInstanceOf(UpstreamError);
        expect((err as UpstreamError).status).toBe(NO_RESPONSE);
        expect((err as UpstreamError).definitelyRejected).toBe(false);
        // Nothing of the cause (which can carry the URL) is in the message.
        expect((err as Error).message).not.toContain("fetch failed");
      }
      vi.unstubAllGlobals();
    }
  });

  it("wraps a 2xx whose body is not JSON as an UpstreamError, not a SyntaxError", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("<html>", { status: 200 })));
    const err = await new FetchAnthropicGateway("provider-key").createBatch([]).then(() => undefined, (e: unknown) => e);
    expect(err).toBeInstanceOf(UpstreamError);
    expect((err as UpstreamError).definitelyRejected).toBe(false);
  });

  it("answers 502 'upstream failure' and keeps the claim unknown when the create's connection dies", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => { throw new TypeError("fetch failed"); }));
    const usage = new InMemoryUsageStore();
    const rig = await makeRig({ gateway: new FetchAnthropicGateway("provider-key"), usage });
    const body = { ...sampleBody, client_request_id: "night-787" };
    const res = await postBatch(rig, body);
    expect(res.status).toBe(502);
    expect(await res.json()).toEqual({ error: "upstream failure" });
    expect(rig.logLines.at(-1)).toContain("event=submission_unknown");
    // Not "definitely rejected": the reservation is kept and the same request is not resubmitted.
    expect(await usage.usedOn("lic_test_1", new Date().toISOString().slice(0, 10))).toBe(2);
    const retry = await postBatch(rig, body);
    expect(retry.status).toBe(503);
    expect(((await retry.json()) as { state: string }).state).toBe("submission_unknown");
  });

  it("logs a reservation that lost its claim as submission_unknown, matching the answer", async () => {
    // The claim was taken over between claim and reserve: the device is told the submission is
    // unknown, and the lifecycle event an operator alerts on has to say the same.
    class LostClaimStore extends InMemoryUsageStore {
      override async tryReserve() {
        return "unknown" as const;
      }
    }
    const rig = await makeRig({ usage: new LostClaimStore() });
    const res = await postBatch(rig, { ...sampleBody, client_request_id: "night-lost-claim" });
    expect(res.status).toBe(503);
    expect(((await res.json()) as { state: string }).state).toBe("submission_unknown");
    expect(rig.logLines.at(-1)).toContain("event=submission_unknown");
  });

  it("answers a poll whose connection dies 502, not 500", async () => {
    const rig = await makeRig({ gateway: new FetchAnthropicGateway("provider-key") });
    await rig.usage.attachBatch("rb_1", "lic_test_1", new Date().toISOString().slice(0, 10), 1, "dev_test_device", "msgbatch_1");
    vi.stubGlobal("fetch", vi.fn(async () => { throw new TypeError("fetch failed"); }));
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    expect(res.status).toBe(502);
  });
});

describe("a failed bind keeps its lifecycle event in the log (#787 item 5)", () => {
  it("logs provider_accepted_unbound, not metering_unavailable", async () => {
    const usage = new InMemoryUsageStore();
    usage.bindIdempotency = () => Promise.reject(new LedgerUnavailable("usage worker unavailable"));
    const rig = await makeRig({ usage });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(503);
    expect(rig.gateway.createCalls).toHaveLength(1);
    const line = rig.logLines.at(-1) ?? "";
    expect(line).toContain("event=provider_accepted_unbound");
    expect(line).not.toContain("metering_unavailable");
  });

  it("still logs metering_unavailable when a ledger failure is the first thing that happened", async () => {
    const usage = new InMemoryUsageStore();
    usage.batchOwner = () => Promise.reject(new LedgerUnavailable("usage worker unavailable"));
    const rig = await makeRig({ usage });
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    expect(res.status).toBe(503);
    expect(rig.logLines.at(-1)).toContain("event=metering_unavailable");
  });
});

describe("boot checks (#787 items 4, 7)", () => {
  it("parses whole-number env settings and refuses anything else by name", () => {
    expect(intFromEnv({}, "RATE_BURST", 12)).toBe(12);
    expect(intFromEnv({ RATE_BURST: "" }, "RATE_BURST", 12)).toBe(12);
    expect(intFromEnv({ RATE_BURST: " 30 " }, "RATE_BURST", 12)).toBe(30);
    expect(intFromEnv({ TRIAL_DAILY_CHUNK_CAP: "0" }, "TRIAL_DAILY_CHUNK_CAP", 500, 0)).toBe(0);
    for (const bad of ["abc", "12/min", "1e3", "0x10", "-1", "1.5", "0", "9007199254740993", "NaN", "Infinity"]) {
      expect(() => intFromEnv({ RATE_PER_MIN: bad }, "RATE_PER_MIN", 12)).toThrow(BootError);
      try {
        intFromEnv({ RATE_PER_MIN: bad }, "RATE_PER_MIN", 12);
      } catch (e) {
        // The variable's name, never its value.
        expect((e as Error).message).toBe("invalid RATE_PER_MIN");
      }
    }
    expect(() => intFromEnv({ PORT: "70000" }, "PORT", 8787, 1, 65_535)).toThrow("invalid PORT");
  });

  it("keeps the server timeouts shorter than Node's defaults and consistent with each other", () => {
    expect(SERVER_TIMEOUTS.requestTimeout).toBeLessThan(300_000);
    expect(SERVER_TIMEOUTS.headersTimeout).toBeLessThan(60_000);
    expect(SERVER_TIMEOUTS.headersTimeout).toBeLessThanOrEqual(SERVER_TIMEOUTS.requestTimeout);
  });
});
