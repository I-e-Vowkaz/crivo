//! Comandos chamados pela UI (`invoke`). Nenhum devolve segredo.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;
use tauri::{Emitter, State};

use crate::bitbucket::{self, Account, client::Client};
use crate::db;
use crate::github;
use crate::review;
use crate::error::{AppError, AppResult};
use crate::secrets::{self, KeyringStore, SecretKind, SecretStore};

pub struct AppState {
    /// Gatilhos (processos de longa duração das configurações de execução).
    pub triggers: std::sync::Arc<crate::triggers::manager::Manager>,
    pub db: Mutex<Connection>,
    pub secrets: Box<dyn SecretStore>,
    pub db_path: PathBuf,
    pub reviews: review::Registry,
    /// No máximo 5 reviews ao mesmo tempo; os demais esperam na fila.
    pub review_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// Um navegador só: um Browser Tester por vez.
    pub browser_lock: tokio::sync::Mutex<()>,
    pub publish_lock: tokio::sync::Mutex<()>,
    /// Limite de uso do Claude atingido: reviews pausam e o chat recusa até liberar.
    pub claude_limit: Mutex<Option<review::LimitState>>,
}

impl AppState {
    fn conn(&self) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
        self.db.lock().map_err(|_| AppError::Invalid("conexão com o banco indisponível".into()))
    }
}

#[derive(Serialize)]
pub struct AppInfo {
    version: &'static str,
    db_path: String,
    schema_version: u32,
    schema_latest: u32,
    /// `None` = cofre do sistema acessível; `Some(motivo)` caso contrário.
    keyring_error: Option<String>,
}

#[derive(Serialize)]
pub struct SecretStatus {
    kind: SecretKind,
    saved: bool,
}

#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> AppResult<AppInfo> {
    let conn = state.conn()?;
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        db_path: state.db_path.display().to_string(),
        schema_version: db::schema_version(&conn)?,
        schema_latest: db::latest_version(),
        keyring_error: KeyringStore::status().err(),
    })
}

#[tauri::command]
pub fn secret_status(state: State<'_, AppState>, kind: SecretKind) -> AppResult<SecretStatus> {
    Ok(SecretStatus { kind, saved: secrets::is_saved(state.secrets.as_ref(), kind)? })
}

#[tauri::command]
pub fn secret_save(state: State<'_, AppState>, kind: SecretKind, value: String) -> AppResult<SecretStatus> {
    if kind == SecretKind::Bitbucket || kind == SecretKind::Github {
        return Err(AppError::Invalid("use o comando de validação do provedor para salvar o token".into()));
    }
    secrets::save(&*state.conn()?, state.secrets.as_ref(), kind, &value)?;
    secret_status(state, kind)
}

#[tauri::command]
pub fn secret_delete(state: State<'_, AppState>, kind: SecretKind) -> AppResult<SecretStatus> {
    {
        let conn = state.conn()?;
        secrets::remove(&conn, state.secrets.as_ref(), kind)?;
        if kind == SecretKind::Bitbucket {
            bitbucket::forget_account(&conn)?;
        } else if kind == SecretKind::Github {
            db::set_setting(&conn, "github.auth_mode", &serde_json::json!("gh"))?;
        }
    }
    secret_status(state, kind)
}

/// Valida o token novo contra a API e só então o salva no cofre.
/// Usa o email e o workspace já salvos nas configurações.
#[tauri::command]
pub async fn bitbucket_save_token(state: State<'_, AppState>, value: String) -> AppResult<Account> {
    let token = secrets::normalize_secret(&value)?;
    let (email, workspace) = bitbucket::account_settings(&*state.conn()?)?;
    let account = bitbucket::validate(&Client::new(&email, &token)?, &workspace).await?;
    let conn = state.conn()?;
    secrets::save(&conn, state.secrets.as_ref(), SecretKind::Bitbucket, &token)?;
    bitbucket::record_validation(&conn, &account)
}

/// Revalida o token já salvo (ex.: depois de trocar o email ou o workspace).
#[tauri::command]
pub async fn bitbucket_validate(state: State<'_, AppState>) -> AppResult<Account> {
    let (email, workspace) = bitbucket::account_settings(&*state.conn()?)?;
    let token = state
        .secrets
        .get(&SecretKind::Bitbucket.account())?
        .ok_or_else(|| AppError::Invalid("nenhum token do Bitbucket salvo".into()))?;
    let result = bitbucket::validate(&Client::new(&email, &token)?, &workspace).await;
    let conn = state.conn()?;
    match result {
        Ok(account) => bitbucket::record_validation(&conn, &account),
        Err(e) => {
            // a validação anterior não vale mais
            conn.execute("UPDATE credentials SET validated_at = NULL WHERE kind = 'bitbucket'", [])?;
            bitbucket::forget_account(&conn)?;
            Err(e)
        }
    }
}

#[tauri::command]
pub fn bitbucket_account(state: State<'_, AppState>) -> AppResult<Option<Account>> {
    bitbucket::stored_account(&*state.conn()?)
}

#[derive(Serialize)]
pub struct GithubAccount { login: String, mode: String }

#[tauri::command]
pub async fn github_save_token(state: State<'_, AppState>, value: String) -> AppResult<GithubAccount> {
    let token = secrets::normalize_secret(&value)?;
    let api = github::Api::new(token.clone())?;
    let user: github::User = api.get("/user").await?;
    let conn = state.conn()?;
    secrets::save(&conn, state.secrets.as_ref(), SecretKind::Github, &token)?;
    db::set_setting(&conn, "github.auth_mode", &serde_json::json!("token"))?;
    Ok(GithubAccount { login: user.login, mode: "token".into() })
}

#[tauri::command]
pub async fn github_use_gh(state: State<'_, AppState>) -> AppResult<GithubAccount> {
    let token = github::token("gh", state.secrets.as_ref()).await?;
    let user: github::User = github::Api::new(token)?.get("/user").await?;
    let conn = state.conn()?;
    db::set_setting(&conn, "github.auth_mode", &serde_json::json!("gh"))?;
    Ok(GithubAccount { login: user.login, mode: "gh".into() })
}

