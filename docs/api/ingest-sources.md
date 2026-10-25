# Ingest sources beyond the Mac

How work reaches a principal's memory when it does not come from the ShogunAI Mac app's capture:
provider webhooks through the hosted gateway's **ingest hooks**, and the **browser extension**
(specification only). Plan: E44. Implementation: `apps/api/src/ingest-hooks/` (gateway),
`crates/shogun-mcp/src/webhook_ingest.rs` (core: `memory.ingest_webhook`). Tests:
`apps/api/test/ingest-hooks.test.ts`, `webhook_ingest::tests`, `rest_tests.rs`, the CLI parser.

Every source ends in the same place: the principal's own store, through the `memory.ingest`
append path (instruction-shaped text refused, secrets masked, duplicates collapsed, the hourly
review deciding what becomes state). Text only. **No source stores an image, a screenshot, audio or
an attachment**, and none is fetched to be read here.

## 1. The rules every source follows

| rule | how |
|---|---|
| Bodies stay in the principal's store (invariant 2) | The gateway verifies, relays and forgets. A delivery body is held in memory only until its signature verifies (at most 1 MiB), sent to the core base64-encoded inside the `memory.ingest_webhook` call, and dropped. It is never written to Postgres, a log line, a metric, a trace or an error report, and the provider is never answered with anything from it or from the core's reply (providers keep response bodies in their delivery logs). |
| The core reads payloads (invariant 1) | `apps/api` parses nothing beyond what verification needs; the three bounded exceptions are listed in §3. Normalizing a payload into events is `webhook_ingest::normalize`, in Rust. |
| Provenance on every row (invariant 5) | Each stored row's `actor` is the hook (`agent:<hook id>`, from the derived sidecar bearer), its `kind` is `<provider>.<event>` (`github.issue`, `slack.message`, …), its title names the provider and the object, and its text ends with `(via <Provider> webhook, delivery <id>, <url>)`. The receipt's `metadata` carries `provider`, `delivery_id`, `url`, `event`, `hook_id`. |
| Secrets never in clear (invariant 7) | `INGEST_HOOKS_SECRET` derives or seals every hook secret (§2.2). The table holds generation numbers, AES-256-GCM envelopes bound to the hook id, and an HMAC of a Gmail address. |
| Idempotency | The idempotency key of a delivery is `hook:<provider>:<delivery id>`. The provider's own delivery id is used when it sends one (`X-GitHub-Delivery`, `Linear-Delivery`, Calendar's channel id + `X-Goog-Message-Number`, Pub/Sub's `messageId`); Slack and Notion send none in a header, so the id is `sha256-<hex of the body>` (their retries resend the same bytes). A redelivery replays and stores nothing. A replay under a forged delivery header (the header is not signed by GitHub or Linear) collapses onto the stored row as an exact duplicate and settles at 0 events. |
| Billing | Metered as **ingest** per stored event (docs/api/billing.md §"Ingest hooks"). Handshakes are free. A bad signature costs nothing, reaches no store, and spends the per-IP and per-hook failure budget (30 per 10 minutes, then `429`). |
| Three faces (invariant 4) | `memory.ingest_webhook` is an MCP tool, `POST /v1/memory/ingest-webhook` on the core's REST face and `shogun ingest-webhook <json>` on the CLI, all L1 and all on one parser. Hook management is the gateway's REST face (hooks exist only where there is a public URL). |

## 2. Provider webhooks (hosted gateway)

### 2.1 Register a hook

The principal's own agent key (`write` scope) registers one hook per provider endpoint:

```sh
curl -s -X POST "$BASE/v1/ingest-hooks" -H "Authorization: Bearer $KEY" \
  -H "Idempotency-Key: github-acme-web" -H "content-type: application/json" \
  -d '{"provider":"github"}'
# → 201 {"hook_id":"hk_…","provider":"github","url":"$BASE/v1/ingest-hooks/github/hk_…",
#        "status":"active","secret":"…","setup":"…"}
```

