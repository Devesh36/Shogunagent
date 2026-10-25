# ShogunAgent Service Level Agreement

> **DRAFT — for legal review. Not in force.** This draft is written to match how the hosted service is built and measured as of 2026-10-25. Counsel must review it, fill the placeholders marked `[TBD]`, and confirm the targets with operations before it is offered to any customer. Until then, `/legal/sla` serves this text with the same banner.

Last updated: 2026-10-25 (draft). This SLA applies to customers with a signed enterprise agreement (the "Agreement") that refers to it. Self-serve use has no service-level guarantee (Terms, section 8).

## 1. Availability target

We aim for **99.9% monthly availability** of the hosted service in the customer's region `[TBD: 99.95% for an annual commit above a threshold]`.

- **Covered endpoints:** the metered MCP endpoint (`/mcp`), the REST state and memory routes, the router verbs, key and wallet routes, SCIM and SSO.
- **Available** means the endpoint answers a well-formed, authenticated request with something other than a 5xx status, within 30 seconds, as measured by our external probes in the customer's region every minute.
- **Monthly availability** = (minutes in the month − minutes unavailable) ÷ minutes in the month. A minute is unavailable when more than 5% of probe and customer requests in it fail as above.
- Live status and incident history are published at `/.well-known/status.json` and `/health`.

## 2. Service credits

If monthly availability falls below the target, Customer receives a credit against the next invoice or wallet, as a share of that month's fees for the affected region:

| Monthly availability | Credit |
|---|---|
| Below 99.9%, at least 99.0% | 10% |
| Below 99.0%, at least 95.0% | 25% |
| Below 95.0% | 50% |

- Customer requests the credit within 30 days of the end of the month, through support, with the affected region and times.
- Credits are capped at 50% of the month's fees for the affected region, cannot be exchanged for cash, and are Customer's sole remedy for unavailability `[TBD: confirm against the Agreement's liability terms]`.
- Fees here mean usage charges and the pro-rated share of an annual commit; third-party tool prices passed through the router are excluded.

## 3. Exclusions

Unavailability does not count when it is caused by:

- scheduled maintenance announced at least `[TBD: 72]` hours ahead, up to `[TBD: 4]` hours a month, outside `[TBD: business hours in the customer's region]`;
- Customer's own systems, network, identity provider or SCIM client, or a configuration Customer controls (caps, revoked keys, an SSO setup that refuses sign-in);
- refusals the service is designed to give: `402` for an empty wallet or a reached cap, `401` / `403` for invalid credentials or scopes, `421 wrong_region` for a request sent to another region, `429` rate limits, and approvals waiting for a human;
- third-party tools run through the router, and model providers' outages beyond our control where no alternative provider is configured;
- suspension under the Agreement for breach, non-payment or abuse;
- force majeure, or a beta or preview feature marked as such.

## 4. Support tiers

| | Standard (self-serve) | Enterprise | Enterprise Premium `[TBD]` |
|---|---|---|---|
| Channel | E-mail, issue tracker | E-mail, shared channel | E-mail, shared channel, phone for Severity 1 |
| Hours | Business days `[TBD: time zone]` | 24×7 for Severity 1, business days otherwise | 24×7 |
| First response, Severity 1 (service down or data at risk) | Best effort | 1 hour | 30 minutes |
| First response, Severity 2 (major feature impaired) | 2 business days | 4 business hours | 2 hours |
| First response, Severity 3 (minor issue, question) | Best effort | 1 business day | 4 business hours |
| Named contact, quarterly review | — | — | Yes |

Customer assigns severity when opening a ticket; we may adjust it with an explanation. Response times measure the first human response, not resolution.

## 5. Incidents

For every Severity 1 incident we publish a status entry while it lasts and a post-incident report within `[TBD: 5]` business days, covering impact, timeline, cause and follow-up actions. Security incidents affecting personal data follow the breach-notice terms of the Data Processing Addendum (`/legal/dpa`).

## 6. Changes

We may update this SLA with `[TBD: 90]` days' notice; a change that lowers the target or the credits does not apply before the current term of the Agreement renews.
