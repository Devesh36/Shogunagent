# Threat model

Plan E38. Status: first full pass, 2026-10-25, against `main` at the E26 merge (ae8d067) plus the
E37/E38 changes. Scope: the hosted gateway (`apps/api`), the router (`apps/router`), the sidecar
(`crates/`, `shogun-api --multi`), and the enterprise lane (SSO, SCIM, customer KMS). The ShogunAI
desktop app is out of scope except where it shares code. Review this document whenever a trust
boundary moves (a new face, a new outbound call, a new secret).

Method: assets first, then each trust boundary with STRIDE (Spoofing, Tampering, Repudiation,
Information disclosure, Denial of service, Elevation of privilege). Every mitigation points at the
code that enforces it; a mitigation without a pointer is a hope and is listed under open risks.

## 1. Assets

| Asset | Where it lives | Why it matters |
|---|---|---|
| A1 Principal state and evidence (events, people, projects, commitments, open loops, quotes) | One SQLCipher file per principal (`crates/shogun-core/src/tenancy/store.rs`) | The product. A leak across principals breaks invariant 6; a leak to logs breaks invariant 2 |
| A2 Request and response bodies in transit | gateway, router, sidecar memory only | Must never persist outside the principal's store (invariant 2) |
| A3 Agent keys (`sa_…`), OAuth tokens (`sat_…`), licence tokens | Hashed in the agent store (`apps/api/src/agent-keys.ts`, PBKDF2); licence tokens are signed, not stored | Spend authority and data access for a principal |
| A4 Store keys | KMS-wrapped per principal (`tenancy/key_provider*`), cached unwrapped for `SHOGUN_KMS_KEY_CACHE_SECS` | Decrypt A1 |
| A5 Operator secrets: provider keys, Stripe, `SHOGUN_APPROVAL_SIGNING_KEY`, `SHOGUN_ADMIN_TOKEN`, `METRICS_TOKEN`, `ROUTER_METRICS_TOKEN`, `OTEL_EXPORTER_OTLP_HEADERS` | Platform secret stores; never in the repository | Spend on company keys, forged approvals, operator access |
| A6 Wallets, ledgers, receipts, payouts | Postgres (`apps/api/sql/`) | Money; a double charge or a free run |
| A7 Approval decisions (L3) | Sidecar approval queue; decision tokens `sat1.…` | The human gate on external sends (invariant 3) |
| A8 Tenant directory, SSO configuration, SCIM tokens | Enterprise store (`apps/api/src/enterprise/store.ts`) | Who is an admin of a tenant |
| A9 Operational telemetry (logs, `/metrics`, OTLP spans) | Workers Logs, the operator's collector | Must stay content-free; also reconnaissance for an attacker |
| A10 Connector grants (vendor access and refresh tokens for Gmail, Calendar, Slack, Linear, GitHub; E50) | The principal's sidecar token store (`SHOGUN_TOKEN_STORE_DIR`), sealed per principal under a key derived from its KMS-unwrapped store key (`tenancy/credentials.rs`); an access token transits the gateway and the router for one vendor request | Acting as the human at the vendor: read mail, send, post |

## 2. Trust boundaries

```
agent / MCP client ──(1)──▶ gateway (apps/api, Workers) ──(2)──▶ sidecar (shogun-api, loopback)
        │                        │   │                                   │
        │                        │   └──(5)──▶ Stripe / x402 facilitator  └──(7)──▶ KMS
        └──────(3)──▶ router (apps/router) ──(4)──▶ tool providers (Monid, first-party face)
human approver ──(6)──▶ approval page / Slack / webhooks ──▶ gateway ──▶ sidecar
IdP (customer) ◀──(8)── SSO / SCIM ──▶ gateway (enterprise lane)
operator ──(9)──▶ /metrics, admin routes, OTLP collector
human ──(10)──▶ connect page / callback (gateway) ──▶ sidecar connector routes ──▶ vendor OAuth token endpoints
router ──(10)──▶ gateway connector-token route ──▶ sidecar connector routes
```

## 3. STRIDE per boundary

### (1) Agent → gateway

