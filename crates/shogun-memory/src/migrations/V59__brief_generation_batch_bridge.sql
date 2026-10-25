-- V59: bridge the main V55 Morning Brief ledger to Batch submission metadata.
-- Rollback: docs/migrations/V59-rollback.md.
-- V55 is already shipped on main, so retain its table, rows, provider_id and state CHECK.
-- These nullable columns let a Batch request keep its custom ID and submission timestamp
-- without rebuilding or discarding existing brief-generation history.
ALTER TABLE brief_generation ADD COLUMN expected_custom_id TEXT;
ALTER TABLE brief_generation ADD COLUMN submitted_at INTEGER;
