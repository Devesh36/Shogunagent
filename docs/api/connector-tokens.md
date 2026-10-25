# Connector tokens for the router (E48 contract)

The router's first-party connectors (Gmail, Google Calendar, GitHub, Linear, Slack) act as the
**calling principal's own account** at the vendor. In production the router never holds a vendor
token in its Worker environment: for each run it asks apps/api for the token of the principal behind
the caller's agent key, uses it for that one vendor request, and drops it.

This page is the contract between the two. Both sides are built: the router's client
(`apps/router/src/transport.ts`, `caller_connector` auth; tests in `apps/router/test/hardening.test.ts`)
and the apps/api route (`apps/api/src/connector-tokens.ts`, served with the router's wallet when
`ROUTER_SETTLEMENT_TOKEN` is set; tests in `apps/api/test/connector-tokens.test.ts`, which also drive
the router's real client against the route in-process). What is still missing before the hosted
first-party connectors can work is listed under "What the gateway does not have yet" below.

## Route

```
POST /v1/internal/router/connector-token
Authorization: Bearer <ROUTER_SETTLEMENT_TOKEN>
X-Caller-Authorization: Bearer <the calling agent's key: sa_… or an OAuth access token sat_…>
Content-Type: application/json
Accept: application/json

{ "slot": "gmail-tokenset", "provider_id": "gmail", "run_id": "0b6f…" }
```

- **Base URL**: the router's `ROUTER_WALLET_URL` (apps/api's origin), the same as
  `/v1/internal/router/reserve` and `/settlements`.
- **Service authentication**: `Authorization` carries `ROUTER_SETTLEMENT_TOKEN`, the secret the
  router already presents on its other internal routes. apps/api compares it in constant time and
  answers `401` without reading the body when it does not match. The service token alone grants
  nothing: it does not name a principal.
- **Caller authentication**: `X-Caller-Authorization` carries the agent's own bearer exactly as the
  agent sent it to the router. apps/api verifies it as on `/v1/internal/router/reserve` (hash
  lookup, not revoked, not expired, the `run` scope) and resolves **the principal from the key**.
  The request body never names a principal, so the router cannot ask for another principal's token.
- **Body** (content-free, ≤ 1 KiB; apps/api rejects unknown fields with `400`):

  | Field | Type | Meaning |
  |---|---|---|
  | `slot` | string, one of `gmail-tokenset`, `gcal-tokenset`, `github-tokenset`, `linear-tokenset`, `slack-tokenset` | the `connector_tokens.account` row to open (`shogun_integrations::token_store::CredentialSlot::Connector`) |
  | `provider_id` | string | the router provider id (`gmail`, `calendar`, `github`, `linear`, `slack`); for audit and for checking that the slot belongs to the provider |
  | `run_id` | string, optional | the router's run id, for correlating with the run's settlement; not a secret |

  `provider_id` → `slot` is fixed: `gmail` → `gmail-tokenset`, `calendar` → `gcal-tokenset`,
  `github` → `github-tokenset`, `linear` → `linear-tokenset`, `slack` → `slack-tokenset`. A pair
  outside this table is `400`.

## What apps/api does

1. Verify the service token, then the caller's key; resolve `principal_id`.
2. Read the row `(principal_id, slot)` from `connector_tokens` (`apps/api/sql/connector-tokens.sql`)
   and open its KMS envelope with associated data `(principal_id, slot)` (the
   `RemoteTokenStore` / `KmsSealer` contract). A row that fails to open is treated as absent and
   logged `connector_token_unreadable` (fixed words, no ids).
