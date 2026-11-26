-- Chat com o Claude sobre um review: perguntas do usuário e respostas, por execução.
ALTER TABLE review_runs ADD COLUMN chat_session_id TEXT;

CREATE TABLE review_chat (
    id         INTEGER PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES review_runs (id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'error')),
    content    TEXT NOT NULL,
    cost_usd   REAL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX review_chat_run ON review_chat (run_id, id);
