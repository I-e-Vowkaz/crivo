//! GitHub.com: credenciais locais e cliente HTTP. Tokens nunca são enviados à UI.
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use reqwest::{Client, Method};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::secrets::{SecretKind, SecretStore};
use rusqlite::{Connection, params};

const API: &str = "https://api.github.com";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub login: String,
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Repo {
    pub full_name: String,
}

#[derive(Debug, Deserialize)]
pub struct Branch {
    pub r#ref: String,
    pub sha: String,
    pub repo: Option<Repo>,
}

#[derive(Debug, Deserialize)]
pub struct Pull {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub user: User,
    pub head: Branch,
    pub base: Branch,
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
    #[serde(default)]
    pub review_comments: i64,
}

pub struct Api {
    http: Client,
    token: String,
    requests: AtomicU32,
}

impl Api {
    pub fn new(token: String) -> AppResult<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("crivo/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AppError::Invalid(e.to_string()))?;
        Ok(Self {
            http,
            token,
            requests: AtomicU32::new(0),
        })
    }

    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> AppResult<T> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains("..") {
            return Err(AppError::Invalid("caminho inválido para API GitHub".into()));
        }
        let mut req = self
            .http
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            req = req.json(body);
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        let res = req
            .send()
            .await
            .map_err(|e| AppError::Invalid(format!("GitHub: {e}")))?;
        let status = res.status();
        if !status.is_success() {
            return Err(AppError::Invalid(format!("GitHub HTTP {status} em {path}")));
        }
        res.json()
            .await
            .map_err(|e| AppError::Invalid(format!("resposta GitHub inválida: {e}")))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> AppResult<T> {
        self.request(Method::GET, path, None).await
    }

    pub async fn pulls(&self, owner: &str, repo: &str) -> AppResult<Vec<Pull>> {
        let mut out = Vec::new();
        for page in 1..=100 {
            let path = format!("/repos/{owner}/{repo}/pulls?state=open&per_page=100&page={page}");
            let mut batch: Vec<Pull> = self.get(&path).await?;
            let last = batch.len() < 100;
            out.append(&mut batch);
            if last {
                return Ok(out);
            }
        }
        Err(AppError::Invalid(format!(
            "GitHub {owner}/{repo}: mais de 10000 PRs; limite de paginação"
        )))
    }
}

pub async fn token(mode: &str, store: &dyn SecretStore) -> AppResult<String> {
    if mode == "token" {
        return store
            .get(&SecretKind::Github.account())?
            .ok_or_else(|| AppError::Invalid("salve um token GitHub nas Configurações".into()));
    }
    if mode != "gh" {
        return Err(AppError::Invalid(
            "modo de autenticação GitHub inválido".into(),
        ));
    }
    let out = tokio::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .await
        .map_err(|e| AppError::Invalid(format!("gh auth token: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Invalid(
            "execute gh auth login para conectar ao GitHub".into(),
        ));
    }
    crate::secrets::normalize_secret(&String::from_utf8_lossy(&out.stdout))
}

pub fn configured_repos(raw: Option<Value>) -> AppResult<Vec<(String, String)>> {
    let Some(Value::Array(items)) = raw else {
        return Ok(Vec::new());
    };
    let mut repos = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(AppError::Invalid(
                "repositório GitHub deve ser owner/repo".into(),
            ));
        };
        let Some((owner, repo)) = name.split_once('/') else {
            return Err(AppError::Invalid(format!(
                "repositório GitHub inválido: {name}"
            )));
        };
        let valid = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        };
        if !valid(owner) || !valid(repo) || repo.starts_with('.') {
            return Err(AppError::Invalid(format!(
                "repositório GitHub inválido: {name}"
            )));
        }
        repos.push((owner.to_owned(), repo.to_owned()));
    }
    Ok(repos)
}


/// Repasse temporário para `hosting::RepoRef::clone_path` (sai quando os chamadores usarem o `RepoRef`).
pub fn clone_path(root: &Path, owner: &str, repo: &str) -> std::path::PathBuf {
    crate::hosting::RepoRef { kind: crate::hosting::Kind::Github, owner: owner.into(), slug: repo.into() }.clone_path(root)
}

