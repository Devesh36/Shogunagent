-- V53: L2 Clarify questions (issue #394).
--
-- An agent run may pause on ONE focused question ("Which Sam — Sam Lee or Sam Ortiz?"), with up to
-- three tap options and/or a free-text answer, and carries on when the user answers. The run lives
-- in the desktop (the Agent lane); the answer can come from the Notch or from any Memory API face
-- (`actions.answer` on MCP / REST / CLI), which run in other processes — so the question is a row
-- here, like a skill run (V44).
--
-- Provenance: `run_kind` / `run_id` name the run that asked (a `skill_runs` row, a Notch action, a
-- chat turn); the answer resumes that same run. `run_kind` is validated in Rust
-- (`shogun_agents::clarify::RunKind`) rather than CHECKed, so a new kind of run does not need a
-- table rebuild. No FK: a skill run can be deleted while its question is kept for the record.
--
-- Separation from L3: this table is NOT the L3 approval queue (`l3_approvals.json`) and holds no
-- action. Answering writes text for the run to read and grants nothing; a send the run makes
-- afterwards still waits for the full preview and the dedicated confirm.
--
-- Lifecycle: `awaiting_input` → `answered` | `expired` (deadline passed unanswered: the run keeps
-- its partial result as a draft and does nothing further) | `cancelled` (the run ended first).
-- At most one open question per run (the partial unique index).
--
-- `question` / `options` are agent-written (they can quote captured text) and `answer` is the
-- user's own words: local only, never logged, wiped by delete-all / delete-since. The answered
-- rows are also the "which questions were needless" signal for Work Patterns (#242).
--
-- Additive: one new table and three indexes. Old binaries ignore it.
-- Rollback: docs/migrations/V53-rollback.md.

CREATE TABLE clarify_questions (
    id            INTEGER PRIMARY KEY,
    run_kind      TEXT    NOT NULL,
    run_id        INTEGER NOT NULL,
    question      TEXT    NOT NULL,
    options       TEXT    NOT NULL DEFAULT '[]',
    free_text     INTEGER NOT NULL CHECK (free_text IN (0, 1)),
    status        TEXT    NOT NULL CHECK (status IN ('awaiting_input', 'answered', 'expired', 'cancelled')),
    answer        TEXT,
    answer_choice INTEGER,
    answer_origin TEXT    CHECK (answer_origin IS NULL OR answer_origin IN ('ui', 'api', 'mcp')),
    created_at    INTEGER NOT NULL,
    deadline_at   INTEGER NOT NULL,
    resolved_at   INTEGER,
    CHECK (deadline_at > created_at),
    CHECK ((status = 'answered') = (answer IS NOT NULL AND answer_origin IS NOT NULL))
) STRICT;

CREATE INDEX idx_clarify_questions_status ON clarify_questions (status, deadline_at);
CREATE INDEX idx_clarify_questions_run ON clarify_questions (run_kind, run_id);
CREATE UNIQUE INDEX idx_clarify_questions_one_open_per_run
    ON clarify_questions (run_kind, run_id) WHERE status = 'awaiting_input';
