/** Shared test rig: Ed25519 key pair, `v1.` licence-token signing (the exact format the licence
 * API issues — apps/website/src/lib/license.ts), the in-memory gateway fake, and a fully-wired
 * app whose logger writes into an inspectable array. */
import { generateKeyPairSync, randomUUID, sign as edSign, type KeyObject } from "node:crypto";

import { createApp, type RelayDeps } from "../src/app.js";
import type { AnthropicGateway, BatchStatus } from "../src/gateway.js";
import type { AnthropicBatchRequestItem } from "../src/types.js";
import { InMemoryUsageStore } from "../src/usage.js";

export interface Keys {
  privateKey: KeyObject;
  publicKey: KeyObject;
}

export async function makeKeys(): Promise<Keys> {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  return { privateKey, publicKey };
}

/** `"-1h"` / `"24h"` → seconds offset from now, mirroring the jose-style strings the tests use. */
function parseExpiresIn(v: string): number {
  const m = /^(-?\d+)h$/.exec(v);
  if (!m) throw new Error(`unsupported expiresIn: ${v}`);
  return Number(m[1]) * 3600;
}

export async function signLicense(
  privateKey: KeyObject,
  opts: {
    sub?: string;
    account?: unknown;
    plan?: string;
    status?: string;
    expiresIn?: string;
    /** Override the `device` claim. `null` omits it entirely — the pre-#334 token shape. */
    device?: string | null;
    /** Override `iat` (seconds). `null` omits the claim — the pre-rotation token shape. */
    iat?: number | null;
    /** Override `exp` (seconds) outright, instead of `expiresIn` from now. */
    exp?: number;
    /** Add a `kid` claim (audit M6). Any string, so tests can forge an unknown one. */
    kid?: string;
    /** Add an `acc` claim (#1101 D7): the account the token was issued through. */
    acc?: string;
    /** The `period_end` claim (seconds): a trial token's trial end. Default `null`. */
    periodEnd?: number | null;
  } = {},
): Promise<string> {
  const now = Math.floor(Date.now() / 1000);
  const iat = opts.iat === undefined ? now : opts.iat;
  const payload: Record<string, unknown> = {
    v: 1,
    lic: opts.sub ?? "lic_test_1",
    plan: opts.plan ?? "standard",
    status: opts.status ?? "active",
    ...(iat === null ? {} : { iat }),
    exp: opts.exp ?? (iat ?? now) + parseExpiresIn(opts.expiresIn ?? "24h"),
    period_end: opts.periodEnd ?? null,
    cancel_at_period_end: false,
    grace_days: 14,
  };
  if (opts.account !== undefined) payload.acc = opts.account;
  if (opts.kid !== undefined) payload.kid = opts.kid;
  if (opts.acc !== undefined) payload.acc = opts.acc;
  const device = opts.device === undefined ? "dev_test_device" : opts.device;
  if (device !== null) payload.device = device;
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  const sig = edSign(null, body, privateKey);
  return `v1.${body.toString("base64url")}.${sig.toString("base64url")}`;
}

/**
 * The gateway fake. Deliberately persistence-free: it keeps the forwarded requests reachable for
 * assertions (that is the test observing the passthrough, not the relay storing content) and
 * replays canned status/results. `streamResults` yields from the canned lines — the app must
 * consume them incrementally.
 */
export class FakeGateway implements AnthropicGateway {
  createCalls: AnthropicBatchRequestItem[][] = [];
  getCalls: string[] = [];
  resultsCalls: string[] = [];

  nextBatchId = "rb_fake_1";
  status: BatchStatus = {
    id: "rb_fake_1",
    processing_status: "in_progress",
    request_counts: { processing: 1, succeeded: 0, errored: 0, canceled: 0, expired: 0 },
  };
  resultLines: string[] = [];
  releaseResults?: (id: string) => Promise<void>;

  createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus> {
    this.createCalls.push(requests);
    return Promise.resolve({
      id: this.nextBatchId,
      processing_status: "in_progress",
      request_counts: {
        processing: requests.length,
        succeeded: 0,
        errored: 0,
        canceled: 0,
        expired: 0,
      },
    });
  }

  getBatch(id: string): Promise<BatchStatus> {
    this.getCalls.push(id);
    return Promise.resolve(this.status);
  }

  async *streamResults(id: string): AsyncIterable<string> {
    this.resultsCalls.push(id);
    for (const line of this.resultLines) {
      yield line;
    }
  }
}

export interface Rig {
  app: ReturnType<typeof createApp>;
  gateway: FakeGateway;
  usage: InMemoryUsageStore;
  logLines: string[];
  keys: Keys;
  token: string;
}

export async function makeRig(overrides: Partial<Omit<RelayDeps, "log">> = {}): Promise<Rig> {
  const keys = await makeKeys();
  const gateway = new FakeGateway();
  const usage = new InMemoryUsageStore();
  const logLines: string[] = [];
  const app = createApp({
    gateway,
    usage,
    licensePublicKey: keys.publicKey,
    dailyChunkCap: 100,
    log: (line) => logLines.push(line),
    ...overrides,
  });
  // Signed against the rig's clock: a frozen test clock in the past must not read a real-time
  // `iat` as "issued in the future" (auth.ts MAX_IAT_SKEW_SECONDS).
  const token = await signLicense(keys.privateKey, { iat: Math.floor((overrides.now?.() ?? new Date()).getTime() / 1000) });
  return { app, gateway, usage, logLines, keys, token };
}

export async function postBatch(
  rig: Rig,
  body: unknown,
  token: string = rig.token,
): Promise<Response> {
  return rig.app.request("/v1/batch", {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify(body && typeof body === "object" && !Array.isArray(body)
      ? { client_request_id: randomUUID(), ...body } : body),
  });
}

export const sampleBody = {
  purpose: "consolidation",
  model_class: "classify",
  items: [
    { custom_id: "1234", chunk: "TOP-SECRET-CHUNK-ALPHA" },
    { custom_id: "5678", chunk: "TOP-SECRET-CHUNK-BRAVO" },
  ],
};
