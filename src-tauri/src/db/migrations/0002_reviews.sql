-- Execuções de review (Sonnet pela skill + Opus condicional) e seus achados.
-- O stream bruto do Claude fica em arquivo, na pasta da execução (run_dir), não aqui.

CREATE TABLE review_runs (
    id           TEXT PRIMARY KEY,
    pr_id        INTEGER NOT NULL REFERENCES pull_requests (id) ON DELETE CASCADE,
    source_sha   TEXT,
    dest_sha     TEXT,
    card_key     TEXT,
    status       TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'cancelled')),
    stage        TEXT,
    audit_status TEXT CHECK (audit_status IN ('not_needed', 'running', 'completed', 'failed', 'cancelled')),
    started_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at  TEXT,
    cost_usd     REAL,
    error        TEXT,
    run_dir      TEXT NOT NULL
);

CREATE INDEX review_runs_pr ON review_runs (pr_id, started_at);

CREATE TABLE findings (
    id                 INTEGER PRIMARY KEY,
    run_id             TEXT NOT NULL REFERENCES review_runs (id) ON DELETE CASCADE,
    seq                INTEGER NOT NULL,
    project            TEXT,
    kind               TEXT,
    severity           TEXT,
    severity_reason    TEXT,
    impact             TEXT,
    file               TEXT,
    line               INTEGER,
    title              TEXT,
    description        TEXT,
    steps_json         TEXT NOT NULL DEFAULT '[]',
    pattern_ref        TEXT,
    confidence         TEXT,
    markdown           TEXT,
    -- conferido pelo app: o arquivo:linha existe no commit do PR?
    validation         TEXT NOT NULL CHECK (validation IN ('ok', 'rejected')),
    reject_reason      TEXT,
    in_diff            INTEGER,
    audit_decision     TEXT,
    audit_new_severity TEXT,
    audit_rationale    TEXT,
    UNIQUE (run_id, seq)
);
