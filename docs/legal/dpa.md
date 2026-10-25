# ShogunAgent Data Processing Addendum

> **DRAFT — for legal review. Not in force.** This draft describes what the code in this repository actually does as of 2026-10-25. Counsel must review it, fill the placeholders marked `[TBD]`, attach the Standard Contractual Clauses and confirm the subprocessor list before it is offered to any customer. Until then, `/legal/dpa` serves this text with the same banner.

Last updated: 2026-10-25 (draft). This Data Processing Addendum ("DPA") forms part of the agreement between the customer named in the order form ("Customer") and Select KK `[TBD: registered address, company number]` ("we") for the hosted ShogunAgent service (the "Agreement"). Where this DPA and the Agreement disagree about personal data, this DPA wins.

## 1. Roles

- **Customer is the controller** of the personal data its principals, agents and people put into the service ("Customer Data"). Customer decides what is ingested, which agents may read and write it, and how long it is kept.
- **We are the processor.** We process Customer Data only to provide the service, on Customer's documented instructions. The Agreement, this DPA and Customer's use of the API (including its permission tiers, access lists and deletion calls) are those instructions.
- **Where we act as a controller.** For account, billing and security records (principal and tenant ids, key hashes, receipts, invoices, request log lines), we are a controller, and the Privacy Policy (`/privacy`) applies.
- If an instruction would, in our opinion, breach data protection law, we will tell Customer and need not follow it.

## 2. What is processed

| | |
|---|---|
| **Subject matter** | Hosting a typed, provenance-backed model of people's work (people, projects, commitments, open loops) that AI agents read and write. |
| **Duration** | The term of the Agreement, plus the deletion periods in section 8. |
| **Nature and purpose** | Storing, deriving, searching and returning Customer Data; enforcing permission tiers; relaying calls to tools and model providers chosen by Customer; metering and billing. |
| **Categories of data subjects** | Customer's principals (usually employees or contractors), and people who appear in the data Customer ingests (colleagues, customers, correspondents). |
| **Categories of personal data** | Whatever Customer ingests (names, e-mail addresses, message and document excerpts, meeting notes, tasks), the state derived from it, and identity metadata for provisioning: SCIM user attributes (userName, name, e-mail addresses, active flag, external id) and tenant admin e-mail addresses. |
| **Special categories** | Not intended. Customer must not ingest special-category data unless the order form says otherwise `[TBD]`. |

## 3. How we protect it

