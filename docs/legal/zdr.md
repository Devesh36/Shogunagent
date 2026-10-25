# ShogunAgent Zero-Retention Commitment

> **DRAFT — for legal review. Not in force.** This draft describes what the code in this repository actually does as of 2026-10-25. Counsel must review it and the provider rows marked `[TBD]` must be confirmed in writing with each provider before it is offered to any customer. Until then, `/legal/zdr` serves this text with the same banner.

Last updated: 2026-10-25 (draft). This commitment is part of the Data Processing Addendum (`/legal/dpa`) for hosted ShogunAgent. It says exactly where your content is **not** kept, what metadata **is** kept, and for how long.

## 1. The commitment

Your content — what your agents ingest, the state derived from it, and the bodies of the requests and responses that pass through the hosted service — is stored **only in your principal's encrypted store**. No other part of the hosted service writes it to disk, a database, a log, a metric, a trace, an error report or a cache. Streams are relayed as they arrive, not buffered whole.

This is enforced in code, not only in policy: the gateway's only console writer (`apps/api/src/logging.ts`) accepts nothing but fixed words, route templates, status codes, durations, counters and hashed ids, and has tests that fail if a body, token or identifier reaches a log line.

## 2. Paths that retain nothing

| Path | What passes through | What is kept |
|---|---|---|
| `/mcp` (the metered MCP proxy) | JSON-RPC request and response bodies, and streams | Nothing of the body. The call is classified from headers, never from the body. A receipt (method, tool name, unit, quantity, cost, time) is kept for billing. |
| Router runs (`/v1/router/run`) | Tool inputs and outputs | Nothing of the body. A receipt as above; provider credentials are injected at the transport layer and never reach the router engine. |
| `POST /v1/handoff` | The request and the state-of-work object the core builds from one recall | Nothing; both are relayed to and from the core as streams, never parsed or kept. |
| Remote approvals (`/approve/{token}`, Slack, webhooks) | A redacted summary fetched from your store | Nothing; the summary is rendered or delivered and dropped. Webhook endpoint URLs and event names you register are kept until you delete them. |
| Tenant audit export (`GET /v1/tenant/audit/export`) | Each member's audit records | Nothing; bytes are streamed from the store to the admin and never parsed beyond the per-member frame. |
| SSO sign-in (OIDC ID tokens, SAML assertions) | Identity claims from your IdP | Nothing of the token or assertion. The admin's IdP subject is bound once; a SAML assertion id is kept only as a SHA-256 hash until the assertion expires (minutes), to refuse replays. |
| SCIM provisioning | User and group resources from your IdP | Only the attributes the protocol needs: userName, name, e-mail addresses, external id, active flag, group names and members. Nothing else in the request (for example passwords or enterprise-extension attributes) is kept. Deleted users keep a tombstone of ids and times. |
| Model calls (extraction, consolidation, AI search) | The chunk needed for one call | Nothing on our side; provider-side retention is in section 4. |

## 3. Metadata we keep, and for how long

| Metadata | Contents | Kept for |
|---|---|---|
| Request log lines | Method, route template (ids collapsed), status, duration, a fixed event word, a 12-character hash of the key id | `[TBD: the hosting provider's log retention, set short in its dashboard]` |
| Status window | Counts and latency percentiles of recent requests, in memory | A rolling window inside one running instance; never written out |
| Receipts | Method, tool name, unit, quantity, cost, time, ids | 400 days (a year of statements plus a dispute window); unsettled receipts until settled |
| Statements and ledger entries | Totals, amounts, ids, a checksum | `[TBD: statutory accounting retention]` |
| API keys | Salted PBKDF2-SHA256 hash and a 10-character lookup prefix | Until the principal is deleted |
| OAuth codes and tokens | SHA-256 hashes, client id URL, scopes, times | Codes: 60 seconds plus one day; tokens: until expiry plus one day |
| Registration proof-of-work | SHA-256 of the spent challenge | Five minutes (the challenge's lifetime) |
| Per-IP rate limiting | SHA-256 of the client IP | About ten minutes after the one-hour window closes |
| SAML replay cache | SHA-256 of tenant and assertion id | Until the assertion expires |
| Tenant directory | Tenant name, region, plan, SSO configuration (OIDC client secret sealed with AES-256-GCM), SCIM bearer as SHA-256, admin e-mail addresses, SCIM attributes | While the tenant exists; SCIM attributes are cleared when a user is deleted |

None of this metadata contains the content of your state or of a request or response body.

## 4. Model providers

We send a provider only the chunk needed for one call, never train on it and never let the provider train on it. Provider-side zero retention must be in place for each provider before it carries customer traffic:

| Provider | Operations | Zero-retention status |
|---|---|---|
| `[TBD: OpenAI]` | `[TBD]` | `[TBD: ZDR approval reference; store:false on every call]` |
| `[TBD: Anthropic]` | `[TBD]` | `[TBD: zero-retention agreement reference]` |
| `[TBD: Azure OpenAI]` | `[TBD]` | `[TBD: modified abuse monitoring exemption; no customer traffic until approved]` |
| `[TBD: others]` | `[TBD]` | `[TBD]` |

Tools run through the router are operated by third parties Customer chooses; their retention is theirs and is shown on each tool's listing.

## 5. Exceptions

- **Your store.** By design it keeps what you ingest, until you delete it. Deleting a principal deletes its store (after the SCIM grace period when deletion comes from your IdP).
- **Legal holds.** If the law compels us to preserve specific content, we will tell Customer unless the law forbids it.
- **Abuse.** We do not inspect bodies to detect abuse; rate limits, caps and per-key revocation work on metadata alone.

## 6. Verification

Customer may review the open-source gateway and engine in this repository, the log hygiene tests, and on request `[TBD: third-party audit reports]`. A change that would retain content on any path in section 2 is a material change to this commitment and needs `[TBD: notice period]` notice.
