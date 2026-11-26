//! A factory: `Provider::connect` devolve o cliente autenticado do provedor, e os métodos fazem o
//! que depende da API (sync, comentários, push, contexto extra). O Bitbucket é só leitura.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use serde_json::{Value, json};

use super::{Kind, RepoRef, RepoSync, Thread};
use crate::bitbucket::{self, client::Client};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::github;
use crate::review;
use crate::secrets::{SecretKind, SecretStore};

pub struct BitbucketSession {
    pub client: Client,
    pub workspace: String,
}

pub struct GithubSession {
    pub api: github::Api,
    pub token: String,
}

pub enum Provider {
    Bitbucket(BitbucketSession),
    Github(GithubSession),
}

fn lock(db: &Mutex<Connection>) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
    db.lock().map_err(|_| AppError::Invalid("conexão com o banco indisponível".into()))
}

/// Repositórios monitorados do Bitbucket: `bitbucket.repos` (lista ou texto separado por vírgula).
fn bitbucket_repos(conn: &Connection) -> AppResult<Vec<String>> {
    let list: Vec<String> = match db::get_setting(conn, "bitbucket.repos")?.unwrap_or(Value::Null) {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        Value::String(s) => s.split(',').map(str::to_owned).collect(),
        _ => vec![],
    };
    Ok(list.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
}

fn github_mode(conn: &Connection) -> AppResult<String> {
    Ok(db::get_setting(conn, "github.auth_mode")?.and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "gh".into()))
}

/// `git fetch --prune` no clone local, para o Checkout e a aba Diff funcionarem sem rede. Clone
/// ausente ou de outro repositório vira aviso, sem fetch.
async fn fetch_clone(repo: &RepoRef, root: &Path) -> Option<String> {
    match repo.clone_dir(root) {
        Ok(dir) => review::git::run(&dir, &["fetch", "--prune", "-q", "origin"]).await.err().map(|e| e.to_string()),
        Err(e) => Some(e.to_string()),
    }
}

