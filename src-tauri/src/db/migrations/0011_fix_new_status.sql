-- foreign_keys: off
-- Meus PRs: os comentários entram como 'new' (não analisados) e o usuário escolhe quais o
-- Claude analisa ("Analisar" em cada um, ou "Analisar todos"). Item 'new' não bloqueia o commit.
-- O SQLite não muda um CHECK: a tabela é recriada com as chaves estrangeiras desligadas, para o
-- DROP não apagar a conversa (fix_messages).

CREATE TABLE fix_comments_new (
    id            INTEGER PRIMARY KEY,
    fix_id        TEXT NOT NULL REFERENCES fix_sessions (id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    -- origem do item: comentário do Bitbucket ou achado de um review (run + seq do achado)
    bb_comment_id INTEGER,
    finding_run_id TEXT,
    finding_seq   INTEGER,
    author        TEXT,
    body          TEXT NOT NULL,
    path          TEXT,
    line          INTEGER,
    outdated      INTEGER NOT NULL DEFAULT 0 CHECK (outdated IN (0, 1)),
    url           TEXT,
    created_on    TEXT,
    -- respostas da thread: [{author, body, created_on}]
    replies_json  TEXT NOT NULL DEFAULT '[]',
    -- new: ainda não analisado (o usuário escolhe); queued: esperando o agente; analyzing: o agente
    -- está trabalhando; procede: sugestão esperando aprovação; nao_procede / resposta: só a
    -- explicação; applied: sugestão aprovada e gravada no worktree
    status        TEXT NOT NULL CHECK (status IN ('new', 'queued', 'analyzing', 'procede', 'nao_procede', 'resposta', 'applied', 'failed')),
    explanation   TEXT,
    -- [{file, line, end_line, existing_code, improved_code}]
    edits_json    TEXT NOT NULL DEFAULT '[]',
    error         TEXT,
    applied_at    TEXT,
    UNIQUE (fix_id, bb_comment_id),
    UNIQUE (fix_id, finding_run_id, finding_seq),
    UNIQUE (fix_id, seq)
);

INSERT INTO fix_comments_new SELECT id, fix_id, seq, bb_comment_id, finding_run_id, finding_seq, author, body, path, line,
    outdated, url, created_on, replies_json, status, explanation, edits_json, error, applied_at FROM fix_comments;

DROP TABLE fix_comments;
ALTER TABLE fix_comments_new RENAME TO fix_comments;
