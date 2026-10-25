-- Rollback: docs/migrations/V98-rollback.md
-- V98 (ShogunAgent E2-T4): the writer and namespace of a user note. `memory.append_note` stores
-- its text here (V76), not in event_log, so the note needs the same two marks V92 gave events:
-- `actor` ("human:owner", "agent:<id>"; pre-V98 rows read as NULL, the owner's) and `namespace`
-- ('world', or 'agent_scratch:<agent_id>' for an agent's private notes, which default reads leave
-- out). Each version row keeps the actor that wrote it.
ALTER TABLE user_memory_notes ADD COLUMN actor TEXT;
ALTER TABLE user_memory_notes ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
ALTER TABLE user_memory_note_versions ADD COLUMN actor TEXT;
CREATE INDEX user_memory_notes_namespace ON user_memory_notes (namespace, updated_at);
