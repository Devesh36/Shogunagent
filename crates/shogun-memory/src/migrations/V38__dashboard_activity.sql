-- Content-free dashboard activity. App credits cite their canonical capture event so explicit
-- event deletion removes derived usage too; edit totals store no text, app, provider, or request.

CREATE TABLE dashboard_activity_credits (
    event_id     INTEGER NOT NULL REFERENCES event_log(id) ON DELETE CASCADE,
    local_date   TEXT    NOT NULL,
    captured_ms  INTEGER NOT NULL CHECK (captured_ms > 0),
    latest_activity_ms INTEGER NOT NULL,
    PRIMARY KEY (event_id, local_date)
) WITHOUT ROWID, STRICT;

CREATE INDEX idx_dashboard_activity_credits_date
    ON dashboard_activity_credits(local_date);

CREATE TABLE dashboard_edit_counts (
    local_date  TEXT    NOT NULL,
    kind        TEXT    NOT NULL CHECK (kind IN ('scribe_rewrite', 'inline_edit')),
    count       INTEGER NOT NULL CHECK (count > 0),
    latest_activity_ms INTEGER NOT NULL,
    PRIMARY KEY (local_date, kind)
) WITHOUT ROWID, STRICT;