3. The stored value is the connector's tokenset (access token, refresh token, expiry). If the access
   token expires within 60 seconds, refresh it with the vendor using the refresh token (through the
   OAuth broker's refresh path, #1101 D6) and write the new tokenset back, sealed, before answering.
4. Answer with the **access token only**. The refresh token never leaves apps/api.

apps/api never logs the token, the tokenset, the request body or the caller's key; the route is
metered as an internal call (no charge, invariant 10 does not apply: it is not an agent-facing read).

## Responses

| Status | Body | Router behaviour |
|---|---|---|
| `200` | `{ "access_token": "<vendor token>", "token_type": "Bearer", "expires_at": <unix ms> }` with `Cache-Control: no-store` | injects `Authorization: Bearer <access_token>` into the one vendor request. A token that is not 8–4096 visible ASCII characters, or whose `expires_at` has passed, is refused as unavailable (below). |
| `404` | `{ "error": { "code": "connector_not_connected", "message": "…", "next_action": "authenticate" } }` | the principal has no row for the slot (never connected, disconnected, or unreadable). The agent gets `403 connector_not_connected`, `next_action: authenticate`, $0: the user must connect the account in ShogunAgent first. |
| `401` / `403` | the agent-surface error body | the caller's key is invalid, revoked, lacks the `run` scope, or the principal may not use this connector (e.g. a plan or tenant policy). The agent gets `401 authentication_required`. |
| `400` | the agent-surface error body | malformed request (a router bug). The agent gets `503 provider_unavailable`, `retry_after`. |
| `5xx`, timeout (5 s), network error | anything | the agent gets `503 provider_unavailable` with `retry_after: 5`, $0. |

Every refusal happens after the run's reservation and is settled at $0 (`transport_error`), so a
caller is never charged for a connector it could not use; the run's idempotency key is released and
can be retried.

## Guarantees the router keeps

- The access token exists only inside `src/transport.ts`, between the token response and the vendor
  request. It is not cached across runs, not returned to the engine, not in a run record, a
  settlement, an idempotency row, a log line, a metric or a span (`test/hardening.test.ts` drives a
  sentinel token through and asserts none of those contain it).
- The caller's key goes to apps/api in `X-Caller-Authorization` and never to the vendor.
- With `ROUTER_WALLET_URL` set, the Worker env variables the connectors name
  (`GOOGLE_OAUTH_ACCESS_TOKEN`, `GITHUB_TOKEN`, `LINEAR_OAUTH_TOKEN`, `SLACK_TOKEN`) are never read,
  even if set, and `ROUTER_DEV_SHARED_CREDENTIALS` is ignored (logged
  `dev_shared_credentials_ignored` at isolate start).

## Development

Without `ROUTER_WALLET_URL`, `ROUTER_DEV_SHARED_CREDENTIALS=1` makes the connectors read one shared
token from the env variable each names. That is one account for every caller and is for a developer's
own machine only. Without the flag (and without the wallet URL) the connectors answer
`503 provider_unavailable` (`try_alternative`) before any network call.

## How apps/api opens a row (E48)

`apps/api/src/connector-tokens.ts` reads the row with the `connector_tokens` store
(`SqlConnectorRows`) and opens it with `KmsSealer`, a byte-compatible TypeScript twin of the Rust
`shogun_integrations::token_store::KmsSealer`: the row is
`[1][u16 BE wrapped-key length][wrapped data key][12-byte nonce][ChaCha20-Poly1305 ciphertext ‖ tag]`,
the data key is unwrapped by the KMS under the context `shogun/token/v1/<principal>/<slot>`, and the
AEAD's associated data is `shogun-token-kms/v1|<principal>|<slot>`. The stored value is the JSON
`shogun_integrations::token::serialize` writes (`access_token`, `refresh_token`, `expires_at_ms`,
`account_label`). A KMS that does not answer is `503` (nothing opened); a row that does not open, or
does not parse, is `404` and logs `connector_token_unreadable`.

The KMS itself is a `KeyWrapper` the runtime passes in (`RelayShared.connectorKeyWrapper` in
`apps/api/src/boot.ts`), and the refresh is a `ConnectorRefresher` (`connectorRefresher`). Without a
wrapper the route still authenticates both bearers, then answers `404 connector_not_connected` and
logs `connector_vault_unconfigured`: the same answer an unconnected principal gets, never a token.
Without a refresher, a token expiring within 60 seconds is returned as is while it is still valid
(`expires_at` tells the router), and an expired one is `404` (`connector_token_expired`). A refresh
that answers `revoked` is `404`; one that fails while the old token is still valid returns the old
token, and `503` when it has expired. A refreshed tokenset is sealed again and written back before
the answer (a failed write-back is logged `connector_token_writeback_failed` and the fresh token is
still returned); a refresh response without a refresh token keeps the stored one.

## What the gateway does not have yet

- **A KMS key wrapper in any runtime.** The AWS KMS, GCP Cloud KMS and Vault Transit clients live in
  the Rust core (`shogun-core` `tenancy::key_provider`), behind its single traced egress. Neither the
  Worker nor the Node entry passes a `KeyWrapper`, so production answers `404` for every caller.
  Options for the owner: a TypeScript client for the region's KMS in the gateway, or a sidecar
  endpoint that opens the row in the core (needs a `crates/` change).
- **A writer.** Nothing writes `connector_tokens` rows yet: the hosted connect flow (the user
  authorising Gmail etc. for their principal) is not built.
- **A refresh path.** No `ConnectorRefresher` is wired (the OAuth broker's refresh, #1101 D6). Two
  runs refreshing the same grant at once would both call the vendor; providers that rotate refresh
  tokens need a per-row lock when the refresher lands.

## Until all of that exists

Production must not list the first-party connectors in the hosted catalog: every caller would get
`connector_not_connected`. The pre-launch checklist (`docs/deploy/production.md` §9) keeps them out
until the wrapper, the writer and the refresh path ship.
