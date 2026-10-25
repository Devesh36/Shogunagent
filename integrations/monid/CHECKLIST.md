# Monid listing checklist (E31)

Tick an item only with evidence (a link, a commit, a written answer). Nothing here has been submitted; do not submit until every **blocking** item is ticked and the owner has approved.

## Blocking: answers from Monid (ask in writing; record the answer and date in README.md)

- [ ] 1. **Caller-provided credential.** Monid supports a per-run, caller-provided bearer (or per-user OAuth for a provider account) that `auth.inject` can read from `params`, and that is never stored with the run. (README open question 1)
- [ ] 2. **Category.** Monid adds `memory` (or `state`) to `connectors/categories.ts`, or we agree in writing on the category to use. (open question 2; the plan's condition for E31)
- [ ] 3. **Zero retention.** Inputs and outputs of `shogunagent/*` runs are not stored or returned by Monid's runs API, in writing. (open question 3; CLAUDE.md invariant 2)
- [ ] 4. **Terms and privacy.** Monid publishes terms and a privacy policy (they returned 404 in August 2026), and our counsel has read them.
- [ ] 5. **Price.** A $0 per-call endpoint is allowed, is not floored, and carries no fee to the caller. (open question 4; CLAUDE.md invariant 10)

## Blocking: our side

- [ ] 6. Import paths and helper names in `connectors/shogunagent/**` are aligned with Monid's reference connector `connectors/exa/` and DEVELOPMENT.md at the commit we fork from.
- [ ] 7. `deno task check`, `deno task lint`, `deno task test` (offline) and `deno task ids:check` pass in the fork.
- [ ] 8. Real fixtures recorded against a test principal (no real user data, headers not captured); one `synthetic-` failure fixture per endpoint kept.
- [ ] 9. Summaries, descriptions and input schemas still match `apps/router/connectors/shogunagent/endpoints/` (wire names frozen, invariant 11), and the selection eval (`pnpm --filter @shogunagent/selection-eval harness` + `gate`) does not fall with the Monid listing text.
- [ ] 10. The production REST face serves `GET /v1/state/commitments`, `GET /v1/state/open_loops` and `POST /v1/memory/recall` with agent-key auth, `401` without a key, and free metering (`read` unit).
- [ ] 11. A test principal and key exist for Monid's reviewers, with synthetic data only, revocable after review.
- [ ] 12. The owner has approved the submission (PR + one email to support@monid.ai with the PR link, `https://shogunai.si/docs/router/shogunagent` and the company name).

## After listing

- [ ] 13. Run counts per endpoint are visible to us, and the conversion of Monid callers to ingest is tracked (README §7 router KPI).
- [ ] 14. A deprecation path is agreed: Monid endpoint ids map to frozen wire names; a wire-name change is a new endpoint id, never an in-place edit.
