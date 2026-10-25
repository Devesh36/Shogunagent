# pii-bench

Measures **what actually survives** each boundary where SHOGUN handles sensitive text.

```bash
cargo run -p pii-bench --release              # print the report
cargo run -p pii-bench --release -- --json    # machine-readable
cargo test -p pii-bench                       # the regression assertions
```

Linux-only by construction, like the crates it measures — `shogun-memory` bundles SQLCipher, so
the whole benchmark runs on the cheap CI runner. Never add a macOS job for it (CLAUDE.md, CI cost).

## What this is, and what it is not

The audit in `research/pii-benchmark/` established *what exists*, by reading code. This crate
establishes *what survives*, by writing payloads through the real product paths and reading back
what is stored or emitted. That distinction is the reason the crate exists: PII-001 (an unsanitized
`window_title`) was invisible to code reading, because the gate was present and correct — it simply
was not on that column.

It is **not** a certification, and a clean run does not mean "no PII leaks". It means the 40 cases
in `corpus.rs` behaved as the design says they should.

## The three boundaries

| Boundary | Path under test | Job |
|---|---|---|
| `content` | `event_log::insert` → `sanitize::persist_body` | Mask credentials on the way into the encrypted store |
| `window_title` | the same insert, via the title column, `thread_key` and FTS | Same gate, different column — this is where PII-001 lived |
| `egress` | `shogun_redact::redact_egress` | Everything above, **plus** national IDs and payment cards, before text leaves the device |

The boundary is the independent variable, not a detail. A case that is masked on one path and
survives on another is the finding; fixing the column would have hidden it.

## Why the DB path deliberately keeps personal data

The single most common misreading of this benchmark is that `PersonalData` cases "leak". They do
not — they are `SurvivesByDesign`, and the report scores them as a characterisation rather than a
defect.

Masking emails, names and handles on the way into the store would corrupt the memory the product
exists to build: `people.emails` and `identity::resolve` need those values intact, and a name
masked at write time can never be recovered. The store is encrypted; the threat model it answers is
device loss, not its own contents.

**National IDs and payment cards are the one exception**, and the reason is narrow: they have no
legitimate product use at all. Nothing in SHOGUN resolves a person by their card number, so the
argument that protects an email does not reach them. They are therefore masked at `egress` — the
Composio, ASR and Batch boundaries — and still kept on the DB path, which is exactly the split
`corpus::expectation_at` encodes.

## Reading the report

- **Recall** — of the cases that *must* be masked at a boundary, the share that were. Below 1.000
  is a defect.
- **Survives by design** — documented behaviour, reported so it is visible rather than silently
  absent. Not a failure.
- **Look-alikes** — the precision half. These must pass through *untouched*; masking them is the
  failure. A Luhn or SSN detector that eats order numbers and timestamps is worse than no detector,
  because it silently removes text the product legitimately holds.
- **`survived_in_fts`** — tracked separately from the stored row, because a derived index can leak
  what the base table masked, and FTS is what the product actually searches.

Every payload is synthetic. Keys use repeated-letter bodies so no value here could ever be live,
while keeping the issuer prefix and the length the detector keys on. Card numbers are the published
test numbers, which are Luhn-valid and were never issued.

## Files

| File | Contents |
|---|---|
| `src/corpus.rs` | The cases. Hand-written — a generator producing both payload and label would only test itself. |
| `src/harness.rs` | The probes, one per boundary. Goes through the real `event_log::insert`, never a reimplementation. |
| `src/report.rs` | Scoring and rendering. |
| `tests/boundary.rs` | Regression assertions: a refactor that moves a write off the sanitized path fails CI. |
| `BASELINE.md` | Frozen numbers from a known commit. |
