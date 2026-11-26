-- Fase 1: configuração, credenciais (só referências ao keyring) e PRs do Bitbucket.
-- Jira e engine de review entram em migrations próprias quando chegarem.
-- Datas em texto ISO 8601 UTC.

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
);

-- Nunca guarda o segredo: só onde ele está no keyring e os metadados da validação.
CREATE TABLE credentials (
    kind            TEXT PRIMARY KEY CHECK (kind IN ('bitbucket', 'jira')),
    keyring_service TEXT NOT NULL,
    keyring_account TEXT NOT NULL,
    scopes_json     TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    validated_at    TEXT
);

CREATE TABLE repositories (
    id         INTEGER PRIMARY KEY,
    workspace  TEXT NOT NULL,
    slug       TEXT NOT NULL,
    local_path TEXT,
    monitored  INTEGER NOT NULL DEFAULT 1 CHECK (monitored IN (0, 1)),
    UNIQUE (workspace, slug)
);

CREATE TABLE sync_runs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('incremental', 'full')),
    status      TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    requests    INTEGER NOT NULL DEFAULT 0,
    error       TEXT
);

CREATE TABLE pull_requests (
    id                 INTEGER PRIMARY KEY,
    repository_id      INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    bb_id              INTEGER NOT NULL,
    title              TEXT NOT NULL,
    description        TEXT,
    url                TEXT NOT NULL,
    author_uuid        TEXT NOT NULL,
    author_name        TEXT,
    reviewers_json     TEXT NOT NULL DEFAULT '[]',
    participants_json  TEXT NOT NULL DEFAULT '[]',
    source_branch      TEXT NOT NULL,
    dest_branch        TEXT NOT NULL,
    source_sha         TEXT NOT NULL,
    dest_sha           TEXT NOT NULL,
    source_repo        TEXT,
    state              TEXT NOT NULL,
    created_on         TEXT NOT NULL,
    updated_on         TEXT NOT NULL,
    category           TEXT NOT NULL CHECK (category IN ('review_requested', 'authored', 'both')),
    -- algum participante com state = changes_requested (achado 15 da Fase 0)
    changes_requested  INTEGER NOT NULL DEFAULT 0 CHECK (changes_requested IN (0, 1)),
    diffstat_json      TEXT,
    last_synced_sha    TEXT,
    changed_since_sync INTEGER NOT NULL DEFAULT 0 CHECK (changed_since_sync IN (0, 1)),
    synced_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (repository_id, bb_id)
);

CREATE INDEX pull_requests_state_category ON pull_requests (state, category);
