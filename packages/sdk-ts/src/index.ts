/**
 * @shogunagent/sdk — the TypeScript client of ShogunAgent, the state layer for AI agents.
 *
 * Framework middleware lives in subpath exports: `@shogunagent/sdk/ai-sdk`,
 * `/openai-agents`, `/claude-agent-sdk`, `/mastra`, `/langgraph`.
 */
export {
  HOSTED_BASE_URL, LOCAL_BASE_URL, MAX_INGEST_ITEMS, MCP_PROTOCOL_VERSION, SDK_VERSION, ShogunAgent, newIdempotencyKey,
  type ActionOutcome, type MandateRequestOutcome, type PendingApproval, type ShogunAgentOptions, type SpendOutcome, type WaitOptions,
} from "./client.js";
export {
  AuthenticationError, HandoffError, PaymentRequiredError, PermissionDeniedError, RateLimitError, ServiceUnavailableError,
  ShogunAgentError, ToolError, TransportError, decodePaymentRequiredHeader, errorFromResponse,
} from "./errors.js";
export {
  CONTEXT_CLOSE, CONTEXT_OPEN, contextBlock, escapeContext, hasContext, renderContext, replaceContext, stripContext,
} from "./context.js";
export {
  HANDOFF_FORMAT, HANDOFF_SCHEMA_URL, checkHandoff, extractHandoff, handoffEvents,
  type Handoff, type HandoffDataPart, type HandoffEntity, type HandoffFact, type HandoffParams, type HandoffProvenance, type HandoffResult,
} from "./handoff.js";
export {
  ShogunAgentMemory, textOf, turnIdempotencyKey, type CompletedTurn, type WithShogunAgentOptions,
} from "./middleware.js";
export type * from "./types.js";
