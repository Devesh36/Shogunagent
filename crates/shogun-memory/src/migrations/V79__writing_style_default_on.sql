-- V79: writing-style examples ride along with rewrites by default (owner decision 2026-10-12).
-- Everyone who never answered the one-time offer is switched on; anyone who explicitly chose
-- (offer_state = 'decided', either way) keeps their choice. The Settings toggle stays.
-- Data-only UPDATE, no schema change. Rollback: docs/migrations/V79-rollback.md.
UPDATE writing_voice_settings
   SET enabled = 1, revision = revision + 1
 WHERE offer_state = 'pending' AND enabled = 0;
