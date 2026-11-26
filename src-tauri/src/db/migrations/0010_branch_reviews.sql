-- foreign_keys: off
-- Review de branch local (tela "Minhas branches"): a execução aponta para um PR ou para uma
-- branch local (repo_slug + branch, comparada com base_branch na origin). O SQLite não tira o
-- NOT NULL de uma coluna, então a tabela é recriada (procedimento de 12 passos da documentação,
-- com as chaves estrangeiras desligadas para o DROP não apagar achados, testes e conversa).

CREATE TABLE review_runs_new (
    id           TEXT PRIMARY KEY,
    pr_id        INTEGER REFERENCES pull_requests (id) ON DELETE CASCADE,
    repo_slug    TEXT,
    branch       TEXT,
    base_branch  TEXT,
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
    run_dir      TEXT NOT NULL,
    warning      TEXT,
    browser_status TEXT CHECK (browser_status IN ('blocked', 'running', 'completed', 'failed', 'cancelled')),
    browser_note TEXT,
    browser_checkout_pr_id INTEGER,
    chat_session_id TEXT,
    quality_status TEXT CHECK (quality_status IN ('running', 'completed', 'failed', 'cancelled')),
    CHECK (pr_id IS NOT NULL OR (repo_slug IS NOT NULL AND branch IS NOT NULL))
);

INSERT INTO review_runs_new (id, pr_id, source_sha, dest_sha, card_key, status, stage, audit_status, started_at,
    finished_at, cost_usd, error, run_dir, warning, browser_status, browser_note, browser_checkout_pr_id,
    chat_session_id, quality_status)
SELECT id, pr_id, source_sha, dest_sha, card_key, status, stage, audit_status, started_at,
    finished_at, cost_usd, error, run_dir, warning, browser_status, browser_note, browser_checkout_pr_id,
    chat_session_id, quality_status
FROM review_runs;

DROP TABLE review_runs;
ALTER TABLE review_runs_new RENAME TO review_runs;

CREATE INDEX review_runs_pr ON review_runs (pr_id, started_at);
CREATE INDEX review_runs_branch ON review_runs (repo_slug, branch, started_at);
