# dedup-bench

How many state rows **one real memory** becomes — a commitment, an open loop, a person, a
project — and how often two **different** memories are wrongly fused into one row.

```bash
cargo run -p dedup-bench --release              # the table, plus the match-cost run
cargo run -p dedup-bench --release -- --no-cost # the table only (fast)
cargo run -p dedup-bench --release -- --json    # machine-readable
cargo test -p dedup-bench                       # the CI gate (tests/gate.rs)
```

Linux-only by construction: `shogun-memory` bundles SQLCipher, so it runs on the cheap CI runner.
Never add a macOS job for it (CLAUDE.md, CI cost).

## Why it exists

Repetition in the memory went unmeasured because the extraction eval cannot see it.
`extract-eval`'s scorer collapses predictions by normalised key *before* scoring, on the stated
assumption that "the product keeps one row per key". This crate drives the product's real persist
path instead:

```
event_log::insert_or_touch  →  extract::extract_untrusted  →  extract::persist_candidates_deduped
                                                           →  knowledge::persist   (Batch entities)
```

Nothing here re-implements a dedup rule, so every number describes the product's own decision.

## What it measures

Each scenario isolates **one mechanism** by which a single memory reaches the log more than once.
A mixed corpus would report one rate and hide which mechanism produced it.

| Mechanism | What happens |
|---|---|
| `control` | Exact recapture; a connector thread re-synced. The existing dedup handles these. |
| `recapture` | The same window re-read after new messages arrived (a full checkpoint). |
| `title_drift` | The window title's unread counter changes: `(2) Inbox` → `(5) Inbox`. |
| `day_header` | The sender line reads `Today` one day and `Yesterday` the next. |
| `quoted_reply` | A reply quotes the promise with `>`, on screen or in a synced thread. |
| `truncated_preview` | A message list shows `… by Fr…` next to the open message. |
| `cross_surface` | The same message is on screen *and* arrives through its connector. |
| `restatement` | The same commitment restated in different words. |
| `entity_mentions` | One person or project mentioned in several messages. |
| `precision` | Different memories that look alike. **These must stay separate.** |

Two numbers come out of every run, because they trade against each other:

- **Extra rows** — rows beyond one per real memory. The repetition.
- **Wrong merges** — rows that absorbed two different memories. The worse failure: a duplicate is
  visible and fixable; a wrong merge silently loses a memory.

## Ground truth

Each capture declares which memory each extractable sentence belongs to (`says`). A row's identity
is read from the candidate that produced it — candidates are persisted one at a time, which is the
same decision sequence `persist_candidates_deduped` runs — so the benchmark never infers truth from
the text matching it is evaluating. Identical sentences from two people in one capture are
labelled by position.

Semantics follow the product's own tests in `extract.rs`: different senders with identical words
are different memories, and a recurrence after completion is a new memory.

## The capture stream is post-collapse

Upstream, `shogun_core::capture` drops a body ≥ 98% similar to a recent one (FR-CAP-03) and stores
a view that changed ≥ 40% of its lines as a full checkpoint, which is re-extracted in full. Every
same-window recapture here is written as such a checkpoint. The crate therefore does not depend on
`shogun-core`, and measures exactly the events that reach extraction twice.

## Known limitations

A scenario may carry `known_limitation`: a failure that is expected, documented, and excluded from
the gate. The gate also fails if a known limitation starts passing, so a stale note cannot hide the
next real regression. Current ones:

- `restated-promise` — paraphrase is not matched at write time, by design; see
  `research/MEMORY-DEDUP.md`.
- `two-alexes-one-channel` — a one-word name is taken as one person within one conversation.
- `two-senders-one-capture` — pre-existing: the same-event arm of `RECAPTURE_CONTEXT_SQL` ignores
  the sender.

## Match cost

The dedup lookup runs on the capture path, once per candidate. The cost run grows a database to 100,
1,000 and 5,000 unresolved rows and times persisting a fresh candidate, in four layouts:

| | uniform vocabulary | 200 topics |
|---|---|---|
| **same window** | one busy channel, every sentence sharing every long word — worst case for a word prefilter | one busy channel, realistic variety |
| **other windows** | pure scan: none of the rows can ever match | pure scan, realistic variety |

**Compare binaries, not runs.** On a loaded machine the same binary varies severalfold between
runs. To compare two versions, build both from the same harness and run them alternately; read the
results pairwise. `BASELINE.md` records how that was done for the identity fix.

## Files

| File | Contents |
|---|---|
| `src/scenario.rs` | The corpus, one mechanism per scenario. |
| `src/harness.rs` | Drives the real write path; the match-cost run. |
| `src/report.rs` | Scoring and rendering. |
| `tests/gate.rs` | The CI gate. |
| `BASELINE.md` | Frozen before/after numbers. |
