-- Etapas e chamadas ao Claude de cada review (log de terminal, pipeline e custo por modelo).
--
-- run_steps: uma linha por etapa do review (as mesmas do `== …` do run.log), com início e fim.
-- run_calls: uma linha por processo `claude -p` (review, Haiku, juiz, Opus, navegador), com
-- modelo, custo, duração e como terminou. As execuções antigas, de antes desta migration, são
-- preenchidas a partir dos `*.result.json` da pasta delas (`imported = 1`), sem as etapas.

CREATE TABLE run_steps (
    run_id      TEXT NOT NULL REFERENCES review_runs (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    label       TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    PRIMARY KEY (run_id, seq)
);

CREATE TABLE run_calls (
    id          INTEGER PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES review_runs (id) ON DELETE CASCADE,
    -- etapa em que a chamada rodou (nula nas importadas)
    step_seq    INTEGER,
    -- review-sonnet, quality-haiku, judge-sonnet, audit-opus, browser-haiku
    name        TEXT NOT NULL,
    -- sonnet, haiku ou opus
    model       TEXT,
    status      TEXT NOT NULL CHECK (status IN ('completed', 'failed', 'cancelled', 'timed_out')),
    cost_usd    REAL NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    turns       INTEGER,
    started_at  TEXT,
    imported    INTEGER NOT NULL DEFAULT 0 CHECK (imported IN (0, 1))
);

CREATE INDEX run_calls_run ON run_calls (run_id);
CREATE INDEX run_calls_name ON run_calls (name);
