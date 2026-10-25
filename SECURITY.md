# Security policy

## Reporting a vulnerability

Please report security issues privately. Do not open a public issue, pull request or discussion
for a suspected vulnerability.

- **Contact:** `<SECURITY CONTACT — the owner must fill this in before publishing: a monitored
  address or form, and optionally a PGP key fingerprint>`
- **GitHub:** if private vulnerability reporting is enabled on this repository, you may use
  "Report a vulnerability" on the Security tab instead.

Include what you can of: the affected component (gateway `apps/api`, router `apps/router`, sidecar
`crates/`, CLI, SDK), the version or commit, steps to reproduce, the impact you expect, and whether
you believe it is being exploited. Please do not send real user data; synthetic data is enough.

## What to expect

| Step | Target |
|---|---|
| Acknowledgement | within 3 business days |
| Initial assessment (accepted / needs info / not a vulnerability) | within 10 business days |
| Fix or mitigation for a confirmed High/Critical issue | within 30 days, sooner when exploited |
| Disclosure | coordinated with you; credit given unless you prefer not |

These are targets, not guarantees, until the owner confirms them with the contact above.

## Scope

In scope: the code in this repository and the hosted service built from it (the gateway, the
router, the sidecar, OAuth/SSO/SCIM, billing and payouts, the agent-readable documents).

Especially interesting: anything that breaks an invariant in `CLAUDE.md` — a request or response
body persisted outside the principal's store, a cross-principal read, an L3 action (send, post,
pay, calendar create) without a human decision, a secret written in clear, a billing bypass
(a write metered as a free read), a forged approval or assertion.

Out of scope: denial of service by volume alone, findings that need a compromised operator
account or host, missing hardening headers on pages without sensitive actions, and reports from
automated scanners without a demonstrated impact.

## Safe harbour

Good-faith research that respects this policy, avoids privacy violations and service
degradation, and uses only accounts and data you own will not be pursued. Stop and report as soon
as you reach data that is not yours.

## For maintainers

- The threat model is `docs/security/threat-model.md`; update it when a trust boundary moves.
- Dependency advisories: `.github/workflows/audit.yml` (`cargo audit`, `pnpm audit`).
- Parser fuzzing: `fuzz/README.md` (libFuzzer, nightly) and the seeded property tests it lists
  (stable, in the normal test run).