#[tauri::command]
pub fn github_login() -> AppResult<()> {
    std::process::Command::new("x-terminal-emulator")
        .args(["-e", "gh", "auth", "login", "--hostname", "github.com", "--web", "--git-protocol", "https"])
        .spawn().map_err(|e| AppError::Invalid(format!("não consegui abrir o terminal para gh auth login: {e}")))?;
    Ok(())
}

#[tauri::command]
pub fn setting_get(state: State<'_, AppState>, key: String) -> AppResult<Option<Value>> {
    db::get_setting(&*state.conn()?, &key)
}

#[tauri::command]
pub fn setting_set(state: State<'_, AppState>, key: String, value: Value) -> AppResult<()> {
    db::set_setting(&*state.conn()?, &key, &value)
}

// ---------------- PRs ----------------

/// Repositórios monitorados: `bitbucket.repos` (lista ou texto separado por vírgula).
fn monitored_repos(conn: &Connection) -> AppResult<Vec<String>> {
    let v = db::get_setting(conn, "bitbucket.repos")?.unwrap_or(Value::Null);
    let list: Vec<String> = match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        Value::String(s) => s.split(',').map(str::to_owned).collect(),
        _ => vec![],
    };
    let list: Vec<String> = list.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
    if list.is_empty() {
        return Err(AppError::Invalid("informe os repositórios monitorados nas Configurações".into()));
    }
    Ok(list)
}

/// Busca no Bitbucket os PRs abertos em que sou reviewer ou autor, em todos os repos monitorados.
#[tauri::command]
pub async fn sync_prs(state: State<'_, AppState>) -> AppResult<bitbucket::sync::SyncResult> {
    sync_all(&state.db, state.secrets.as_ref()).await
}

fn lock(db: &Mutex<Connection>) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
    db.lock().map_err(|_| AppError::Invalid("conexão com o banco indisponível".into()))
}

/// O sync em si: usado pela tela e pelo modo sem janela (`--sync`, timer das 10h).
pub async fn sync_all(db: &Mutex<Connection>, store: &dyn SecretStore) -> AppResult<bitbucket::sync::SyncResult> {
    let has_bitbucket = {
        let conn = lock(db)?;
        db::get_setting(&conn, "bitbucket.workspace")?.and_then(|v| v.as_str().map(str::to_owned)).is_some_and(|s| !s.trim().is_empty())
    };
    let mut result = bitbucket::sync::SyncResult { user: String::new(), repos: Vec::new(), requests: 0 };
    if has_bitbucket {
        match sync_bitbucket(db, store).await {
            Ok(r) => result = r,
            Err(e) => result.repos.push(bitbucket::sync::RepoSync {
                slug: "Bitbucket".into(), open: 0, error: Some(e.to_string()), fetch_error: None,
            }),
        }
    }
    let (repos, mode, root) = {
        let conn = lock(db)?;
        let repos = github::configured_repos(db::get_setting(&conn, "github.repos")?)?;
        let mode = db::get_setting(&conn, "github.auth_mode")?.and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "gh".into());
        let root = review::repos_root(&conn)?;
        (repos, mode, root)
    };
    if !repos.is_empty() {
        let auth = async {
            let token = github::token(&mode, store).await?;
            let api = github::Api::new(token)?;
            let me: github::User = api.get("/user").await?;
            Ok::<_, AppError>((api, me))
        }.await;
        let (api, me) = match auth {
            Ok(pair) => pair,
            Err(e) => {
                result.repos.extend(repos.into_iter().map(|(owner, name)| bitbucket::sync::RepoSync {
                    slug: format!("{owner}/{name}"), open: 0, error: Some(format!("GitHub: {e}")), fetch_error: None,
                }));
                return Ok(result);
            }
        };
        let sync_id = {
            let conn = lock(db)?;
            conn.execute("INSERT INTO sync_runs (kind, status) VALUES ('full', 'running')", [])?;
            conn.last_insert_rowid()
        };
        {
            let conn = lock(db)?;
            db::set_setting(&conn, "github.me", &serde_json::json!({"login": me.login, "id": me.id}))?;
        }
        if result.user.is_empty() { result.user = me.login.clone(); }
        for (owner, name) in repos {
            let fetched = api.pulls(&owner, &name).await;
            let (open, error) = match fetched {
                Ok(pulls) => {
                    // `sync` também consulta os reviews de cada PR.
                    match github::sync_repo(db, &api, &me, &owner, &name, pulls).await {
                        Ok(n) => (n, None), Err(e) => (0, Some(e.to_string()))
                    }
                }
                Err(e) => (0, Some(e.to_string())),
            };
            let clone = github::clone_path(&root, &owner, &name);
            let fetch_error = if clone.join(".git").exists() {
                review::git::run(&clone, &["fetch", "--prune", "-q", "origin"]).await.err().map(|e| e.to_string())
            } else { Some(format!("clone local não encontrado em {}", clone.display())) };
            result.repos.push(bitbucket::sync::RepoSync { slug: format!("{owner}/{name}"), open, error, fetch_error });
        }
        let failed = result.repos.iter().filter_map(|r| r.error.as_ref()).cloned().collect::<Vec<_>>();
        result.requests += api.requests();
        lock(db)?.execute("UPDATE sync_runs SET status = ?1, finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), error = ?2, requests = ?3 WHERE id = ?4",
            rusqlite::params![if failed.is_empty() { "completed" } else { "failed" }, (!failed.is_empty()).then(|| failed.join("; ")), api.requests(), sync_id])?;
    }
    if !has_bitbucket && result.repos.is_empty() {
        return Err(AppError::Invalid("configure ao menos um repositório Bitbucket ou GitHub".into()));
    }
    Ok(result)
}

