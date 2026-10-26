# Connector tokens for the router (E48 contract, E50 credentials, E51 Notion and revocation)

The router's first-party connectors (Gmail, Google Calendar, GitHub, Linear, Slack, Notion) act as the
**calling principal's own account** at the vendor. In production the router never holds a vendor
token in its Worker environment: for each run it asks apps/api for the token of the principal behind
the caller's agent key, uses it for that one vendor request, and drops it.

This page is the contract between the router, apps/api and the principal's sidecar, and how a human
connects an account in the first place. All of it is built:

| Piece | Code | Tests |
|---|---|---|
| The router's client (`caller_connector` auth) | `apps/router/src/transport.ts` | `apps/router/test/hardening.test.ts` |
| apps/api's route for the router | `apps/api/src/connector-tokens.ts` | `apps/api/test/connector-tokens.test.ts` (drives the router's real client in-process) |
| The sidecar vault apps/api calls (open, refresh, write back) | `crates/shogun-mcp/src/connector_vault.rs` (routes), `crates/shogun-integrations/src/hosted_connect.rs` (vault), composed in `shogun-api --multi` | `hosted_connect` unit tests, `crates/shogun-core/tests/connector_vault.rs` (the real server) |
| The hosted connect flow (OAuth redirect, Notion's Basic-auth exchange; GitHub's device flow) | `apps/api/src/connector-connect.ts` + the sidecar's connect routes; `crates/shogun-integrations/src/device_flow.rs`; the core's `HttpTokenExchange::send` (`crates/shogun-core/src/mcp_http.rs`) | `apps/api/test/connector-connect.test.ts`, `hosted_connect` and `device_flow` unit tests |
| Status and disconnect on every face | the sidecar's `status` / `disconnect` routes; `connections.list` / `connections.disconnect` (MCP, REST, CLI) served from the vault (`crates/shogun-core/src/db_backend.rs`); the gateway's `GET /v1/connectors`, `POST /v1/connectors/{provider}/disconnect` | `crates/shogun-core/tests/connector_vault.rs` (REST and MCP faces), `apps/api/test/connector-connect.test.ts` |
| Audit rows (connect, refresh, revoke, disconnect, read-sync fetch) | `hosted_connect::AuditSink`, written by `shogun_core::tenancy::connector_sync::record_audit` into the principal's traceability log | `hosted_connect` unit tests, `crates/shogun-core/tests/connector_vault.rs` |

What still stands between this and the hosted catalog is under "Before the hosted catalog lists
these tools" below.

## Route (router → apps/api)

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
  | `slot` | string, one of `gmail-tokenset`, `gcal-tokenset`, `github-tokenset`, `linear-tokenset`, `slack-tokenset`, `notion-tokenset` | the slot of the principal's token store to open (`shogun_integrations::token_store::CredentialSlot::Connector`) |
  | `provider_id` | string | the router provider id (`gmail`, `calendar`, `github`, `linear`, `slack`, `notion`); for audit and for checking that the slot belongs to the provider |
  | `run_id` | string, optional | the router's run id, for correlating with the run's settlement; not a secret |

  `provider_id` → `slot` is fixed: `gmail` → `gmail-tokenset`, `calendar` → `gcal-tokenset`,
  `github` → `github-tokenset`, `linear` → `linear-tokenset`, `slack` → `slack-tokenset`,
  `notion` → `notion-tokenset`. A pair outside this table is `400`.

## What apps/api does

1. Verify the service token, then the caller's key; resolve `principal_id`.
2. Ask the principal's sidecar (below) for the slot's access token, over the same transport as the
   `/mcp` relay (`SHOGUN_SIDECAR_URL`, or the Workers binding `SHOGUN_SIDECAR_BINDING`), with
   `SHOGUN_SIDECAR_ADMIN_TOKEN` as its own credential and the principal's sidecar bearer (the
   derived `sgw1.<principal>.<key_id>.<mac>`, or a listed token) naming the principal. Timeout
   4.5 s, so the answer fits the router's 5 s.
