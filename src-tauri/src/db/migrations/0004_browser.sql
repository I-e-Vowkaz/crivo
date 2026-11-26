-- Browser Tester: situação por execução e um registro por teste.
ALTER TABLE review_runs ADD COLUMN browser_status TEXT
    CHECK (browser_status IN ('blocked', 'running', 'completed', 'failed', 'cancelled'));
ALTER TABLE review_runs ADD COLUMN browser_note TEXT;

CREATE TABLE browser_tests (
    id            INTEGER PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES review_runs (id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    title         TEXT,
    url           TEXT,
    preconditions TEXT,
    actions_json  TEXT NOT NULL DEFAULT '[]',
    expected      TEXT,
    observed      TEXT,
    evidence      TEXT,
    status        TEXT NOT NULL CHECK (status IN ('aprovado', 'reprovado', 'bloqueado', 'nao_executado')),
    finding_ref   TEXT,
    UNIQUE (run_id, seq)
);
