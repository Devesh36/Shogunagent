/**
 * A local agent gateway for load tests (plan E37, bench/load/gateway.mjs): the real `createApp`
 * with the agent routes, an in-memory agent store, a FAKE core behind `/mcp` (answers a fixed
 * JSON-RPC result after --core-ms; never a real sidecar) and tiny spend caps, so a cost-bearing
 * call answers 402 while reads (free, invariant 10) pass through. Registers --agents agents by
 * solving the (cheap) proof of work in-process and writes their keys to --keys-out.
 * Prints `LISTENING <port>` once ready.
 *
 *   tsx scripts/local-gateway.ts --keys-out /tmp/keys.txt [--agents 4] [--core-ms 5] [--port 0]
 *
 * The keys are throwaway: generated here, held in this process's memory store, gone on exit.
 */
import { serve } from "@hono/node-server";
import { generateKeyPairSync } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { parseArgs } from "node:util";

import { InMemoryAgentStore } from "../src/agent-store.js";
import { createApp } from "../src/app.js";
import type { AnthropicGateway, BatchStatus } from "../src/gateway.js";
import { solvePow } from "../src/pow.js";
import { NoRateLimit } from "../src/ratelimit.js";
import { InMemoryUsageStore } from "../src/usage.js";

const { values: args } = parseArgs({
  options: {
    agents: { type: "string", default: "4" },
    "keys-out": { type: "string" },
    "core-ms": { type: "string", default: "5" },
    port: { type: "string", default: "0" },
    "pow-bits": { type: "string", default: "8" },
  },
});

/** The Batch lane is not exercised here; this gateway never reaches a provider. */
class NoProvider implements AnthropicGateway {
  createBatch(): Promise<BatchStatus> { return Promise.reject(new Error("no provider in the load gateway")); }
  getBatch(): Promise<BatchStatus> { return Promise.reject(new Error("no provider in the load gateway")); }
  async *streamResults(): AsyncIterable<string> { /* nothing */ }
}

const BASE = "http://127.0.0.1";
const CORE_BODY = JSON.stringify({ jsonrpc: "2.0", id: 1, result: { content: [{ type: "text", text: "{}" }] } });

async function main(): Promise<void> {
  if (!args["keys-out"]) throw new Error("--keys-out is required");
  const coreMs = Number(args["core-ms"]);
  const powBits = Number(args["pow-bits"]);
  const { publicKey } = generateKeyPairSync("ed25519");
  const app = createApp({
    gateway: new NoProvider(),
    usage: new InMemoryUsageStore(),
    licensePublicKey: publicKey,
    dailyChunkCap: 2000,
    rateLimit: new NoRateLimit(),
    log: () => undefined,
    agent: {
      store: new InMemoryAgentStore(),
      publicBaseUrl: BASE,
      powSecret: "load-test-pow-secret-not-a-real-one",
      powBits,
      keyIterations: 1_000,
      // Smaller than one ingest event (500 µ$): every cost-bearing call is a 402, reads are free.
      caps: { key: { day: 50, month: 50 }, principal: { day: 50, month: 50 } },
      registerLimiter: new NoRateLimit(),
      mcpUpstream: async () => {
        if (coreMs > 0) await new Promise((r) => setTimeout(r, coreMs));
        return new Response(CORE_BODY, { status: 200, headers: { "content-type": "application/json" } });
      },
    },
  });

  const keys: string[] = [];
  for (let i = 0; i < Number(args.agents); i++) {
    const challenge = ((await (await app.request("/v1/agents/challenge")).json()) as { challenge: string }).challenge;
    const nonce = solvePow(challenge, powBits);
    if (nonce === undefined) throw new Error("unsolvable challenge");
    const res = await app.request("/v1/agents", {
      method: "POST",
      headers: { "content-type": "application/json", "x-agent-pow": `${challenge}:${nonce}`, "cf-connecting-ip": `198.51.100.${i % 250}` },
      body: JSON.stringify({ name: `load-${i}` }),
    });
    if (res.status !== 201) throw new Error(`registration failed: ${res.status}`);
    keys.push(String(((await res.json()) as { key: string }).key));
  }
  await writeFile(args["keys-out"], keys.join("\n") + "\n", { mode: 0o600 });

  const server = serve({ fetch: app.fetch, port: Number(args.port), hostname: "127.0.0.1" });
  await new Promise<void>((r) => server.once("listening", () => r()));
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : 0;
  for (const signal of ["SIGTERM", "SIGINT"] as const) {
    process.once(signal, () => {
      server.close(() => process.exit(0));
      if ("closeAllConnections" in server) server.closeAllConnections();
    });
  }
  console.log(`LISTENING ${port}`);
}

main().catch((e: unknown) => {
  console.error(e instanceof Error ? e.message : "local gateway failed");
  process.exit(1);
});