3. Check what comes back the way the router will (8–4096 visible ASCII characters, `expires_at` in
   the future) and answer with the **access token only**.

apps/api holds no credential row, no KMS client and no refresh token, and has no vendor client:
the refresh token never leaves the sidecar. apps/api never logs the token, the request body or the
caller's key; the route is metered as an internal call (no charge, invariant 10 does not apply: it is
not an agent-facing read).

## Responses (apps/api → router)

| Status | Body | When | Router behaviour |
|---|---|---|---|
| `200` | `{ "access_token": "<vendor token>", "token_type": "Bearer", "expires_at": <unix ms> }` with `Cache-Control: no-store` | the sidecar served a usable token | injects `Authorization: Bearer <access_token>` into the one vendor request. A token that is not 8–4096 visible ASCII characters, or whose `expires_at` has passed, is refused as unavailable (below). |
| `404` | `{ "error": { "code": "connector_not_connected", "message": "…", "next_action": "authenticate" } }` | the principal has no grant for the slot (never connected), the vendor revoked it, it expired with no way to refresh, the sidecar's token would be refused by the router, or no vault is configured on this gateway | the agent gets `403 connector_not_connected`, `next_action: authenticate`, $0: a human connects the account (below), then the run is retried. |
| `401` / `403` | the agent-surface error body | the caller's key is invalid, revoked, lacks the `run` scope, the principal may not be served here (region / tenant policy), or the sidecar refuses the principal (`403 principal_unavailable`: purged) | the agent gets `401 authentication_required`. |
| `400` | the agent-surface error body | malformed request (a router bug) | the agent gets `503 provider_unavailable`, `retry_after`. |
| `503` | `connector_token_unavailable`, `Retry-After: 5` | the sidecar did not answer or answered an error, refused the gateway's own credential (an operator fault, never surfaced as a `401`), the principal lives on another shard, or the vendor did not answer a refresh while the stored token has expired | the agent gets `503 provider_unavailable` with `retry_after: 5`, $0. |
| `5xx`, timeout (5 s), network error | anything | — | as `503`. |

Every refusal happens after the run's reservation and is settled at $0 (`transport_error`), so a
caller is never charged for a connector it could not use; the run's idempotency key is released and
can be retried.

## The sidecar vault (apps/api → sidecar, E50)

The principal's sidecar (`shogun-api --multi`) owns the principal's token store: the file-sealed
store under `SHOGUN_TOKEN_STORE_DIR` (default `<root>/.tokens`), each file sealed with a key derived
from the principal's own store key, which the configured key provider supplies (a customer KMS
envelope in production, `deploy/README.md` §3). The same store holds what the approval executor
sends with, and a principal purge (E39) removes it.

```
POST /v1/internal/connectors/token            { "slot": "gmail-tokenset" }
POST /v1/internal/connectors/connect/start    { "provider": "gmail", "state": "<gateway's sealed state>" }
POST /v1/internal/connectors/connect/finish   { "state": "<same>", "code": "<the vendor's code>" }
POST /v1/internal/connectors/connect/poll     { "state": "<same>" }            (GitHub's device flow)
POST /v1/internal/connectors/status           {}
POST /v1/internal/connectors/disconnect       { "provider": "calendar" }
Authorization: Bearer <SHOGUN_ADMIN_TOKEN>
X-Shogun-Principal-Authorization: Bearer <the principal's sidecar bearer>
```

- **Who**: the operator's admin credential (the gateway's `SHOGUN_SIDECAR_ADMIN_TOKEN`) **and** the
  principal's own sidecar bearer. The principal is the one that bearer resolves to (`sgw1` verified
  against `SHOGUN_SIDECAR_BEARER_SECRET`, or a listed token); a body that names anything else is
  `400`. An agent's bearer in `Authorization` is `403 admin_required`; no admin credential `401`; no
  or a forged principal bearer `401`; a purged principal `410`. Multi-principal mode only. Like every
  sidecar route, loopback `Host` only.