| Threat | Mitigation (code) |
|---|---|
| S: stolen or guessed agent key | Keys are 256-bit random, stored PBKDF2-hashed, compared in constant time (`agent-keys.ts`); registration costs proof of work and is IP-limited (`pow.ts`, `agent-app.ts`) |
| S: forged licence token | Ed25519 verification against the release key pair with rotation (`auth.ts`, `licensePublicKey`) |
| T: billing a write as a free read | The gateway classifies from `Mcp-Method`/`Mcp-Name` (`pricing.ts`); the sidecar refuses a body whose method or tool differs from the headers (`crates/shogun-mcp/src/mcp_http.rs`), so the header the gateway billed is what ran (property-tested, `jsonrpc_prop_tests.rs`) |
| T: replayed write | Every write carries an idempotency key; receipts settle once (`metering.ts`, `agent-store.ts`) |
| R: disputed charge | Receipts with `X-Receipt-Id`, usage headers, the ledger (`billing/ledger.ts`) |
| I: bodies or keys in logs/metrics | The redacting logger is the first middleware and sees no body (`logging.ts`); metrics labels are closed sets (`observability.ts`), guarded by `scripts/check-log-hygiene.py`; Workers invocation logs are off (`wrangler.jsonc`) |
| D: request floods, huge bodies | Body limits before parsing (`app.ts` `bodyLimit`), per-payer rate limits (`ratelimit.ts`), spend caps and the global breaker (`caps.ts`, `spend.ts`), 402 before any upstream call |
| E: read-scoped key calling a write | `SCOPE_BY_UNIT` (`metering.ts`); L3 tools never run without a human decision (sidecar `scope.rs`) |

### (2) Gateway → sidecar

| Threat | Mitigation (code) |
|---|---|
| S: another process answering as the sidecar | Loopback only, same network namespace in compose/Helm. The server build binds exactly its port (`bind_exact`) and refuses to start when it is taken (R3, closed in E34) |
| T: gateway altering bodies | Bodies are relayed, not parsed (invariant 1); `POST /v1/handoff` too since E35 (the core assembles the object, `handoff.ts` only relays) |
| I: cross-principal read | One SQLCipher file per principal; the bearer resolves to exactly one principal (`tenancy/resolver.rs`, `tenancy/multi.rs`); `crates/shogun-core/tests/invariant_tenant.rs` proves zero leaked rows |
| I: DNS rebinding against the loopback API | Host header must be loopback (`host_is_loopback`, `crates/shogun-mcp/src/server.rs`) |
| D: one principal starving others | LRU of open stores (`SHOGUN_MULTI_MAX_OPEN`); body cap 8 MiB on `/mcp`. Writes to one principal serialize on its file (see the load numbers in `docs/slo.md`) |
| E: agent bearer deciding an approval | Decide routes take the operator token or a signed decision token; an agent bearer is `403 human_only` (`docs/memory-api-mcp.md`, `server_governance.rs`) |

### (3) Agent → router, (4) router → providers

| Threat | Mitigation (code) |
|---|---|
| E: an agent running an external send without approval | The operation table (`apps/router/src/scope.ts`; `shogun-integrations/src/scope.rs`) denies unknown operations; `ExternalSend` is always L3 (invariant 3) |
| I: provider keys reaching the engine or a connector | Keys are read only in `apps/router/src/transport.ts` and injected at send time; `Secret` serialises as `[redacted]` (`secret.ts`) |
| I: provider responses echoing data into logs | Transport returns status and body only, never headers; the logger takes typed fields (`logging.ts`), sentinel-tested (`test/zero-retention.test.ts`) |
| T: self-preferencing | First-party ranked by the published score (invariant 13, `discover.ts`) |
| D: slow provider | Per-call timeout (`createFetchTransport`) |
| R: disputed run | Run records without bodies (`runlog.ts`), settlements to the payout ledger (`settlement.ts`) |

### (5) Gateway → Stripe / x402

| Threat | Mitigation (code) |
|---|---|
| S: forged Stripe webhook | Signature verification (`billing/stripe.ts`) |
| T: replayed x402 payment | Replay store and settle-once (`billing/x402.ts`) |
| R/T: double credit of a top-up | Idempotent credit by session id (`topup_duplicate` event) |

### (6) Approvals