impl Provider {
    /// Provedores com repositório configurado, na ordem em que o sync os percorre.
    pub fn configured(conn: &Connection) -> AppResult<Vec<Kind>> {
        let mut out = Vec::new();
        let ws = db::get_setting(conn, "bitbucket.workspace")?.and_then(|v| v.as_str().map(str::to_owned));
        if ws.is_some_and(|s| !s.trim().is_empty()) {
            out.push(Kind::Bitbucket);
        }
        // só a presença decide: um `owner/repo` mal escrito vira erro do sync do GitHub, sem tirar
        // o Bitbucket do sync
        let gh = db::get_setting(conn, "github.repos")?;
        if gh.as_ref().and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
            out.push(Kind::Github);
        }
        Ok(out)
    }

    /// A factory: o cliente do provedor com as credenciais salvas. Não chama a API.
    pub async fn connect(kind: Kind, db: &Mutex<Connection>, store: &dyn SecretStore) -> AppResult<Provider> {
        match kind {
            Kind::Bitbucket => {
                let (email, workspace) = bitbucket::account_settings(&*lock(db)?)?;
                let token = store
                    .get(&SecretKind::Bitbucket.account())?
                    .ok_or_else(|| AppError::Invalid("salve o token do Bitbucket nas Configurações".into()))?;
                Ok(Provider::Bitbucket(BitbucketSession { client: Client::new(&email, &token)?, workspace }))
            }
            Kind::Github => {
                let mode = github_mode(&*lock(db)?)?;
                let token = github::token(&mode, store).await?;
                Ok(Provider::Github(GithubSession { api: github::Api::new(token.clone())?, token }))
            }
        }
    }

    pub fn requests(&self) -> u32 {
        match self {
            Provider::Bitbucket(s) => s.client.requests(),
            Provider::Github(s) => s.api.requests(),
        }
    }

    /// Busca os PRs dos repositórios configurados, grava, e atualiza cada clone. Devolve o meu
    /// nome no provedor e o resultado por repositório.
    pub async fn sync(&self, db: &Mutex<Connection>) -> AppResult<(String, Vec<RepoSync>)> {
        let sync_id: i64 = {
            let conn = lock(db)?;
            conn.execute("INSERT INTO sync_runs (kind, status) VALUES ('full', 'running')", [])?;
            conn.last_insert_rowid()
        };
        let done = self.sync_repos(db).await;
        let failed: Vec<String> = match &done {
            Ok((_, out)) => out.iter().filter_map(|r| r.error.as_ref().map(|e| format!("{}: {e}", r.slug))).collect(),
            Err(e) => vec![e.to_string()],
        };
        lock(db)?.execute(
            "UPDATE sync_runs SET status = ?1, finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), requests = ?2, error = ?3
             WHERE id = ?4",
            rusqlite::params![
                if failed.is_empty() { "completed" } else { "failed" },
                self.requests(),
                (!failed.is_empty()).then(|| failed.join("; ")),
                sync_id
            ],
        )?;
        done
    }

    async fn sync_repos(&self, db: &Mutex<Connection>) -> AppResult<(String, Vec<RepoSync>)> {
        let root = review::repos_root(&*lock(db)?)?;
        let mut out = Vec::new();
        let user = match self {
            Provider::Bitbucket(s) => {
                let me: bitbucket::User = s.client.get_json("/user", &[]).await?.body;
                // quem sou eu, para marcar os PRs que já aprovei
                db::set_setting(&*lock(db)?, "bitbucket.me", &json!({"uuid": me.uuid, "display_name": me.display_name}))?;
                let repos = bitbucket_repos(&*lock(db)?)?;
                for slug in repos {
                    // um nome mal digitado (ex.: `ws/app`) falha só o próprio repositório, sem ir à API
                    let rr = match RepoRef::new("bitbucket", s.workspace.clone(), slug.clone()) {
                        Ok(rr) => rr,
                        Err(e) => {
                            out.push(RepoSync { slug, open: 0, error: Some(e.to_string()), fetch_error: None });
                            continue;
                        }
                    };
                    let fetched = bitbucket::sync::fetch_repo(&s.client, &s.workspace, &slug, &me.uuid).await;
                    let (open, error) = match fetched {
                        Ok(prs) => {
                            let n = bitbucket::sync::upsert_repo_prs(&mut *lock(db)?, &s.workspace, &slug, &prs)?;
                            (n, None)
                        }
                        Err(e) => (0, Some(e.to_string())),
                    };
                    out.push(RepoSync { slug, open, error, fetch_error: fetch_clone(&rr, &root).await });
                }
                me.display_name
            }
            Provider::Github(s) => {
                let me: github::User = s.api.get("/user").await?;
                db::set_setting(&*lock(db)?, "github.me", &json!({"login": me.login, "id": me.id}))?;
                let repos = github::configured_repos(db::get_setting(&*lock(db)?, "github.repos")?)?;
                for (owner, name) in repos {
                    let (open, error) = match s.api.pulls(&owner, &name).await {
                        Ok(pulls) => match github::sync_repo(db, &s.api, &me, &owner, &name, pulls).await {
                            Ok(n) => (n, None),
                            Err(e) => (0, Some(e.to_string())),
                        },
                        Err(e) => (0, Some(e.to_string())),
                    };
                    let rr = RepoRef::new("github", owner, name)?;
                    out.push(RepoSync { slug: format!("{}/{}", rr.owner, rr.slug), open, error, fetch_error: fetch_clone(&rr, &root).await });
                }
                me.login
            }
        };
        Ok((user, out))
    }

    /// Comentários abertos de outras pessoas no PR; `me` é o de `hosting::me`.
    pub async fn open_threads(&self, repo: &RepoRef, number: i64, me: &str) -> AppResult<Vec<Thread>> {
        match self {
            Provider::Bitbucket(s) => bitbucket::comments::open_threads(&s.client, &s.workspace, &repo.slug, number, me).await,
            Provider::Github(s) => github::open_threads(&s.api, &repo.owner, &repo.slug, number, me).await,
        }
    }

    /// Push da correção para a branch do PR, sem force. No GitHub vai para `source_repo` (o fork,
    /// quando o PR vem de um) com o token; no Bitbucket, pelo `origin` do clone.
    pub async fn push(&self, wt: &Path, source_repo: &str, branch: &str) -> AppResult<()> {
        match self {
            Provider::Bitbucket(_) => review::git::run(wt, &["push", "origin", &format!("HEAD:refs/heads/{branch}")]).await.map(|_| ()),
            Provider::Github(s) => github::push(wt, source_repo, branch, &s.token).await,
        }
    }

    /// Publica um review com os achados no PR e devolve o ID e o link dele. Só no GitHub: o Crivo
    /// não escreve no Bitbucket. Se um review meu com o mesmo `marker` já existe (publicação
    /// repetida depois de uma falha), devolve esse em vez de criar outro.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish(
        &self,
        repo: &RepoRef,
        number: i64,
        sha: &str,
        body: &str,
        marker: &str,
        inline: &[Value],
        request_changes: bool,
    ) -> AppResult<(String, Option<String>)> {
        let Provider::Github(s) = self else {
            return Err(AppError::Invalid("o Crivo não escreve no Bitbucket".into()));
        };
        let (owner, name) = (&repo.owner, &repo.slug);
        let current: Value = s.api.get(&format!("/repos/{owner}/{name}/pulls/{number}")).await?;
        if current.get("state").and_then(Value::as_str) != Some("open") {
            return Err(AppError::Invalid("a PR já não está aberta".into()));
        }
        if current.pointer("/head/sha").and_then(Value::as_str) != Some(sha) {
            return Err(AppError::Invalid("a PR recebeu novos commits; revise novamente antes de publicar".into()));
        }
        let me: github::User = s.api.get("/user").await?;
        let mut prior: Option<Value> = None;
        for page in 1..=100 {
            let reviews: Vec<Value> =
                s.api.get(&format!("/repos/{owner}/{name}/pulls/{number}/reviews?per_page=100&page={page}")).await?;
            let last = reviews.len() < 100;
            prior = reviews.into_iter().find(|r| {
                r.pointer("/user/login").and_then(Value::as_str) == Some(me.login.as_str())
                    && r.get("body").and_then(Value::as_str).is_some_and(|b| b.contains(marker))
            });
            if prior.is_some() || last {
                break;
            }
        }
        let response: Value = match prior {
            Some(prior) => prior,
            None => {
                let event = if request_changes { "REQUEST_CHANGES" } else { "COMMENT" };
                s.api
                    .request(
                        reqwest::Method::POST,
                        &format!("/repos/{owner}/{name}/pulls/{number}/reviews"),
                        Some(&json!({"commit_id": sha, "event": event, "body": body, "comments": inline})),
                    )
                    .await?
            }
        };
        let remote = response
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| AppError::Invalid("GitHub não devolveu ID do review".into()))?
            .to_string();
        Ok((remote, response.get("html_url").and_then(Value::as_str).map(str::to_owned)))
    }

    /// Texto a acrescentar ao contexto do review: as issues citadas no PR (GitHub). Os avisos vão
    /// para o log de quem chamou.
    pub async fn extra_context(&self, repo: &RepoRef, title: &str, body: &str, log: &(dyn Fn(&str) + Sync)) -> String {
        let Provider::Github(s) = self else { return String::new() };
        let mut out = String::new();
        for (owner, name, number) in github::issue_refs(&format!("{title} {body}"), &repo.owner, &repo.slug) {
            let issue: Value = match s.api.get(&format!("/repos/{owner}/{name}/issues/{number}")).await {
                Ok(issue) => issue,
                Err(e) => {
                    log(&format!("AVISO: não consegui ler a issue {owner}/{name}#{number}: {e}"));
                    continue;
                }
            };
            if issue.get("pull_request").is_some() {
                continue;
            }
            out.push_str(&format!(
                "\n## GitHub Issue {owner}/{name}#{number}\n\n{}\n\n{}\n",
                issue.get("title").and_then(Value::as_str).unwrap_or(""),
                issue.get("body").and_then(Value::as_str).unwrap_or("")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitbucket::client::testserver::{ok, start};
    use serde_json::json;

    #[test]
    fn configurados_conforme_as_configuracoes() {
        let conn = crate::db::open_in_memory().unwrap();
        assert!(Provider::configured(&conn).unwrap().is_empty());
        crate::db::set_setting(&conn, "bitbucket.workspace", &json!("  ")).unwrap();
        assert!(Provider::configured(&conn).unwrap().is_empty(), "workspace em branco não conta");
        crate::db::set_setting(&conn, "github.repos", &json!(["acme/app"])).unwrap();
        assert_eq!(Provider::configured(&conn).unwrap(), vec![Kind::Github]);
        crate::db::set_setting(&conn, "bitbucket.workspace", &json!("ws")).unwrap();
        assert_eq!(Provider::configured(&conn).unwrap(), vec![Kind::Bitbucket, Kind::Github]);
    }

    #[test]
    fn github_mal_configurado_nao_tira_o_bitbucket() {
        let conn = crate::db::open_in_memory().unwrap();
        crate::db::set_setting(&conn, "bitbucket.workspace", &json!("ws")).unwrap();
        crate::db::set_setting(&conn, "github.repos", &json!(["https://github.com/acme/app"])).unwrap();
        assert_eq!(Provider::configured(&conn).unwrap(), vec![Kind::Bitbucket, Kind::Github]);
    }

    #[tokio::test]
    async fn sync_do_bitbucket_pela_factory() {
        let pr = r#"{"id":1,"title":"PROJ-1","state":"OPEN","created_on":"2026-11-01T00:00:00Z","updated_on":"2026-11-02T00:00:00Z",
            "links":{"html":{"href":"https://bb/1"}},"author":{"uuid":"{me}","display_name":"Eu"},
            "source":{"branch":{"name":"PROJ-1"},"commit":{"hash":"s1"},"repository":{"full_name":"ws/r"}},
            "destination":{"branch":{"name":"dev"},"commit":{"hash":"d1"}}}"#;
        let srv = start(vec![
            ok(r#"{"uuid":"{me}","display_name":"Eu"}"#),
            ok(r#"{"values":[]}"#),
            ok(&format!(r#"{{"values":[{pr}]}}"#)),
        ]);
        let root = std::env::temp_dir().join(format!("crivo-provider-{}", std::process::id()));
        let db = std::sync::Mutex::new(crate::db::open_in_memory().unwrap());
        {
            let conn = db.lock().unwrap();
            crate::db::set_setting(&conn, "bitbucket.repos", &json!(["r"])).unwrap();
            crate::db::set_setting(&conn, "repos.root", &json!(root.display().to_string())).unwrap();
        }
        let client = crate::bitbucket::client::Client::with_base(&srv.base, "e@x", "tok-secreto-123456").unwrap();
        let p = Provider::Bitbucket(BitbucketSession { client, workspace: "ws".into() });
        let (user, repos) = p.sync(&db).await.unwrap();
        assert_eq!(user, "Eu");
        assert_eq!((repos[0].slug.as_str(), repos[0].open, repos[0].error.is_none()), ("r", 1, true));
        assert!(repos[0].fetch_error.as_deref().is_some_and(|e| e.contains("não achei o clone local")));
        let conn = db.lock().unwrap();
        assert_eq!(crate::hosting::me(&conn, Kind::Bitbucket).unwrap().as_deref(), Some("{me}"));
        let (cat, status): (String, String) = conn
            .query_row("SELECT p.category, s.status FROM pull_requests p, sync_runs s", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((cat.as_str(), status.as_str()), ("authored", "completed"));
        assert_eq!(p.requests(), 3);
    }

    #[tokio::test]
    async fn bitbucket_recusa_publicar_sem_chamar_a_api() {
        let srv = start(vec![]);
        let client = crate::bitbucket::client::Client::with_base(&srv.base, "e@x", "tok-secreto-123456").unwrap();
        let p = Provider::Bitbucket(BitbucketSession { client, workspace: "ws".into() });
        let rr = RepoRef::new("bitbucket", "ws", "r").unwrap();
        let err = p.publish(&rr, 1, "abc", "corpo", "<!-- m -->", &[], false).await.unwrap_err();
        assert_eq!(err.to_string(), "o Crivo não escreve no Bitbucket");
        assert_eq!(p.requests(), 0);
    }

    #[tokio::test]
    async fn slug_invalido_falha_so_o_proprio_repositorio() {
        let srv = start(vec![ok(r#"{"uuid":"{me}","display_name":"Eu"}"#), ok(r#"{"values":[]}"#), ok(r#"{"values":[]}"#)]);
        let db = std::sync::Mutex::new(crate::db::open_in_memory().unwrap());
        crate::db::set_setting(&db.lock().unwrap(), "bitbucket.repos", &json!(["ws/app", "r"])).unwrap();
        let client = crate::bitbucket::client::Client::with_base(&srv.base, "e@x", "tok-secreto-123456").unwrap();
        let p = Provider::Bitbucket(BitbucketSession { client, workspace: "ws".into() });
        let (_, repos) = p.sync(&db).await.unwrap();
        assert_eq!(repos.len(), 2);
        assert!(repos[0].slug == "ws/app" && repos[0].error.as_deref().is_some_and(|e| e.contains("inválido")), "{:?}", repos[0]);
        assert!(repos[1].slug == "r" && repos[1].error.is_none(), "{:?}", repos[1]);
        assert_eq!(p.requests(), 3, "o slug inválido não vai para a API");
    }

    #[tokio::test]
    async fn sync_sem_provedor_configurado_e_erro() {
        let db = std::sync::Mutex::new(crate::db::open_in_memory().unwrap());
        let err = crate::commands::sync_all(&db, &crate::secrets::MemoryStore::default()).await.unwrap_err();
        assert_eq!(err.to_string(), "configure ao menos um repositório Bitbucket ou GitHub");
    }
}
