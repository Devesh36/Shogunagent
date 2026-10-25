/**
 * Every environment variable the router reads, in one place (as apps/api/src/boot.ts does).
 *
 * Secrets (set with `wrangler secret put`, read by src/transport.ts ONLY):
 *   MONID_API_KEY              bearer for the Monid hosted-catalog passthrough
 *   ROUTER_SETTLEMENT_TOKEN    bearer the router presents to apps/api's internal router routes
 *                              (reserve, settlements; E29/E32); the same value as apps/api's
 *                              ROUTER_SETTLEMENT_TOKEN
 *
 * Non-secrets (`vars` in wrangler.jsonc):
 *   SHOGUNAGENT_API_BASE_URL   base URL of the ShogunAgent REST face the first-party connector calls
 *   APPROVAL_BASE_URL          where L3 approval URLs point
 *   ROUTER_TOPUP_URL           where a 402 sends the human (development metering only)
 *   ROUTER_WALLET_URL          apps/api's base URL (E32). Set = every metered run is reserved and
 *                              settled through apps/api's wallet (`POST /v1/internal/router/reserve`
 *                              and `/settlements`), with the durable outbox below. Unset = the
 *                              in-memory development wallet; nothing is forwarded.
 *
 * Bindings:
 *   HYPERDRIVE                 the Postgres holding `router_settlement_outbox` (E33,
 *                              apps/router/sql/settlement-outbox.sql). Required with
 *                              ROUTER_WALLET_URL: without it metered runs are refused (503
 *                              wallet_unavailable) rather than charged without a durable settlement.
 *
 * Cron: `wrangler.jsonc` `triggers.crons` runs the outbox retry job (`scheduled` in src/worker.ts).
 */
export interface RouterEnv {
  MONID_API_KEY?: string;
  ROUTER_SETTLEMENT_TOKEN?: string;
  SHOGUNAGENT_API_BASE_URL?: string;
  APPROVAL_BASE_URL?: string;
  ROUTER_TOPUP_URL?: string;
  ROUTER_WALLET_URL?: string;
  HYPERDRIVE?: { connectionString: string };
}

export const ENV_NAMES = {
  secrets: ["MONID_API_KEY", "ROUTER_SETTLEMENT_TOKEN"],
  vars: ["SHOGUNAGENT_API_BASE_URL", "APPROVAL_BASE_URL", "ROUTER_TOPUP_URL", "ROUTER_WALLET_URL"],
  bindings: ["HYPERDRIVE"],
} as const;

export const DEFAULTS = {
  SHOGUNAGENT_API_BASE_URL: "https://api.shogunaios.com",
  APPROVAL_BASE_URL: "https://api.shogunaios.com/approve",
  ROUTER_TOPUP_URL: "https://api.shogunaios.com/wallet/topup",
} as const;
