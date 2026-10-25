# ShogunAgent Privacy Policy

> **DRAFT — for legal review. Not in force.** This draft describes what the code in this repository actually does as of 2026-10-25. Counsel must review it, fill the placeholders marked `[TBD]`, and confirm the subprocessor list before it is published. Until then, `/privacy` serves this text with the same banner.

Last updated: 2026-10-25 (draft). Operator: Select KK `[TBD: registered address, company number]`. Contact: `[TBD: privacy@ address]`.

## 1. What ShogunAgent is

ShogunAgent is a state layer for AI agents: a typed model of a person's work (people, projects, commitments, open loops) that agents read and write over MCP, REST and a CLI. It runs in two ways:

- **Local (open source).** The engine runs on your own machine or server. Nothing in this policy applies to data that never leaves it: we do not receive it.
- **Hosted.** We run the engine and a gateway for you at `api.shogunagent.dev`. This policy covers the hosted service.

## 2. The short version

- Your state lives in **one encrypted store per principal** (one SQLCipher file per person). Other customers cannot query it; access across principals happens only through access lists you set up.
- The **gateway never stores request or response bodies.** Logs, metrics, billing records and error reports carry ids, counts, amounts and timestamps, never the content of what your agents send or receive.
- **We do not train models on your data,** and we do not let our model providers do so.
- **Export is always free** and available at any time, in an open, documented format. Deleting your data is free too.
- **No telemetry by default.** Anything beyond what is needed to run and bill the service is opt-in and counts only.

## 3. What we process, and why

| Data | Where it lives | Why | How long |
|---|---|---|---|
| Your state and memory (events you ingest, notes, the people / projects / commitments / open loops derived from them, their provenance) | Your principal's encrypted store | To provide the service you asked for | Until you delete it or close the principal |
| Request and response bodies passing through the gateway (`/mcp`, router runs) | Relayed in memory only; never written to disk, logs or metrics | To forward the call | Not retained |
| Account identifiers: principal id, tenant id, an optional referrer id you supply | Gateway database | To identify who owns a store and a wallet | While the principal exists |
| API keys | Gateway database, as a salted PBKDF2-SHA256 hash plus a short lookup prefix; the key itself is shown to you once and never stored | Authentication | Until the key is revoked or expires, then as long as the principal exists |
| OAuth grants for apps you connect (the app's client id URL, its redirect URI, granted scopes; codes and tokens only as SHA-256 hashes) | Gateway database | Letting MCP clients connect on your behalf | Codes: 60 seconds plus one day; tokens: until expiry plus one day |
| Billing records: wallet balance, top-ups, per-call receipts (method, tool name, unit, quantity, cost, time), disputes, monthly statements, commit contracts | Gateway database | Metering, invoicing, disputes, tax and accounting | Receipts: 400 days; closed statements and ledger entries: `[TBD: statutory accounting retention]` |
| Dispute reasons you write | Gateway database (bounded to 1,000 characters) | Resolving the dispute | With the dispute record; never copied into statements |
| Payment details | Held by Stripe, not by us; we keep Stripe's session, payment and charge ids | Taking card top-ups and refunds | As billing records |
| Client IP address at registration | Used for per-IP rate limiting, stored only as a SHA-256 hash | Abuse prevention | About ten minutes after the one-hour window closes |
| Request log lines (method, route template, status, duration, a fixed event word, a hash of the key id) | Our hosting provider's log store | Operating and securing the service | `[TBD: log retention of the hosting provider]` |

We do not sell personal data, we do not use it for advertising, and we do not build profiles of you beyond what you put into your own state.

## 4. Model providers and zero retention

Some operations (ingest extraction, consolidation, AI search) call a large-language-model provider. We send only the chunk needed for that call, under zero-retention terms where the provider offers them, and we never allow the provider to train on it. Bodies are relayed, not stored, on our side. Which provider handles which operation is listed in section 7.

## 5. Your choices and rights

- **Export:** `GET /v1/export` (or the CLI / MCP equivalent) returns your full state in the documented export format (`docs/api/export-v1.md`). It is free and never rate-limited into uselessness.
- **Delete:** deleting a principal deletes its store. Billing records we must keep by law are kept without your state content.
- **Revoke access:** revoke any API key or OAuth grant at any time; tokens minted from a revoked key stop working immediately.
- **Statutory rights:** depending on where you live (for example the EU/UK GDPR, the California CCPA/CPRA, Japan's APPI) you can ask to access, correct, delete or port your personal data, or object to processing. Write to `[TBD: privacy@ address]`. `[TBD: EU/UK representative, if required]`.

## 6. Security

Per-principal encrypted stores, keys stored only as hashes, secrets held in a dedicated type and never logged, tenant keys from a key management service or the environment, and a gateway that is structurally unable to log bodies. Report a vulnerability to `[TBD: security@ address]`.

## 7. Subprocessors

`[TBD: confirm each entry, its region and the contract in place before publishing.]`

| Subprocessor | Purpose | Data |
|---|---|---|
| Cloudflare, Inc. `[TBD]` | Gateway hosting (Workers), TLS, request logs | Metadata in transit; log lines as described above |
| `[TBD: managed Postgres provider]` | Gateway database | Account, key-hash, billing and OAuth records |
| `[TBD: host of the per-principal stores]` | Hosted engine and encrypted stores | Your state |
| Stripe, Inc. | Card payments, refunds, metered commit billing | Payment details (held by Stripe), amounts, ids |
| `[TBD: model providers, e.g. OpenAI, Anthropic]` | Extraction, consolidation, AI search | The chunk needed for one call, under zero retention |

We will give `[TBD: notice period]` notice before adding a subprocessor that processes your state.

## 8. International transfers

`[TBD: transfer mechanism (SCCs / UK addendum / APPI cross-border rules) and the regions used.]`

## 9. Children

The service is not directed to children under 16 and we do not knowingly collect their data.

## 10. Changes

We will post changes here and, for material changes, notify account holders `[TBD: how and how far in advance]`.
