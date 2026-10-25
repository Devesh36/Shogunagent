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
    oauth: {
      type: "oauth2",
      description: "OAuth 2.1 for MCP clients (docs/api/oauth.md): authorization code with PKCE S256; the client_id is the URL of a Client ID Metadata Document; no dynamic registration. Access tokens (sat_…) act as the consenting key, with at most its scopes and never admin.",
      flows: { authorizationCode: { authorizationUrl: "/oauth/authorize", tokenUrl: "/oauth/token", refreshUrl: "/oauth/token", scopes: { read: "Read state and memory (free)", write: "Ingest and write notes (metered)", run: "Run tools (metered; L3 waits for approval)" } } },
    },
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
        accepts: {
          type: "array", description: "Ordered by preference. The x402 entry appears only where X402_ENABLED (docs/decisions/0002-x402.md).",
          items: {
            oneOf: [
              { type: "object", required: ["rail", "url"], properties: { rail: { type: "string", enum: ["card"] }, url: { type: "string", format: "uri" }, method: { type: "string" } } },
              {
                type: "object", required: ["rail", "scheme", "network", "asset", "payTo", "amount", "maxAmountRequired", "maxTimeoutSeconds", "resource"],
                description: "x402 PaymentRequirements: pay this call in USDC and resend it with X-PAYMENT. 1 atomic unit = 1 micro-USD.",
                properties: {
                  rail: { type: "string", enum: ["x402"] }, scheme: { type: "string", enum: ["exact"] }, network: { type: "string" }, asset: { type: "string" }, payTo: { type: "string" },
                  amount: { type: "string", pattern: "^[0-9]+$" }, maxAmountRequired: { type: "string", pattern: "^[0-9]+$" }, maxTimeoutSeconds: { type: "integer" },
                  resource: { type: "string", format: "uri" }, description: { type: "string" }, mimeType: { type: "string" }, extra: { type: "object" },
                },
              },
            ],
          },
        },
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
    Receipt: { type: "object", required: ["id", "key_id", "method", "name", "unit", "quantity", "cost_micro_usd", "reserved_micro_usd", "settled", "ts"], properties: { id: { type: "string", pattern: "^rcpt_" }, key_id: { type: "string" }, method: { type: "string" }, name: { type: ["string", "null"] }, unit: { type: "string" }, quantity: { type: "integer" }, cost_micro_usd: micro("Settled cost"), reserved_micro_usd: { oneOf: [micro("Held while in flight"), { type: "null" }] }, settled: { type: "boolean" }, funding: { type: "string", enum: ["wallet", "commit", "x402"], description: "How the call was paid: the wallet, an annual commit, or one x402 payment (only where X402_ENABLED)." }, ts: { type: "string", format: "date-time" } } },
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
    Statement: {
      type: "object", description: "shogunagent/statement/v1. Amounts are integer micro-USD; `hash` is sha256 over the canonical JSON (RFC 8785) of every other field.",
      required: ["schema", "statement_id", "principal_id", "month", "status", "usage", "totals", "credits", "adjustments", "disputes", "commit", "hash"],
      properties: {
        schema: { type: "string", enum: ["shogunagent/statement/v1"] }, statement_id: { type: "string" }, principal_id: { type: "string" }, month: { type: "string" },
        status: { type: "string", enum: ["open", "closed"] },
        usage: { type: "array", items: { type: "object", properties: { unit: { type: "string" }, calls: { type: "integer" }, quantity: { type: "integer" }, list_micro_usd: micro("At list price"), charged_micro_usd: micro("Charged"), discount_micro_usd: micro("Commit discount"), commit_funded_micro_usd: micro("Drawn from a commit"), wallet_funded_micro_usd: micro("Drawn from the wallet"), free: { type: "boolean" } } } },
        totals: { type: "object" }, credits: { type: "array", items: { type: "object" } }, adjustments: { type: "array", items: { type: "object" } },
        disputes: { type: "array", items: { type: "object", properties: { dispute_id: { type: "string" }, receipt_id: { type: "string" }, status: { type: "string", enum: ["open", "resolved", "rejected"] }, credit_micro_usd: micro("Credited"), opened_at: { type: "string" }, resolved_at: { type: ["string", "null"] } } } },
        commit: { oneOf: [{ type: "object" }, { type: "null" }] }, unsettled_receipts: { type: "integer" }, generated_at: { type: "string", format: "date-time" },
        hash: { type: "string", pattern: "^sha256:[0-9a-f]{64}$" },
      },
    },
    Commit: {
      type: "object",
      properties: {
        commit_id: { type: "string", pattern: "^cmt_" }, subject: { type: "string", enum: ["principal", "tenant"] }, subject_id: { type: "string" },
        discount_bps: { type: "integer", minimum: 1000, maximum: 2500 }, amount_micro_usd: micro("Committed spend"), drawn_micro_usd: micro("Drawn so far (discounted)"),
        remaining_micro_usd: micro("amount − drawn"), overage_micro_usd: micro("Drawn above the amount"), overage_reported_micro_usd: micro("Reported to Stripe"),
        overage_billing: { type: "boolean" }, active: { type: "boolean" }, starts_at: { type: "string", format: "date-time" }, ends_at: { type: "string", format: "date-time" },
      },
    },
    CommitRequest: {
      type: "object", required: ["subject_id", "amount_micro_usd", "discount_bps", "starts_at", "ends_at"],
      properties: {
        subject: { type: "string", enum: ["principal", "tenant"], default: "principal" }, subject_id: { type: "string" }, amount_micro_usd: micro("Committed spend"),
        discount_bps: { type: "integer", minimum: 1000, maximum: 2500 }, starts_at: { type: "string", format: "date-time" }, ends_at: { type: "string", format: "date-time" },
        stripe_customer_id: { type: "string", pattern: "^cus_" },
      },
    },
    AdjustmentRequest: {
      type: "object", required: ["principal_id", "kind", "amount_micro_usd"],
      properties: {
        principal_id: { type: "string" }, kind: { type: "string", enum: ["credit", "refund"] }, amount_micro_usd: micro("Positive amount"),
        reason: { type: "string", enum: ["goodwill", "dispute", "service_credit", "refund", "chargeback", "correction"] }, charge_id: { type: "string", pattern: "^(ch|py)_" }, ref: { type: "string" },
      },
    },
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
  method: "get" | "post" | "patch" | "delete" | "all";
  path: string;
  tag: string;
  summary: string;
  description?: string;
  security: "none" | "key" | "stripe" | "pow" | "service";
  scope?: "read" | "write" | "run" | "admin" | "any";
  /** Takes the agent key itself, never an OAuth access token. */
  keyOnly?: boolean;
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
    { method: "post", path: "/v1/keys/{id}/revoke", tag: "keys", security: "key", scope: "any", keyOnly: true, summary: "Revoke a key (own key; admin for others).", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object", properties: { key_id: { type: "string" }, revoked: { type: "boolean" } } }), "404": errorResponse("key_not_found (unknown and someone else's answer the same)") } },
    { method: "patch", path: "/v1/keys/{id}", tag: "keys", security: "key", scope: "any", keyOnly: true, summary: "Set a key's hard caps. Lowering: any key; raising above the defaults: admin.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], requestBody: jsonBody(ref("CapsPatch")), responses: { "200": jsonBody({ type: "object", properties: { key_id: { type: "string" }, caps: ref("Caps") } }), "400": errorResponse("invalid_caps | invalid_json"), "403": errorResponse("caps_above_default"), "404": errorResponse("key_not_found") } },
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
    // --- E19: statements, commits, operator billing ---
    {
      method: "get", path: "/v1/statements", tag: "billing", security: "key", scope: "read",
      summary: "The month's statement: usage per unit (free units at $0), credits, adjustments, disputes and their resolution, commit drawdown. Immutable once the month closes (stored, with a SHA-256).",
      parameters: [{ name: "month", in: "query", schema: { type: "string", pattern: "^\\d{4}-(0[1-9]|1[0-2])$" }, description: "YYYY-MM (UTC). Default: the current month." }, { name: "format", in: "query", schema: { type: "string", enum: ["json", "csv"], default: "json" } }],
      responses: { "200": { description: "The statement; X-Statement-Hash repeats its hash.", content: { "application/json": { schema: ref("Statement") }, "text/csv": { schema: { type: "string" } } } }, "400": errorResponse("invalid_month | month_not_started | invalid_format") },
    },
    { method: "get", path: "/v1/commit", tag: "billing", security: "key", scope: "read", summary: "The active annual commit for this principal (or its tenant): discount, amount, drawn, remaining, overage. null when none.", responses: { "200": jsonBody({ type: "object", properties: { commit: { oneOf: [ref("Commit"), { type: "null" }] }, next_action: { type: "null" } } }) } },
    { method: "post", path: "/v1/agents/me/licence", tag: "billing", security: "key", scope: "write", keyOnly: true, summary: "Link a ShogunAI licence (X-Shogun-Licence header). A Pro licence earns $20 of credit per month.", parameters: [{ name: "X-Shogun-Licence", in: "header", required: true, schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object" }), "400": errorResponse("licence_missing"), "401": errorResponse("invalid_licence"), "403": errorResponse("licence_plan_not_eligible | oauth_not_allowed") } },
    { method: "post", path: "/v1/admin/commits", tag: "admin", security: "key", scope: "admin", summary: "Create an annual commit (10–25% off) for a principal or tenant; optional Stripe customer for metered overage.", parameters: [{ $ref: "#/components/parameters/IdempotencyKey" }], requestBody: jsonBody(ref("CommitRequest")), responses: { "201": jsonBody({ type: "object", properties: { commit: ref("Commit"), created: { type: "boolean" } } }), "200": jsonBody({ type: "object" }), "400": errorResponse("idempotency_key_required | invalid_subject | invalid_amount | invalid_discount | invalid_period | invalid_customer | overage_billing_unavailable"), "403": errorResponse("insufficient_scope | oauth_not_allowed"), "409": errorResponse("commit_overlap") } },
    { method: "post", path: "/v1/admin/adjustments", tag: "admin", security: "key", scope: "admin", summary: "Credit or refund a principal: one ledger entry per Idempotency-Key; a refund with charge_id also refunds the card through Stripe.", parameters: [{ $ref: "#/components/parameters/IdempotencyKey" }], requestBody: jsonBody(ref("AdjustmentRequest")), responses: { "201": jsonBody({ type: "object" }), "200": jsonBody({ type: "object" }), "400": errorResponse("idempotency_key_required | invalid_kind | invalid_amount | invalid_reason | invalid_charge | refunds_unavailable"), "404": errorResponse("principal_not_found") } },
    { method: "post", path: "/v1/admin/disputes/{id}/resolve", tag: "admin", security: "key", scope: "admin", summary: "Resolve a dispute: credit (default: the receipt's cost) or reject. Once.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], requestBody: jsonBody({ type: "object", required: ["outcome"], properties: { outcome: { type: "string", enum: ["credit", "reject"] }, amount_micro_usd: micro("Credit amount") } }), responses: { "200": jsonBody({ type: "object" }), "404": errorResponse("dispute_not_found") } },
    // --- E3-T2: OAuth 2.1 + Client ID Metadata Documents ---
    { method: "get", path: "/.well-known/oauth-protected-resource", tag: "oauth", security: "none", summary: "RFC 9728 Protected Resource Metadata for /mcp (also at /.well-known/oauth-protected-resource/mcp).", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/oauth-protected-resource/mcp", tag: "oauth", security: "none", summary: "RFC 9728 Protected Resource Metadata, path-suffixed form for /mcp.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/.well-known/oauth-authorization-server", tag: "oauth", security: "none", summary: "RFC 8414 Authorization Server Metadata: code + PKCE S256, client_id_metadata_document_supported, no registration endpoint.", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "get", path: "/oauth/authorize", tag: "oauth", security: "none", summary: "Validate the client (its metadata document) and redirect_uri, then render the consent page.", responses: { "200": { description: "The consent page.", content: { "text/html": { schema: { type: "string" } } } }, "302": { description: "An error returned to the validated redirect_uri." }, "400": { description: "An HTML error page; never a redirect." } } },
    { method: "post", path: "/oauth/authorize", tag: "oauth", security: "none", summary: "The human's decision (form post from the consent page) → 302 to redirect_uri with code, state and iss.", responses: { "302": { description: "code or error" }, "400": { description: "invalid_consent" }, "401": { description: "The key was not accepted; the page is shown again." } } },
    { method: "post", path: "/oauth/token", tag: "oauth", security: "none", summary: "grant_type=authorization_code (PKCE S256) or refresh_token (rotated; reuse revokes the family). Form-encoded.", responses: { "200": jsonBody({ type: "object", properties: { access_token: { type: "string", pattern: "^sat_" }, token_type: { type: "string", enum: ["Bearer"] }, expires_in: { type: "integer" }, refresh_token: { type: "string", pattern: "^sar_" }, scope: { type: "string" } } }), "400": jsonBody({ type: "object", properties: { error: { type: "string" }, error_description: { type: "string" } } }), "401": jsonBody({ type: "object" }) } },
    { method: "post", path: "/oauth/revoke", tag: "oauth", security: "none", summary: "RFC 7009: revoke a token and everything minted from the same consent. Always 200.", responses: { "200": { description: "Revoked, or nothing to revoke." } } },
    // --- E23: legal pages ---
    { method: "get", path: "/privacy", tag: "well-known", security: "none", summary: "Privacy policy (DRAFT pending legal review), rendered from docs/legal/privacy-policy.md.", responses: { "200": { content: { "text/html": { schema: { type: "string" } } } } } },
    { method: "get", path: "/terms", tag: "well-known", security: "none", summary: "Terms of service (DRAFT pending legal review), rendered from docs/legal/terms.md.", responses: { "200": { content: { "text/html": { schema: { type: "string" } } } } } },
    // --- remote approvals (E15): webhooks for agents; the page, Slack and the notification are for humans and the core ---
    { method: "post", path: "/v1/webhooks", tag: "approvals", security: "key", scope: "write", summary: "Register an HTTPS endpoint for approval.created / approval.decided. Deliveries are signed (X-Shogun-Signature: t=<unix>,v1=HMAC-SHA256(secret, \"<t>.<body>\")), retried with backoff, and carry the redacted summary only. The secret is shown once.", requestBody: jsonBody({ type: "object", required: ["url"], properties: { url: { type: "string", format: "uri" }, events: { type: "array", items: { type: "string", enum: ["approval.created", "approval.decided"] } } } }), responses: { "201": jsonBody({ type: "object", properties: { id: { type: "string" }, url: { type: "string" }, events: { type: "array", items: { type: "string" } }, secret: { type: "string" } } }), "400": errorResponse("invalid_url | invalid_events | invalid_json"), "409": errorResponse("too_many_webhooks") } },
    { method: "get", path: "/v1/webhooks", tag: "approvals", security: "key", scope: "read", summary: "The principal's approval webhooks (no secrets).", responses: { "200": jsonBody({ type: "object" }) } },
    { method: "delete", path: "/v1/webhooks/{id}", tag: "approvals", security: "key", scope: "write", summary: "Remove one of the principal's webhooks.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object" }), "404": errorResponse("webhook_not_found") } },
    { method: "get", path: "/approve/{token}", tag: "approvals", security: "none", summary: "The human approval page for an approval URL (redacted summary, Approve / Reject). Not for agents: an agent never decides its own L2/L3.", parameters: [{ name: "token", in: "path", required: true, schema: { type: "string" } }], responses: { "200": { description: "HTML page" }, "410": { description: "The link expired" } } },
    { method: "post", path: "/v1/slack/interactions", tag: "approvals", security: "none", summary: "Slack button presses on approval messages (Slack-signed). Not for agents.", responses: { "200": { description: "Acknowledged" }, "401": errorResponse("invalid_signature") } },
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
    // --- E29: the open catalog (apps/router) and provider payouts ---
    { method: "get", path: "/v1/catalog", tag: "catalog", security: "none", servedBy: "apps/router", summary: "Every listed endpoint: price, live health and p50/p95, categories, first-party label, permission level, data-handling statement, and the published score formula. Free, no key.", parameters: [{ name: "category", in: "query", schema: { type: "string" } }, { name: "provider", in: "query", schema: { type: "string" } }, { name: "first_party", in: "query", schema: { type: "string", enum: ["true", "false"] } }], responses: { "200": jsonBody({ type: "object", properties: { format: { type: "string", enum: ["shogunagent-open-catalog/1"] }, catalog_hash: { type: "string" }, entry_count: { type: "integer" }, ranking: { type: "object" }, entries: { type: "array", items: { type: "object" } } } }) } },
    { method: "get", path: "/v1/catalog/{tool_id}", tag: "catalog", security: "none", servedBy: "apps/router", summary: "One listed endpoint with its description, exclusions, input/output JSON Schema and the score formula.", parameters: [{ name: "tool_id", in: "path", required: true, schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object" }), "404": errorResponse("tool_not_found") } },
    { method: "get", path: "/catalog", tag: "catalog", security: "none", servedBy: "apps/router", summary: "The open catalog as a server-rendered HTML page (no JavaScript).", responses: { "200": { content: { "text/html": { schema: { type: "string" } } } } } },
    { method: "post", path: "/v1/internal/router/settlements", tag: "providers", security: "service", summary: "The router's run settlements (content-free; idempotent by run_id) → the provider's daily accrual. Not for agents.", requestBody: jsonBody({ type: "object", required: ["settlements"], properties: { settlements: { type: "array", minItems: 1, maxItems: 100, items: { type: "object", required: ["format", "run_id", "ts", "tool_id", "provider_id", "first_party", "payout_account_ref", "outcome", "quantity", "provider_micro_usd"], properties: { format: { type: "string", enum: ["shogunagent-run-settlement/1"] }, run_id: { type: "string" }, ts: { type: "integer" }, tool_id: { type: "string" }, provider_id: { type: "string" }, first_party: { type: "boolean" }, payout_account_ref: { type: ["string", "null"] }, outcome: { type: "string", enum: ["ok", "empty", "provider_error", "transport_error"] }, quantity: { type: "integer", minimum: 0 }, provider_micro_usd: micro("The provider's price; 0 unless ok") } } } } }), responses: { "200": jsonBody({ type: "object", properties: { recorded: { type: "integer" }, duplicates: { type: "integer" } } }), "400": errorResponse("invalid_settlements | invalid_settlement"), "401": errorResponse("invalid_token"), "503": errorResponse("settlements_unavailable | store_unavailable") } },
    { method: "get", path: "/v1/providers/me", tag: "providers", security: "key", scope: "read", summary: "The provider this key's principal owns: payout account status, this month's runs and earnings, the minimum payout.", parameters: [{ name: "provider_id", in: "query", schema: { type: "string" }, description: "Required when the principal owns several providers." }], responses: { "200": jsonBody({ type: "object" }), "404": errorResponse("provider_not_found") } },
    { method: "post", path: "/v1/providers/me/connect", tag: "providers", security: "key", scope: "write", keyOnly: true, summary: "A Stripe Connect Express onboarding link for the provider's payout account. Hand the URL to a human at the provider once.", responses: { "201": jsonBody({ type: "object", properties: { provider_id: { type: "string" }, url: { type: "string", format: "uri" }, expires_at: { type: ["string", "null"] }, next_action: { type: "string", enum: ["approve_at_url"] } } }), "404": errorResponse("provider_not_found"), "503": errorResponse("connect_unavailable") } },
    { method: "get", path: "/v1/providers/connect/return", tag: "providers", security: "none", summary: "Where Stripe returns the provider after onboarding (signed state). Binds payout_account_ref once payouts are enabled. HTML, no JavaScript. Not for agents.", parameters: [{ name: "state", in: "query", required: true, schema: { type: "string" } }], responses: { "200": { content: { "text/html": { schema: { type: "string" } } } }, "400": { description: "Invalid or expired state" } } },
    { method: "get", path: "/v1/providers/connect/refresh", tag: "providers", security: "none", summary: "A fresh onboarding link when Stripe's has expired (signed state) → 302. Not for agents.", parameters: [{ name: "state", in: "query", required: true, schema: { type: "string" } }], responses: { "302": { description: "To Stripe's hosted onboarding" }, "400": { description: "Invalid or expired state" } } },
    { method: "get", path: "/v1/providers/me/statements", tag: "providers", security: "key", scope: "read", summary: "The provider's monthly statement: runs and earnings per day (failures and empty results at $0), our take, refunds, disputes and the payout decision.", parameters: [{ name: "month", in: "query", schema: { type: "string", pattern: "^\\d{4}-(0[1-9]|1[0-2])$" } }, { name: "provider_id", in: "query", schema: { type: "string" } }], responses: { "200": jsonBody({ type: "object", properties: { schema: { type: "string", enum: ["shogunagent/provider-statement/v1"] }, hash: { type: "string" } } }), "400": errorResponse("invalid_month | provider_id_required"), "404": errorResponse("provider_not_found") } },
    { method: "post", path: "/v1/admin/providers", tag: "admin", security: "key", scope: "admin", summary: "Bind a listed provider to the principal whose keys act for it (after the listing PR is merged).", requestBody: jsonBody({ type: "object", required: ["provider_id", "owner_principal_id"], properties: { provider_id: { type: "string" }, owner_principal_id: { type: "string" } } }), responses: { "200": jsonBody({ type: "object" }), "400": errorResponse("invalid_provider | invalid_owner") } },
    { method: "post", path: "/v1/admin/providers/{id}/adjustments", tag: "admin", security: "key", scope: "admin", summary: "A refund (deducted) or a dispute (holds the provider's payout while open) against a provider. One per Idempotency-Key.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }, { $ref: "#/components/parameters/IdempotencyKey" }], requestBody: jsonBody({ type: "object", required: ["kind", "amount_micro_usd", "reason"], properties: { kind: { type: "string", enum: ["refund", "dispute"] }, amount_micro_usd: micro("Amount"), reason: { type: "string", enum: ["refund", "chargeback", "dispute", "correction", "fraud"] }, run_id: { type: "string" } } }), responses: { "201": jsonBody({ type: "object" }), "200": jsonBody({ type: "object" }), "400": errorResponse("idempotency_key_required | invalid_kind | invalid_amount | invalid_reason"), "404": errorResponse("provider_not_found") } },
    { method: "post", path: "/v1/admin/providers/{id}/disputes/{entry}/resolve", tag: "admin", security: "key", scope: "admin", summary: "Resolve a provider dispute: provider_won (nothing deducted) or provider_lost (deducted). Once.", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }, { name: "entry", in: "path", required: true, schema: { type: "string" } }], requestBody: jsonBody({ type: "object", required: ["outcome"], properties: { outcome: { type: "string", enum: ["provider_won", "provider_lost"] } } }), responses: { "200": jsonBody({ type: "object" }), "404": errorResponse("dispute_not_found") } },
    { method: "post", path: "/v1/admin/payouts/run", tag: "admin", security: "key", scope: "admin", summary: "Run the monthly provider payout for a closed month. Idempotent per provider-month (Stripe Idempotency-Key payout:<provider>:<month>).", requestBody: jsonBody({ type: "object", required: ["month"], properties: { month: { type: "string" } } }), responses: { "200": jsonBody({ type: "object", properties: { month: { type: "string" }, providers: { type: "integer" }, paid: { type: "integer" }, held: { type: "integer" }, below_minimum: { type: "integer" }, already_paid: { type: "integer" }, failed: { type: "integer" } } }), "409": errorResponse("month_open") } },
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
      // An OAuth access token is accepted wherever a key is, except on admin routes and key /
      // licence management, which take the key itself.
      security: route.security === "key"
        ? [{ agentKey: route.scope && route.scope !== "any" ? [route.scope] : [] },
          ...(route.scope !== "admin" && !route.keyOnly ? [{ oauth: route.scope && route.scope !== "any" ? [route.scope] : [] }] : [])]
        : [],
      ...(route.security === "none" ? { "x-anonymous": true } : {}),
      ...(route.security === "pow" ? { "x-proof-of-work": true } : {}),
      ...(route.security === "stripe" ? { "x-webhook-signature": "Stripe-Signature" } : {}),
      ...(route.security === "service" ? { "x-service-token": "ROUTER_SETTLEMENT_TOKEN" } : {}),
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
      { name: "billing", description: "Quotes, receipts, disputes, statements, commits." },
      { name: "a2a", description: "State-of-work handoff between agents (docs/api/handoff.md)." },
      { name: "admin", description: "Operator routes (admin key only): commits, credits and refunds, dispute resolution." },
      { name: "oauth", description: "OAuth 2.1 + Client ID Metadata Documents for MCP clients. No dynamic registration." },
      { name: "mcp", description: "The metered MCP Streamable HTTP proxy." },
      { name: "router", description: "discover / inspect / run (apps/router)." },
      { name: "approvals", description: "Remote L2/L3 approvals: webhooks for the principal, the approval page and Slack for humans." },
      { name: "catalog", description: "The open public tool catalog (apps/router): prices, health, data handling and the score formula. No auth." },
      { name: "providers", description: "Tool providers: run settlements, Stripe Connect onboarding, monthly statements and payouts (docs/api/payouts.md)." },
    ],
    paths,
    components,
  };
}