| Threat | Mitigation (code) |
|---|---|
| S: forged approval link | `sat1` HMAC-SHA256 tokens with expiry and nonce, verified by the gateway and again by the sidecar, which spends the nonce (`approvals/token.ts`, `crates/shogun-mcp/src/remote_approvals.rs`) |
| T: tampered Slack interaction | Slack request signature (`approvals/slack.ts`) |
| I: approval summary leaking to Slack/webhooks | Notifications carry the link, not the draft body; pending L3 bodies are sealed at rest (ShogunAI audit M2) |
| E: decision token in logs | The `/approve/:token` path is templated before logging and metrics (`logging.ts` `routeTemplate`) |

### (7) Sidecar → KMS

| Threat | Mitigation (code) |
|---|---|
| I: key material in logs | KMS calls go through the single egress and log provider/op/host/size/status only (`deploy/README.md` §3, `llm/transport.rs`) |
| D: KMS outage | Fail closed: the store does not open (`503 memory_unavailable`); no fallback key |
| T: swapped wrapped-key file | A missing wrapped key is refused, never re-keyed; the encryption context binds the principal |

### (8) Enterprise: SSO and SCIM

| Threat | Mitigation (code) |
|---|---|
| S: forged SAML assertion | Assertion must carry the only enveloped signature, one reference to its own ID, RSA-SHA256/512 and SHA-256/512 only, verified against configured certificates (KeyInfo ignored), values read only from the signed bytes, DOCTYPE refused, replay cache, SAML 2.0 only, `IssueInstant` bounded to the sign-in flow, XML Encryption refused (E39) (`enterprise/saml.ts`). Fuzzed: `apps/api/test/saml-fuzz.test.ts` |
| S: forged OIDC ID token | Issuer, audience, nonce, expiry and algorithm checks (`enterprise/oidc.ts`) |
| I/S: SSRF through IdP or CIMD URLs | `idpFetch` / CIMD guard: https only, public addresses only, no redirects, size and time caps (`enterprise/fetch-guard.ts`, `oauth/cimd.ts`) |
| E: SCIM token used across tenants | One SCIM token per tenant, hashed, rotatable (`enterprise/scim.ts`, `store.ts`) |
| I: SCIM bodies in logs | Fixed event words only (`logging.ts` `LogVars`) |
| D: SCIM delete removing data immediately | Grace period before purge (`SCIM_DELETE_GRACE_DAYS`) |

### (9) Operator surfaces

| Threat | Mitigation (code) |
|---|---|
| I: `/metrics` as reconnaissance | 404 unless `METRICS_TOKEN` / `ROUTER_METRICS_TOKEN` is set; bearer compared as SHA-256 digests in constant time (`apps/api/src/app.ts`, `apps/router/src/transport.ts`) |
| I: content in metrics or spans | Closed-set labels only (route template, method, status, `Mcp-Method`, `Mcp-Name`); `scripts/check-log-hygiene.py` refuses new labels, body reads and header reads in `observability.ts` |
| I: OTLP credentials leaking | `OTEL_EXPORTER_OTLP_HEADERS` is held in config (apps/api) or read at send time (router `transport.ts`); https required except a loopback collector; URL userinfo refused |
| D: collector outage slowing requests | Spans are buffered (bounded) and posted off the request path; failures drop and count (`observability.ts`) |

### (10) Connector credentials (E50)

The router's first-party tools need the calling principal's vendor token; a human connects the
account. Contract: `docs/api/connector-tokens.md`.

