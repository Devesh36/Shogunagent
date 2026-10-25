/**
 * Every environment variable the router reads, in one place (as apps/api/src/boot.ts does).
 *
 * Secrets (set with `wrangler secret put`, read by src/transport.ts ONLY):
 *   MONID_API_KEY              bearer for the Monid hosted-catalog passthrough
 *   ROUTER_SETTLEMENT_TOKEN    bearer the router presents to apps/api's settlement route (E29);
 *                              the same value as apps/api's ROUTER_SETTLEMENT_TOKEN
 *
 * Non-secrets (`vars` in wrangler.jsonc):
 *   SHOGUNAGENT_API_BASE_URL   base URL of the ShogunAgent REST face the first-party connector calls
 *   APPROVAL_BASE_URL          where L3 approval URLs point
 *   ROUTER_TOPUP_URL           where a 402 sends the human (stub metering only; E9 replaces it)
 *   ROUTER_SETTLEMENT_URL      apps/api's `POST /v1/internal/router/settlements` (E29). Unset =
 *                              run settlements are not forwarded (development)
 */
export interface RouterEnv {
  MONID_API_KEY?: string;
  ROUTER_SETTLEMENT_TOKEN?: string;
  SHOGUNAGENT_API_BASE_URL?: string;
  APPROVAL_BASE_URL?: string;
  ROUTER_TOPUP_URL?: string;
  ROUTER_SETTLEMENT_URL?: string;
}

export const ENV_NAMES = {
  secrets: ["MONID_API_KEY", "ROUTER_SETTLEMENT_TOKEN"],
  vars: ["SHOGUNAGENT_API_BASE_URL", "APPROVAL_BASE_URL", "ROUTER_TOPUP_URL", "ROUTER_SETTLEMENT_URL"],
} as const;

export const DEFAULTS = {
  SHOGUNAGENT_API_BASE_URL: "https://api.shogunaios.com",
  APPROVAL_BASE_URL: "https://api.shogunaios.com/approve",
  ROUTER_TOPUP_URL: "https://api.shogunaios.com/wallet/topup",
} as const;
