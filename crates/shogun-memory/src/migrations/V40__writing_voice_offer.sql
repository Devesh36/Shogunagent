-- V40: remember whether the one-time "use my writing style?" offer has been answered.
--
-- The writing-style setting now gates only what leaves the device: examples are learned
-- locally regardless (they are the user's own drafts, already in the local capture log),
-- and `enabled` decides whether up to four of them ride along to the model provider. The
-- notch asks once — after enough examples exist for the feature to show its value — and
-- never again after any answer, in Settings, the API or the offer itself.
--
-- Additive: one column with a constant default. Rollback: docs/migrations/V40-rollback.md.
ALTER TABLE writing_voice_settings
    ADD COLUMN offer_state TEXT NOT NULL DEFAULT 'pending'
        CHECK (offer_state IN ('pending', 'decided'));