| Threat | Mitigation (code) |
|---|---|
| S: the router (or anyone with `ROUTER_SETTLEMENT_TOKEN`) asking for another principal's token | The principal comes from the caller's own key in `X-Caller-Authorization` (hash lookup, `run` scope), never the body (`apps/api/src/connector-tokens.ts`); the gateway names it to the sidecar only by that principal's derived `sgw1` bearer, which the sidecar verifies (`server_governance.rs` `connector_principal`); unknown body fields are `400` on both hops |
| S/E: an agent bearer reaching the sidecar's connector routes | They need the operator's admin credential *and* the principal's bearer; an agent bearer alone is `403 admin_required` (`crates/shogun-core/tests/connector_vault.rs`) |
| E: an agent connecting an account by itself (granting itself a vendor credential) | The agent route only returns a link (`202`, `next_action: ask_human`, `connector-connect.ts`); Continue needs the page's CSRF cookie and same origin; the callback needs the flow cookie of the browser that pressed Continue, so a consent URL forwarded to another person cannot bind their account to this principal (`apps/api/test/connector-connect.test.ts`) |
| T: a forged or replayed callback | The state is sealed (AES-256-GCM, `CONNECTOR_CONNECT_SECRET`, purpose-bound), expires in 10 minutes, and the sidecar's pending flow is single use and bound to the principal that started it (`hosted_connect.rs`); PKCE S256 with the verifier only in the sidecar's memory |
| I: tokens, codes or refresh tokens leaking | The refresh token never leaves the sidecar; the access token crosses the gateway in one `no-store` response and the router drops it after one vendor request; the code passes through one request; logs carry route templates and fixed words only (`logging.ts` `routeTemplate` never includes a query); `.expose(` only in the allowlisted `hosted_connect.rs` / `connector_vault.rs`; the gateway stores nothing (the E48 `connector_tokens` table was removed) |
| I: a grant read across principals | Grants are keyed principal × slot in the file-sealed store, sealed with the principal's own key; `hosted_connect` tests and `connector_vault.rs` prove one principal's bearer reads only its own slot |
| T/D: two runs burning a rotating refresh token | A per-(principal, slot) lock around read-refresh-write with a re-read under the lock (`hosted_connect.rs`, concurrency test) |
| D: vendor or sidecar outage | `503` with `Retry-After: 5`, the run settles at $0; a still-valid token is served when a refresh does not get through; a failed refresh never deletes the grant |
| R: who connected what | The sidecar logs nothing per call today; see R10 |

## 4. Mitigations mapped to invariant guards

| Guard | Holds |
|---|---|
| `scripts/check-http-egress.py` | Only `shogun-core` has an HTTP client (invariant 9) |
| `scripts/check-secret-exposure.py` | `Secret::expose` only in allowlisted files (invariant 7) |
| `scripts/check-log-hygiene.py` | No user content in Rust log lines; closed-set ops telemetry labels (E37) |
| `scripts/check-media-writes.py` | No image/audio bytes to files |
| `scripts/check-migrations.py` | Migrations additive and frozen (invariant 8) |
| `crates/shogun-core/tests/invariant_tenant.rs` | Zero rows leak between principals (invariant 6) |
| `apps/router/test/zero-retention.test.ts`, `apps/api/test/logging.test.ts`, `apps/api/test/observability.test.ts` | Sentinel strings never reach a log, metric or span |
| `.github/workflows/audit.yml` | Daily `cargo audit` and `pnpm audit` |

## 5. Dependency and secret scanning

- Dependencies: `audit.yml` runs `cargo audit` (ignores justified in `.cargo/audit.toml`) and
  `pnpm audit` daily and on lockfile changes. Not re-run for this review (it needs the advisory
  feed; this pass made no external calls).
- Secrets: a local pattern scan (Anthropic, Stripe live, AWS, GitHub, Slack, Google API keys,
  PEM private keys) over tracked files found only synthetic fixtures in tests and benches
  (`crates/pii-bench/src/corpus.rs`, redaction tests). Recommendation: add a secret scanner
  (e.g. gitleaks) to CI with an allowlist for those fixtures — open risk R8.

## 6. Open risks (ranked)

