# pii-bench baseline — v0.2

Frozen output of `cargo run -p pii-bench --release`.

| | |
|---|---|
| Commit | `1014d7e6` **plus the working-tree changes described below** — not a clean tree |
| Date | 2026-09-29 |
| Platform | Windows 11 (10.0.26200), x86_64 |
| Toolchain | rustc 1.94.0, MSVC 14.44, SQLCipher via `bundled-sqlcipher-vendored-openssl` |
| Build | `--release` |
| Cases | 40, across 3 boundaries = 120 probes |

**This run is not from a clean tree.** It measures the redaction changes made in the same session
(case-insensitive issuer prefixes, `redact_egress`), so re-run it after those land before quoting
it as the committed baseline. Survival is deterministic — it is a property of the code path, not
of the machine — so the numbers themselves will reproduce exactly.

## Results

Recall = of the cases that **must** be masked at that boundary, the share that were.

| Boundary | Category | Cases | Masked | Leaked | By design | Recall |
|---|---|---:|---:|---:|---:|---:|
| content | TargetedSecret | 9 | 9 | 0 | 0 | **1.000** |
| content | UntargetedSecret | 4 | 0 | 4 | 0 | 0.000 |
| content | PersonalData | 5 | 0 | 0 | 5 | n/a |
| content | HighRiskIdentifier | 6 | 0 | 0 | 6 | n/a |
| content | LookAlike | 10 | 0 | 0 | 0 | n/a |
| content | Obfuscated | 5 | 4 | 1 | 0 | 0.800 |
| window_title | TargetedSecret | 9 | 9 | 0 | 0 | **1.000** |
| window_title | UntargetedSecret | 4 | 0 | 4 | 0 | 0.000 |
| window_title | PersonalData | 5 | 0 | 0 | 5 | n/a |
| window_title | HighRiskIdentifier | 6 | 0 | 0 | 6 | n/a |
| window_title | LookAlike | 10 | 0 | 0 | 0 | n/a |
| window_title | Obfuscated | 5 | 4 | 1 | 0 | 0.800 |
| egress | TargetedSecret | 9 | 9 | 0 | 0 | **1.000** |
| egress | UntargetedSecret | 4 | 0 | 4 | 0 | 0.000 |
| egress | PersonalData | 5 | 0 | 0 | 3 | n/a |
| egress | HighRiskIdentifier | 6 | **6** | 0 | 0 | **1.000** |
| egress | LookAlike | 10 | 0 | 0 | 0 | n/a |
| egress | Obfuscated | 5 | 4 | 1 | 0 | 0.800 |

**Total leaks: 15. Total false positives: 0.**

## What these numbers say

**`window_title` now matches `content` exactly.** That is the PII-001 fix holding: before it, the
title column bypassed the persist gate entirely, across five surfaces (the column, the `thread_key`
string, `threads.title`, `threads.thread_key`, and the FTS index).

**HighRiskIdentifier is 0/6 on the DB path and 6/6 at egress.** That split is the design, not an
inconsistency — see the README. Cards and national IDs are kept in the encrypted store and removed
before anything leaves the device.

**Zero false positives across 10 look-alikes, at every boundary.** This is the number that makes
the Luhn and SSN detectors safe to ship. The corpus deliberately includes a non-Luhn digit run one
digit off a real test card, a 13-digit epoch timestamp, a reserved-range SSN, and the phrase
"passport application" — every one survives untouched.

**Obfuscated is 0.800 everywhere.** The one failure is `ocr-corrupted-prefix`, where the issuer
prefix itself has been mangled by OCR. Detecting that would require fuzzy prefix matching, whose
false-positive cost has not been measured and which is not currently proposed.

## The 15 leaks, and why they are not all the same

All 15 come from 5 cases, each appearing at all 3 boundaries.

| Case | Category | Status |
|---|---|---|
| `stripe-style-key` | UntargetedSecret | **Known gap (PII-003).** No `rk_`/`pk_` prefix in `ISSUER_PREFIXES`. |
| `unlabelled-high-entropy` | UntargetedSecret | **Known gap.** No entropy heuristic exists; adding one trades against the zero-false-positive result above. |
| `short-password` | UntargetedSecret | **By construction.** Below `MIN_SECRET_LEN` (12), where masking is more likely to destroy prose than to protect a credential. |
| `private-key-header` | UntargetedSecret | **Known gap.** `-----BEGIN ... PRIVATE KEY-----` is not a recognised shape. |
| `ocr-corrupted-prefix` | Obfuscated | **Documented limit.** |

Note the `fts=false` column on every egress row: the egress path has no index, so a needle can only
survive in the returned text. On the two DB boundaries `fts=true` accompanies every leak, which is
the point of tracking them separately — a derived index can leak what the base table masked.

## Changes since v0.1

- Third boundary added (`egress`), so the bench describes the third-party handoff and not only
  storage.
- 13 cases added: 6 high-risk identifiers, 4 look-alikes for the new detectors, 3 case-variant
  issuer prefixes.
- `uppercased-prefix` moved from leaking to masked (PII-004 fixed).
- `zwsp-split-key` leaked at egress on the first run of this baseline and does not now: the egress
  path was not stripping hidden characters before redacting, making it strictly weaker than the DB
  path. The bench caught it; `redact_egress` now strips first, as `sanitize::persist_body` does.
