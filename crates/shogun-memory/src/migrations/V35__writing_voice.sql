-- Encrypted memory DB only. Rollback: docs/migrations/V35-rollback.md.
CREATE TABLE writing_voice_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 0
);
INSERT INTO writing_voice_settings (id) VALUES (1);

CREATE TABLE writing_voice_examples (
    id INTEGER PRIMARY KEY,
    app TEXT NOT NULL,
    text TEXT NOT NULL CHECK (length(text) BETWEEN 12 AND 600),
    ts_ms INTEGER NOT NULL,
    UNIQUE(app, text)
);
CREATE INDEX writing_voice_app ON writing_voice_examples(app, ts_ms DESC, id DESC);

-- Generated text is never a positive training signal. Digests prevent exact self-training.
CREATE TABLE writing_voice_outputs (
    app TEXT NOT NULL,
    digest TEXT NOT NULL,
    ts_ms INTEGER NOT NULL,
    PRIMARY KEY(app, digest)
);