- **Token**: the sidecar opens the grant, and when it expires within 60 seconds refreshes it with the
  vendor through the core's HTTP token exchange (the single egress, `shogun-core` `mcp_http`), writes
  the refreshed tokenset back, and answers `200 { access_token, token_type: "Bearer", expires_at }`.
  Refreshes of one principal's slot hold a per-(principal, slot) lock and re-read the store under it,
  so concurrent runs never spend a rotating refresh token twice (Slack and Linear rotate; a replayed
  one is revoked by the vendor). A refresh the vendor refuses (`invalid_grant`, a revoked app) is
  `404 connector_not_connected`; one that does not get through while the stored token is still valid
  serves the stored token, and once it has expired `503 connector_unavailable`. A refresh response
  without a refresh token keeps the stored one. A vendor with no hosted OAuth client configured
  (any vendor whose `SHOGUN_CONNECT_*` client is unset) serves a stored token while valid and `503 connect_not_configured` after.
  The grant is never deleted by a failed refresh: an operator's misconfigured client must not
  disconnect everyone.
- **Connect**: `start` generates a PKCE pair and keeps the verifier **sealed in the principal's own
  token store** (slot `connector-connect-flows`, under the principal's key) for ten minutes, keyed
  by a hash of the gateway's opaque state, so a sidecar restart does not lose a flow in progress (at
  most 8 pending per principal; a ninth start drops that principal's oldest, never another's). It
  answers `200 { authorize_url, expires_at }`. `finish` is single use and searches only the calling
  principal's flows (another principal's finish finds nothing: `404 connect_flow_unknown`, no vendor
  call); it exchanges the code with the verifier and the hosted callback URL, checks that every
  scope the connector needs was granted (Google: and that a refresh token came back), and stores
  the tokenset in the principal's store. It answers `200 { connected: true, provider, slot }`;
  neither the code nor the token goes back to the gateway. `400 connect_rejected` when the vendor
  refused the code or the scopes; `503 connect_not_configured` when the provider has no client on
  this sidecar.