async fn sync_bitbucket(db: &Mutex<Connection>, store: &dyn SecretStore) -> AppResult<bitbucket::sync::SyncResult> {
    let (email, workspace, repos) = {
        let conn = lock(db)?;
        let (email, workspace) = bitbucket::account_settings(&conn)?;
        (email, workspace, monitored_repos(&conn)?)
    };
    let token = store
        .get(&SecretKind::Bitbucket.account())?
        .ok_or_else(|| AppError::Invalid("salve o token do Bitbucket nas Configurações".into()))?;
    let client = Client::new(&email, &token)?;
    let me: bitbucket::User = client.get_json("/user", &[]).await?.body;
    // quem sou eu, para marcar os PRs que já aprovei
    db::set_setting(&*lock(db)?, "bitbucket.me", &serde_json::json!({"uuid": me.uuid, "display_name": me.display_name}))?;

    let sync_id: i64 = {
        let conn = lock(db)?;
        conn.execute("INSERT INTO sync_runs (kind, status) VALUES ('full', 'running')", [])?;
        conn.last_insert_rowid()
    };
    let mut out = Vec::new();
    let root = review::repos_root(&*lock(db)?)?;
    for slug in repos {
        let fetched = bitbucket::sync::fetch_repo(&client, &workspace, &slug, &me.uuid).await;
        let (open, error) = match fetched {
            Ok(prs) => {
                let mut conn = lock(db)?;
                (bitbucket::sync::upsert_repo_prs(&mut conn, &workspace, &slug, &prs)?, None)
            }
            Err(e) => (0, Some(e.to_string())),
        };
        // o clone local também recebe as branches novas (o Checkout não acessa a rede)
        let clone = root.join(&slug);
        let fetch_error = if clone.join(".git").exists() {
            review::git::run(&clone, &["fetch", "--prune", "-q", "origin"]).await.err().map(|e| e.to_string())
        } else {
            Some(format!("clone local não encontrado em {}", clone.display()))
        };
        out.push(bitbucket::sync::RepoSync { slug, open, error, fetch_error });
    }
    let failed: Vec<String> = out.iter().filter_map(|r| r.error.as_ref().map(|e| format!("{}: {e}", r.slug))).collect();
    {
        let conn = lock(db)?;
        conn.execute(
            "UPDATE sync_runs SET status = ?1, finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), requests = ?2, error = ?3
             WHERE id = ?4",
            rusqlite::params![
                if failed.is_empty() { "completed" } else { "failed" },
                client.requests(),
                (!failed.is_empty()).then(|| failed.join("; ")),
                sync_id
            ],
        )?;
    }
    Ok(bitbucket::sync::SyncResult { user: me.display_name, repos: out, requests: client.requests() })
}

#[derive(Serialize)]
pub struct LastRun {
    id: String,
    status: String,
    stage: Option<String>,
    started_at: String,
    findings: i64,
    high: i64,
    /// Melhorias técnicas válidas (não entram em `findings`).
    improvements: i64,
    warning: Option<String>,
    /// O PR tem commits mais novos que o commit revisado.
    outdated: bool,
}

#[derive(Serialize)]
pub struct PrView {
    id: i64,
    repo: String,
    provider: String,
    owner: String,
    bb_id: i64,
    title: String,
    url: String,
    author: Option<String>,
    category: String,
    changes_requested: bool,
    changed_since_sync: bool,
    /// Eu já aprovei este PR (participante com approved = true).
    approved_by_me: bool,
    /// Commit do último review **concluído** (curto) e se o PR recebeu commits depois dele.
    /// Fica marcado até um review novo cobrir o commit mais recente.
    reviewed_sha: Option<String>,
    needs_rereview: bool,
    /// Início do último review concluído nos mesmos commits de origem e destino que a lista
    /// conhece: revisar de novo repetiria o mesmo trabalho (a UI pede confirmação).
    same_commits_reviewed_at: Option<String>,
    reviewers: Vec<bitbucket::sync::Reviewer>,
    source_branch: String,
    dest_branch: String,
    updated_on: String,
    last_run: Option<LastRun>,
    /// Comentários no PR (listagem do Bitbucket; inclui os meus).
    comment_count: Option<i64>,
    /// Última análise dos comentários (só nos meus PRs).
    fix: Option<crate::fix::FixSummary>,
}

#[derive(Serialize)]
pub struct PrList {
    prs: Vec<PrView>,
    last_sync: Option<String>,
}

#[tauri::command]
pub fn list_prs(state: State<'_, AppState>) -> AppResult<PrList> {
    pr_list(&*state.conn()?)
}

