/**
 * A local relay for load tests (issue #29 FR-SVC-07): the real `createApp` with a FAKE upstream
 * that never calls Anthropic, a throwaway licence key pair, and N device tokens written to a file.
 * Prints `LISTENING <port>` once ready. Started by `load-test.ts --self-host`; can also be run by
 * hand and targeted with `load-test.ts --url http://127.0.0.1:<port> --tokens-file <file>`.
 *
 *   tsx scripts/local-relay.ts --devices 300 --tokens-out /tmp/tokens.txt [--store memory|postgres]
 */
import { serve } from "@hono/node-server";
import { generateKeyPairSync, randomUUID, sign as edSign, type KeyObject } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { parseArgs } from "node:util";

import { createApp } from "../src/app.js";
import type { AnthropicGateway, BatchStatus } from "../src/gateway.js";
import { SqlRateLimiter, SqlUsageStore } from "../src/pg-store.js";
import { TokenBucketLimiter, type RateLimiter } from "../src/ratelimit.js";
import { postgresClient } from "../src/sql.js";
import type { AnthropicBatchRequestItem } from "../src/types.js";
import { InMemoryUsageStore, type UsageStore } from "../src/usage.js";

const { values: args } = parseArgs({
  options: {
    devices: { type: "string", default: "300" },
    "tokens-out": { type: "string" },
    store: { type: "string", default: "memory" },
    "database-url": { type: "string" },
    "upstream-ms": { type: "string", default: "50" },
    port: { type: "string", default: "0" },
  },
});

class FakeUpstream implements AnthropicGateway {
  constructor(private readonly latencyMs: number) {}
  private wait(): Promise<void> {
    return new Promise((r) => setTimeout(r, this.latencyMs));
  }
  async createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus> {
    await this.wait();
    return { id: `msgbatch_${randomUUID()}`, processing_status: "in_progress",
      request_counts: { processing: requests.length, succeeded: 0, errored: 0, canceled: 0, expired: 0 } };
  }
  async getBatch(id: string): Promise<BatchStatus> {
    await this.wait();
    return { id, processing_status: "in_progress", request_counts: { processing: 5, succeeded: 5, errored: 0, canceled: 0, expired: 0 } };
  }
  async *streamResults(): AsyncIterable<string> {
    // Polling in this test never reaches "ended".
  }
}

function mintToken(privateKey: KeyObject, device: string): string {
  const iat = Math.floor(Date.now() / 1000);
  const payload = { v: 1, lic: `lic_load_${device}`, plan: "standard", status: "active", device, iat, exp: iat + 3600 };
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  return `v1.${body.toString("base64url")}.${edSign(null, body, privateKey).toString("base64url")}`;
}

async function main(): Promise<void> {
  const devices = Number(args.devices);
  if (!args["tokens-out"]) throw new Error("--tokens-out is required");
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  let usage: UsageStore;
  let rateLimit: RateLimiter = new TokenBucketLimiter(12, 12);
  let close = async (): Promise<void> => undefined;
  if (args.store === "memory") {
    usage = new InMemoryUsageStore();
  } else if (args.store === "postgres") {
    // The production store: the number that matters for capacity (runbook §9-7).
    if (!args["database-url"]) throw new Error("--store postgres needs --database-url");
    const db = postgresClient(args["database-url"]);
    const shared = new SqlUsageStore(db);
    await shared.checkSchema();
    usage = shared;
    rateLimit = new SqlRateLimiter(db, 12, 12);
    close = () => db.close();
  } else {
    throw new Error("--store must be memory or postgres");
  }
  const app = createApp({
    gateway: new FakeUpstream(Number(args["upstream-ms"])),
    usage,
    licensePublicKey: publicKey,
    dailyChunkCap: 2000,
    rateLimit,
    log: () => undefined,
  });
  const server = serve({ fetch: app.fetch, port: Number(args.port), hostname: "127.0.0.1" });
  await new Promise<void>((r) => server.once("listening", () => r()));
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : 0;
  const tokens = Array.from({ length: devices }, (_, i) => mintToken(privateKey, `dev_load_${i}`));
  await writeFile(args["tokens-out"], tokens.join("\n") + "\n", { mode: 0o600 });
  for (const signal of ["SIGTERM", "SIGINT"] as const) {
    process.once(signal, () => {
      server.close(() => { void close().then(() => process.exit(0)); });
      // Keep-alive sockets from the load generator would otherwise hold close() open.
      if ("closeAllConnections" in server) server.closeAllConnections();
    });
  }
  console.log(`LISTENING ${port}`);
}

main().catch((e: unknown) => {
  console.error(e instanceof Error ? e.message : "local relay failed");
  process.exit(1);
});
