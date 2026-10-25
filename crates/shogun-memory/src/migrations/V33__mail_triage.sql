-- V33: mail triage (issue #319, docs/specs/issue-319-mail-triage-spec.md §4-2).
--
-- Adds the per-message mail metadata that classification needs, the per-thread triage state
-- (a state table: provenance + confidence, never a fact below the confidence gate), the label
-- registry (built-in + custom), user feedback and the sender rules distilled from it, the Batch
-- refinement ledger, the incremental sync cursor, and the user's own mailbox identities.
--
-- Additive only: no existing table changes. Rollback: docs/migrations/V33-rollback.md.
--
-- No table here references `threads` — that table is a Dream-Cycle-rebuildable cache that
-- maintenance may clear wholesale (`maintenance.rs`), and a triage row must survive that.

-- One row per Gmail message. Headers are metadata (§6.9.2 read scope); attachments and images
-- are never stored — only presence flags (`has_ics`). `event_id` is the latest thread snapshot
-- event that carried this message (provenance); a deleted snapshot takes its messages with it.
CREATE TABLE mail_messages (
    message_id           TEXT    PRIMARY KEY,
    thread_key           TEXT    NOT NULL,
    event_id             INTEGER NOT NULL REFERENCES event_log(id) ON DELETE CASCADE,
    from_addr            TEXT    NOT NULL,
    from_name            TEXT,
    from_domain          TEXT    NOT NULL,
    to_addrs             TEXT    NOT NULL DEFAULT '[]',
    cc_addrs             TEXT    NOT NULL DEFAULT '[]',
    is_from_me           INTEGER NOT NULL DEFAULT 0 CHECK (is_from_me IN (0, 1)),
    in_reply_to          TEXT,
    gmail_label_ids      TEXT    NOT NULL DEFAULT '[]',
    has_list_unsubscribe INTEGER NOT NULL DEFAULT 0 CHECK (has_list_unsubscribe IN (0, 1)),
    has_ics              INTEGER NOT NULL DEFAULT 0 CHECK (has_ics IN (0, 1)),
    internal_date_ms     INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_mail_messages_thread ON mail_messages (thread_key, internal_date_ms DESC);
CREATE INDEX idx_mail_messages_from   ON mail_messages (from_addr);
CREATE INDEX idx_mail_messages_event  ON mail_messages (event_id);

-- The label registry. Built-ins are seeded below; customs are `custom:<slug>` with a natural
-- language `definition`. `auto_archive` / `sync_to_gmail` are the standing L2 approvals
-- (FR-TG-21) — both default OFF.
CREATE TABLE mail_labels (
    key            TEXT    PRIMARY KEY,
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('builtin', 'custom')),
    definition     TEXT,
    examples       TEXT    NOT NULL DEFAULT '[]',
    default_bucket TEXT    NOT NULL CHECK (default_bucket IN ('important', 'other')),
    auto_archive   INTEGER NOT NULL DEFAULT 0 CHECK (auto_archive IN (0, 1)),
    sync_to_gmail  INTEGER NOT NULL DEFAULT 0 CHECK (sync_to_gmail IN (0, 1)),
    gmail_label_id TEXT,
    enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    sort_order     INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
) STRICT;

INSERT INTO mail_labels (key, name, kind, default_bucket, sort_order, created_at_ms, updated_at_ms) VALUES
    ('people',       'People',        'builtin', 'important', 10, 0, 0),
    ('team',         'Team',          'builtin', 'important', 20, 0, 0),
    ('calendar',     'Calendar',      'builtin', 'important', 30, 0, 0),
    ('finance',      'Finance',       'builtin', 'important', 40, 0, 0),
    ('notification', 'Notifications', 'builtin', 'other',     50, 0, 0),
    ('news',         'News',          'builtin', 'other',     60, 0, 0),
    ('marketing',    'Marketing',     'builtin', 'other',     70, 0, 0),
    ('social',       'Social',        'builtin', 'other',     80, 0, 0),
    ('pitch',        'Pitch',         'builtin', 'other',     90, 0, 0);

-- One row per thread: the estimate. `reasons` is a JSON array of reason codes only (never body
-- text). `bucket_lock` is the safety rule that pins `important` (FR-TG-14). `stage='user'` rows
-- are never overwritten by Batch (FR-TG-02).
CREATE TABLE mail_triage (
    thread_key       TEXT    PRIMARY KEY,
    latest_event_id  INTEGER NOT NULL,
    label_key        TEXT    NOT NULL REFERENCES mail_labels(key),
    bucket           TEXT    NOT NULL CHECK (bucket IN ('important', 'other')),
    attention        TEXT    NOT NULL CHECK (attention IN ('needs_reply', 'waiting_on_them', 'fyi', 'none')),
    confidence       REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    stage            TEXT    NOT NULL CHECK (stage IN ('local', 'batch', 'user')),
    reasons          TEXT    NOT NULL DEFAULT '[]',
    bucket_lock      TEXT    CHECK (bucket_lock IN ('known_person', 'same_domain', 'my_reply', 'commitment', 'user')),
    -- 'pending' / 'unarchive_pending' are explicit requests recorded by the API faces (which hold
    -- no Composio transport); the desktop write-back tick executes them under a one-tap L2 grant.
    archive_status   TEXT    NOT NULL DEFAULT 'none' CHECK (archive_status IN ('none', 'pending', 'unarchive_pending', 'done', 'failed', 'undone')),
    archived_at_ms   INTEGER,
    handled_at_ms    INTEGER,
    open_loop_id     INTEGER,
    synced_label_key TEXT,
    classified_at_ms INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_mail_triage_bucket ON mail_triage (bucket, attention, updated_at_ms DESC);
CREATE INDEX idx_mail_triage_label  ON mail_triage (label_key, updated_at_ms DESC);
CREATE INDEX idx_mail_triage_stage  ON mail_triage (stage, confidence);

-- Batch refinement ledger (FR-TG-16): which threads went out in which provider batch, and
-- whether the result came back and was applied. The daily cap counts `submitted_at_ms`.
CREATE TABLE mail_triage_batch_items (
    batch_id        TEXT    NOT NULL,
    custom_id       TEXT    NOT NULL,
    thread_key      TEXT    NOT NULL,
    submitted_at_ms INTEGER NOT NULL,
    applied_at_ms   INTEGER,
    PRIMARY KEY (batch_id, custom_id)
) STRICT;
CREATE INDEX idx_mail_triage_batch_pending ON mail_triage_batch_items (applied_at_ms, submitted_at_ms);

-- Stage U corrections (FR-TG-19). Label keys only — never subject or body.
CREATE TABLE mail_triage_feedback (
    id             INTEGER PRIMARY KEY,
    ts_ms          INTEGER NOT NULL,
    thread_key     TEXT    NOT NULL,
    from_label_key TEXT    NOT NULL,
    to_label_key   TEXT    NOT NULL,
    from_bucket    TEXT    NOT NULL,
    to_bucket      TEXT    NOT NULL,
    origin         TEXT    NOT NULL CHECK (origin IN ('ui', 'api')),
    -- The mirrored `feedback_events` row (kind 'state_resolve', action_kind 'mail_relabel') that
    -- lesson distillation cites as provenance (V16 `lesson_provenance` references that table).
    feedback_event_id INTEGER,
    distilled      INTEGER NOT NULL DEFAULT 0 CHECK (distilled IN (0, 1))
) STRICT;
CREATE INDEX idx_mail_triage_feedback_undistilled ON mail_triage_feedback (distilled, ts_ms);

-- "This sender / domain is always X" — the deterministic, immediately effective learning.
CREATE TABLE mail_sender_rules (
    id            INTEGER PRIMARY KEY,
    match_kind    TEXT    NOT NULL CHECK (match_kind IN ('addr', 'domain')),
    match_value   TEXT    NOT NULL,
    label_key     TEXT    NOT NULL REFERENCES mail_labels(key),
    bucket        TEXT    CHECK (bucket IN ('important', 'other')),
    source        TEXT    NOT NULL CHECK (source IN ('user', 'feedback_x2')),
    created_at_ms INTEGER NOT NULL,
    UNIQUE (match_kind, match_value)
) STRICT;

-- Incremental read-sync cursor per service (FR-TG-09).
CREATE TABLE connector_sync_cursors (
    service       TEXT    PRIMARY KEY,
    watermark_ms  INTEGER NOT NULL,
    page_token    TEXT,
    updated_at_ms INTEGER NOT NULL
) STRICT;

-- The user's own mailbox addresses, learned from messages Gmail marks SENT. Drives the
-- `same_domain` lock and the "addressed to me" signal. Addresses only — no secret.
CREATE TABLE mail_identities (
    addr        TEXT    PRIMARY KEY,
    domain      TEXT    NOT NULL,
    seen_at_ms  INTEGER NOT NULL
) STRICT;

-- Execution history for the Gmail write-back (FR-AG-18 / FR-TG-21): every label / archive write,
-- under which approval (one-tap or the per-label standing approval), and whether it was undone.
-- Thread keys and label ids only — never subject or sender.
CREATE TABLE mail_write_log (
    id            INTEGER PRIMARY KEY,
    ts_ms         INTEGER NOT NULL,
    thread_key    TEXT    NOT NULL,
    action_kind   TEXT    NOT NULL CHECK (action_kind IN ('archive_thread', 'unarchive_thread', 'modify_labels')),
    add_label_ids TEXT    NOT NULL DEFAULT '[]',
    remove_label_ids TEXT NOT NULL DEFAULT '[]',
    approval      TEXT    NOT NULL CHECK (approval IN ('l2', 'l2_standing')),
    status        TEXT    NOT NULL CHECK (status IN ('ok', 'failed', 'undone')),
    error_class   TEXT,
    undone_at_ms  INTEGER
) STRICT;
CREATE INDEX idx_mail_write_log_ts ON mail_write_log (ts_ms DESC);