| provider | body | what the answer shows once | where to point the provider |
|---|---|---|---|
| `github` | — | `secret` (derived; a replay under the same `Idempotency-Key` shows it again) | repository or organization → Settings → Webhooks: Payload URL = `url`, content type `application/json`, Secret = `secret` |
| `linear` | `signing_secret` (the one Linear shows for the webhook) | nothing (sealed) | Linear → Settings → API → Webhooks: URL = `url` |
| `slack` | `signing_secret` (the Slack app's, Basic Information) | nothing (sealed) | the Slack app → Event Subscriptions: Request URL = `url`; subscribe to `message.channels`, `message.im`, `app_mention` |
| `notion` | — | nothing yet: `status` is `awaiting_verification` | the Notion integration → Webhooks → create a subscription with `url`, then `GET /v1/ingest-hooks/{id}` and paste its `verification_token` into Notion's Verify form |
| `google_calendar` | — | `channel_id` (= the hook id) and `channel_token` | `events.watch` with `id = channel_id`, `type = web_hook`, `address = url`, `token = channel_token` (the core does this once Calendar is connected, §2.5) |
| `gmail` | `mailbox` (the Gmail address) | nothing; the address is kept only as an HMAC | `users.watch` on the operator's Pub/Sub topic; the operator's push subscription points at `$BASE/v1/ingest-hooks/gmail` with authentication on |

`GET /v1/ingest-hooks` lists the principal's hooks without secrets (`read`, free).
`POST /v1/ingest-hooks/{id}/rotate` issues a new secret (`github`, `google_calendar`: shown once;
`linear`, `slack`: send the provider's new `signing_secret`; `notion`: back to
`awaiting_verification`); the previous one keeps verifying for **24 hours**.
`DELETE /v1/ingest-hooks/{id}` removes a hook and erases its sealed secret and mailbox route.
At most 20 hooks per principal; one live hook per Gmail mailbox (`409 mailbox_taken`).

### 2.2 Secrets

- **Derived** (GitHub secret, Calendar channel token): `base64url(HMAC-SHA256(K_derive, "<hook id>\n<generation>"))`
  where `K_derive = HMAC-SHA256(INGEST_HOOKS_SECRET, "shogun/ingest-hook/derive/v1")`. Only the
  generation is stored; rotation bumps it.
- **Sealed** (Linear and Slack signing secrets, Notion's verification token): AES-256-GCM under
  `HMAC-SHA256(INGEST_HOOKS_SECRET, "shogun/ingest-hook/seal/v1")`, the hook id as associated data,
  stored as `v1.<base64url(nonce‖ciphertext‖tag)>`. A row copied to another hook does not open.
- **Gmail route**: `hex(HMAC-SHA256(K_route, lowercase(trim(address))))`.

Changing `INGEST_HOOKS_SECRET` invalidates every hook's secret; rotate each hook afterwards.

### 2.3 Verification, per provider

All comparisons are constant-time over fixed-length digests; a malformed header costs the same
work as a wrong one. Every candidate secret (current, and previous inside the grace period) is
checked without an early exit.

| provider | route | verified by | handshake (free, not relayed) | delivery id |
|---|---|---|---|---|
| GitHub | `POST /v1/ingest-hooks/github/{hook}` | `X-Hub-Signature-256: sha256=<hex HMAC-SHA256(secret, raw body)>` | `X-GitHub-Event: ping` | `X-GitHub-Delivery` |
| Linear | `POST /v1/ingest-hooks/linear/{hook}` | `Linear-Signature: <hex HMAC-SHA256(signing secret, raw body)>` | — | `Linear-Delivery` |
| Slack | `POST /v1/ingest-hooks/slack/{hook}` | `X-Slack-Signature: v0=<hex HMAC-SHA256(signing secret, "v0:" + X-Slack-Request-Timestamp + ":" + raw body)>`, timestamp within ±300 s | `url_verification` (signed): `{"challenge": …}` echoed | body digest |
| Notion | `POST /v1/ingest-hooks/notion/{hook}` | `X-Notion-Signature: sha256=<hex HMAC-SHA256(verification_token, raw body)>` | the one-time `{"verification_token": "secret_…"}` (unsigned; accepted once, only while the hook has no token) | body digest |
| Google Calendar | `POST /v1/ingest-hooks/google_calendar/{hook}` | `X-Goog-Channel-ID` = the hook id and `X-Goog-Channel-Token` = the channel token; no body | `X-Goog-Resource-State: sync` | `<channel id>:<X-Goog-Message-Number>` |
| Gmail | `POST /v1/ingest-hooks/gmail` | `Authorization: Bearer <Google OIDC ID token>`: RS256 against Google's certs (`www.googleapis.com/oauth2/v3/certs`, cached by `max-age`, an unknown `kid` refetches at most once a minute), `iss` Google's, `aud` = `INGEST_HOOKS_GMAIL_AUDIENCE`, `email` = `INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT` and verified, `exp` / `iat` within 60 s skew | a push for a mailbox no hook claims (`{"received":true,"routed":false}`) | Pub/Sub `messageId` |

A delivery to an unknown hook, a hook of another provider, or a deleted hook is answered exactly
like a bad signature: `401 invalid_signature`, nothing stored or charged.

### 2.4 What reaches the core

The gateway sends one JSON-RPC request through the sidecar relay (`mcp-proxy.ts`), with
`Mcp-Method: tools/call`, `Mcp-Name: memory.ingest_webhook`, `Idempotency-Key`, and the hook's
derived bearer (`sgw1.<principal>.<hook id>.<mac>`):

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory.ingest_webhook","arguments":{
  "provider":"github","delivery_id":"72d3162e-…","idempotency_key":"hook:github:72d3162e-…",
  "hook_id":"hk_…","received_ms":1761393600000,"event":"issues","payload_b64":"eyJhY3Rpb24iOi…"}}}