pub fn issue_refs(
    text: &str,
    default_owner: &str,
    default_repo: &str,
) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let word =
            word.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | ',' | '.' | ';'));
        let Some((prefix, number)) = word.rsplit_once('#') else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if number == 0 {
            continue;
        }
        let pair = if prefix.is_empty() {
            Some((default_owner, default_repo))
        } else {
            prefix.split_once('/')
        };
        let Some((owner, repo)) = pair else {
            continue;
        };
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        };
        if !valid(owner) || !valid(repo) {
            continue;
        }
        let item = (owner.to_owned(), repo.to_owned(), number);
        if !out.contains(&item) {
            out.push(item);
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

pub async fn sync_repo(
    db: &std::sync::Mutex<Connection>,
    api: &Api,
    me: &User,
    owner: &str,
    repo: &str,
    all: Vec<Pull>,
) -> AppResult<usize> {
    let mut relevant = Vec::new();
    for pr in all {
        let mine = pr.user.id == me.id;
        let requested = pr.requested_reviewers.iter().any(|u| u.id == me.id);
        let reviews: Vec<Value> = api
            .get(&format!(
                "/repos/{owner}/{repo}/pulls/{}/reviews?per_page=100",
                pr.number
            ))
            .await?;
        let reviewed_by_me = reviews
            .iter()
            .any(|r| r.pointer("/user/id").and_then(Value::as_i64) == Some(me.id));
        if !mine && !requested && !reviewed_by_me {
            continue;
        }
        let mut reviewers: std::collections::BTreeMap<String, Value> =
            std::collections::BTreeMap::new();
        for r in reviews {
            let Some(login) = r.pointer("/user/login").and_then(Value::as_str) else {
                continue;
            };
            let state = r.get("state").and_then(Value::as_str).unwrap_or("");
            if !matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
                continue;
            }
            reviewers.insert(login.to_owned(), serde_json::json!({"name": login, "uuid": login, "approved": state == "APPROVED", "state": if state == "APPROVED" { Some("approved") } else if state == "CHANGES_REQUESTED" { Some("changes_requested") } else { None }}));
        }
        for u in &pr.requested_reviewers {
            reviewers.entry(u.login.clone()).or_insert_with(|| serde_json::json!({"name": u.login, "uuid": u.login, "approved": false, "state": null}));
        }
        relevant.push((
            pr,
            mine,
            requested,
            reviewers.into_values().collect::<Vec<_>>(),
        ));
    }
    let mut conn = db
        .lock()
        .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
    let tx = conn.transaction()?;
    tx.execute("INSERT INTO repositories (provider, workspace, slug) VALUES ('github', ?1, ?2) ON CONFLICT(provider, workspace, slug) DO NOTHING", params![owner, repo])?;
    let repo_id: i64 = tx.query_row(
        "SELECT id FROM repositories WHERE provider = 'github' AND workspace = ?1 AND slug = ?2",
        params![owner, repo],
        |r| r.get(0),
    )?;
    let count = relevant.len();
    let mut seen = Vec::new();
    for (pr, mine, requested, reviewers) in relevant {
        let category = if mine && requested {
            "both"
        } else if mine {
            "authored"
        } else {
            "review_requested"
        };
        let changes = reviewers
            .iter()
            .any(|r| r.get("state").and_then(Value::as_str) == Some("changes_requested"));
        let prev: Option<String> = tx
            .query_row(
                "SELECT source_sha FROM pull_requests WHERE repository_id = ?1 AND bb_id = ?2",
                params![repo_id, pr.number],
                |r| r.get(0),
            )
            .ok();
        tx.execute("INSERT INTO pull_requests (repository_id, bb_id, title, description, url, author_uuid, author_name, reviewers_json, source_branch, dest_branch, source_sha, dest_sha, source_repo, state, created_on, updated_on, category, changes_requested, last_synced_sha, changed_since_sync, comment_count)
          VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'OPEN',?14,?15,?16,?17,?11,?18,?19)
          ON CONFLICT(repository_id, bb_id) DO UPDATE SET title=excluded.title, description=excluded.description, url=excluded.url, author_uuid=excluded.author_uuid, author_name=excluded.author_name, reviewers_json=excluded.reviewers_json, source_branch=excluded.source_branch, dest_branch=excluded.dest_branch, source_sha=excluded.source_sha, dest_sha=excluded.dest_sha, source_repo=excluded.source_repo, state='OPEN', updated_on=excluded.updated_on, category=excluded.category, changes_requested=excluded.changes_requested, changed_since_sync=excluded.changed_since_sync, comment_count=excluded.comment_count",
          params![repo_id, pr.number, pr.title, pr.body, pr.html_url, pr.user.login, pr.user.login, serde_json::to_string(&reviewers)?, pr.head.r#ref, pr.base.r#ref, pr.head.sha, pr.base.sha, pr.head.repo.as_ref().map(|r| &r.full_name), pr.created_at, pr.updated_at, category, changes, prev.is_some_and(|old| old != pr.head.sha), pr.review_comments])?;
        seen.push(pr.number);
    }
    let mut stmt =
        tx.prepare("SELECT bb_id FROM pull_requests WHERE repository_id = ?1 AND state = 'OPEN'")?;
    let old: Vec<i64> = stmt
        .query_map([repo_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for number in old.into_iter().filter(|id| !seen.contains(id)) {
        tx.execute(
            "UPDATE pull_requests SET state = 'GONE' WHERE repository_id = ?1 AND bb_id = ?2",
            params![repo_id, number],
        )?;
    }
    tx.commit()?;
    Ok(count)
}

pub async fn open_threads(
    api: &Api,
    owner: &str,
    repo: &str,
    number: i64,
    me: &str,
) -> AppResult<Vec<crate::hosting::Thread>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let query = r#"query($owner:String!,$repo:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$cursor){nodes{isResolved comments(first:100){nodes{databaseId body author{login} createdAt url path line originalLine}}}pageInfo{hasNextPage endCursor}}}}}"#;
        let response: Value = api.request(Method::POST, "/graphql", Some(&serde_json::json!({"query": query, "variables": {"owner": owner, "repo": repo, "number": number, "cursor": cursor}}))).await?;
        if response.get("errors").is_some() {
            return Err(AppError::Invalid(
                "GitHub GraphQL recusou a leitura das threads".into(),
            ));
        }
        let threads = response
            .pointer("/data/repository/pullRequest/reviewThreads")
            .ok_or_else(|| AppError::Invalid("GitHub não devolveu as threads da PR".into()))?;
        if let Some(nodes) = threads.get("nodes").and_then(Value::as_array) {
            for thread in nodes {
                if thread.get("isResolved").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(comments) = thread.pointer("/comments/nodes").and_then(Value::as_array)
                else {
                    continue;
                };
                let Some(first) = comments.first() else {
                    continue;
                };
                if first.pointer("/author/login").and_then(Value::as_str) == Some(me) {
                    continue;
                }
                let Some(id) = first.get("databaseId").and_then(Value::as_i64) else {
                    continue;
                };
                out.push(crate::hosting::Thread {
                    id,
                    author: first
                        .pointer("/author/login")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_owned(),
                    body: first
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    path: first.get("path").and_then(Value::as_str).map(str::to_owned),
                    line: first
                        .get("line")
                        .or_else(|| first.get("originalLine"))
                        .and_then(Value::as_i64),
                    outdated: first.get("line").is_none_or(Value::is_null),
                    url: first.get("url").and_then(Value::as_str).map(str::to_owned),
                    created_on: first
                        .get("createdAt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    replies: comments
                        .iter()
                        .skip(1)
                        .map(|c| crate::hosting::Reply {
                            author: c
                                .pointer("/author/login")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_owned(),
                            body: c
                                .get("body")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            created_on: c
                                .get("createdAt")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                        .collect(),
                });
            }
        }
        if threads
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            != Some(true)
        {
            break;
        }
        cursor = threads
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    for page in 1..=100 {
        let comments: Vec<Value> = api
            .get(&format!(
                "/repos/{owner}/{repo}/issues/{number}/comments?per_page=100&page={page}"
            ))
            .await?;
        let last = comments.len() < 100;
        for c in comments {
            if c.pointer("/user/login").and_then(Value::as_str) == Some(me) {
                continue;
            }
            let Some(id) = c.get("id").and_then(Value::as_i64) else {
                continue;
            };
            out.push(crate::hosting::Thread {
                id: -id,
                author: c
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                body: c
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                path: None,
                line: None,
                outdated: false,
                url: c.get("html_url").and_then(Value::as_str).map(str::to_owned),
                created_on: c
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                replies: Vec::new(),
            });
        }
        if last {
            break;
        }
    }
    Ok(out)
}

pub async fn push(repo_dir: &Path, full_name: &str, branch: &str, token: &str) -> AppResult<()> {
    let parts: Vec<&str> = full_name.split('/').collect();
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    if parts.len() != 2
        || !parts.iter().all(|s| valid(s))
        || !branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        return Err(AppError::Invalid(
            "destino GitHub inválido para push".into(),
        ));
    }
    let helper = std::env::temp_dir().join(format!("crivo-askpass-{}", uuid::Uuid::new_v4()));
    std::fs::write(
        &helper,
        "#!/bin/sh\ncase \"$1\" in *Username*) printf 'x-access-token';; *) printf '%s' \"$CRIVO_GH_TOKEN\";; esac\n",
    )?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))?;
    let result = tokio::process::Command::new("git")
        .args([
            "-c",
            "credential.helper=",
            "push",
            &format!("https://github.com/{full_name}.git"),
            &format!("HEAD:refs/heads/{branch}"),
        ])
        .current_dir(repo_dir)
        .env("GIT_ASKPASS", &helper)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("CRIVO_GH_TOKEN", token)
        .output()
        .await;
    let _ = std::fs::remove_file(&helper);
    let out = result.map_err(|e| AppError::Invalid(format!("git push GitHub: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Invalid(format!(
            "push GitHub recusado: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repos_exigem_owner_e_nome() {
        let repos = configured_repos(Some(serde_json::json!(["acme/front", "ana/back"]))).unwrap();
        assert_eq!(
            repos,
            vec![
                ("acme".into(), "front".into()),
                ("ana".into(), "back".into())
            ]
        );
        for bad in ["../front", "front", "acme/front/extra"] {
            assert!(
                configured_repos(Some(serde_json::json!([bad]))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn referencia_de_issue_explicita() {
        let refs = issue_refs(
            "corrige #12 e acme/docs#34, sem usar #12 de novo",
            "ana",
            "front",
        );
        assert_eq!(
            refs,
            vec![
                ("ana".into(), "front".into(), 12),
                ("acme".into(), "docs".into(), 34)
            ]
        );
    }
}