- **One encrypted store per principal.** Each principal's state is a separate SQLCipher file with its own key (hosted keys sealed by a key management service; customer-managed keys where the order form provides BYOC or KMS). Cross-principal reads happen only through access lists Customer sets up (Teams), never by query.
- **Bodies are not retained outside the store.** The gateway relays request and response bodies without writing them to disk, logs, metrics or error reports. The exact paths and the metadata we do keep are listed in the Zero-Retention Commitment (`/legal/zdr`).
- **Secrets are stored hashed or sealed.** API keys as salted PBKDF2-SHA256 hashes, OAuth codes and tokens and the SCIM bearer as SHA-256 hashes, OIDC client secrets sealed with AES-256-GCM.
- **Access control.** Every action carries a permission tier (L1 read, L2 write to Customer's own data, L3 external send, payment or calendar creation, which always waits for a human). Tenant admins sign in through Customer's own identity provider (OIDC or SAML 2.0); SCIM deactivation revokes a principal's keys and app grants at once.
- **Personnel.** People with access to production are bound by confidentiality and get access on a need-to-know basis `[TBD: access review cadence]`.
- A fuller description of technical and organisational measures is in Annex II `[TBD: attach]`.

## 4. Subprocessors

Customer authorises the subprocessors below. We will give at least `[TBD: 30]` days' notice of a new subprocessor that processes Customer Data, by `[TBD: e-mail to the tenant admins / a subscribable page]`; Customer may object on reasonable data protection grounds, and if we cannot resolve the objection Customer may terminate the affected service and receive a pro-rata refund of prepaid amounts.

| Subprocessor | Purpose | Customer Data processed | Region |
|---|---|---|---|
| Cloudflare, Inc. `[TBD: confirm]` | Gateway hosting (Workers), TLS termination | Bodies in transit only; never stored | The tenant's region `[TBD]` |
| `[TBD: managed Postgres provider]` | Gateway database | Identity and billing metadata; SCIM attributes; no state content | The tenant's region |
| `[TBD: host of the per-principal stores]` | Encrypted per-principal stores and the engine | All Customer Data | The tenant's region |
| Stripe, Inc. | Card payments, refunds, metered commit billing | None (billing contacts only, under the Privacy Policy) | `[TBD]` |
| `[TBD: model providers, e.g. OpenAI, Anthropic]` | Extraction, consolidation, AI search | The chunk needed for one call, under zero retention (`/legal/zdr`) | `[TBD]` |

Tools that Customer's agents choose to run through the router are not our subprocessors: Customer selects them, and the router shows each tool's operator and terms.

We impose data protection terms on each subprocessor that are no less protective than this DPA, and remain liable for their performance.

## 5. Regions and international transfers

- A tenant is created in one region (`us`, `eu`, `jp`, …) and its principals are served only by that region's deployment. Each region has its own gateway, database, engine hosts and keys; deployments share no database and never call each other, so Customer Data does not move between regions in the normal course of the service. A request that reaches the wrong region is refused and told which endpoint to use; it is not forwarded.
- Where Customer Data is nevertheless transferred out of the EEA, the UK or Switzerland (for example to a model provider or for support), the parties agree to the Standard Contractual Clauses (Commission Implementing Decision (EU) 2021/914), Module Two (controller to processor) and, where applicable, Module Three, with the UK International Data Transfer Addendum and the Swiss amendments `[TBD: attach as Annex III, with docking clause, governing law and supervisory authority selections]`. For transfers out of Japan, `[TBD: APPI Article 28 basis]`.

## 6. Assistance and audits

- **Data subject requests.** The API gives Customer the tools to answer most requests itself: export (`GET /v1/export`, free), correction through the write tools, deletion of a principal, and SCIM for joiners and leavers. We forward to Customer any request we receive directly, and help with the rest at no charge `[TBD: reasonable-effort wording]`.
- **Impact assessments and consultations.** We provide the information we reasonably hold.
- **Audits.** We make available our `[TBD: SOC 2 Type II / ISO 27001 reports when available]` and this repository's open-source engine. Customer may audit once a year on `[TBD: notice]` notice, at its cost, under confidentiality, or more often after a personal data breach. Tenant admins can export the audit log of every member principal at any time (`GET /v1/tenant/audit/export`).

## 7. Personal data breaches

We will notify Customer **without undue delay and in any case within `[TBD: 48]` hours** of becoming aware of a personal data breach affecting Customer Data, to the tenant admins and the security contact in the order form. The notice will describe, as far as known, the nature of the breach, the categories and approximate number of data subjects and records, the likely consequences, and the measures taken or proposed. We will update it as we learn more, take reasonable steps to contain the breach, and not tell third parties about it on Customer's behalf without Customer's consent unless the law requires us to.

## 8. Deletion and export

- **Export, any time, free.** Customer can export every principal's full state in the documented open format (`docs/api/export-v1.md`) during the term and for `[TBD: 30]` days after it ends.
- **Deleting a person.** A SCIM `DELETE` of a user revokes the principal's keys and app grants immediately and schedules deletion of its store after a grace period (30 days by default, configurable per deployment from 1 to 365 days) during which Customer can still export it. A tombstone (ids and times, no attributes) is kept so the deletion is not undone by a retry.
- **End of the Agreement.** We delete Customer Data within `[TBD: 30]` days after the export window closes, including backups within `[TBD: backup rotation period]`, unless the law requires us to keep it, in which case we keep it isolated and only for as long as required.
- **What is kept.** Billing records (receipts, statements, ledger entries) carry ids, amounts and times, never state content, and are kept as the law requires.
- On request we will certify the deletion in writing.

## 9. Liability and precedence

Each party's liability under this DPA is subject to the limitations in the Agreement `[TBD: confirm carve-outs]`. If the Standard Contractual Clauses apply, they prevail over this DPA and the Agreement where they conflict.

## Annexes

- **Annex I** — list of parties, description of the processing (section 2 above), competent supervisory authority `[TBD]`.
- **Annex II** — technical and organisational measures `[TBD]`.
- **Annex III** — Standard Contractual Clauses and transfer addenda `[TBD]`.