/// As listas do app ("Para eu revisar" e "Meus PRs"), também usadas pelo `prs` do terminal.
pub fn pr_list(conn: &Connection) -> AppResult<PrList> {
    let me: Option<String> = db::get_setting(&conn, "bitbucket.me")?
        .and_then(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned));
    let github_me: Option<String> = db::get_setting(&conn, "github.me")?
        .and_then(|v| v.get("login").and_then(Value::as_str).map(str::to_owned));
    let mut stmt = conn.prepare(
        "SELECT p.id, r.slug, p.bb_id, p.title, p.url, p.author_name, p.category, p.changes_requested,
                p.changed_since_sync, p.reviewers_json, p.source_branch, p.dest_branch, p.updated_on,
                rr.id, rr.status, rr.stage, rr.started_at,
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok'
                   AND ifnull(f.kind, '') != 'melhoria'
                   AND ifnull(f.audit_decision, '') != 'falso_positivo'),
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok'
                   AND ifnull(f.audit_decision, '') != 'falso_positivo'
                   AND coalesce(f.audit_new_severity, f.severity) IN ('high', 'critical')),
                rr.warning,
                rr.source_sha IS NOT NULL AND substr(rr.source_sha, 1, length(p.source_sha)) != p.source_sha,
                (SELECT source_sha FROM review_runs WHERE pr_id = p.id AND status = 'completed'
                   ORDER BY started_at DESC LIMIT 1),
                p.source_sha,
                (SELECT started_at FROM review_runs WHERE pr_id = p.id AND status = 'completed'
                   AND substr(source_sha, 1, length(p.source_sha)) = p.source_sha
                   AND substr(dest_sha, 1, length(p.dest_sha)) = p.dest_sha
                   ORDER BY started_at DESC LIMIT 1),
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok' AND f.kind = 'melhoria'),
                p.comment_count, r.provider, r.workspace
         FROM pull_requests p
         JOIN repositories r ON r.id = p.repository_id
         LEFT JOIN review_runs rr ON rr.id = (SELECT id FROM review_runs WHERE pr_id = p.id ORDER BY started_at DESC LIMIT 1)
         WHERE p.state = 'OPEN'
         ORDER BY p.updated_on DESC",
    )?;
    let mut fixes = crate::fix::summaries(conn)?;
    let mut prs = stmt
        .query_map([], |r| {
            let reviewers: Vec<bitbucket::sync::Reviewer> =
                serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default();
            let provider: String = r.get(26)?;
            let identity = if provider == "github" { &github_me } else { &me };
            let approved_by_me = identity.as_ref().is_some_and(|m| reviewers.iter().any(|x| x.approved && x.uuid.as_ref() == Some(m)));
            let run_id: Option<String> = r.get(13)?;
            let reviewed: Option<String> = r.get(21)?;
            let current: String = r.get(22)?;
            let needs_rereview = reviewed.as_ref().is_some_and(|s| !current.is_empty() && !s.starts_with(&current));
            Ok(PrView {
                id: r.get(0)?,
                repo: r.get(1)?,
                provider,
                owner: r.get(27)?,
                bb_id: r.get(2)?,
                title: r.get(3)?,
                url: r.get(4)?,
                author: r.get(5)?,
                category: r.get(6)?,
                changes_requested: r.get(7)?,
                changed_since_sync: r.get(8)?,
                approved_by_me,
                reviewed_sha: reviewed.map(|s| s.chars().take(12).collect()),
                needs_rereview,
                same_commits_reviewed_at: r.get(23)?,
                reviewers,
                source_branch: r.get(10)?,
                dest_branch: r.get(11)?,
                updated_on: r.get(12)?,
                last_run: match run_id {
                    Some(id) => Some(LastRun {
                        id,
                        status: r.get(14)?,
                        stage: r.get(15)?,
                        started_at: r.get(16)?,
                        findings: r.get(17)?,
                        high: r.get(18)?,
                        improvements: r.get(24)?,
                        warning: r.get(19)?,
                        outdated: r.get(20)?,
                    }),
                    None => None,
                },
                comment_count: r.get(25)?,
                fix: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for pr in &mut prs {
        pr.fix = fixes.remove(&format!("pr:{}", pr.id));
    }
    let last_sync: Option<String> = conn
        .query_row("SELECT max(finished_at) FROM sync_runs WHERE status IN ('completed', 'failed')", [], |r| r.get(0))?;
    Ok(PrList { prs, last_sync })
}

/// Custo dos reviews e de cada agente (aba Uso).
#[tauri::command]
pub fn usage_get(state: State<'_, AppState>) -> AppResult<review::usage::Usage> {
    review::usage::usage(&*state.conn()?)
}

// ---------------- Review ----------------

#[tauri::command]
pub fn review_start(app: tauri::AppHandle, pr_id: i64) -> AppResult<String> {
    review::start(review::Host::App(app), pr_id)
}

#[tauri::command]
pub fn review_cancel(state: State<'_, AppState>, run_id: String) -> AppResult<()> {
    review::cancel(&state, &run_id)
}

#[derive(Serialize)]
pub struct FindingView {
    seq: i64,
    project: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
    severity_reason: Option<String>,
    impact: Option<String>,
    file: Option<String>,
    line: Option<i64>,
    title: Option<String>,
    description: Option<String>,
    steps: Vec<String>,
    pattern_ref: Option<String>,
    confidence: Option<String>,
    markdown: Option<String>,
    validation: String,
    reject_reason: Option<String>,
    in_diff: Option<bool>,
    audit_decision: Option<String>,
    audit_new_severity: Option<String>,
    audit_rationale: Option<String>,
    /// Só em melhoria (`kind = "melhoria"`): categoria, o que garante a equivalência e a forma simples.
    category: Option<String>,
    evidence: Option<String>,
    /// Forma livre das melhorias gravadas antes da migration 0008.
    suggestion: Option<String>,
    /// Melhoria no formato do Qodo: trecho literal (`line..=end_line`), substituição completa e
    /// nota do juiz (0 a 10) com a importância derivada dela.
    end_line: Option<i64>,
    existing_code: Option<String>,
    improved_code: Option<String>,
    score: Option<i64>,
    score_why: Option<String>,
    importance: Option<String>,
    /// Pedido de mudanças (`rc-validator`): bloqueante, sugestao ou fora; o principal, quando
    /// este repete outro; o comentário pronto e o porquê (só para o revisor).
    rc_decision: Option<String>,
    rc_duplicate_of: Option<i64>,
    rc_body: Option<String>,
    rc_reason: Option<String>,
}

#[derive(Serialize)]
pub struct RunView {
    id: String,
    /// Nulo no review de branch local (`repo_slug` + `branch`).
    pr_id: Option<i64>,
    repo_slug: Option<String>,
    branch: Option<String>,
    status: String,
    stage: Option<String>,
    audit_status: Option<String>,
    /// Análise de complexidade (Haiku): running, completed, failed ou cancelled.
    quality_status: Option<String>,
    card_key: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    cost_usd: Option<f64>,
    error: Option<String>,
    warning: Option<String>,
    browser_status: Option<String>,
    browser_note: Option<String>,
    /// PR que precisa de checkout para o teste no navegador rodar, e o rótulo `repo#id`.
    browser_checkout_pr_id: Option<i64>,
    browser_checkout_label: Option<String>,
    /// Pedido de mudanças: running, completed, failed, cancelled, not_needed ou nulo (não se aplica).
    rc_status: Option<String>,
    rc_summary: Option<String>,
    /// Link do review enviado ao GitHub.
    rc_sent_url: Option<String>,
    run_dir: String,
    findings: Vec<FindingView>,
    browser_tests: Vec<BrowserTestView>,
    log: Vec<String>,
    /// Etapas com hora de início e fim (vazio nas execuções anteriores à migration 0012).
    steps: Vec<review::usage::StepView>,
    /// Chamadas ao Claude, com modelo, custo e duração.
    calls: Vec<review::usage::CallView>,
}

#[derive(Serialize)]
pub struct BrowserTestView {
    seq: i64,
    title: Option<String>,
    url: Option<String>,
    preconditions: Option<String>,
    actions: Vec<String>,
    expected: Option<String>,
    observed: Option<String>,
    evidence: Option<String>,
    status: String,
    finding_ref: Option<String>,
}

#[tauri::command]
pub fn review_get(state: State<'_, AppState>, run_id: String) -> AppResult<RunView> {
    run_view(&*state.conn()?, &run_id)
}

/// Execução com os achados, os testes do navegador e o fim do log (painel e modo terminal).
pub fn run_view(conn: &Connection, run_id: &str) -> AppResult<RunView> {
    let mut run = conn.query_row(
        "SELECT rr.id, rr.pr_id, rr.status, rr.stage, rr.audit_status, rr.card_key, rr.started_at, rr.finished_at,
                rr.cost_usd, rr.error, rr.run_dir, rr.warning, rr.browser_status, rr.browser_note,
                rr.browser_checkout_pr_id, cr.slug || '#' || cp.bb_id, rr.quality_status, rr.repo_slug, rr.branch,
                rr.rc_status, rr.rc_summary, rr.rc_sent_url
         FROM review_runs rr
         LEFT JOIN pull_requests cp ON cp.id = rr.browser_checkout_pr_id
         LEFT JOIN repositories cr ON cr.id = cp.repository_id
         WHERE rr.id = ?1",
        [run_id],
        |r| {
            Ok(RunView {
                id: r.get(0)?,
                pr_id: r.get(1)?,
                status: r.get(2)?,
                stage: r.get(3)?,
                audit_status: r.get(4)?,
                card_key: r.get(5)?,
                started_at: r.get(6)?,
                finished_at: r.get(7)?,
                cost_usd: r.get(8)?,
                error: r.get(9)?,
                run_dir: r.get(10)?,
                warning: r.get(11)?,
                browser_status: r.get(12)?,
                browser_note: r.get(13)?,
                browser_checkout_pr_id: r.get(14)?,
                browser_checkout_label: r.get(15)?,
                quality_status: r.get(16)?,
                repo_slug: r.get(17)?,
                branch: r.get(18)?,
                rc_status: r.get(19)?,
                rc_summary: r.get(20)?,
                rc_sent_url: r.get(21)?,
                findings: vec![],
                browser_tests: vec![],
                log: vec![],
                steps: vec![],
                calls: vec![],
            })
        },
    )?;
    let mut stmt = conn.prepare(
        "SELECT seq, project, kind, severity, severity_reason, impact, file, line, title, description, steps_json,
                pattern_ref, confidence, markdown, validation, reject_reason, in_diff, audit_decision,
                audit_new_severity, audit_rationale, category, evidence, suggestion,
                end_line, existing_code, improved_code, score, score_why, importance,
                rc_decision, rc_duplicate_of, rc_body, rc_reason
         FROM findings WHERE run_id = ?1 ORDER BY seq",
    )?;
    run.findings = stmt
        .query_map([run_id], |r| {
            let steps: String = r.get(10)?;
            Ok(FindingView {
                seq: r.get(0)?,
                project: r.get(1)?,
                kind: r.get(2)?,
                severity: r.get(3)?,
                severity_reason: r.get(4)?,
                impact: r.get(5)?,
                file: r.get(6)?,
                line: r.get(7)?,
                title: r.get(8)?,
                description: r.get(9)?,
                steps: serde_json::from_str(&steps).unwrap_or_default(),
                pattern_ref: r.get(11)?,
                confidence: r.get(12)?,
                markdown: r.get(13)?,
                validation: r.get(14)?,
                reject_reason: r.get(15)?,
                in_diff: r.get(16)?,
                audit_decision: r.get(17)?,
                audit_new_severity: r.get(18)?,
                audit_rationale: r.get(19)?,
                category: r.get(20)?,
                evidence: r.get(21)?,
                suggestion: r.get(22)?,
                end_line: r.get(23)?,
                existing_code: r.get(24)?,
                improved_code: r.get(25)?,
                score: r.get(26)?,
                score_why: r.get(27)?,
                importance: r.get(28)?,
                rc_decision: r.get(29)?,
                rc_duplicate_of: r.get(30)?,
                rc_body: r.get(31)?,
                rc_reason: r.get(32)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT seq, title, url, preconditions, actions_json, expected, observed, evidence, status, finding_ref
         FROM browser_tests WHERE run_id = ?1 ORDER BY seq",
    )?;
    run.browser_tests = stmt
        .query_map([run_id], |r| {
            let actions: String = r.get(4)?;
            Ok(BrowserTestView {
                seq: r.get(0)?,
                title: r.get(1)?,
                url: r.get(2)?,
                preconditions: r.get(3)?,
                actions: serde_json::from_str(&actions).unwrap_or_default(),
                expected: r.get(5)?,
                observed: r.get(6)?,
                evidence: r.get(7)?,
                status: r.get(8)?,
                finding_ref: r.get(9)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let log = std::fs::read_to_string(std::path::Path::new(&run.run_dir).join("run.log")).unwrap_or_default();
    run.log = log.lines().rev().take(400).map(str::to_owned).collect::<Vec<_>>().into_iter().rev().collect();
    // execução antiga: as chamadas vêm dos *.result.json na primeira leitura
    if run.status != "running" {
        review::usage::backfill_run(conn, run_id)?;
    }
    run.steps = review::usage::steps(conn, run_id)?;
    run.calls = review::usage::calls(conn, run_id)?;
    Ok(run)
}

// ---------------- Ambientes de teste e checkout ----------------

#[tauri::command]
pub fn test_apps_get(state: State<'_, AppState>) -> AppResult<Vec<crate::testenv::TestAppView>> {
    crate::testenv::views(&*state.conn()?, state.secrets.as_ref())
}

/// Salva a lista de ambientes; `password` (se vier) vai para o cofre do ambiente `app.id`.
#[tauri::command]
pub fn test_app_save(
    state: State<'_, AppState>,
    apps: Vec<crate::testenv::TestApp>,
    password_for: Option<String>,
    password: Option<String>,
) -> AppResult<Vec<crate::testenv::TestAppView>> {
    let conn = state.conn()?;
    crate::testenv::save_all(&conn, &apps)?;
    if let (Some(id), Some(pw)) = (password_for, password.filter(|p| !p.is_empty())) {
        if !apps.iter().any(|a| a.id == id) {
            return Err(AppError::Invalid(format!("ambiente {id} não existe")));
        }
        state.secrets.set(&secrets::test_password_account(&id), &pw)?;
    }
    crate::testenv::views(&conn, state.secrets.as_ref())
}

#[derive(Serialize)]
pub struct CheckoutResult {
    repo: String,
    branch: String,
    head: String,
    /// O checkout ficou exatamente no commit do PR conhecido pelo app?
    at_pr_commit: bool,
}

/// Pedido explícito do usuário: deixa o clone local na branch do PR, com o que o último
/// sync trouxe (sem rede): checkout → merge --ff-only origin/<branch>.
/// Recusa se houver alteração não commitada ou se a branch local divergiu.
#[tauri::command]
pub async fn checkout_pr(state: State<'_, AppState>, pr_id: i64) -> AppResult<CheckoutResult> {
    let (slug, mut branch, sha, root, provider, number, owner) = {
        let conn = state.conn()?;
        let (slug, branch, sha, provider, number, owner): (String, String, String, String, i64, String) = conn.query_row(
            "SELECT r.slug, p.source_branch, p.source_sha, r.provider, p.bb_id, r.workspace FROM pull_requests p
             JOIN repositories r ON r.id = p.repository_id WHERE p.id = ?1",
            [pr_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        (slug, branch, sha, review::repos_root(&conn)?, provider, number, owner)
    };
    let repo = if provider == "github" { github::clone_path(&root, &owner, &slug) } else { root.join(&slug) };
    if !repo.join(".git").exists() {
        return Err(AppError::Invalid(format!("não achei o clone de {slug} em {}", repo.display())));
    }
    let head = if provider == "github" {
        let (local, head) = review::git::checkout_github(&repo, number).await?;
        branch = local;
        head
    } else { review::git::checkout_synced(&repo, &slug, &branch).await? };
    Ok(CheckoutResult { at_pr_commit: head.starts_with(&sha) || sha.starts_with(&head), repo: slug, branch, head })
}

// ---------------- Chat sobre o review ----------------

#[tauri::command]
pub fn chat_ask(app: tauri::AppHandle, run_id: String, question: String) -> AppResult<()> {
    review::chat::ask(app, run_id, question)
}

#[derive(Serialize)]
pub struct ChatView {
    messages: Vec<review::chat::ChatMessage>,
    /// Há uma pergunta sendo respondida agora.
    busy: bool,
}

#[tauri::command]
pub fn chat_get(state: State<'_, AppState>, run_id: String) -> AppResult<ChatView> {
    let busy = state.reviews.lock().unwrap().contains_key(&format!("chat:{run_id}"));
    Ok(ChatView { messages: review::chat::messages(&*state.conn()?, &run_id)?, busy })
}

#[tauri::command]
pub fn chat_cancel(state: State<'_, AppState>, run_id: String) -> AppResult<()> {
    review::chat::cancel(&state, &run_id)
}

// ---------------- Diff do PR (aba "Diff" do painel) ----------------

/// Arquivos alterados na visão pedida (commit revisado ou ponta atual), sem rede.
#[tauri::command]
pub async fn diff_files(state: State<'_, AppState>, run_id: String, view: crate::diff::View) -> AppResult<crate::diff::DiffSummary> {
    let t = crate::diff::target(&*state.conn()?, &run_id, view)?;
    crate::diff::summary(&t).await
}

/// Diff de um arquivo da visão, cortado em 1 MB, nos commits do resumo que a tela mostra
/// (`base_sha`/`head_sha` do `diff_files`). `old_path` só em renomeação.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn diff_file(
    state: State<'_, AppState>,
    run_id: String,
    view: crate::diff::View,
    base_sha: String,
    head_sha: String,
    path: String,
    old_path: Option<String>,
) -> AppResult<crate::diff::FileDiff> {
    let t = crate::diff::pin(crate::diff::target(&*state.conn()?, &run_id, view)?, &base_sha, &head_sha)?;
    crate::diff::one_file(&t, &path, old_path.as_deref()).await
}

// ---------------- Limite de uso do Claude ----------------

#[tauri::command]
pub fn claude_limit_get(app: tauri::AppHandle) -> Option<review::LimitState> {
    review::active_limit(&app)
}

/// "Tentar agora": libera a pausa; se o limite continuar, a próxima execução marca de novo.
#[tauri::command]
pub fn claude_limit_clear(app: tauri::AppHandle) {
    review::clear_limit(&app)
}

// ---------------- Correção automática (Meus PRs e Minhas branches) ----------------

fn fix_view(state: &AppState, id: Option<String>) -> AppResult<Option<crate::fix::FixView>> {
    let Some(id) = id else { return Ok(None) };
    let busy = state.reviews.lock().unwrap().contains_key(&format!("fix:{id}"));
    Ok(Some(crate::fix::view(&*state.conn()?, &id, busy)?))
}

/// Última correção dos comentários do PR (null se nunca rodou).
#[tauri::command]
pub fn fix_get(state: State<'_, AppState>, pr_id: i64) -> AppResult<Option<crate::fix::FixView>> {
    let id = crate::fix::latest_pr_id(&*state.conn()?, pr_id)?;
    fix_view(&state, id)
}

/// Última correção da branch local (null se nunca rodou).
#[tauri::command]
pub fn fix_get_branch(state: State<'_, AppState>, repo_slug: String, branch: String) -> AppResult<Option<crate::fix::FixView>> {
    let id = crate::fix::latest_branch_id(&*state.conn()?, &repo_slug, &branch)?;
    fix_view(&state, id)
}

#[tauri::command]
pub fn fix_start(app: tauri::AppHandle, pr_id: i64, force: bool) -> AppResult<String> {
    crate::fix::start(review::Host::App(app), pr_id, force).map(|(id, _)| id)
}

/// "Buscar novos comentários" numa correção de PR.
#[tauri::command]
pub fn fix_refresh(app: tauri::AppHandle, fix_id: String) -> AppResult<()> {
    crate::fix::refresh(review::Host::App(app), fix_id).map(|_| ())
}

/// "Analisar" um ou mais comentários (Meus PRs).
#[tauri::command]
pub fn fix_analyze(app: tauri::AppHandle, comment_ids: Vec<i64>) -> AppResult<()> {
    crate::fix::analyze(review::Host::App(app), comment_ids).map(|_| ())
}

/// "Corrigir" em achados de um review de branch local.
#[tauri::command]
pub async fn fix_add_findings(app: tauri::AppHandle, run_id: String, seqs: Vec<i64>) -> AppResult<String> {
    crate::fix::add_findings(review::Host::App(app), run_id, seqs).await
}

#[tauri::command]
pub fn fix_cancel(state: State<'_, AppState>, fix_id: String) -> AppResult<()> {
    crate::fix::cancel(&state, &fix_id)
}

#[tauri::command]
pub async fn fix_discard(app: tauri::AppHandle, fix_id: String) -> AppResult<()> {
    crate::fix::discard(review::Host::App(app), fix_id).await
}

#[tauri::command]
pub fn fix_ask(app: tauri::AppHandle, comment_id: i64, text: String) -> AppResult<()> {
    crate::fix::ask(review::Host::App(app), comment_id, text).map(|_| ())
}

/// Grava a sugestão aprovada no worktree da correção; devolve os arquivos alterados.
#[tauri::command]
pub fn fix_approve(app: tauri::AppHandle, comment_id: i64) -> AppResult<Vec<String>> {
    crate::fix::approve(&review::Host::App(app), comment_id)
}

/// Commit pelo committer; no PR, já faz o push; na branch local, avança a branch.
#[tauri::command]
pub fn fix_commit(app: tauri::AppHandle, fix_id: String) -> AppResult<()> {
    crate::fix::commit(review::Host::App(app), fix_id).map(|_| ())
}

/// Push da branch local depois do commit.
#[tauri::command]
pub async fn fix_push(app: tauri::AppHandle, fix_id: String) -> AppResult<()> {
    crate::fix::push_branch(review::Host::App(app), fix_id).await
}

// ---------------- Minhas branches ----------------

#[derive(Serialize)]
pub struct BranchView {
    #[serde(flatten)]
    branch: crate::branches::LocalBranch,
    last_run: Option<LastRun>,
    fix: Option<crate::fix::FixSummary>,
}

#[derive(Serialize)]
pub struct ProjectView {
    slug: String,
    kind: &'static str,
    base: Option<String>,
    last_commit: Option<String>,
    error: Option<String>,
    branches: Vec<BranchView>,
}

/// Repositórios da pasta configurada e as branches locais, com o último review e a última
/// correção de cada uma. Sem rede.
#[tauri::command]
pub async fn local_branches(state: State<'_, AppState>) -> AppResult<Vec<ProjectView>> {
    let (root, hidden) = {
        let conn = state.conn()?;
        let hidden: Vec<String> = db::get_setting(&conn, "branches.hidden")?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        (review::repos_root(&conn)?, hidden)
    };
    let projects = crate::branches::list(&root, &hidden).await?;
    let conn = state.conn()?;
    let mut fixes = crate::fix::summaries(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT rr.repo_slug, rr.branch, rr.id, rr.status, rr.stage, rr.started_at,
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok'
                   AND ifnull(f.kind, '') != 'melhoria' AND ifnull(f.audit_decision, '') != 'falso_positivo'),
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok'
                   AND ifnull(f.audit_decision, '') != 'falso_positivo'
                   AND coalesce(f.audit_new_severity, f.severity) IN ('high', 'critical')),
                (SELECT count(*) FROM findings f WHERE f.run_id = rr.id AND f.validation = 'ok' AND f.kind = 'melhoria'),
                rr.warning, ifnull(rr.source_sha, '')
         FROM review_runs rr
         WHERE rr.repo_slug IS NOT NULL
           AND rr.id = (SELECT x.id FROM review_runs x WHERE x.repo_slug = rr.repo_slug AND x.branch = rr.branch
                        ORDER BY x.started_at DESC LIMIT 1)",
    )?;
    let mut runs: std::collections::HashMap<(String, String), (LastRun, String)> = stmt
        .query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                (
                    LastRun {
                        id: r.get(2)?,
                        status: r.get(3)?,
                        stage: r.get(4)?,
                        started_at: r.get(5)?,
                        findings: r.get(6)?,
                        high: r.get(7)?,
                        improvements: r.get(8)?,
                        warning: r.get(9)?,
                        outdated: false,
                    },
                    r.get::<_, String>(10)?,
                ),
            ))
        })?
        .collect::<Result<_, _>>()?;
    Ok(projects
        .into_iter()
        .map(|p| ProjectView {
            branches: p
                .branches
                .into_iter()
                .map(|b| {
                    let last_run = runs.remove(&(p.slug.clone(), b.name.clone())).map(|(mut run, sha)| {
                        // a branch tem commits depois do commit revisado
                        run.outdated = !sha.is_empty() && sha != b.sha;
                        run
                    });
                    let fix = fixes.remove(&format!("branch:{}:{}", p.slug, b.name));
                    BranchView { branch: b, last_run, fix }
                })
                .collect(),
            slug: p.slug,
            kind: p.kind,
            base: p.base,
            last_commit: p.last_commit,
            error: p.error,
        })
        .collect())
}

/// Review de uma branch local (sem PR), comparada com a base da origin.
#[tauri::command]
pub async fn review_start_branch(app: tauri::AppHandle, repo_slug: String, branch: String) -> AppResult<String> {
    review::start_branch(review::Host::App(app), repo_slug, branch).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_view_traz_etapas_e_chamadas() {
        let conn = db::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("prr-runview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review-sonnet.result.json"), r#"{"total_cost_usd": 0.3, "is_error": false}"#).unwrap();
        conn.execute(
            "INSERT INTO review_runs (id, repo_slug, branch, status, run_dir) VALUES ('antigo', 'back', 'PROJ-1', 'completed', ?1),
                                                                                  ('novo', 'back', 'PROJ-2', 'running', ?1)",
            [dir.display().to_string()],
        )
        .unwrap();
        // execução antiga: as chamadas vêm dos arquivos na primeira leitura
        let v = run_view(&conn, "antigo").unwrap();
        assert_eq!((v.calls.len(), v.calls[0].imported, v.steps.len()), (1, true, 0));
        // em andamento: não importa nada (as chamadas chegam pelo review)
        review::usage::step_start(&conn, "novo", "Sonnet revisando").unwrap();
        let v = run_view(&conn, "novo").unwrap();
        assert_eq!((v.calls.len(), v.steps.len()), (0, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------- Gatilhos ----------------

use crate::triggers::{self, manager::{Emit, TriggerEvent, TriggerState}, runtime::NodeRuntime, Trigger};

#[derive(Serialize)]
pub struct TriggerView {
    pub trigger: Trigger,
    pub state: TriggerState,
    /// Comando que vai rodar, para mostrar na tela.
    pub preview: String,
    /// Motivo de o gatilho não poder rodar (validação), se houver.
    pub error: Option<String>,
}

fn trigger_view(t: Trigger, all: &[Trigger], mgr: &triggers::manager::Manager) -> TriggerView {
    TriggerView {
        state: mgr.state(&t.id),
        preview: triggers::build::launch(&t).display,
        error: triggers::validate(&t, all).err().map(|e| e.to_string()),
        trigger: t,
    }
}

/// Emite os eventos para a UI. Roda com a trava do gerenciador presa: só `app.emit`.
pub(crate) fn trigger_emit(app: tauri::AppHandle) -> Emit {
    std::sync::Arc::new(move |ev| match ev {
        TriggerEvent::Status { id, state } => {
            let _ = app.emit("trigger-status", serde_json::json!({ "id": id, "state": state }));
        }
        TriggerEvent::Log { id, lines } => {
            let _ = app.emit("trigger-log", serde_json::json!({ "id": id, "lines": lines }));
        }
    })
}

/// Carrega o gatilho e confere que ele pode rodar.
fn trigger_checked(state: &AppState, id: &str) -> AppResult<Trigger> {
    let all = triggers::load(&*state.conn()?)?;
    let t = all
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| AppError::Invalid("gatilho não encontrado".into()))?;
    triggers::validate(&t, &all)?;
    Ok(t)
}

#[tauri::command]
pub fn triggers_list(state: State<'_, AppState>) -> AppResult<Vec<TriggerView>> {
    let all = triggers::load(&*state.conn()?)?;
    Ok(all.iter().cloned().map(|t| trigger_view(t, &all, &state.triggers)).collect())
}

#[tauri::command]
pub fn trigger_save(state: State<'_, AppState>, trigger: Trigger) -> AppResult<Trigger> {
    triggers::save(&*state.conn()?, trigger)
}

#[tauri::command]
pub fn trigger_delete(state: State<'_, AppState>, id: String) -> AppResult<()> {
    if matches!(state.triggers.state(&id), TriggerState::Running { .. } | TriggerState::Stopping) {
        return Err(AppError::Invalid("pare o gatilho antes de apagar".into()));
    }
    state.triggers.forget(&id)?;
    triggers::delete(&*state.conn()?, &id)
}

#[tauri::command]
pub async fn trigger_start(app: tauri::AppHandle, state: State<'_, AppState>, id: String) -> AppResult<()> {
    let t = trigger_checked(&state, &id)?;
    state.triggers.start(&t, trigger_emit(app)).await
}

#[tauri::command]
pub async fn trigger_stop(app: tauri::AppHandle, state: State<'_, AppState>, id: String) -> AppResult<()> {
    state.triggers.stop(&id, trigger_emit(app)).await
}

#[tauri::command]
pub async fn trigger_restart(app: tauri::AppHandle, state: State<'_, AppState>, id: String) -> AppResult<()> {
    let t = trigger_checked(&state, &id)?;
    state.triggers.restart(&t, trigger_emit(app)).await
}

#[tauri::command]
pub fn trigger_log(state: State<'_, AppState>, id: String) -> AppResult<Vec<String>> {
    state.triggers.tail(&id, 2000)
}

#[tauri::command]
pub fn trigger_scripts(package_json: String) -> AppResult<Vec<String>> {
    triggers::runtime::package_scripts(std::path::Path::new(&triggers::expand_tilde(package_json.trim())))
}

/// Node do sistema primeiro, depois os dos gerenciadores (nvm, fnm, volta).
#[tauri::command]
pub async fn node_runtimes() -> AppResult<Vec<NodeRuntime>> {
    let mut out: Vec<NodeRuntime> = triggers::runtime::system_node().await.into_iter().collect();
    if let Some(home) = std::env::var_os("HOME") {
        out.extend(triggers::runtime::detect_managed(std::path::Path::new(&home)));
    }
    Ok(out)
}

#[tauri::command]
pub fn trigger_preview(trigger: Trigger) -> String {
    triggers::build::launch(&trigger).display
}
