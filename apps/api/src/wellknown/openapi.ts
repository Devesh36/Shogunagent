/**
 * `/openapi.json` (E10-T6 scaffolding): OpenAPI 3.1 for the REST face of the agent API as it
 * exists today in apps/api — registration, keys, wallet, quote, receipts, the metered MCP proxy,
 * the router verbs (served by apps/router on the same public base URL, plan §7.5) and the
 * well-known documents. The state / memory / actions REST routes of the core
 * (`crates/shogun-mcp/src/rest_routing.rs`) are added once E4 lands: the core will export its
 * route table in the same way the tool table is exported (docs/api/tool-table.json.md), and this
 * module will merge it under `/v1/state/*`, `/v1/memory/*`, `/v1/actions/*`, `/v1/export`.
 *
 * Generated (invariant 12): the route table below is code; the document is rendered from it and
 * from `pricing.ts` / `tool-table.ts`, and a snapshot test pins the output.
 */
import { DEFAULT_CAPS } from "../caps.js";
import { FREE_ALLOWANCE_MICROS, MICRO_USD_PER_USD, RATE_MICROS, RUN_MARKUP_BPS } from "../pricing.js";
import { DISPLAY_NAME, DOCS_URL, LONG_DESCRIPTION, PROVIDER, REPOSITORY_URL, VERSION, mcpUrl, origin, wellknownLinks, type WellknownContext } from "./identity.js";
import { annotationsFor, type ToolTable } from "./tool-table.js";

type Json = Record<string, unknown>;

const ref = (name: string): Json => ({ $ref: `#/components/schemas/${name}` });
const jsonBody = (schema: Json, description?: string): Json => ({ ...(description ? { description } : {}), content: { "application/json": { schema } } });
const errorResponse = (description: string): Json => jsonBody(ref("Error"), description);

const micro = (description: string): Json => ({ type: "integer", minimum: 0, description: `${description} (integer micro-USD; 1 USD = ${MICRO_USD_PER_USD}).` });

