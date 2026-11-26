-- Correção automática: módulo próprio, separado do review.
-- - Meus PRs: os comentários de outras pessoas no PR (kind = 'pr').
-- - Minhas branches: os achados e melhorias do review de uma branch local (kind = 'branch').
-- Um Sonnet confere cada item no código e propõe a correção como sugestão; nada é gravado no
-- código até o usuário aprovar. Resolvidos todos, o committer faz o commit; o app faz o push
-- (no PR, logo depois; na branch local, avança a branch e o push é um botão à parte).

-- comentários no PR, como vem da listagem do Bitbucket (inclui os meus)
ALTER TABLE pull_requests ADD COLUMN comment_count INTEGER;

CREATE TABLE fix_sessions (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('pr', 'branch')),
    pr_id       INTEGER REFERENCES pull_requests (id) ON DELETE CASCADE,
    -- branch local (kind = 'branch'): clone em <pasta dos repositórios>/<repo_slug>
    repo_slug   TEXT,
    branch      TEXT,
    -- base de comparação (destino do PR, ou dev/branch padrão da origin na branch local)
    base_branch TEXT,
    -- ponta da branch quando a análise começou: o worktree parte dela
    base_sha    TEXT,
    run_dir     TEXT NOT NULL,
    session_id  TEXT,
    card_key    TEXT,
    -- committed: só na branch local (commit feito e branch avançada, push ainda não)
    status      TEXT NOT NULL CHECK (status IN ('analyzing', 'ready', 'failed', 'cancelled', 'committing', 'committed', 'pushed')),
    stage       TEXT,
    error       TEXT,
    cost_usd    REAL NOT NULL DEFAULT 0,
    commit_sha  TEXT,
    commit_subject TEXT,
    pushed_at   TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK ((kind = 'pr' AND pr_id IS NOT NULL) OR (kind = 'branch' AND repo_slug IS NOT NULL AND branch IS NOT NULL))
);

CREATE INDEX fix_sessions_pr ON fix_sessions (pr_id, created_at);
CREATE INDEX fix_sessions_branch ON fix_sessions (repo_slug, branch, created_at);

CREATE TABLE fix_comments (
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
    -- queued: esperando o agente; analyzing: o agente está trabalhando; procede: sugestão esperando aprovação;
    -- nao_procede / resposta: só a explicação; applied: sugestão aprovada e gravada no worktree
    status        TEXT NOT NULL CHECK (status IN ('queued', 'analyzing', 'procede', 'nao_procede', 'resposta', 'applied', 'failed')),
    explanation   TEXT,
    -- [{file, line, end_line, existing_code, improved_code}]
    edits_json    TEXT NOT NULL DEFAULT '[]',
    error         TEXT,
    applied_at    TEXT,
    UNIQUE (fix_id, bb_comment_id),
    UNIQUE (fix_id, finding_run_id, finding_seq),
    UNIQUE (fix_id, seq)
);

CREATE TABLE fix_messages (
    id         INTEGER PRIMARY KEY,
    fix_id     TEXT NOT NULL REFERENCES fix_sessions (id) ON DELETE CASCADE,
    comment_id INTEGER NOT NULL REFERENCES fix_comments (id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'error')),
    content    TEXT NOT NULL,
    cost_usd   REAL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX fix_messages_comment ON fix_messages (comment_id, id);
