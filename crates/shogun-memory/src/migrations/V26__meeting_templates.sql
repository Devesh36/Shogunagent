-- Meeting templates (Recap shaping). Additive: two nullable columns.
--
-- `sessions.template` records which template the user chose for the interval ('one_on_one',
-- 'sales_call', …). NULL means the default 'general' — existing rows keep exactly their old
-- behavior. The registry of valid ids lives in code (shogun-core meeting::templates); an id that
-- stops existing degrades to 'general' at read time rather than failing the Recap.
--
-- `meeting_recaps.sections` holds the template-shaped sections the minutes came back with, as a
-- JSON array of {title, content}. NULL for untemplated minutes (and for every pre-V22 row).

ALTER TABLE sessions ADD COLUMN template TEXT;
ALTER TABLE meeting_recaps ADD COLUMN sections TEXT;