const components: Json = {
  securitySchemes: {
    agentKey: { type: "http", scheme: "bearer", bearerFormat: "sa_live_<key>", description: "An agent key from POST /v1/agents. Scopes: read, write, run, admin (admin implies all)." },
  },
  parameters: {
    McpMethod: { name: "Mcp-Method", in: "header", required: true, schema: { type: "string" }, description: "The JSON-RPC method (MCP 2026-07-28 Streamable HTTP). Anything but tools/call is free protocol traffic." },
    McpName: { name: "Mcp-Name", in: "header", required: false, schema: { type: "string", maxLength: 128 }, description: "The tool name for tools/call. Classified by pricing.ts into read / ingest / model_op / run." },
    McpQuantity: { name: "Mcp-Quantity", in: "header", required: false, schema: { type: "integer", minimum: 1, maximum: 1_000_000, default: 1 }, description: "Units in this call (events for memory.ingest)." },
    McpDryRun: { name: "Mcp-Dry-Run", in: "header", required: false, schema: { type: "string", enum: ["true"] }, description: "Quote only: nothing reserved, nothing forwarded." },
    McpToolId: { name: "Mcp-Tool-Id", in: "header", required: false, schema: { type: "string" }, description: "For router.run: the tool whose provider price the router looks up." },
    IdempotencyKey: { name: "Idempotency-Key", in: "header", required: false, schema: { type: "string", minLength: 1, maxLength: 255 }, description: "Repeat with the same key to get the same outcome without a second effect." },
  },
  headers: {
    "X-Receipt-Id": { schema: { type: "string", pattern: "^rcpt_" }, description: "The receipt of this call (GET /v1/receipts)." },
    "X-Usage-Unit": { schema: { type: "string", enum: ["read", "ingest", "model_op", "run", "storage", "protocol"] } },
    "X-Usage-Quantity": { schema: { type: "integer" } },
    "X-Usage-Cost-Micro-Usd": { schema: { type: "integer" }, description: "0 when the upstream answered anything but 2xx: a failed call costs nothing." },
    "X-Latency-Ms": { schema: { type: "integer" } },
    "Retry-After": { schema: { type: "integer" }, description: "Seconds. Honour it; do not spin." },
  },
  schemas: {
    Error: {
      type: "object", required: ["error"],
      properties: {
        error: {
          type: "object", required: ["code", "message", "next_action"],
          properties: {
            code: { type: "string", description: "Branch on this." },
            message: { type: "string", description: "Plain English: what to do next." },
            next_action: { type: ["string", "null"], enum: ["top_up", "approve_at_url", "retry_after", "narrow_query", "not_allowed", "solve_challenge", "register", "fix_input", "discover", "authenticate", "provide_input", "try_alternative", null] },
          },
        },
        payment: { ...ref("Payment"), description: "Present on 402 only." },
        usage: ref("Usage"),
        latency_ms: { type: "integer" },
      },
    },
    Usage: {
      type: "object", required: ["unit", "quantity", "cost_micro_usd"],
      properties: { unit: { type: "string", enum: ["read", "ingest", "model_op", "run", "storage", "protocol"] }, quantity: { type: "integer", minimum: 0 }, cost_micro_usd: micro("What this call cost") },
    },
    Payment: {
      type: "object", description: "x402-shaped payment-required payload (docs/api/402.md).",
      required: ["amount_micro_usd", "unit", "accepts", "retry_after"],
      properties: {
        amount_micro_usd: micro("The amount the refused call needs"),
        unit: { type: "string" },
        accepts: { type: "array", items: { type: "object", required: ["rail", "url"], properties: { rail: { type: "string", enum: ["card"] }, url: { type: "string", format: "uri" }, method: { type: "string" } } } },
        retry_after: { type: "integer" },
        balance_micro_usd: micro("Spendable balance at the time of refusal"),
        cap: { type: "object", properties: { scope: { type: "string", enum: ["key", "principal"] }, period: { type: "string", enum: ["day", "month"] }, limit_micro_usd: micro("The cap"), spent_micro_usd: micro("Spent in the period") } },
      },
    },
    Caps: {
      type: "object", required: ["key", "principal"],
      properties: {
        key: { type: "object", properties: { day_micro_usd: micro("Per key per UTC day"), month_micro_usd: micro("Per key per UTC month") } },
        principal: { type: "object", properties: { day_micro_usd: micro("Per principal per UTC day"), month_micro_usd: micro("Per principal per UTC month") } },
      },
    },
    Challenge: { type: "object", required: ["challenge", "difficulty_bits", "expires_at"], properties: { challenge: { type: "string" }, difficulty_bits: { type: "integer", minimum: 0, maximum: 32 }, expires_at: { type: "string", format: "date-time" } } },
    RegisterRequest: {
      type: "object",
      properties: {
        ref: { type: "string", pattern: "^[A-Za-z0-9_.:-]{1,128}$", description: "Another key id or a developer id, credited as the referrer." },
        caps: { type: "object", properties: { day_micro_usd: micro("May only lower the key default"), month_micro_usd: micro("May only lower the key default") } },
      },
    },
    Registration: {
      type: "object", required: ["key", "key_id", "prefix", "principal_id", "tenant_id", "scopes", "created_at", "expires_at", "wallet", "caps"],
      properties: {
        key: { type: "string", pattern: "^sa_(live|test)_", description: "Shown once. Never paste it into a chat, a log or a file." },
        key_id: { type: "string", pattern: "^ak_" }, prefix: { type: "string" }, principal_id: { type: "string", pattern: "^prn_" }, tenant_id: { type: "string" },
        scopes: { type: "array", items: { type: "string", enum: ["read", "write", "run", "admin"] } },
        created_at: { type: "string", format: "date-time" }, expires_at: { type: ["string", "null"], format: "date-time" }, ref: { type: ["string", "null"] },
        wallet: { type: "object", properties: { balance_micro_usd: micro("The free allowance"), reserved_micro_usd: micro("Held by in-flight calls") } },
        caps: ref("Caps"), next_action: { type: "null" },
      },
    },
    Me: { type: "object", required: ["key_id", "mode", "principal_id", "tenant_id", "scopes", "expires_at", "caps"], properties: { key_id: { type: "string" }, mode: { type: "string", enum: ["live", "test"] }, principal_id: { type: "string" }, tenant_id: { type: "string" }, scopes: { type: "array", items: { type: "string" } }, expires_at: { type: ["string", "null"] }, ref: { type: ["string", "null"] }, caps: ref("Caps") } },
    CapsPatch: { type: "object", properties: { day_micro_usd: micro("Omit to reset to the default"), month_micro_usd: micro("Omit to reset to the default") } },
    Wallet: {
      type: "object", required: ["principal_id", "balance_micro_usd", "reserved_micro_usd", "available_micro_usd", "caps", "spend", "topup"],
      properties: {
        principal_id: { type: "string" }, balance_micro_usd: micro("Balance"), reserved_micro_usd: micro("Held by in-flight calls"), available_micro_usd: micro("balance − reserved"),
        caps: ref("Caps"),
        spend: ref("Caps"),
        topup: { type: "object", properties: { rail: { type: "string", enum: ["card"] }, url: { type: "string", format: "uri" }, method: { type: "string", enum: ["POST"] }, min_micro_usd: micro("Smallest top-up"), max_micro_usd: micro("Largest top-up") } },
      },
    },
    TopupRequest: { type: "object", required: ["amount_micro_usd"], properties: { amount_micro_usd: micro("Between topup.min_micro_usd and topup.max_micro_usd") } },
    TopupSession: { type: "object", required: ["rail", "url", "session_id", "amount_micro_usd", "next_action"], properties: { rail: { type: "string" }, url: { type: "string", format: "uri", description: "Hand it to a human once." }, session_id: { type: "string" }, amount_micro_usd: micro("Amount"), next_action: { type: "string", enum: ["approve_at_url"] }, instructions: { type: "string" } } },
    QuoteRequest: {
      type: "object",
      properties: {
        method: { type: "string", default: "tools/call" }, name: { type: "string" }, quantity: { type: "integer", minimum: 1, default: 1 },
        provider_price_micro_usd: micro("For router.run / actions.execute: the provider price from router.inspect"), tool_id: { type: "string" },
      },
    },
    QuoteResponse: { type: "object", required: ["quote", "method", "name", "balance_micro_usd", "sufficient", "caps", "usage", "next_action"], properties: { quote: ref("Usage"), method: { type: "string" }, name: { type: ["string", "null"] }, balance_micro_usd: micro("Spendable now"), sufficient: { type: "boolean" }, caps: ref("Caps"), usage: ref("Usage"), next_action: { type: ["string", "null"], enum: ["top_up", null] } } },
    Receipt: { type: "object", required: ["id", "key_id", "method", "name", "unit", "quantity", "cost_micro_usd", "reserved_micro_usd", "settled", "ts"], properties: { id: { type: "string", pattern: "^rcpt_" }, key_id: { type: "string" }, method: { type: "string" }, name: { type: ["string", "null"] }, unit: { type: "string" }, quantity: { type: "integer" }, cost_micro_usd: micro("Settled cost"), reserved_micro_usd: { oneOf: [micro("Held while in flight"), { type: "null" }] }, settled: { type: "boolean" }, ts: { type: "string", format: "date-time" } } },
    HandoffRequest: {
      type: "object", required: ["task", "to_agent"],
      properties: {
        task: { type: "string", minLength: 1, maxLength: 2000, description: "What the receiving agent is asked to do." },
        to_agent: { type: "string", pattern: "^[A-Za-z0-9_.:@/-]{1,128}$", description: "The receiving agent's id." },
        from_agent: { type: "string", pattern: "^[A-Za-z0-9_.:@/-]{1,128}$", description: "Your agent id; default agent:<key id>." },
        query: { type: "string", minLength: 1, maxLength: 2000, description: "What to recall for the task; default task." },
        budget_tokens: { type: "integer", minimum: 100, maximum: 32000, default: 2000 },
        ttl_seconds: { type: "integer", minimum: 60, maximum: 86400, default: 3600 },
        thread_key: { type: "string", maxLength: 256 },
      },
    },
    HandoffResponse: {
      type: "object", required: ["result", "usage", "latency_ms", "next_action"],
      properties: {
        result: {
          type: "object", required: ["handoff", "a2a"],
          properties: {
            handoff: { $ref: "https://shogunagent.dev/schemas/handoff.schema.json", description: "docs/api/handoff.schema.json: a strict subset of export v1." },
            a2a: { type: "object", required: ["part", "message"], properties: { part: { type: "object", description: "{kind: \"data\", data: <handoff>, metadata: {schema}}" }, message: { type: "object", description: "An A2A message carrying the part." } } },
          },
        },
        usage: ref("Usage"), latency_ms: { type: "integer" }, hints: { type: "array", items: { type: "object" } },
        next_action: { type: ["string", "null"], enum: ["narrow_query", null] },
      },
    },
    Receipts: { type: "object", required: ["receipts", "next_since"], properties: { receipts: { type: "array", items: ref("Receipt") }, next_since: { type: ["string", "null"], format: "date-time" } } },
    DisputeRequest: { type: "object", required: ["reason"], properties: { reason: { type: "string", minLength: 1, maxLength: 1000 } } },
    Dispute: { type: "object", required: ["dispute_id", "receipt_id", "status", "ts"], properties: { dispute_id: { type: "string", pattern: "^dsp_" }, receipt_id: { type: "string" }, status: { type: "string", enum: ["open", "resolved", "rejected"] }, ts: { type: "string", format: "date-time" } } },
    DryRun: { type: "object", required: ["dry_run", "quote", "usage", "latency_ms", "next_action"], properties: { result: { type: "null" }, dry_run: { type: "boolean", enum: [true] }, quote: ref("Usage"), usage: ref("Usage"), balance_micro_usd: micro("Spendable now"), sufficient: { type: "boolean" }, latency_ms: { type: "integer" }, next_action: { type: "null" } } },
    JsonRpc: { type: "object", description: "A JSON-RPC 2.0 envelope. Relayed to the core untouched; the gateway never parses it." },
    RouterDiscoverRequest: { type: "object", required: ["task"], additionalProperties: false, properties: { task: { type: "string", minLength: 1, maxLength: 500 }, budget_micro_usd: micro("Exclude tools whose typical run costs more"), limit: { type: "integer", minimum: 1, maximum: 50, default: 10 }, categories: { type: "array", items: { type: "string" } } } },
    RouterInspectRequest: { type: "object", required: ["tool_id"], additionalProperties: false, properties: { tool_id: { type: "string", minLength: 1 } } },
    RouterRunRequest: { type: "object", required: ["tool_id", "input", "idempotency_key"], additionalProperties: false, properties: { tool_id: { type: "string" }, input: { type: "object" }, idempotency_key: { type: "string", minLength: 1, maxLength: 200 }, dryRun: { type: "boolean" }, task_hint: { type: "string", maxLength: 500 }, hint_followed: { type: "boolean" }, inputResponses: { type: "object" } } },
    RouterEnvelope: {
      type: "object", required: ["usage", "latency_ms", "hints", "next_action"], description: "The common response (plan §7.2): result, usage, latency_ms, hints, next_action.",
      properties: { result: {}, resultType: { type: "string", enum: ["result", "quote", "input_required", "approval_required", "error"] }, usage: ref("Usage"), latency_ms: { type: "integer" }, hints: { type: "array", items: { type: "object", properties: { tool_id: { type: "string" }, reason: { type: "string" }, estimated_cost_micro_usd: micro("Estimated") } } }, next_action: { type: ["string", "null"] } },
    },
  },
};

