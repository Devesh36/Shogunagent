# Migrations

SQL lives in `crates/shogun-memory/src/migrations/V<n>__<name>.sql` and is embedded by refinery
(`refinery::embed_migrations!`). Each `V<n>` needs a `V<n>-rollback.md` in this directory.

## Migration rules

refinery records every applied migration's version, name and checksum in
`refinery_schema_history`. If the binary's migration for an applied version has a different
name or different bytes (comments included), refinery aborts with `DivergentVersion` and the
database does not open.

1. **A migration is frozen once it is on main.** Never edit, rename or delete a merged or
   shipped `V<n>__*.sql`, not even a comment. To change the schema, add a new migration.
2. **Add new migrations as `V<max+1>__<name>.sql`**, where max is the highest version on main.
   Filenames must match `V<int>__<name>.sql` (word characters only). refinery silently skips
   anything else.
3. **Duplicate-version error?** Another branch merged the same `V<n>` first. Renumber yours
   above main's current max, and rename its rollback doc to match. Until your PR merges, your own
   migration isn't frozen, so you can renumber or edit it freely.
4. **Never edit `refinery_schema_history`** by hand to get a diverged database to open. That
   hides the divergence and leaves the schema in an unknown state.
5. Migrations stay additive (no DROP / RENAME / ALTER COLUMN) unless there is a reviewed
   exception with a rollback plan. See `scripts/check-migrations.py`.

### Enforcement

`scripts/check-migrations.py` runs in the `MIGRATION SAFETY — MUST PASS` job
(`.github/workflows/migration-safety.yml`, no `paths:` filter, so it can be a required check) on
every PR and every push to main. It also runs in `release.yml` before any tag build. It
compares each migration file's git blob id against a base ref and against every `v*` release
tag. Git history is the ledger: no lockfile to maintain. An unresolvable ref, or no release tags
at all, is an error, not a pass.

- **Base ref.** On a PR, the base it merges into (`HEAD^1` of GitHub's merge commit). On a push
  to main, main as it was before the push (`github.event.before`), so a multi-commit push can't
  hide an edit in an earlier commit. If that commit can't be fetched (a new branch, or a
  force-push), the job warns and falls back to `HEAD^1` plus the release tags. Every push run
  completes; only PR runs cancel each other.
- **Release tags** skip files whose version is above the highest version in the working tree. A
  hotfix tagged from an older line (say `v0.2.5.1` from `v0.2.5`) doesn't have the newer line's
  migrations, and that isn't a deletion. Files it shares with a tag must still match. The base
  ref has no such allowance, so deleting main's newest migration still fails there.

- **Rollback docs** (#780). Every `V<n>` must have `V<n>-rollback.md`, and any
  `refinery_schema_history WHERE version = …` step in it must name `n`. Deleting another
  version's row leaves that version unapplied below the max, so refinery aborts with
  `MissingVersion` and the database does not open with either binary. When an applied
  migration's inline SQL comment gives a wrong step, fix the rollback doc and say so there: the
  SQL file itself is frozen.

```sh
python3 scripts/check-migrations.py --self-test
git fetch origin --tags
python3 scripts/check-migrations.py --immutable-against-releases --immutable-against origin/main
```

## #800: how the fork happened (production not affected)

A developer database stopped opening with `DivergentVersion`. On the same day, five different
branches each created a `V64`, and in-flight migrations were edited after they had already
been applied locally. The divergent lineage came from an **unpushed local branch**.
**Production was not affected.** Every release tag from `v0.2.0` through `v0.2.6` ships
migration files that are byte-identical to main (same blob ids; `v0.1.0` matches too). The
checks above now fail on that situation before it can merge or ship.