```

Calendar and Gmail carry `changed` instead of `payload_b64`:
`{"channel_id","resource_id","resource_state","message_number","resource_uri"}` from the
`X-Goog-*` headers, or `{"history_id","message_id"}` from the Pub/Sub message. The mailbox address
is not relayed.

The provider is answered with a fixed acknowledgement and the usage headers, never the core's reply:
`200 {"received":true,"stored_events":n}`; `{"refused":true}` is added when the core refused the
payload's shape (`Mcp-Error-Code: -32602`; final, so the provider does not retry). A core that
cannot store it now (`Mcp-Error-Code: -32000`, a 5xx, no answer) is answered `503` with
`Retry-After: 30` so the provider retries; the idempotency key makes the retry safe.

### 2.5 What the core stores (`memory.ingest_webhook`)

| provider | events kept | text | thread |
|---|---|---|---|
| GitHub | `push` (commit messages, up to 20 listed), `issues` opened / edited / closed / reopened, `issue_comment` created, `pull_request` opened / edited / closed (merged) / reopened / ready, `pull_request_review` submitted, `pull_request_review_comment` created, `release` published | who did what, the title, the body | `github:<repo>#<n>` or `github:<repo>@<branch or tag>` |
| Linear | `Issue`, `Comment`, `Project`, `ProjectUpdate` (create / update; removals are ignored) | who did what, identifier, title, state, description / body | `linear:<identifier>` or `linear:project:<name>` |
| Slack | `event_callback` with `message` (no subtype, `thread_broadcast`, `file_share`: the text only, never the file) and `app_mention`; bot messages, edits and deletions are ignored | `<@user> wrote in <#channel>:` and the text; time from the message `ts` | `slack:<team>/<channel>/<thread ts>` |
| Notion | nothing: Notion events carry ids, not content | a change notice for the entity (`page`, `database`, `comment`) with its id and `notion.so` URL | — |
| Google Calendar | nothing: a push carries only the channel | a change notice for the calendar (resource id, state, message number) | — |
| Gmail | nothing: a push carries a history id | a change notice for the mailbox (history id, message id) | — |

Each delivery becomes at most 20 events (in practice one). Bodies are cut to the ingest limit
(16,000 characters) with the provenance line kept.

**Change notices (the core side, the fetch).** A Notion, Calendar or Gmail delivery is answered
`outcome: "changed"`, stores nothing and settles at 0 events. Fetching the content is the core's
connector read-sync with the principal's own credentials (`crates/shogun-mcp/src/sync.rs` →
`shogun-integrations`, credentials per E5-T5 / E50): Calendar `events.list` with the channel's
sync token, Gmail `users.history.list` from the history id, Notion the page or comment by id. The
read-sync ingests what it fetches through the same append path and is billed as ingest then. Until
the hosted core runs that read-sync on a change notice (it needs the hosted connector credentials
of E50), a change notice is acknowledged and the content arrives on the next scheduled sync of a
connected core.

### 2.6 The bounded exceptions to "no parsing in apps/api"

Each is the minimum a provider's protocol requires, and each is tested to persist nothing
(`ingest-hooks.test.ts`, "nothing from a body or a secret persists"):

1. **Slack `url_verification`** (≤ 1 KiB, only after its signature verifies): parsed to echo
   `challenge`.
2. **Notion's verification token** (≤ 1 KiB, only while the hook has no token): parsed to seal
   `verification_token` as the hook's signing secret. Anyone who knows the (unguessable, 128-bit)
   hook URL could race Notion to it; Notion's own Verify step then fails for the owner, who
   rotates the hook.
3. **Gmail's Pub/Sub envelope** (≤ 16 KiB, only after the OIDC token verifies): parsed for
   `message.messageId` and the notification `{emailAddress, historyId}` inside `message.data`.
   The address is only HMAC'd to find the hook; the two ids are all that is relayed.

### 2.7 Operator setup

`INGEST_HOOKS_SECRET` (≥ 32 characters) turns the routes on; they need the `/mcp` relay
(`SHOGUN_SIDECAR_URL` and a bearer source) and `apps/api/sql/ingest-hooks.sql` (runbook §SQL).
Gmail additionally needs one Pub/Sub topic that `gmail-api-push@system.gserviceaccount.com` may
publish to, and one push subscription to `$BASE/v1/ingest-hooks/gmail` with authentication on:
its service account is `INGEST_HOOKS_GMAIL_SERVICE_ACCOUNT` and its audience
`INGEST_HOOKS_GMAIL_AUDIENCE`.

