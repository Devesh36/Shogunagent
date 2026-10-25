-- Keep prior samples unscoped; never infer recipients from historical text.
-- Rollback: docs/migrations/V36-rollback.md.
CREATE TABLE writing_voice_examples_scoped (
    id INTEGER PRIMARY KEY,
    app TEXT NOT NULL,
    person TEXT NOT NULL DEFAULT '' CHECK (length(person) <= 256),
    text TEXT NOT NULL CHECK (length(text) BETWEEN 12 AND 600),
    ts_ms INTEGER NOT NULL,
    UNIQUE(app, person, text)
);
INSERT INTO writing_voice_examples_scoped(id, app, person, text, ts_ms)
    SELECT id, app, '', text, ts_ms FROM writing_voice_examples;
DROP TABLE writing_voice_examples;
ALTER TABLE writing_voice_examples_scoped RENAME TO writing_voice_examples;
CREATE INDEX writing_voice_app ON writing_voice_examples(app, ts_ms DESC, id DESC);
CREATE INDEX writing_voice_person ON writing_voice_examples(app, person, ts_ms DESC, id DESC);
