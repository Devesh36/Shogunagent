# dedup-bench baseline

Scenario counts are deterministic — they depend only on the code, not the machine — so these
reproduce exactly. Both columns were run on the **same corpus** (25 scenarios), the "before" column
by running it against `shogun-memory` as it is on `origin/main` at `4b9d1e38`.

| | |
|---|---|
| Date | 2026-10-09 / 10 |
| Platform | Windows 11 (10.0.26300), x86_64, 8 GB RAM |
| Toolchain | rustc 1.94.0, MSVC 14.44, SQLCipher via `bundled-sqlcipher-vendored-openssl` |

## Rows per memory, by mechanism

| Mechanism | Scenarios | Memories | Rows before | Rows after | Wrong merges before | after |
|---|---:|---:|---:|---:|---:|---:|
| control | 2 | 2 | 2 | 2 | 0 | 0 |
| recapture | 1 | 2 | 2 | 2 | 0 | 0 |
| title_drift | 2 | 3 | 8 | 3 | 0 | 0 |
| day_header | 1 | 1 | 2 | 1 | 0 | 0 |
| quoted_reply | 2 | 2 | 5 | 2 | 0 | 0 |
| truncated_preview | 1 | 1 | 2 | 1 | 0 | 0 |
| cross_surface | 2 | 2 | 4 | 2 | 0 | 0 |
| restatement | 1 | 1 | 2 | 2 | 0 | 0 |
| entity_mentions | 3 | 3 | 10 | 3 | 0 | 0 |
| precision | 10 | 20 | 19 | 18 | 1 | 2 |
| **repetition (all but precision)** | 15 | 17 | **37 (2.18 / memory)** | **18 (1.06 / memory)** | | |

Scenarios passing: **12 / 25 before, 22 / 25 after.** The three that do not pass after are
documented limitations (`known` in the report):

| Scenario | Before | After | Why it is accepted |
|---|---|---|---|
| `restated-promise` | 2 rows | 2 rows | Paraphrase is not matched at write time, by design; see `research/MEMORY-DEDUP.md` §6.3. |
| `two-senders-one-capture` | wrong merge | wrong merge | Pre-existing: the same-event arm of `RECAPTURE_CONTEXT_SQL` ignores the sender. |
| `two-alexes-one-channel` | 2 rows (correct) | wrong merge | **Introduced by the fix.** A one-word name is one person within one conversation; the user's split corrects it permanently. |
| `namesakes` | 2 rows (correct) | wrong merge | **Owner decision 2026-10-14.** A person's name, one word included, resolves to the existing row everywhere, so a case variant (`mikel` / `Mikel`) is never a second person. The review model (shown the known people, `existing_id`) tells real namesakes apart; the user's split corrects the rest. |

The adversarial precision scenarios added to break the fix — `numbered-windows`,
`two-mail-accounts`, `screen-and-connector-different-threads` — pass before and after.

**2026-10-13 addendum (trailing mentions).** Two scenarios were added when quote identity began to
ignore trailing `@mentions` and full-width terminators: `slack-app-mention-and-connector` (the app
shows `… 明日修正します @dana`, the connector text has no mention) and the precision scenario
`same-words-different-mentions` (one sender, the same long promise to `@dana` and then `@lee`).
Without the mention stripping the first writes 2 rows; stripping without the both-sides-name-someone
check merges the second. With both: 24 / 27 passing, 1 extra row over 18 memories (1.06), the
same 2 documented wrong merges.

## Match cost

COST_SECTION
