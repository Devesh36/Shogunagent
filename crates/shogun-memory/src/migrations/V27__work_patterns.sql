-- Continual learning v2 / Work Patterns (Issue #242, FR-PAT-03/04,
-- docs/continual-learning-v2-design.md). Additive: two new tables.
--
-- offer_impressions is the presentation log (FR-PAT-04): which action kinds were shown, where,
-- in which slot, with which app frontmost. "Ignored" is only derivable as presented − decided,
-- and a CTR needs a denominator. Metadata only, deliberately — never the action's content,
-- never captured text (same rule as V19's offer-context columns). High-volume, so rows older
-- than 30 days are purged by the writer (`work_patterns::record_impressions`).
--
-- work_patterns is the numeric behavior profile (FR-PAT-03): what the user does a lot
-- (activity), what they work on (focus), when they do it (rhythm), and what keeps being
-- off-target (avoid). Rows are RECOMPUTED nightly by the distillation job — never accumulated —
-- with the state-table discipline: confidence CHECKed, capped at 0.75 upstream (offered, never
-- asserted), and a user-visible `active` switch (Patterns UI, FR-PAT-06).

CREATE TABLE offer_impressions (
    id          INTEGER PRIMARY KEY,
    ts_ms       INTEGER NOT NULL,
    action_kind TEXT    NOT NULL,
    -- Same closed vocabulary as feedback_events.surface (V19).
    surface     TEXT    NOT NULL CHECK (surface IN ('notch', 'option_key', 'chat', 'recap', 'api')),
    -- Slot position when offered (0 = top). NULL when the surface does not rank.
    rank        INTEGER,
    -- Frontmost app bundle id at offer time (context, not content).
    context_app TEXT
) STRICT;

CREATE INDEX idx_offer_impressions_kind_ts ON offer_impressions (action_kind, ts_ms);
CREATE INDEX idx_offer_impressions_ts ON offer_impressions (ts_ms);

CREATE TABLE work_patterns (
    id           INTEGER PRIMARY KEY,
    kind         TEXT    NOT NULL CHECK (kind IN ('activity', 'focus', 'rhythm', 'avoid')),
    -- The pattern's subject: an action kind ('translate_ja_en'), a focus label ('ShogunAI'),
    -- or for avoid the suppressed action kind.
    key          TEXT    NOT NULL,
    -- 'global'; 'app' scopes avoid rows to a context bundle id; 'band' scopes rhythm rows to a
    -- time-of-day band (early/morning/afternoon/evening/night — closed vocabulary in code).
    scope        TEXT    NOT NULL CHECK (scope IN ('global', 'app', 'band')),
    -- '' means "no ref" (global rows). NOT NULL deliberately: a NULL here would make the UNIQUE
    -- constraint treat every global row as distinct (SQLite NULLs never equal), and the nightly
    -- upsert would append duplicates forever instead of replacing.
    scope_ref    TEXT    NOT NULL DEFAULT '',
    count_7d     INTEGER NOT NULL DEFAULT 0,
    count_30d    INTEGER NOT NULL DEFAULT 0,
    -- 7-day EWMA slope; the Brief's "translation is up 2× this week" supply.
    trend        REAL    NOT NULL DEFAULT 0.0,
    confidence   REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    active       INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    UNIQUE (kind, key, scope, scope_ref)
) STRICT;

CREATE INDEX idx_work_patterns_kind ON work_patterns (kind, active);
