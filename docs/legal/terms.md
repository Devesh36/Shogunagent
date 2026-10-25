# ShogunAgent Terms of Service

> **DRAFT — for legal review. Not in force.** This draft is written to match how the service is actually built and billed as of 2026-10-25. Counsel must review it, fill the placeholders marked `[TBD]`, and settle governing law before it is published. Until then, `/terms` serves this text with the same banner.

Last updated: 2026-10-25 (draft). These terms are between you and Select KK `[TBD: registered address]` ("we").

## 1. The service

ShogunAgent gives AI agents a shared, typed model of a person's work, read and written over MCP, REST and a CLI, with permission tiers on every action. The open-source engine is licensed separately under Apache-2.0; these terms cover the **hosted** service at `api.shogunagent.dev` and its gateway, router and wallet.

## 2. Accounts, principals and keys

- An agent may register a principal itself (`POST /v1/agents`). Whoever controls that principal's keys is responsible for its use. If an agent registers on your behalf, you are bound by these terms when you use the result.
- Keep keys secret. A key is shown once; we store only its hash and cannot recover it. Revoke a key you believe is exposed; access tokens minted from it stop working at once.
- Apps you connect over OAuth act with the scopes you granted and never with `admin`. You can revoke a grant at any time.

## 3. Permission tiers

Reads are L1. Writes to your own data are L2. Anything that sends, posts, pays or creates something outside your store is L3 and waits for an explicit human approval. We do not offer a setting that turns an L3 action into an automatic one. You are responsible for the approvals you give.

## 4. Prices, wallet and payment

- **Usage-based, no subscription.** Prices are published at `/.well-known/pricing.json`, which is generated from the same table that meters your calls. State reads, remote approvals, export and deletion cost nothing.
- **Prepaid wallet.** Metered calls draw from a prepaid micro-USD balance. When the balance or a hard cap would be exceeded, the call is refused with `402` before anything happens; nothing is charged for a refused call, and failed calls and empty results cost nothing.
- **Caps.** Every key and principal has daily and monthly hard caps. Defaults are conservative; you may lower them at any time.
- **Top-ups** are taken by card through Stripe. A new principal receives a free allowance, which has no cash value.
- **Annual commits.** An enterprise customer may agree a commit contract (an amount over a period) with a 10–25% discount, applied to every metered call during the period; receipts show both prices. Usage past the committed amount is billed monthly in arrears at the same discount, if the contract says so. `[TBD: invoicing, payment terms and what happens to unused commit at the end of the period.]`
- **Statements and disputes.** You can download a monthly statement in JSON or CSV at any time; once a month closes, its statement is fixed and carries a checksum. Dispute any receipt through the API within `[TBD: dispute window, e.g. 60 days]`; a dispute we accept is credited to your wallet.
- **Refunds.** Unused prepaid balance is refundable `[TBD: conditions]`; refunds go back to the original card through Stripe and are deducted from the wallet.
- **Taxes.** Prices exclude taxes. `[TBD: tax handling.]`

## 5. Your data

You own your data. We process it only to provide the service, as described in the Privacy Policy (`/privacy`). We do not train models on it. You can export everything, free, at any time, in the documented open format, and delete it.

## 6. Acceptable use

Do not use the service to break the law, to infringe others' rights, to process data you have no right to process, to attack or overload the service or the providers behind the router, or to evade caps, rate limits or permission tiers. We may suspend a key or principal that does, with notice where we can give it.

## 7. Third-party tools through the router

The router can run tools operated by third parties. Their price, behaviour and terms are theirs; the router publishes each tool's price and ranking breakdown and labels our own first-party tools. A tool run that fails costs nothing.

## 8. Availability and changes

We publish live status at `/.well-known/status.json`. We aim for high availability but, outside a signed enterprise agreement, offer no service-level guarantee. We may change prices with `[TBD: notice period]` notice published in `/.well-known/pricing.json`; a call is always charged at the price in force when it was made.

## 9. Warranties and liability

`[TBD: disclaimer of warranties, limitation of liability (for example capped at the amounts paid in the preceding twelve months), indemnities.]`

## 10. Termination

You may stop using the service at any time; export first. We may terminate for material breach. On termination, unused prepaid balance is handled as in section 4, and your data is deleted after `[TBD: grace period]` unless the law requires us to keep billing records.

## 11. Governing law

`[TBD: governing law and venue.]`

## 12. Contact

`[TBD: support and legal contact addresses.]`
