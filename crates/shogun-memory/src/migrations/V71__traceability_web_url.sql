-- Rollback: docs/migrations/V71-rollback.md
-- V71: a public web fetch's traceability row keeps the full URL it read (issue #928).
--
-- `destination` holds the host. That says "linkedin.com" but not which page, so the user cannot
-- tell a profile from a job post when reviewing what SHOGUN fetched for them. `url` keeps the
-- address, path and query, after `web_fetch::ledger_url` drops the fragment and userinfo and masks
-- credential-looking parameters and path segments (invariant 7). Only `route = 'web'` rows set it; every other route
-- leaves it NULL. Existing rows stay NULL: their URL was never kept, only its digest.
--
-- Additive only: one nullable column, no default, no CHECK. Older binaries name their columns on
-- INSERT and SELECT, so they ignore it. A future traceability_log rebuild (the V18 / V46 pattern)
-- must carry it across, as with V67's `kind`.

ALTER TABLE traceability_log ADD COLUMN url TEXT;