| # | Risk | Severity | Next step |
|---|---|---|---|
| R1 | **Rate limiters and in-flight limiters fail open** when the shared store is unavailable (`ratelimit_unavailable (failing open)` in `boot.ts`). Spend caps and the breaker still hold, but request-rate protection vanishes during a Postgres incident. | Medium | Decide per route: fail closed for registration and the agent `/mcp`, keep open for the device Batch lane |
| R2 | **Router stores were per-isolate stubs.** Metering and settlement now go through apps/api's wallet with a durable Postgres outbox (E32/E33); run idempotency is a Postgres store keyed by caller and key hash (E48). The health and personal stores remain per-isolate. | Medium (health/personal only) | Mitigated by E32/E33/E48; the router still needs its Hyperdrive binding before a route is bound (runbook §3 step 8) |
| R3 | ~~**Sidecar port fallback**~~ **Closed (E34).** `shogun-api` in server builds (`--multi`, and single-principal without the `desktop` feature) binds exactly `SHOGUN_API_PORT` (`shogun_mcp::server::bind_exact`) and exits with a message naming the port when it is taken; only the Mac app's build (feature `desktop`) keeps the ephemeral fallback, recorded in `api.port` for the CLI. A squatter on 7464 now makes the sidecar fail to start instead of receiving the gateway's bearers. | — | Optional hardening: authenticate the gateway→sidecar hop (mTLS) |
| R4 | ~~**`SHOGUN_PRINCIPAL_TOKENS` holds principal bearers in clear**~~ **Mitigated (E34).** With `SHOGUN_SIDECAR_BEARER_SECRET` on both sides, the gateway presents a derived bearer `sgw1.<principal>.<key_id>.<mac>` (HMAC-SHA256) and the sidecar verifies it in constant time (`tenancy/derived_bearer.rs`); no principal bearer is listed and `SHOGUN_PRINCIPAL_TOKENS` is not needed. The one shared secret remains in both environments (from the secret store / KMS, held as `Secret`). | Low | Leave `SHOGUN_PRINCIPAL_TOKENS` empty in production; rotate the shared secret with the gateway |
| R5 | ~~**Invariant 1 exception**: `apps/api/src/handoff.ts` parses recall bodies.~~ **Resolved (E35)**: the core assembles the handoff (`handoff.create` / `handoff.accept`, `crates/shogun-mcp/src/handoff_api.rs`) and `handoff.ts` relays the request and the answer as streams through the sidecar relay without reading either. | — | — |
| R6 | **xml-crypto as a SAML trust anchor**: a large, complex library; its silent skip of an `Algorithm`-less transform is now refused by our own check, but future versions may change verification semantics. | Medium | Pin (already exact), keep the fuzz test, review release notes on every bump |
| R7 | **Single-principal write throughput**: concurrent ingest to one principal serialises on its SQLCipher file (p95 ~200 ms at 8 concurrent writers locally, `docs/slo.md`). A noisy agent can raise latency for every agent of that principal. | Low | Per-principal write queue with back-pressure (429 with `Retry-After`) |
| R8 | Secret scanning in CI. | Low | Mitigated (E48): `secret-scan.yml` runs gitleaks with `.gitleaks.toml`; the first CI run may need allowlist entries for test values |
| R9 | Fuzzing: seeded property tests on stable; libFuzzer targets in `fuzz/` need nightly. | Low | Mitigated (E48): `fuzz-nightly.yml` runs both targets time-boxed on a schedule |
| R10 | **Connector credentials (E50): no per-principal audit of connects and refreshes, and the pending connect flows live in one sidecar's memory.** A connect or a refresh writes no row in the principal's audit / traceability log, and vendor token-endpoint calls are not on the principal's traceability screen; a sidecar restart drops flows in progress (the human starts again). The sidecar also needs egress to the vendors' token endpoints, which the default NetworkPolicy does not open. | Low | Record `connector.connected` / `connector.refreshed` (ids, slot, time; no token) in the principal's audit log; open the egress per vendor with `egress.extraRules` (`deploy/README.md` §3.3) |

## 7. Fuzzing findings (E38)

| Parser | Finding | Fix |
|---|---|---|
| MCP JSON-RPC (stdio) | A notification method sent **with** an `id` (`notifications/initialized`, `notifications/cancelled`) got no response: a request, by JSON-RPC 2.0, so the client waited forever | Answer it with an empty result, as `ping` (`crates/shogun-mcp/src/mcp.rs`) |
| MCP Streamable HTTP | A refused request echoed the body's `method` / `params.name` into the reply's `Mcp-Method` / `Mcp-Name` headers verbatim. A newline made the axum response builder fail (500 instead of 400); an 8 MiB method was copied into a header | `metering_header_value`: visible ASCII, at most 128 characters, else `invalid` (`crates/shogun-mcp/src/mcp_http.rs`) |
| SAML Response | xml-crypto 6 silently skips a reference `Transform` that has no `Algorithm` (deprecation warning surfaced by the mutation test). Not exploitable as found (the digest no longer matches), but the verifier accepted a transform list the signer never declared | Every `Transform` must name an accepted algorithm, else `saml_algorithm_refused` (`apps/api/src/enterprise/saml.ts`) |
| Export v1 import | None: 400 seeded malformed packages imported or were refused cleanly, every record accounted for, no row without provenance | — |