interface Route {
  method: "get" | "post" | "patch" | "all";
  path: string;
  tag: string;
  summary: string;
  description?: string;
  security: "none" | "key" | "stripe" | "pow";
  scope?: "read" | "write" | "run" | "any";
  parameters?: Json[];
  requestBody?: Json;
  responses: Record<string, Json>;
  servedBy?: "apps/api" | "apps/router";
}

function routes(table: ToolTable, ctx: WellknownContext): Route[] {
  const links = wellknownLinks(ctx);
  const toolNames = table.tools.map((t) => t.name);
  const meteredHeaders = ["McpMethod", "McpName", "McpQuantity", "McpDryRun", "McpToolId"].map((p) => ({ $ref: `#/components/parameters/${p}` }));
  const usageHeaders = Object.fromEntries(["X-Receipt-Id", "X-Usage-Unit", "X-Usage-Quantity", "X-Usage-Cost-Micro-Usd", "X-Latency-Ms"].map((h) => [h, { $ref: `#/components/headers/${h}` }]));
  const r: Route[] = [
    // --- well-known ---
    { method: "get", path: "/.well-known/mcp/server-card.json", tag: "well-known", security: "none", summary: "MCP Server Card: endpoint, auth, tools with annotations.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/agent-card.json", tag: "well-known", security: "none", summary: "A2A Agent Card (v1.0), Ed25519-signed when the deployment holds a key.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/pricing.json", tag: "well-known", security: "none", summary: "The rate card, free allowance and caps, rendered from the metering table.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/status.json", tag: "well-known", security: "none", summary: "Live p50 / p95, error rate over the last five minutes, and the incident history.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/mcp-config", tag: "well-known", security: "none", summary: "JSON Schema of the client configuration (Smithery).", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/health", tag: "well-known", security: "none", summary: "Health with p50 / p95 and error rate; 503 during a major incident.", responses: { "200": jsonBody({ type: "object" }), "503": jsonBody({ type: "object" }) } },
    { method: "get", path: "/openapi.json", tag: "well-known", security: "none", summary: "This document.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/llms.txt", tag: "well-known", security: "none", summary: "Agent-readable documentation index.", responses: { "200": { content: { "text/plain": { schema: { type: "string" } } } } } },
    { method: "get", path: "/llms-full.txt", tag: "well-known", security: "none", summary: "The full agent-readable documentation in one file.", responses: { "200": { content: { "text/plain": { schema: { type: "string" } } } } } },
    // --- agents ---
    { method: "get", path: "/v1/agents/challenge", tag: "agents", security: "none", summary: "A proof-of-work challenge for self-registration.", responses: { "200": jsonBody(ref("Challenge")) } },
    {
      method: "post", path: "/v1/agents", tag: "agents", security: "pow", summary: "Self-register: a key with read, write and run, a fresh principal and the free allowance.",
      description: `Send X-Agent-Pow: <challenge>:<nonce> where sha256(challenge + nonce) has the challenge's leading zero bits. Rate-limited per IP per hour. The allowance is ${FREE_ALLOWANCE_MICROS} µ$.`,
      parameters: [{ name: "X-Agent-Pow", in: "header", required: true, schema: { type: "string" } }],
      requestBody: jsonBody(ref("RegisterRequest")),
      responses: { "201": jsonBody(ref("Registration")), "400": errorResponse("invalid_ref | invalid_caps | caps_above_default | invalid_json"), "401": errorResponse("pow_missing | pow_malformed | pow_invalid | pow_expired | pow_insufficient | pow_reused — next_action solve_challenge"), "429": errorResponse("rate_limited, with Retry-After") },
    },
    { method: "get", path: "/v1/agents/me", tag: "agents", security: "key", scope: "read", summary: "The key's principal, scopes, expiry and caps.", responses: { "200": jsonBody(ref("Me")), "401": errorResponse("Invalid key"), "403": errorResponse("insufficient_scope") } },
    // --- keys ---
    { method: "post", path: "/v1/keys/{id}/revoke", tag: "keys", security: "key", scope: "any", summary: "Revoke a key (own key; admin for others).", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object", properties: { key_id: { type: "string" }, revoked: { type: "boolean" } } }), "404": errorResponse("key_not_found (unknown and someone else's answer the same)") } },
    { method: "patch", path: "/v1/keys/{id}", tag: "keys", security: "key", scope: "any", summary: "Set a key's hard caps. Lowering: any key; raising above the defaults: admin.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], requestBody: jsonBody(ref("CapsPatch")), responses: { "200": jsonBody({ type: "object", properties: { key_id: { type: "string" }, caps: ref("Caps") } }), "400": errorResponse("invalid_caps | invalid_json"), "403": errorResponse("caps_above_default"), "404": errorResponse("key_not_found") } },
    // --- wallet ---
    { method: "get", path: "/v1/wallet", tag: "wallet", security: "key", scope: "read", summary: "Balance, reserved, spendable, caps, this day's and month's spend.", responses: { "200": jsonBody(ref("Wallet")) } },
    { method: "post", path: "/v1/wallet/topup", tag: "wallet", security: "key", scope: "write", summary: "A Stripe Checkout session (card rail). Hand the URL to a human once.", parameters: [{ $ref: "#/components/parameters/IdempotencyKey" }], requestBody: jsonBody(ref("TopupRequest")), responses: { "201": jsonBody(ref("TopupSession")), "400": errorResponse("invalid_amount | invalid_idempotency_key | invalid_json"), "503": errorResponse("topup_unavailable") } },
    { method: "post", path: "/v1/stripe/webhook", tag: "wallet", security: "stripe", summary: "Stripe → wallet credit (signed, idempotent by event id). Not for agents.", responses: { "200": jsonBody({ type: "object", properties: { received: { type: "boolean" }, credited: { type: "boolean" } } }), "401": errorResponse("invalid_signature") } },
    // --- quote / receipts ---
    { method: "post", path: "/v1/quote", tag: "billing", security: "key", scope: "read", summary: "The exact cost of a call. Nothing is reserved.", requestBody: jsonBody(ref("QuoteRequest")), responses: { "200": jsonBody(ref("QuoteResponse")), "400": errorResponse("invalid_quantity | invalid_price | run_price_unavailable | invalid_json") } },
    { method: "get", path: "/v1/receipts", tag: "billing", security: "key", scope: "read", summary: "Per-call receipts, oldest first. Page with next_since.", parameters: [{ name: "since", in: "query", schema: { type: "string" }, description: "ISO-8601 or epoch milliseconds." }, { name: "limit", in: "query", schema: { type: "integer", minimum: 1, maximum: 500, default: 100 } }], responses: { "200": jsonBody(ref("Receipts")), "400": errorResponse("invalid_since | invalid_limit") } },
    { method: "post", path: "/v1/receipts/{id}/dispute", tag: "billing", security: "key", scope: "write", summary: "Open a dispute on a receipt (one per receipt; a second answers 200 with the existing one).", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], requestBody: jsonBody(ref("DisputeRequest")), responses: { "201": jsonBody(ref("Dispute")), "200": jsonBody(ref("Dispute")), "400": errorResponse("invalid_reason | invalid_json"), "404": errorResponse("receipt_not_found") } },
    // --- A2A handoff (E16) ---
    {
      method: "post", path: "/v1/handoff", tag: "a2a", security: "key", scope: "read",
      summary: "The state of work for a task as a typed handoff object (a strict subset of export v1) plus its A2A data part. Free.",
      description: "Built from one memory.recall of the task through the core: people, projects, commitments and open loops with their confidence band and provenance quotes; Low never travels. The receiver ingests it into its own agent_scratch namespace, never the principal's world.",
      requestBody: jsonBody(ref("HandoffRequest")),
      responses: { "200": jsonBody(ref("HandoffResponse")), "400": errorResponse("missing_task | invalid_to_agent | invalid_from_agent | invalid_budget_tokens | invalid_ttl_seconds | invalid_json"), "403": errorResponse("insufficient_scope"), "502": errorResponse("upstream_error | memory_update_pending: the core refused the recall"), "503": errorResponse("upstream_unavailable, with Retry-After") },
    },
    // --- the metered MCP proxy ---
    {
      method: "all", path: "/mcp", tag: "mcp", security: "key", scope: "any",
      summary: "The metered MCP Streamable HTTP endpoint. Classified from headers, never from the body.",
      description: `Sequence: key → classify → quote → reserve → forward → settle once (2xx: the quote; anything else: 0). Tool names today: ${toolNames.join(", ")}. Annotations per tool are in the server card (${links.server_card}).`,
      parameters: meteredHeaders,
      requestBody: jsonBody(ref("JsonRpc")),
      responses: {
        "200": { description: "The core's response, relayed untouched, with the usage headers; or a dry-run quote.", headers: usageHeaders, content: { "application/json": { schema: { oneOf: [ref("JsonRpc"), ref("DryRun")] } }, "text/event-stream": { schema: { type: "string" } } } },
        "400": errorResponse("invalid_metering_header | run_price_unavailable"), "401": errorResponse("Invalid key — next_action register"), "402": errorResponse("payment_required | cap_exceeded, with payment (x402 shape) and PAYMENT-REQUIRED header"), "403": errorResponse("insufficient_scope"),
        "503": errorResponse("upstream_unavailable | wallet_unavailable | capacity_exhausted | metering_unavailable, with Retry-After"),
      },
    },
    // --- router verbs (apps/router, same base URL; plan §7.4 / §7.5) ---
    { method: "post", path: "/v1/router/discover", tag: "router", security: "none", servedBy: "apps/router", summary: "Find tools for a task, ranked by task fit × (price, health, latency) with the score breakdown. Free.", requestBody: jsonBody(ref("RouterDiscoverRequest")), responses: { "200": jsonBody(ref("RouterEnvelope")), "400": errorResponse("invalid_request") } },
    { method: "post", path: "/v1/router/inspect", tag: "router", security: "none", servedBy: "apps/router", summary: "One tool: schemas, price, live health, permission level, examples, cheaper alternatives. Free.", requestBody: jsonBody(ref("RouterInspectRequest")), responses: { "200": jsonBody(ref("RouterEnvelope")), "404": errorResponse("tool_not_found") } },
    { method: "post", path: "/v1/router/run", tag: "router", security: "key", scope: "run", servedBy: "apps/router", summary: "Run a tool: L1 runs, L2 asks once (input_required), L3 returns an approval URL. Failures and empty results cost 0.", requestBody: jsonBody(ref("RouterRunRequest")), responses: { "200": jsonBody(ref("RouterEnvelope")), "400": errorResponse("invalid_input"), "401": errorResponse("authentication_required"), "402": errorResponse("payment_required"), "403": errorResponse("operation_denied"), "404": errorResponse("tool_not_found"), "409": errorResponse("idempotency_conflict"), "502": errorResponse("upstream_error ($0)"), "503": errorResponse("provider_unavailable") } },
    { method: "get", path: "/v1/router/catalog", tag: "router", security: "none", servedBy: "apps/router", summary: "The content-hashed catalog manifest.", responses: { "200": jsonBody({ type: "object" }) } },
  ];
  return r;
}

export function openapiDocument(table: ToolTable, ctx: WellknownContext): Json {
  const paths: Record<string, Json> = {};
  for (const route of routes(table, ctx)) {
    const op: Json = {
      tags: [route.tag],
      summary: route.summary,
      ...(route.description ? { description: route.description } : {}),
      operationId: `${route.method}_${route.path.replace(/^\//, "").replace(/[^A-Za-z0-9]+/g, "_").replace(/_+$/, "")}`,
      ...(route.parameters ? { parameters: route.parameters } : {}),
      ...(route.requestBody ? { requestBody: route.requestBody } : {}),
      responses: route.responses,
      security: route.security === "key" ? [{ agentKey: route.scope && route.scope !== "any" ? [route.scope] : [] }] : [],
      ...(route.security === "none" ? { "x-anonymous": true } : {}),
      ...(route.security === "pow" ? { "x-proof-of-work": true } : {}),
      ...(route.security === "stripe" ? { "x-webhook-signature": "Stripe-Signature" } : {}),
      ...(route.servedBy ? { "x-served-by": route.servedBy } : {}),
    };
    const methods = route.method === "all" ? ["get", "post", "delete"] : [route.method];
    paths[route.path] = { ...(paths[route.path] ?? {}), ...Object.fromEntries(methods.map((m) => [m, { ...op, operationId: `${m}_${String(op.operationId).replace(/^[a-z]+_/, "")}` }])) };
  }
  const links = wellknownLinks(ctx);
  return {
    openapi: "3.1.0",
    info: {
      title: `${DISPLAY_NAME} Agent API`,
      version: VERSION,
      summary: "The REST face of the state layer for AI agents: registration, wallet, quotes, receipts, the metered MCP proxy, the router verbs and the agent-readable documents.",
      description: LONG_DESCRIPTION,
      contact: { name: PROVIDER.organization, url: PROVIDER.url },
      license: { name: "Apache-2.0", url: `${REPOSITORY_URL}/blob/main/LICENSE` },
      "x-pricing": { ingest_micro_usd_per_event: RATE_MICROS.ingest, model_op_micro_usd: RATE_MICROS.model_op, run_markup_bps: RUN_MARKUP_BPS, free_allowance_micro_usd: FREE_ALLOWANCE_MICROS, default_caps: { key: { day_micro_usd: DEFAULT_CAPS.key.day, month_micro_usd: DEFAULT_CAPS.key.month }, principal: { day_micro_usd: DEFAULT_CAPS.principal.day, month_micro_usd: DEFAULT_CAPS.principal.month } }, document: links.pricing },
      "x-mcp": { url: mcpUrl(ctx), protocolVersion: table.protocol_version, server_card: links.server_card, tools: table.tools.map((t) => ({ name: t.name, level: t.level, cost_unit: t.cost_unit, annotations: annotationsFor(t.level) })) },
      "x-pending": "The core's /v1/state/*, /v1/memory/*, /v1/actions/* and /v1/export routes join this document when E4 exports rest_routing.rs (docs/api/tool-table.json.md).",
    },
    externalDocs: { description: "Agent API guide", url: DOCS_URL },
    servers: [{ url: origin(ctx) }],
    tags: [
      { name: "well-known", description: "Machine-readable documents. No auth." },
      { name: "agents", description: "Self-registration and identity." },
      { name: "keys", description: "Key caps and revocation." },
      { name: "wallet", description: "Prepaid micro-USD wallet (Stripe card rail)." },
      { name: "billing", description: "Quotes, receipts, disputes." },
      { name: "a2a", description: "State-of-work handoff between agents (docs/api/handoff.md)." },
      { name: "mcp", description: "The metered MCP Streamable HTTP proxy." },
      { name: "router", description: "discover / inspect / run (apps/router)." },
    ],
    paths,
    components,
  };
}
