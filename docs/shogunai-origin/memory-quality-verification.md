# Memory quality verification

This replacement for PR #563 combines its memory-quality changes with the current main branch's
durable Morning Brief lifecycle. Earlier #563 experiments used a different provider route and
schema; their live-provider and native-app results do not validate this integrated version.

## Behavior and boundaries

- Model source selection ranks concrete outcomes, decisions, blockers, and next actions across
  the full window. It filters accessibility-label noise and deduplicates exact variants. Local
  briefs never turn captured screen text into "What happened" prose: that section keeps only mail
  counts, with meeting recaps separate, preserving the newer main-branch fix from PR #717.
- Both dashboard entry points use the same durable generation ledger. Background synthesis uses
  the company Batch lane: the licence relay in release builds, or the explicitly enabled Select KK
  route in debug builds. It does not use user BYOK credentials.
- New model requests include at most 32 selected cloud-eligible excerpts and 12,000 source
  characters. Up to three returned highlights must cite a known source and an exact supporting
  quote. The local fallback remains available when generation cannot complete.
- Existing main migrations through V58 remain unchanged. Additive V59-V63 migrations bridge Batch metadata,
  About You, daily summaries, and source invalidation. Upgrade coverage includes a main V58 store
  with outstanding provider handles and completed briefs. Unreleased branch-only databases are not
  migrated; no startup repair or archived job restoration remains.
- Uncited new captures do not erase completed briefs or cancel work whose selected evidence is
  unchanged. Cited-source edits and deletions still invalidate affected generated content.
- The daily-summary upgrade clears legacy generated About You prose before switching from raw
  event evidence to daily-summary evidence. Pending paid-request identities remain available for
  safe completion or retirement; user-declared profile fields are unchanged.
- Memory/Wiki extraction and Brief generation remain separate consumers. This change does not
  implement the evidence-preserving retention/compaction policy tracked by issue #709.

## Final review follow-up

- The migration-conflict regression checks startup refusal, preservation of the pre-existing row,
  and that the conflicting migration is not recorded. Earlier migrations may already have committed.
- HTTP 400 is a typed rejection across the shared provider layer. If a relay submission was already
  uncertain, a later refusal cannot erase that uncertainty or authorize a second paid submission.
- About You stops automatic no-handle recovery after 48 hours. The unresolved request identity and
  source manifest remain parked; elapsed time alone never authorizes another paid request.
- Removed the unused local-highlight helper and its test, unrelated Notch send cap, and formatting
  changes to main's existing code. Restored Brief display comments and consolidated Today tests.

## Verification status

Final integrated verification passed:

- The full Rust feature matrix (`scripts/check-feature-matrix.sh`): workspace tests and all CI
  feature combinations, including 713 memory tests, 1,708 core DB tests and 1,833 core exec tests.
  This includes the formerly failing migration-conflict regression and the new recovery checks.
- Strict workspace Clippy (`--all-targets --all-features --locked -- -D warnings`) and four
  desktop Rust Dream helpers passed. The desktop shell compiled locally on Apple Silicon.
- `pnpm typecheck`, `pnpm test`, and `pnpm lint` passed: 763 desktop, 128 API and 180 website
  tests; the unchanged tokens package reused its cached 17-test result. Desktop lint has no
  warnings; website lint reports 11 pre-existing warnings in unchanged files.
- All six invariant guards and self-tests passed. All 58 main migration files remain byte-identical.
  Diff whitespace checks passed.

A synthetic SQLite query-plan check used 100,000 events with 1,000 in the selected day: both time
and activity-credit indexes were used, with no full event scan. Across 30 samples the query took
1.78 ms p50 / 2.06 ms p95. This measures the query on an in-memory fixture, not the encrypted live
store or application idle CPU.

Independent agents reviewed the cleanup and request lifecycle. Review caught and fixed the
ambiguous-submit/later-refusal duplicate-charge path; its regression passed in the full matrix.

Native UI behavior and the live company Batch provider have not been exercised on the integrated
head. Unit tests, compilation, and schema-upgrade fixtures do not establish those results.
