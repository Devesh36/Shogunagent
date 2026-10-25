# Contributing

Thank you for considering a contribution. Two kinds are welcome: connector definitions for the router, and engine changes.

## Before you start

Read `CLAUDE.md` (the rules every change must keep) and `docs/implementation-plan.md` (what is planned and where it lives). Open an issue for anything larger than a connector or a bug fix so the design is agreed before the code.

## Connector definitions (lowest friction)

A connector describes how an external API is called, what it accepts and returns, and how usage is billed. It lives under `apps/router/connectors/<provider>/` and consists of a `provider.ts`, a `provider.json` listing manifest (legal name, contact, an opaque payout account reference — never bank data —, a price declaration per endpoint and the data-handling statement), one file per endpoint under `endpoints/`, and recorded fixtures for replay tests. The format is documented in `apps/router/connectors/schema.md` (§1–§9).

Checklist:
- The endpoint description is written for agents: name the task it serves (`Use for: …`), state the cost class, keep it ≤ 400 characters, no marketing superlatives; what it must not be used for goes in `exclusions`.
- The usage model is exact (flat per call, per result, or per unit) and failures cost zero.
- Fixtures let the replay tests run with no network.
- The permission level is declared and correct: reads are L1, writes that touch user data are L2, anything that sends externally or moves money is L3. An operation missing from the scope table is denied.
- No credentials in the definition. Credentials are injected at the transport layer by the host.

Then run the listing pipeline, which is what reviews your PR (`apps/router/connectors/schema.md` §10):

```bash
pnpm --filter @shogunagent/router listing:check connectors/<provider>   # schema, fixture replay, scope, description lint, price sanity; JSON report
pnpm --filter @shogunagent/router test
pnpm --filter @shogunagent/router build:catalog
pnpm --filter @shogunagent/selection-eval harness --mock && pnpm --filter @shogunagent/selection-eval gate --baseline results/baseline.json
```

`.github/workflows/router-listing.yml` runs the same check on every PR that touches `apps/router/connectors/**` and posts the report as the job summary. A passing report is required for listing; a maintainer still reviews the connector.

## Engine changes (Rust)

- Every crate builds and tests on Linux. Do not add macOS-only dependencies; desktop-only code goes behind the `desktop` feature.
- `cargo clippy --workspace --all-targets --locked` must be clean. `unwrap()` is denied outside tests.
- Schema changes are additive migrations `V<max+1>__<name>.sql` with a rollback note in `docs/migrations/`. Migrations already on `main` are frozen.
- Writes carry provenance and an actor. Reads stay free. Wire names of the core tools are frozen; add a new name and a deprecation entry rather than changing one.
- Changes to tool descriptions, `SKILL.md` or directory listings run the selection eval and must not lower the selection rate.

Local verification is listed in `CLAUDE.md`. CI runs the same commands.

## Commits and pull requests

Conventional Commits (`feat:`, `fix:`, `docs:`, `perf:`, `ci:`). One logical change per commit. Pull requests target `main`, describe the behavior change, and link the plan task (for example `E8-T3`) when one applies.

## License

By contributing you agree that your contribution is licensed under Apache-2.0, the license of this repository.
