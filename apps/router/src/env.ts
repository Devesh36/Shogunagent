/**
 * Every environment variable the router reads, in one place (as apps/api/src/boot.ts does).
 *
 * Secrets (set with `wrangler secret put`, read by src/transport.ts ONLY):
 *   MONID_API_KEY              bearer for the Monid hosted-catalog passthrough
 *   ROUTER_SETTLEMENT_TOKEN    bearer the router presents to apps/api's internal router routes
 *                              (reserve, settlements; E29/E32); the same value as apps/api's
 *                              ROUTER_SETTLEMENT_TOKEN
 *   ROUTER_METRICS_TOKEN       bearer for `GET /metrics` (E37, Prometheus text). Unset = 404
 *   OTEL_EXPORTER_OTLP_HEADERS `k=v,k2=v2` sent to the OTLP collector only (E37), optional
 *
 * Non-secrets (`vars` in wrangler.jsonc):
 *   SHOGUNAGENT_API_BASE_URL   base URL of the ShogunAgent REST face the first-party connector calls
 *   APPROVAL_BASE_URL          where L3 approval URLs point
 *   ROUTER_TOPUP_URL           where a 402 sends the human (development metering only)
 *   ROUTER_WALLET_URL          apps/api's base URL (E32). Set = every metered run is reserved and
 *                              settled through apps/api's wallet (`POST /v1/internal/router/reserve`
 *                              and `/settlements`), with the durable outbox below. Unset = the
 *                              in-memory development wallet; nothing is forwarded.
 *   OTEL_EXPORTER_OTLP_ENDPOINT / OTEL_EXPORTER_OTLP_TRACES_ENDPOINT
 *                              E37 opt-in span export (https, or http to loopback). Unset = off
 *   OTEL_SERVICE_NAME          default `shogunagent-router`
 *   OTEL_TRACES_SAMPLER_ARG    span sample ratio 0–1 (default 1)
 *   ROUTER_DEV_SHARED_CREDENTIALS
 *                              E48, development only. `1` = the first-party `caller_connector` tools
 *                              (Gmail, Calendar, GitHub, Linear, Slack) read one shared token from
 *                              the env variable each names (GOOGLE_OAUTH_ACCESS_TOKEN, GITHUB_TOKEN,
 *                              LINEAR_OAUTH_TOKEN, SLACK_TOKEN). Ignored (and logged as
 *                              `dev_shared_credentials_ignored`) whenever ROUTER_WALLET_URL is set:
 *                              production always fetches the calling principal's own token from
 *                              apps/api (docs/api/connector-tokens.md). Unset = those tools answer
 *                              `provider_unavailable` without ROUTER_WALLET_URL.
 *
 * Bindings:
 *   HYPERDRIVE                 the Postgres holding `router_settlement_outbox` (E33,
 *                              apps/router/sql/settlement-outbox.sql) and `router_run_idempotency`
 *                              (E48, apps/router/sql/run-idempotency.sql). Required with
 *                              ROUTER_WALLET_URL: without it metered runs are refused (503
 *                              wallet_unavailable) rather than charged without a durable settlement,
 *                              and idempotency falls back to the per-isolate store.
 *
 * Cron: `wrangler.jsonc` `triggers.crons` runs the outbox retry job and the idempotency prune
 * (`scheduled` in src/worker.ts).
 */
export interface RouterEnv {
  MONID_API_KEY?: string;
  ROUTER_SETTLEMENT_TOKEN?: string;
  SHOGUNAGENT_API_BASE_URL?: string;
  APPROVAL_BASE_URL?: string;
  ROUTER_TOPUP_URL?: string;
  ROUTER_WALLET_URL?: string;
  HYPERDRIVE?: { connectionString: string };
  ROUTER_METRICS_TOKEN?: string;
  OTEL_EXPORTER_OTLP_HEADERS?: string;
  OTEL_EXPORTER_OTLP_ENDPOINT?: string;
  OTEL_EXPORTER_OTLP_TRACES_ENDPOINT?: string;
  OTEL_SERVICE_NAME?: string;
  OTEL_TRACES_SAMPLER_ARG?: string;
  ROUTER_DEV_SHARED_CREDENTIALS?: string;
}

export const ENV_NAMES = {
  secrets: ["MONID_API_KEY", "ROUTER_SETTLEMENT_TOKEN", "ROUTER_METRICS_TOKEN", "OTEL_EXPORTER_OTLP_HEADERS"],
  vars: ["SHOGUNAGENT_API_BASE_URL", "APPROVAL_BASE_URL", "ROUTER_TOPUP_URL", "ROUTER_WALLET_URL", "OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_SERVICE_NAME", "OTEL_TRACES_SAMPLER_ARG", "ROUTER_DEV_SHARED_CREDENTIALS"],
  bindings: ["HYPERDRIVE"],
} as const;

export const DEFAULTS = {
  SHOGUNAGENT_API_BASE_URL: "https://api.shogunaios.com",
  APPROVAL_BASE_URL: "https://api.shogunaios.com/approve",
  ROUTER_TOPUP_URL: "https://api.shogunaios.com/wallet/topup",
} as const;
