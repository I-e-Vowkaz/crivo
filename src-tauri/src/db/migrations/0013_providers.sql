-- foreign_keys: off
-- Repositórios de provedores diferentes podem ter o mesmo proprietário e nome.
CREATE TABLE repositories_new (
    id INTEGER PRIMARY KEY,
    workspace TEXT NOT NULL,
    slug TEXT NOT NULL,
    local_path TEXT,
    monitored INTEGER NOT NULL DEFAULT 1 CHECK (monitored IN (0, 1)),
    provider TEXT NOT NULL DEFAULT 'bitbucket' CHECK (provider IN ('bitbucket', 'github')),
    UNIQUE (provider, workspace, slug)
);
INSERT INTO repositories_new (id, workspace, slug, local_path, monitored)
SELECT id, workspace, slug, local_path, monitored FROM repositories;
DROP TABLE repositories;
ALTER TABLE repositories_new RENAME TO repositories;

CREATE TABLE credentials_new (
    kind TEXT PRIMARY KEY CHECK (kind IN ('bitbucket', 'jira', 'github')),
    keyring_service TEXT NOT NULL,
    keyring_account TEXT NOT NULL,
    scopes_json TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    validated_at TEXT
);
INSERT INTO credentials_new SELECT * FROM credentials;
DROP TABLE credentials;
ALTER TABLE credentials_new RENAME TO credentials;

CREATE TABLE published_findings (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES review_runs(id) ON DELETE CASCADE,
    finding_seq INTEGER NOT NULL,
    remote_id TEXT NOT NULL,
    remote_url TEXT,
    published_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (run_id, finding_seq)
);
