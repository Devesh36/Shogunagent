-- Noticed moments (FR-PAT-06b, design §5e). Additive: one nullable column.
--
-- `noticed_at` NULL means "this pattern has never been shown as a noticed moment". The consumer
-- (Brief + Notch banner) picks ONE un-noticed row past the notice threshold, shows it exactly
-- once with Undo in place, and stamps it — one notice per pattern, ever. Deleting a row and
-- having behavior re-derive it makes it noticeable again, which is correct: it is a new pattern
-- to a profile that no longer contained it.

ALTER TABLE work_patterns ADD COLUMN noticed_at INTEGER;