## 3. Browser extension (specification)

Not implemented. A browser extension is a capture source like the Mac app's screen capture, with
the same restraint. This section is the contract an implementation must meet.

### 3.1 What it may send

One event per page the person actually reads (the tab is focused and visible for at least 10
seconds, or they scrolled), and again only when the visible text changed by more than half:

| field | value |
|---|---|
| `content` | the **visible text** of the focused page's main document (`innerText` of what is rendered, not hidden elements, not `<input>` / `<textarea>` / `contenteditable` values), trimmed to 16,000 characters, followed by a line `URL: <url>` |
| `title` | the page `<title>`, at most 200 characters |
| `kind` | `web_page` |
| `ts_ms` | when the page was read |
| the URL | `scheme://host/path` only: the query string and fragment are dropped (they carry tokens and session state), except for an allowlist the person edits |

**Never**: screenshots, page images, canvas or video frames, audio, files or downloads, form field
values (typed or autofilled), cookies, local storage, request or response bodies, clipboard, or
text from other tabs.

### 3.2 Exclusions (nothing is read, nothing is sent)

- private / incognito windows (`chrome.extension.inIncognitoContext`, Firefox private browsing),
  and the extension is not allowed in them by default;
- password managers and their pages and popups (1Password, Bitwarden, LastPass, Dashlane, Keeper,
  the browser's own password and payment settings), and any page while a password, one-time-code
  or payment-card field has focus;
- browser-internal and local pages: `chrome://`, `edge://`, `about:`, `moz-extension://`,
  `chrome-extension://`, `file://`, `view-source:`;
- sign-in, OAuth consent and account-recovery pages (`accounts.google.com`, `login.microsoftonline.com`,
  `appleid.apple.com`, and any page whose path contains `/login`, `/signin`, `/oauth`, `/authorize`);
- banking and health sites on a built-in list the person can extend, and any site, path or title
  pattern the person excludes (the same exclusion vocabulary as the Mac app's capture exclusions);
- a global pause the person controls from the toolbar.

### 3.3 Auth

The extension holds a **principal-scoped ingest key**: an agent key of the principal whose scopes
are exactly `["write"]` (no `read`, no `run`, never `admin`), issued for this install and named
after it, with its own day cap. It can append to memory and nothing else: it cannot read memory
back, run tools or queue a send. It is stored in the extension's storage only (never synced), and
revoking it (`POST /v1/keys/{id}/revoke`) stops the install. Issuing a write-only key to an
install is a key-management step the gateway does not serve yet (today keys come from
self-registration with `read`, `write`, `run`, or from a tenant admin); until it does, the
extension is not shipped.

### 3.4 Where it sends

- **Hosted**: `POST $BASE/mcp` with `Mcp-Method: tools/call`, `Mcp-Name: memory.ingest`,
  `Mcp-Quantity: <events>` and the key as bearer: metered ingest, like any agent.
- **Local core** (the principal runs `shogun-api` themselves): `POST /v1/memory/ingest` with the
  local token.

Body (the `memory.ingest` contract, `docs/api/core-tools.md`):

```json
{
  "source": "browser-extension",
  "actor": "agent:browser-<install id>",
  "thread_key": "web:<host>",
  "idempotency_key": "bx:<install id>:<sha256(url + visible text)>",
  "events": [
    { "ts_ms": 1761393600000, "kind": "web_page", "title": "Q4 plan — Docs",
      "content": "…visible text…\n\nURL: https://docs.example.com/d/abc" }
  ]
}
```

### 3.5 Rate limits

- client side: at most 1 event per tab per minute, 60 events per hour and 600 per day per
  install; a page is not resent while its text is unchanged (the idempotency key replays anyway);
- batching: up to 20 events per call, at least 30 seconds apart; offline events are dropped after
  24 hours rather than replayed in a burst;
- server side: the key's day and month caps (`PATCH /v1/keys/{id}`) and the principal's caps bound
  spend; past them the gateway answers `402 cap_exceeded` and the extension stops until the next
  period.

## 4. Status

| part | state |
|---|---|
| GitHub, Linear, Slack, Notion, Google Calendar, Gmail hooks (verification, registration, relay, billing, failure budget) | implemented (gateway) |
| `memory.ingest_webhook` normalizers (GitHub, Linear, Slack text; Notion, Calendar, Gmail change notices) | implemented (core, three faces) |
| Change notice → connector fetch in the hosted core | specified (§2.5); needs E50 hosted connector credentials |
| Browser extension | specified (§3); not built; needs write-only key issuance |