- **Notion (E51)**: `start` for `notion` answers Notion's authorize URL
  (`api.notion.com/v1/oauth/authorize`, `owner=user`, the hosted callback, the gateway's state; no
  scopes and no PKCE, which Notion does not take). `finish` exchanges the code at
  `api.notion.com/v1/oauth/token` with the integration's client id and secret as **HTTP Basic
  auth** and a JSON body (`grant_type`, `code`, `redirect_uri`), through the same single egress
  (`TokenExchange::send`, the core's `HttpTokenExchange`). The answer must carry an
  `access_token`, `token_type: bearer` and a `bot_id`; the workspace's name is kept as the account
  label (never returned). A Notion token without `expires_in` does not expire; one with
  `expires_in` and a `refresh_token` is refreshed the same way (Basic auth, `grant_type:
  refresh_token`). The tokenset lands in `notion-tokenset`, which the router's `notion.*` tools
  and the read-sync both read.
- **GitHub (device flow)**: `start` for `github` asks GitHub for a device code
  (`github.com/login/device/code`, the core's egress), seals the device code in the same slot and
  answers `200 { authorize_url: "https://github.com/login/device", expires_at, device: { user_code,
  verification_uri, interval_ms } }`. Each `connect/poll` is at most one call to GitHub's token
  endpoint per interval (a poll inside the interval answers `pending` without calling; `slow_down`
  lengthens it): `200 { status: "pending", device, expires_at }`, then
  `200 { connected: true, provider: "github", slot: "github-tokenset" }`. A denied or expired code
  is `400 connect_rejected` and ends the flow. The steps are the ones the Mac app's device flow uses
  (`shogun_integrations::device_flow`).
- **Status**: `200 { connectors: [{ service, provider, state, connected, connectable, expires_at }] }`
  for every service on the Connections list: `connected` (a valid token, or an expired one the
  sidecar can refresh), `needs_reauth`, `disconnected`, or `coming_soon` (a service the hosted
  deployment does not serve). A free read; no vendor call; never a token or an account identity.
- **Disconnect**: deletes the grant and any pending flow for that provider first, then revokes the
  grant at the vendor through the core's egress where the credentials the core holds allow it
  (E51; `shogun_integrations::hosted_connect::Revocation`). Fail-soft: the local deletion never
  waits on the vendor, and a revocation that fails still leaves the grant deleted.

  | Vendor | Request | What it needs | `vendor_revoke` |
  |---|---|---|---|
  | Google | `POST oauth2.googleapis.com/revoke`, form `token=<refresh token>` (RFC 7009) | the stored grant | `revoked` / `failed` |
  | Slack | `POST slack.com/api/auth.revoke`, form `token=<access token>`; only `ok: true` counts | the stored grant | `revoked` / `failed` |
  | Linear | `POST api.linear.app/oauth/revoke`, `Authorization: Bearer <access token>` | a valid access token: an expired one is refreshed first with the hosted Linear client (the fresh tokenset is not stored) | `revoked` / `failed` |
  | GitHub | `DELETE api.github.com/applications/{client_id}/grant`, Basic `client_id:client_secret`, `{"access_token": …}`, `X-GitHub-Api-Version: 2022-11-28`; removes the grant and every token of it | the GitHub App's client secret (`SHOGUN_CONNECT_GITHUB_CLIENT_SECRET`) and a valid access token (refreshed first when expired) | `revoked` / `failed`; `not_supported` without the secret (no vendor call) |
  | Notion | none: Notion documents no revocation endpoint the core can call for a public integration | — | `not_supported`; the human removes the connection in Notion (*Settings → Connections*) |

  Answers `200 { disconnected: true, provider, slot, removed, vendor_revoke }`; idempotent
  (`removed: false`, `"nothing_stored"`). `provider` is a router provider id or a Connections
  service id (`calendar` or `gcal`). Each revocation writes a `connector.revoke.<provider>.<ok|failed>`
  audit row with the vendor host (`api.github.com` for GitHub), and any refresh it needed a
  `connector.refresh.…` row.
- **Audit**: every connect (and device start), refresh, revocation, disconnect and read-sync fetch
  writes one traceability row in the principal's own store: route `mcp`, purpose
  `connector.<action>.<provider>.<outcome>` (`connector.refresh.gmail.ok`,
  `connector.connect.github.rejected`, …), the vendor host as destination (`local` for the deletion
  itself), zero bytes and the digest of nothing. No token, code, verifier, body or account identity
  can reach it (threat model R10). An audit fault never fails the call.
- Every answer is `Cache-Control: no-store`. Nothing is logged but the server's fixed lines.

## Connecting an account (the hosted connect flow, E50)

Connecting an account is a human's act (the L3 rule: an agent never grants itself a vendor
credential). An agent asks for a link and hands it to its human:

```
POST /v1/connectors/{provider}/connect          Authorization: Bearer <agent key with run>
→ 202 { "status": "pending_human", "provider": "gmail", "slot": "gmail-tokenset",
        "connect_url": "https://<gateway>/v1/connectors/connect/<sealed link>",
        "expires_at": "<ISO, 15 minutes>", "next_action": "ask_human", "message": "…" }
```

`provider` is `gmail`, `calendar`, `slack`, `linear`, `github` or `notion` (`404 unknown_connector`
otherwise). The request starts nothing.

1. The human opens `connect_url` (`GET`): a page saying which account will be connected for which
   principal, and a **Continue** button bound to a CSRF cookie. Nothing happens on opening it.
2. **Continue** (`POST`, same origin, CSRF checked) seals a fresh OAuth state (principal, key,
   provider, expiry, and the hash of a flow cookie it sets for `/v1/connectors/callback`), asks the
   principal's sidecar to start the flow, and redirects (`303`) to the vendor's consent page.
   For **GitHub**, Continue starts the device flow instead and shows the code to type at
   `https://github.com/login/device`; each press of **Check**
   (`POST /v1/connectors/connect/{link}/check`: the page's CSRF pair, the sealed state, and the flow
   cookie of the browser that pressed Continue) is one poll through the sidecar. There is no
   callback.
3. The vendor redirects to `GET /v1/connectors/callback?state&code`. The gateway opens the state,
   requires the flow cookie of the browser that pressed Continue (a consent URL forwarded to someone
   else cannot attach their account to this principal), and hands `state` and `code` to the sidecar's
   `finish`. The page says connected, or why not (declined, expired, used, refused).

The gateway stores nothing: the link and the state are sealed with AES-256-GCM under keys derived
from `CONNECTOR_CONNECT_SECRET`; the code passes through one request to the sidecar and is never
logged (logs name the route template only, never a query string); the tokenset is written only by
the sidecar, into the principal's store. Every page is `no-store`, `no-referrer`, not frameable.

| Provider | Scopes requested | The core's flow |
|---|---|---|
| `gmail` | `https://www.googleapis.com/auth/gmail.modify` (drafts and sends; no delete) | Google authorization code + PKCE, offline access (`oauth::AuthConfig::google`) |
| `calendar` | `https://www.googleapis.com/auth/calendar.events` (list and insert) | the same Google flow |
| `slack` | `chat:write` (user token) | Slack `oauth.v2_user` + PKCE (`AuthConfig::slack` / `slack_public`) |
| `linear` | `read`, `issues:create` (user actor) | Linear PKCE (`AuthConfig::linear`) |
| `github` | the GitHub App's permissions (no scopes) | GitHub's device flow (`shogun_integrations::device_flow`, the same steps as the Mac app's `shogun_core::github_device`), polled once per human Check; refreshed with `SHOGUN_CONNECT_GITHUB_CLIENT_ID` (secret optional; needed to revoke at disconnect) |
| `notion` | the integration's capabilities (no scopes; read content only), on the pages the human shares at Notion's consent page | Notion's authorization code (no PKCE), exchanged with Basic `client_id:client_secret` and a JSON body (`SHOGUN_CONNECT_NOTION_CLIENT_{ID,SECRET}`, both required), E51 |

The sidecar's OAuth clients and the callback URL are `SHOGUN_CONNECT_*` (`deploy/README.md` §3.3):
the callback must be `<gateway public base URL>/v1/connectors/callback`, registered with each vendor.
The connect routes are served only when the gateway has the sidecar vault and
`CONNECTOR_CONNECT_SECRET`.

## The principal's connections (status and disconnect, every face)

An agent reads which accounts its principal has connected, and removes one, the same way on every
face (invariant 4). Status is a free read; disconnect only removes access, so any face may ask
(L1 on the core, `write` on the gateway). Reconnecting is always the human's (the flow above).

| Face | Status | Disconnect |
|---|---|---|
| Core MCP | `connections.list` | `connections.disconnect` `{ "service": "gcal" }` |
| Core REST | `GET /v1/connections` | `POST /v1/connections/disconnect` `{ "service": "gcal" }` |
| CLI | `shogun connections list` | `shogun connections disconnect gcal` |
| Gateway REST (hosted agents) | `GET /v1/connectors` (`read`) | `POST /v1/connectors/{provider}/disconnect` (`write`) |

On a hosted sidecar the core faces answer from the vault (`hosted: true`, each service's `state`,
`connected`, `connectable`, `expires_at`; `app_running`, `gmail` and `composio` are `null`); on the
Mac they keep answering from the desktop's Connections files. The gateway routes call the sidecar's
`status` / `disconnect` and copy the known fields only.

A hosted disconnect answers the same receipt on every face (E51, invariant 4): the face's standard
write fields plus the vault's `disconnected`, `provider`, `slot`, `removed` and `vendor_revoke`
(`MemoryBackend::connections_disconnect`, merged by `connections_api::disconnect_answer`; a receipt
key never overwrites a standard field):

```
MCP   connections.disconnect {"service":"gcal"}  → {"accepted":true,"disconnected":true,"provider":"calendar","slot":"gcal-tokenset","removed":true,"vendor_revoke":"revoked"}
REST  POST /v1/connections/disconnect            → 202 {"tool":"connections.disconnect","level":"L1","accepted":true, …the same five fields}
CLI   shogun connections disconnect gcal         → prints the REST body
```

On the Mac the disconnect is queued for the desktop, which alone deletes the Keychain credential,
so the answer stays `{accepted: true}` and `connections.list` shows `disconnect_pending`.

## Guarantees the router keeps

- The access token exists only inside `src/transport.ts`, between the token response and the vendor
  request. It is not cached across runs, not returned to the engine, not in a run record, a
  settlement, an idempotency row, a log line, a metric or a span (`test/hardening.test.ts` drives a
  sentinel token through and asserts none of those contain it).
- The caller's key goes to apps/api in `X-Caller-Authorization` and never to the vendor, and apps/api
  never forwards it to the sidecar.
- With `ROUTER_WALLET_URL` set, the Worker env variables the connectors name
  (`GOOGLE_OAUTH_ACCESS_TOKEN`, `GITHUB_TOKEN`, `LINEAR_OAUTH_TOKEN`, `SLACK_TOKEN`, `NOTION_TOKEN`) are never read,
  even if set, and `ROUTER_DEV_SHARED_CREDENTIALS` is ignored (logged
  `dev_shared_credentials_ignored` at isolate start).

## Development

Without `ROUTER_WALLET_URL`, `ROUTER_DEV_SHARED_CREDENTIALS=1` makes the connectors read one shared
token from the env variable each names. That is one account for every caller and is for a developer's
own machine only. Without the flag (and without the wallet URL) the connectors answer
`503 provider_unavailable` (`try_alternative`) before any network call.

## Why the sidecar and not a gateway table (E50 decision)

E48 left an injected `KeyWrapper` / `ConnectorRefresher` seam in apps/api over a Postgres table of
KMS-sealed rows (`connector_tokens`), with no runtime able to fill it: the KMS clients and every
vendor client live in the Rust core behind its single egress (invariant 9), and a second store of
the same credential outside the principal's sidecar would split ownership of principal data
(invariant 1). E50 removed that path (the TypeScript `KmsSealer`, the row store, the refresher seam
and `apps/api/sql/connector-tokens*.sql`) instead of keeping it for tests: the sidecar already holds
each principal's token store, sealed under the principal's KMS-backed key, and the approval executor
already sends from it. A database that applied the old `connector_tokens` table may drop it; nothing
reads or writes it.

## Before the hosted catalog lists these tools

The code path is complete (including GitHub's device flow, status and disconnect on every face, the
audit rows and flows that survive a restart); what remains is operational, and the pre-launch
checklist (`docs/deploy/production.md` §9) keeps the first-party tools out of the hosted catalog
until it is done:

- **OAuth clients.** A Google "Web application" client (verification of the restricted
  `gmail.modify` scope, with its security assessment, before external users), a Slack app with user
  scopes and the hosted redirect, a Linear OAuth app, a GitHub App with the device flow enabled
  (and its client secret, to revoke grants at disconnect), a Notion public integration with
  read-content capability and the hosted redirect; their ids and secrets in the sidecar's
  `SHOGUN_CONNECT_*`, the callback registered with each redirect vendor.
- **The Worker → sidecar path** (E34) live in each region, with `SHOGUN_SIDECAR_ADMIN_TOKEN` and
  `CONNECTOR_CONNECT_SECRET` on the gateway, and the sidecar's egress open to the vendors' token
  and revocation endpoints (`oauth2.googleapis.com`, `slack.com`, `api.linear.app`, `github.com`,
  `api.github.com`, `api.notion.com`; and, for the read-sync, `gmail.googleapis.com`,
  `www.googleapis.com`; `deploy/README.md` §5.4).
- **Staging checks against the real vendors** that the fakes cannot prove: Notion's token response
  shape for a public integration (`bot_id`, and whether it now carries `expires_in` /
  `refresh_token`), Linear's bearer revocation and GitHub's grant deletion answering 2xx, and that
  each revoked grant then fails at the vendor.
