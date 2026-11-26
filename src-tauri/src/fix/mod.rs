//! Correção automática, separada do review. Dois usos:
//!
//! - **Meus PRs** (`kind = 'pr'`): os comentários abertos de outras pessoas no PR, buscados no
//!   Bitbucket com o token só de leitura;
//! - **Minhas branches** (`kind = 'branch'`): achados e melhorias do review de uma branch local,
//!   escolhidos pelo usuário ("Corrigir").
//!
//! Em ambos:
//! - um worktree `--detach` na ponta atual da branch (`<dados>/fixes/<id>/wt`); o checkout do
//!   usuário não é tocado;
//! - os itens entram numa fila; um Sonnet (`comment-fixer`, só leitura) confere cada um e devolve
//!   o veredito e, quando procede, a correção como sugestão (`existing_code` → `improved_code`);
//! - o usuário conversa sobre cada item (mesma sessão do Claude, `--resume`) e aprova a
//!   sugestão; só então o app grava no worktree (`apply`);
//! - resolvidos todos, o agente `committer` do usuário (Haiku) faz o commit. No PR, o app faz o
//!   push logo depois; na branch local, o app avança a branch e o push é um botão à parte.
//!   Nunca com force.

pub mod apply;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::Emitter;

use crate::bitbucket::comments;
use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::review::claude::{self, RunCtl, Session, Spec, Status};
use crate::review::{self, Host, Running, git};
use apply::Edit;

const ANALYZE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Ferramentas do committer: só o que o commit precisa. Ele roda sem o guard-git, então o deny
/// barra o que move ou apaga branch caso as configurações do usuário liberem `git` em geral.
const COMMITTER_ALLOW: &str = "Read Bash(git status*) Bash(git diff*) Bash(git add *) Bash(git commit *) \
     Bash(git branch --show-current) Bash(git log *) Bash(git rev-parse *)";
/// Sem `Bash(git branch -*)`: casaria com o `git branch --show-current` que o committer.md pede.
const COMMITTER_DENY: &str = "Bash(git push*) \
     Bash(git branch -d*) Bash(git branch -D*) Bash(git branch -m*) Bash(git branch -M*) \
     Bash(git branch -c*) Bash(git branch -C*) Bash(git branch -f*) Bash(git branch -u*) \
     Bash(git branch --delete*) Bash(git branch --move*) Bash(git branch --copy*) Bash(git branch --force*) \
     Bash(git branch --set-upstream*) Bash(git branch --unset-upstream*) Bash(git branch --edit-description*) \
     Bash(git checkout*) Bash(git switch*) Bash(git reset*) Bash(git update-ref*) Bash(git commit --amend*) \
     Edit Write NotebookEdit Agent";

fn key(fix_id: &str) -> String {
    format!("fix:{fix_id}")
}

/// Alguma etapa desta correção está rodando neste processo (o `comments` do terminal espera
/// esvaziar: o `release` pode disparar outra volta da fila).
pub fn running_here(state: &AppState, fix_id: &str) -> bool {
    state.reviews.lock().unwrap().contains_key(&key(fix_id))
}

fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(secs);
    format!("f{secs}-{:06x}", h.finish() & 0xffffff)
}

fn db_err() -> AppError {
    AppError::Invalid("banco indisponível".into())
}

fn with_db<T>(host: &Host, f: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
    let st = host.state();
    let conn = st.db.lock().map_err(|_| db_err())?;
    f(&conn)
}

/// O Claude está rodando nesta correção: neste processo, ou num `comments` do terminal (ou no
/// app, visto do terminal) que gravou `cli.pid` na pasta dela.
pub fn busy(host: &Host, fix_id: &str) -> bool {
    host.state().reviews.lock().unwrap().contains_key(&key(fix_id)) || other_cli_alive(&run_dir_of(host, fix_id))
}

/// Pasta da correção (a mesma do `create_session`), sem passar pelo banco.
fn run_dir_of(host: &Host, fix_id: &str) -> PathBuf {
    host.state().db_path.parent().map(|d| d.join("fixes").join(fix_id)).unwrap_or_default()
}

// ---------------- alvo: PR ou branch local ----------------

#[derive(Clone)]
struct Target {
    kind: &'static str,
    /// Repositório do PR no provedor; `None` na branch local.
    repo: Option<crate::hosting::RepoRef>,
    source_repo: String,
    pr_id: Option<i64>,
    slug: String,
    bb_id: i64,
    title: String,
    branch: String,
    base: String,
}

impl Target {
    fn describe(&self) -> String {
        match self.kind {
            "pr" => format!(
                "PR: {}#{} — {} ({} → {}), aberto pelo usuário",
                self.slug, self.bb_id, self.title, self.branch, self.base
            ),
            _ => format!("BRANCH local: {} em {}, ainda sem PR (base: origin/{})", self.branch, self.slug, self.base),
        }
    }
}

#[derive(Clone, Serialize)]
struct Event<'a> {
    fix_id: &'a str,
    /// 0 na branch local.
    pr_id: i64,
    repo_slug: &'a str,
    branch: &'a str,
    line: Option<&'a str>,
    busy: bool,
}

fn emit(host: &Host, fix_id: &str, t: &Target, line: Option<&str>, busy: bool) {
    let ev = Event { fix_id, pr_id: t.pr_id.unwrap_or(0), repo_slug: &t.slug, branch: &t.branch, line, busy };
    match host {
        Host::App(app) => {
            let _ = app.emit("fix-update", ev);
        }
        // no terminal: a etapa (`== …`) sempre; o resto do log só com --verbose
        Host::Cli(cli) => match line {
            Some(l) if l.starts_with("== ") => cli.print(fix_id, l),
            Some(l) if cli.verbose => cli.print(fix_id, l),
            _ => {}
        },
    }
}

#[derive(Clone)]
struct SessionRow {
    kind: String,
    pr_id: Option<i64>,
    repo_slug: Option<String>,
    branch: Option<String>,
    base_branch: Option<String>,
    status: String,
    session_id: Option<String>,
    run_dir: PathBuf,
    base_sha: Option<String>,
    card_key: Option<String>,
    jira_site: Option<String>,
}

impl SessionRow {
    fn wt(&self) -> PathBuf {
        self.run_dir.join("wt")
    }
}

/// Worktree do outro lado da task (front ↔ back), ao lado do `wt` da correção.
fn related_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("related")
}

/// Numa sessão já começada sem o projeto relacionado (correção anterior a ele, ou clone do par
/// que só apareceu depois): cria agora e devolve o texto que apresenta o projeto ao agente.
async fn late_related(host: &Host, t: &Target, s: &SessionRow) -> String {
    let dir = related_dir(&s.run_dir);
    if dir.join(".git").exists() {
        return String::new();
    }
    let Ok(root) = with_db(host, review::repos_root) else { return String::new() };
    match review::related::prepare(&root, &t.slug, &t.branch, &t.base, &dir).await {
        Some(r) => format!("{}\n\n", r.prompt_line(&t.branch)),
        None => String::new(),
    }
}

fn load_session(conn: &Connection, fix_id: &str) -> AppResult<SessionRow> {
    conn.query_row(
        "SELECT kind, pr_id, repo_slug, branch, base_branch, status, session_id, run_dir, base_sha, card_key
         FROM fix_sessions WHERE id = ?1",
        [fix_id],
        |r| {
            Ok(SessionRow {
                kind: r.get(0)?,
                pr_id: r.get(1)?,
                repo_slug: r.get(2)?,
                branch: r.get(3)?,
                base_branch: r.get(4)?,
                status: r.get(5)?,
                session_id: r.get(6)?,
                run_dir: PathBuf::from(r.get::<_, String>(7)?),
                base_sha: r.get(8)?,
                card_key: r.get(9)?,
                jira_site: None,
            })
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid("correção não encontrada".into()))
    .and_then(|mut s| {
        s.jira_site = review::jira_site(conn)?;
        if s.jira_site.is_none() { s.card_key = None; }
        Ok(s)
    })
}

fn load_pr_target(conn: &Connection, pr_id: i64) -> AppResult<(Target, String)> {
    conn.query_row(
        "SELECT r.slug, p.bb_id, p.title, p.source_branch, p.dest_branch, p.category,
                r.provider, r.workspace, ifnull(p.source_repo, '')
         FROM pull_requests p JOIN repositories r ON r.id = p.repository_id WHERE p.id = ?1",
        [pr_id],
        |r| {
            Ok((
                Target {
                    kind: "pr",
                    repo: Some(crate::hosting::RepoRef::from_row(&r.get::<_, String>(6)?, r.get(7)?, r.get(0)?)?),
                    source_repo: r.get(8)?,
                    pr_id: Some(pr_id),
                    slug: r.get(0)?,
                    bb_id: r.get(1)?,
                    title: r.get(2)?,
                    branch: r.get(3)?,
                    base: r.get(4)?,
                },
                r.get::<_, String>(5)?,
            ))
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("PR {pr_id} não encontrado; atualize a lista")))
}

fn target_of(conn: &Connection, s: &SessionRow) -> AppResult<Target> {
    match (s.kind.as_str(), s.pr_id) {
        ("pr", Some(id)) => Ok(load_pr_target(conn, id)?.0),
        _ => {
            let branch = s.branch.clone().unwrap_or_default();
            Ok(Target {
                kind: "branch",
                repo: None,
                source_repo: String::new(),
                pr_id: None,
                slug: s.repo_slug.clone().unwrap_or_default(),
                bb_id: 0,
                title: branch.clone(),
                branch,
                base: s.base_branch.clone().unwrap_or_else(|| "dev".into()),
            })
        }
    }
}

fn repo_of(conn: &Connection, slug: &str) -> AppResult<PathBuf> {
    let repo = review::repos_root(conn)?.join(slug);
    if !repo.join(".git").exists() {
        return Err(AppError::Invalid(format!(
            "não achei o clone local de {slug} em {} (ajuste a pasta dos repositórios nas Configurações)",
            repo.display()
        )));
    }
    Ok(repo)
}

fn repo_of_target(conn: &Connection, t: &Target) -> AppResult<PathBuf> {
    let Some(rr) = &t.repo else { return repo_of(conn, &t.slug) };
    let repo = rr.clone_path(&review::repos_root(conn)?);
    if !repo.join(".git").exists() {
        return Err(AppError::Invalid(format!(
            "não achei o clone local de {} em {} (ajuste a pasta dos repositórios nas Configurações)",
            rr.slug,
            repo.display()
        )));
    }
    Ok(repo)
}

async fn remove_worktree(repo: &Path, wt: &Path) {
    if let Some(run_dir) = wt.parent() {
        review::related::remove(&related_dir(run_dir)).await;
    }
    if wt.exists() {
        let _ = git::raw(repo, &["worktree", "remove", "--force", &wt.display().to_string()]).await;
    }
    let _ = git::raw(repo, &["worktree", "prune"]).await;
}

// ---------------- trava entre o app e o terminal ----------------

/// PID do `pr-review-orchestrator comments` que está processando a correção (na `run_dir` dela).
pub const CLI_PID_FILE: &str = "cli.pid";

/// Quem está mexendo na correção, visto de fora do processo atual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Ninguém: pode rodar.
    Free,
    /// O app está aberto e a correção tem etapa em andamento.
    App,
    /// Outro `comments` do terminal está rodando nela.
    Terminal,
    /// Etapa em andamento sem dono vivo (app fechado no meio, terminal morto): `recover`.
    Stale,
}

fn own_pid(pid_file: &Path) -> bool {
    std::fs::read_to_string(pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(std::process::id())
}

/// `cli.pid` da pasta é de outro processo vivo deste programa.
fn other_cli_alive(run_dir: &Path) -> bool {
    let f = run_dir.join(CLI_PID_FILE);
    review::pid_alive(&f) && !own_pid(&f)
}

/// Sessão ou item com etapa em andamento (a conversa deixa a sessão 'ready' e marca só o item).
fn has_work(conn: &Connection, fix_id: &str) -> AppResult<bool> {
    Ok(conn.query_row(
        "SELECT status IN ('analyzing', 'committing')
                OR EXISTS (SELECT 1 FROM fix_comments WHERE fix_id = ?1 AND status IN ('analyzing', 'queued'))
         FROM fix_sessions WHERE id = ?1",
        [fix_id],
        |r| r.get(0),
    )?)
}

pub fn owner(conn: &Connection, data_dir: &Path, fix_id: &str) -> AppResult<Owner> {
    owner_with(conn, data_dir, fix_id, |p: &Path| {
        if p.ends_with(crate::APP_PID_FILE) { review::pid_alive(p) || other_app_running() } else { review::pid_alive(p) }
    })
}

/// `alive` recebe o caminho do arquivo de PID (`cli.pid` da correção ou `app.pid` da pasta de dados).
pub fn owner_with(conn: &Connection, data_dir: &Path, fix_id: &str, alive: impl Fn(&Path) -> bool) -> AppResult<Owner> {
    let cli = load_session(conn, fix_id)?.run_dir.join(CLI_PID_FILE);
    if alive(&cli) && !own_pid(&cli) {
        return Ok(Owner::Terminal);
    }
    if !has_work(conn, fix_id)? {
        return Ok(Owner::Free);
    }
    // cli.pid de terminal morto: a etapa era dele (o app apaga esse arquivo ao assumir, no `claim`)
    if cli.exists() && !own_pid(&cli) {
        return Ok(Owner::Stale);
    }
    Ok(if alive(&data_dir.join(crate::APP_PID_FILE)) { Owner::App } else { Owner::Stale })
}

/// Etapa interrompida numa correção: a sessão volta para pronta (ou falha, se o Claude nunca
/// respondeu) e os itens em andamento viram falha (ou continuam aplicados).
pub fn recover(conn: &Connection, fix_id: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE fix_sessions SET status = CASE WHEN status = 'committing' OR session_id IS NOT NULL THEN 'ready' ELSE 'failed' END,
           error = 'o app foi fechado durante a execução'
         WHERE id = ?1 AND status IN ('analyzing', 'committing')",
        [fix_id],
    )?;
    conn.execute(
        "UPDATE fix_comments SET status = CASE WHEN applied_at IS NOT NULL THEN 'applied' ELSE 'failed' END,
           error = CASE WHEN applied_at IS NOT NULL THEN error ELSE 'o app foi fechado enquanto o Claude respondia' END
         WHERE fix_id = ?1 AND status IN ('analyzing', 'queued')",
        [fix_id],
    )?;
    if let Ok(s) = load_session(conn, fix_id) {
        let _ = std::fs::remove_file(s.run_dir.join(CLI_PID_FILE));
    }
    Ok(())
}

/// O app vai rodar uma etapa na correção: recusa se um `comments` do terminal está nela. Um
/// `cli.pid` de terminal morto é apagado: a etapa agora é do app (o `owner` lê esse arquivo).
fn claim(host: &Host, fix_id: &str) -> AppResult<()> {
    if host.is_cli() {
        return Ok(()); // o terminal confere o dono antes, com o `owner`
    }
    claim_with(&run_dir_of(host, fix_id), review::pid_alive)
}

/// `alive` recebe o caminho do `cli.pid`.
fn claim_with(run_dir: &Path, alive: impl Fn(&Path) -> bool) -> AppResult<()> {
    let f = run_dir.join(CLI_PID_FILE);
    if !f.exists() || own_pid(&f) {
        return Ok(());
    }
    if alive(&f) {
        return Err(AppError::Invalid("o terminal está processando esta correção; espere terminar".into()));
    }
    let _ = std::fs::remove_file(&f);
    Ok(())
}

/// Linha de comando do app com janela (não do `comments`, do `-p` nem do `--sync`).
fn is_app_cmdline(args: &[&str]) -> bool {
    !args.iter().skip(1).any(|a| matches!(*a, "comments" | "-p" | "--print" | "--sync" | "-h" | "--help"))
}

/// Algum app com janela deste programa está aberto? Para o `.rpm` antigo, que não grava `app.pid`.
fn other_app_running() -> bool {
    let me = std::process::id().to_string();
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| {
        let pid = e.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let ours = std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim().starts_with("pr-review-orch"));
        let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let args: Vec<String> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        ours && is_app_cmdline(&args.iter().map(String::as_str).collect::<Vec<_>>())
    })
}

/// O terminal marca a correção como sua enquanto roda; o arquivo some ao sair.
pub struct CliLock(PathBuf);

impl CliLock {
    pub fn take(run_dir: &Path) -> AppResult<CliLock> {
        let f = run_dir.join(CLI_PID_FILE);
        std::fs::write(&f, std::process::id().to_string())?;
        Ok(CliLock(f))
    }
}

impl Drop for CliLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------- leitura ----------------

#[derive(Serialize)]
pub struct Message {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub cost_usd: Option<f64>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct CommentView {
    pub id: i64,
    pub seq: i64,
    pub bb_comment_id: Option<i64>,
    /// Item vindo de um achado do review (branch local): run e número do achado (F3).
    pub finding_run_id: Option<String>,
    pub finding_seq: Option<i64>,
    pub author: Option<String>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub outdated: bool,
    pub url: Option<String>,
    pub created_on: Option<String>,
    pub replies: Vec<comments::Reply>,
    pub status: String,
    pub explanation: Option<String>,
    pub edits: Vec<Edit>,
    pub error: Option<String>,
    pub applied_at: Option<String>,
    pub messages: Vec<Message>,
}

#[derive(Serialize)]
pub struct FixView {
    pub id: String,
    pub kind: String,
    pub pr_id: Option<i64>,
    pub repo_slug: Option<String>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub status: String,
    pub stage: Option<String>,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub base_sha: Option<String>,
    pub card_key: Option<String>,
    pub commit_sha: Option<String>,
    pub commit_subject: Option<String>,
    pub pushed_at: Option<String>,
    pub created_at: String,
    pub run_dir: String,
    /// O Claude (análise, conversa ou committer) está rodando agora.
    pub busy: bool,
    /// Quem está rodando é um `comments` do terminal (o painel relê a cada 3 s).
    pub terminal: bool,
    pub comments: Vec<CommentView>,
}

/// Resumo para as listas.
#[derive(Serialize)]
pub struct FixSummary {
    pub id: String,
    pub status: String,
    pub total: i64,
    /// Itens com sugestão esperando aprovação.
    pub pending: i64,
    pub applied: i64,
    /// Achados do review (F1, F2…) que já estão na correção: o painel do review marca.
    pub findings: Vec<(String, i64, String)>,
}

/// Última correção de cada PR (chave `pr:<id>`) e de cada branch local (`branch:<slug>:<branch>`).
pub fn summaries(conn: &Connection) -> AppResult<HashMap<String, FixSummary>> {
    let mut stmt = conn.prepare(
        "SELECT s.kind, s.pr_id, s.repo_slug, s.branch, s.id, s.status, count(c.id),
                sum(CASE WHEN c.status = 'procede' THEN 1 ELSE 0 END),
                sum(CASE WHEN c.status = 'applied' THEN 1 ELSE 0 END)
         FROM fix_sessions s LEFT JOIN fix_comments c ON c.fix_id = s.id
         WHERE s.id = (SELECT x.id FROM fix_sessions x
                       WHERE x.kind = s.kind AND (x.pr_id IS s.pr_id) AND (x.repo_slug IS s.repo_slug) AND (x.branch IS s.branch)
                       ORDER BY x.created_at DESC LIMIT 1)
         GROUP BY s.id",
    )?;
    let rows: Vec<(String, FixSummary)> = stmt
        .query_map([], |r| {
            let kind: String = r.get(0)?;
            let k = if kind == "pr" {
                format!("pr:{}", r.get::<_, i64>(1)?)
            } else {
                format!("branch:{}:{}", r.get::<_, String>(2)?, r.get::<_, String>(3)?)
            };
            Ok((
                k,
                FixSummary {
                    id: r.get(4)?,
                    status: r.get(5)?,
                    total: r.get(6)?,
                    pending: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    applied: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    findings: vec![],
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut out: HashMap<String, FixSummary> = rows.into_iter().collect();
    for s in out.values_mut() {
        s.findings = conn
            .prepare("SELECT finding_run_id, finding_seq, status FROM fix_comments WHERE fix_id = ?1 AND finding_seq IS NOT NULL")?
            .query_map([&s.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
    }
    Ok(out)
}

pub fn latest_pr_id(conn: &Connection, pr_id: i64) -> AppResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM fix_sessions WHERE kind = 'pr' AND pr_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [pr_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn latest_branch_id(conn: &Connection, slug: &str, branch: &str) -> AppResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM fix_sessions WHERE kind = 'branch' AND repo_slug = ?1 AND branch = ?2 ORDER BY created_at DESC LIMIT 1",
            params![slug, branch],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn view(conn: &Connection, fix_id: &str, busy: bool) -> AppResult<FixView> {
    let mut v = conn
        .query_row(
            "SELECT id, kind, pr_id, repo_slug, branch, base_branch, status, stage, error, cost_usd, base_sha, card_key,
                    commit_sha, commit_subject, pushed_at, created_at, run_dir
             FROM fix_sessions WHERE id = ?1",
            [fix_id],
            |r| {
                Ok(FixView {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    pr_id: r.get(2)?,
                    repo_slug: r.get(3)?,
                    branch: r.get(4)?,
                    base_branch: r.get(5)?,
                    status: r.get(6)?,
                    stage: r.get(7)?,
                    error: r.get(8)?,
                    cost_usd: r.get(9)?,
                    base_sha: r.get(10)?,
                    card_key: r.get(11)?,
                    commit_sha: r.get(12)?,
                    commit_subject: r.get(13)?,
                    pushed_at: r.get(14)?,
                    created_at: r.get(15)?,
                    run_dir: r.get(16)?,
                    busy,
                    terminal: false,
                    comments: vec![],
                })
            },
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid("correção não encontrada".into()))?;
    let mut stmt = conn.prepare(
        "SELECT id, seq, bb_comment_id, finding_run_id, finding_seq, author, body, path, line, outdated, url, created_on,
                replies_json, status, explanation, edits_json, error, applied_at
         FROM fix_comments WHERE fix_id = ?1 ORDER BY seq",
    )?;
    let mut items: Vec<CommentView> = stmt
        .query_map([fix_id], |r| {
            Ok(CommentView {
                id: r.get(0)?,
                seq: r.get(1)?,
                bb_comment_id: r.get(2)?,
                finding_run_id: r.get(3)?,
                finding_seq: r.get(4)?,
                author: r.get(5)?,
                body: r.get(6)?,
                path: r.get(7)?,
                line: r.get(8)?,
                outdated: r.get(9)?,
                url: r.get(10)?,
                created_on: r.get(11)?,
                replies: serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_default(),
                status: r.get(13)?,
                explanation: r.get(14)?,
                edits: serde_json::from_str(&r.get::<_, String>(15)?).unwrap_or_default(),
                error: r.get(16)?,
                applied_at: r.get(17)?,
                messages: vec![],
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT comment_id, id, role, content, cost_usd, created_at FROM fix_messages WHERE fix_id = ?1 ORDER BY id",
    )?;
    let msgs = stmt.query_map([fix_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            Message { id: r.get(1)?, role: r.get(2)?, content: r.get(3)?, cost_usd: r.get(4)?, created_at: r.get(5)? },
        ))
    })?;
    for m in msgs {
        let (cid, m) = m?;
        if let Some(c) = items.iter_mut().find(|c| c.id == cid) {
            c.messages.push(m);
        }
    }
    v.comments = items;
    v.terminal = other_cli_alive(Path::new(&v.run_dir));
    v.busy |= v.terminal;
    Ok(v)
}

// ---------------- início: Meus PRs ----------------

/// Começa uma análise nova dos comentários do PR. Com `force`, descarta as correções aprovadas
/// e ainda sem push da análise anterior.
pub fn start(host: Host, pr_id: i64, force: bool) -> AppResult<(String, Running)> {
    let state = host.state();
    let (target, repo, prev) = {
        let conn = state.db.lock().map_err(|_| db_err())?;
        let (target, category) = load_pr_target(&conn, pr_id)?;
        if category == "review_requested" {
            return Err(AppError::Invalid("a correção de comentários é só para PRs que você abriu".into()));
        }
        let repo = repo_of_target(&conn, &target)?;
        let prev = previous(&conn, latest_pr_id(&conn, pr_id)?)?;
        (target, repo, prev)
    };
    check_previous(&host, &prev, force)?;
    let fix_id = create_session(&host, &target)?;
    let prev_wt = prev.map(|p| p.1);
    let id = fix_id.clone();
    let task = tauri::async_runtime::spawn(async move {
        if let Some(wt) = prev_wt {
            remove_worktree(&repo, &wt).await;
        }
        let ctl = Arc::new(RunCtl::default());
        host.state().reviews.lock().unwrap().insert(key(&id), ctl.clone());
        emit(&host, &id, &target, None, true);
        let result = prepare_pr(&host, &id, &target, &repo, ctl.clone()).await;
        finish_worker(&host, &id, &target, &repo, result, &ctl);
    });
    Ok((fix_id, task))
}

/// "Buscar novos comentários": acrescenta as threads que chegaram depois, como 'new'. O que já
/// foi analisado ou aprovado fica como está.
pub fn refresh(host: Host, fix_id: String) -> AppResult<Running> {
    claim(&host, &fix_id)?;
    let (s, t, repo) = with_db(&host, |c| {
        let s = load_session(c, &fix_id)?;
        let t = target_of(c, &s)?;
        let repo = repo_of_target(c, &t)?;
        Ok((s, t, repo))
    })?;
    if t.kind != "pr" || s.status != "ready" {
        return Err(AppError::Invalid("só dá para buscar comentários numa correção de PR pronta".into()));
    }
    let ctl = Arc::new(RunCtl::default());
    {
        let st = host.state();
        let mut reg = st.reviews.lock().unwrap();
        if reg.contains_key(&key(&fix_id)) {
            return Err(AppError::Invalid("espere o Claude terminar".into()));
        }
        reg.insert(key(&fix_id), ctl.clone());
    }
    with_db(&host, |c| Ok(c.execute("UPDATE fix_sessions SET status = 'analyzing', error = NULL WHERE id = ?1", [&fix_id])?))?;
    emit(&host, &fix_id, &t, None, true);
    let task = tauri::async_runtime::spawn(async move {
        let result = add_threads(&host, &fix_id, &t, &ctl).await.map(|n| {
            emit(&host, &fix_id, &t, Some(&format!("{n} comentário(s) novo(s)")), true);
        });
        finish_worker(&host, &fix_id, &t, &repo, result, &ctl);
    });
    Ok(task)
}

/// "Analisar" (um comentário, vários, ou de novo depois de falhar): põe na fila do Claude.
pub fn analyze(host: Host, comment_ids: Vec<i64>) -> AppResult<Option<Running>> {
    let first = *comment_ids.first().ok_or_else(|| AppError::Invalid("escolha o comentário".into()))?;
    if let Some(l) = host.active_limit() {
        return Err(AppError::Invalid(format!(
            "limite de uso do Claude atingido ({}); tente quando liberar",
            l.limit.reset_text.unwrap_or(l.limit.message)
        )));
    }
    let fix_id = with_db(&host, |c| Ok(load_comment(c, first)?.fix_id))?;
    claim(&host, &fix_id)?;
    let (fix_id, t, repo) = with_db(&host, |c| {
        let s = load_session(c, &fix_id)?;
        if !matches!(s.status.as_str(), "ready" | "analyzing") {
            return Err(AppError::Invalid("esta correção não está aberta para análise".into()));
        }
        if !s.wt().join(".git").exists() {
            return Err(AppError::Invalid("o worktree desta correção sumiu; busque os comentários de novo".into()));
        }
        let t = target_of(c, &s)?;
        let repo = repo_of_target(c, &t)?;
        let mut queued = 0;
        for id in &comment_ids {
            queued += c.execute(
                "UPDATE fix_comments SET status = 'queued', error = NULL WHERE id = ?1 AND fix_id = ?2 AND status IN ('new', 'failed')",
                params![id, fix_id],
            )?;
        }
        if queued == 0 {
            return Err(AppError::Invalid("nenhum desses comentários está esperando análise".into()));
        }
        c.execute("UPDATE fix_sessions SET status = 'analyzing', error = NULL WHERE id = ?1 AND status = 'ready'", [&fix_id])?;
        Ok((fix_id, t, repo))
    })?;
    Ok(kick(&host, &fix_id, t, repo))
}

/// Sessão anterior: (id, worktree, status, itens aplicados).
type Previous = Option<(String, PathBuf, String, i64)>;

fn previous(conn: &Connection, id: Option<String>) -> AppResult<Previous> {
    let Some(id) = id else { return Ok(None) };
    let s = load_session(conn, &id)?;
    let applied: i64 =
        conn.query_row("SELECT count(*) FROM fix_comments WHERE fix_id = ?1 AND applied_at IS NOT NULL", [&id], |r| r.get(0))?;
    Ok(Some((id, s.wt(), s.status, applied)))
}

fn check_previous(host: &Host, prev: &Previous, force: bool) -> AppResult<()> {
    if let Some((id, _, status, applied)) = prev {
        if busy(host, id) {
            return Err(AppError::Invalid("a correção anterior ainda está rodando; espere ou cancele".into()));
        }
        if !matches!(status.as_str(), "pushed" | "committed" | "cancelled") && *applied > 0 && !force {
            return Err(AppError::Invalid(format!(
                "a correção anterior tem {applied} sugestão(ões) aprovada(s) e sem commit; começar de novo descarta essas alterações"
            )));
        }
    }
    if let Some(l) = host.active_limit() {
        return Err(AppError::Invalid(format!(
            "limite de uso do Claude atingido ({}); tente quando liberar",
            l.limit.reset_text.unwrap_or(l.limit.message)
        )));
    }
    Ok(())
}

fn create_session(host: &Host, t: &Target) -> AppResult<String> {
    if git::is_protected(&t.branch) {
        return Err(AppError::Invalid(format!("recusado: a branch {} é protegida", t.branch)));
    }
    let state = host.state();
    let fix_id = new_id();
    let run_dir = state.db_path.parent().unwrap().join("fixes").join(&fix_id);
    std::fs::create_dir_all(&run_dir)?;
    let conn = state.db.lock().map_err(|_| db_err())?;
    let card = review::configured_jira_key(&conn, &t.branch, &t.title)?;
    conn.execute(
        "INSERT INTO fix_sessions (id, kind, pr_id, repo_slug, branch, base_branch, run_dir, card_key, status, stage)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'analyzing', 'preparando')",
        params![fix_id, t.kind, t.pr_id, t.slug, t.branch, t.base, run_dir.display().to_string(), card],
    )?;
    Ok(fix_id)
}

fn set_stage(host: &Host, fix_id: &str, t: &Target, s: &str) {
    let _ = with_db(host, |c| Ok(c.execute("UPDATE fix_sessions SET stage = ?1 WHERE id = ?2", params![s, fix_id])?));
    emit(host, fix_id, t, Some(&format!("== {s}")), true);
}

fn check(ctl: &RunCtl) -> AppResult<()> {
    if ctl.is_cancelled() { Err(AppError::Invalid("cancelado".into())) } else { Ok(()) }
}

/// PR: comentários do Bitbucket → worktree na ponta da origin → itens na fila.
/// PR: worktree na ponta da origin e os comentários abertos como 'new' (sem análise: o usuário
/// escolhe quais o Claude analisa).
async fn prepare_pr(host: &Host, fix_id: &str, t: &Target, repo: &Path, ctl: Arc<RunCtl>) -> AppResult<()> {
    set_stage(host, fix_id, t, "buscando o código da branch (git fetch)");
    let rr = t.repo.as_ref().ok_or_else(|| AppError::Invalid("correção de PR sem repositório".into()))?;
    let (src, _) = rr.fetch_tips(repo, t.bb_id, &t.branch, &t.base).await?;
    let s = with_db(host, |c| load_session(c, fix_id))?;
    git::run(repo, &["worktree", "add", "--detach", &s.wt().display().to_string(), &src]).await?;
    with_db(host, |c| Ok(c.execute("UPDATE fix_sessions SET base_sha = ?1 WHERE id = ?2", params![src, fix_id])?))?;
    check(&ctl)?;
    let n = add_threads(host, fix_id, t, &ctl).await?;
    if n == 0 {
        emit(host, fix_id, t, Some("nenhum comentário aberto de outra pessoa neste PR"), true);
    }
    Ok(())
}

/// Threads abertas do PR que ainda não estão na correção → itens 'new'. Devolve quantas entraram.
async fn add_threads(host: &Host, fix_id: &str, t: &Target, ctl: &RunCtl) -> AppResult<usize> {
    set_stage(host, fix_id, t, "buscando os comentários da PR");
    let rr = t.repo.as_ref().ok_or_else(|| AppError::Invalid("correção de PR sem repositório".into()))?;
    let me = with_db(host, |c| crate::hosting::me(c, rr.kind))?.ok_or_else(|| {
        AppError::Invalid("clique em Atualizar na lista antes (o app ainda não sabe quem é você)".into())
    })?;
    let provider = crate::hosting::Provider::connect(rr.kind, &host.state().db, host.state().secrets.as_ref()).await?;
    let threads = provider.open_threads(rr, t.bb_id, &me).await?;
    check(ctl)?;
    with_db(host, |c| {
        let mut added = 0;
        for th in &threads {
            let exists: bool = c.query_row(
                "SELECT count(*) > 0 FROM fix_comments WHERE fix_id = ?1 AND bb_comment_id = ?2",
                params![fix_id, th.id],
                |r| r.get(0),
            )?;
            if exists {
                // comentário ainda não analisado: atualiza texto e respostas
                c.execute(
                    "UPDATE fix_comments SET body = ?1, replies_json = ?2, outdated = ?3
                     WHERE fix_id = ?4 AND bb_comment_id = ?5 AND status = 'new'",
                    params![th.body, serde_json::to_string(&th.replies)?, th.outdated, fix_id, th.id],
                )?;
                continue;
            }
            let next: i64 = c.query_row("SELECT ifnull(max(seq), 0) + 1 FROM fix_comments WHERE fix_id = ?1", [fix_id], |r| r.get(0))?;
            c.execute(
                "INSERT INTO fix_comments (fix_id, seq, bb_comment_id, author, body, path, line, outdated, url, created_on,
                   replies_json, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'new')",
                params![
                    fix_id,
                    next,
                    th.id,
                    th.author,
                    th.body,
                    th.path,
                    th.line,
                    th.outdated,
                    th.url,
                    th.created_on,
                    serde_json::to_string(&th.replies)?
                ],
            )?;
            added += 1;
        }
        Ok(added)
    })
}

// ---------------- início: Minhas branches ----------------

/// Achado ou melhoria do review → texto do item para o agente.
fn finding_item(conn: &Connection, run_id: &str, seq: i64) -> AppResult<(String, String, Option<String>, Option<i64>)> {
    conn.query_row(
        "SELECT kind, coalesce(audit_new_severity, severity), category, file, line, title, description, steps_json,
                markdown, validation
         FROM findings WHERE run_id = ?1 AND seq = ?2",
        params![run_id, seq],
        |r| {
            let kind: Option<String> = r.get(0)?;
            let sev: Option<String> = r.get(1)?;
            let category: Option<String> = r.get(2)?;
            let author = match kind.as_deref() {
                Some("melhoria") => format!("melhoria do review ({})", category.unwrap_or_else(|| "complexidade".into())),
                _ => format!("achado do review ({})", sev.unwrap_or_else(|| "sem severidade".into())),
            };
            let markdown: Option<String> = r.get(8)?;
            let body = match markdown.filter(|m| !m.trim().is_empty()) {
                Some(m) => m,
                None => {
                    let steps: Vec<String> = serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default();
                    let steps = steps.iter().enumerate().map(|(i, s)| format!("{}. {s}", i + 1)).collect::<Vec<_>>().join("\n");
                    [r.get::<_, Option<String>>(5)?.unwrap_or_default(), r.get::<_, Option<String>>(6)?.unwrap_or_default(), steps]
                        .into_iter()
                        .filter(|x| !x.trim().is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            };
            Ok((author, body, r.get(3)?, r.get(4)?))
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("achado F{seq} não encontrado neste review")))
}

/// "Corrigir" em achados de um review de branch local: entram na fila da correção da branch
/// (criada se preciso). Devolve o id da correção.
pub async fn add_findings(host: Host, run_id: String, seqs: Vec<i64>) -> AppResult<String> {
    if seqs.is_empty() {
        return Err(AppError::Invalid("escolha o achado a corrigir".into()));
    }
    let (slug, branch, base, repo, latest) = {
        let st = host.state();
        let conn = st.db.lock().map_err(|_| db_err())?;
        let row: Option<(Option<String>, Option<String>, Option<String>, String)> = conn
            .query_row("SELECT repo_slug, branch, base_branch, status FROM review_runs WHERE id = ?1", [&run_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((Some(slug), Some(branch), base, status)) = row else {
            return Err(AppError::Invalid("a correção de achados é só para review de branch local".into()));
        };
        if status != "completed" {
            return Err(AppError::Invalid("espere o review terminar".into()));
        }
        let repo = repo_of(&conn, &slug)?;
        let latest = latest_branch_id(&conn, &slug, &branch)?;
        (slug, branch, base.unwrap_or_else(|| "dev".into()), repo, latest)
    };
    let t = Target { kind: "branch", repo: None, source_repo: String::new(), pr_id: None, slug, bb_id: 0, title: branch.clone(), branch: branch.clone(), base };
    let tip = git::run(&repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}^{{commit}}")])
        .await
        .map_err(|_| AppError::Invalid(format!("a branch {branch} não existe mais no clone")))?;

    // reaproveita a correção em andamento, se ela ainda parte da ponta atual da branch
    let mut fix_id = None;
    if let Some(id) = latest {
        let (s, applied) = with_db(&host, |c| {
            let applied: i64 =
                c.query_row("SELECT count(*) FROM fix_comments WHERE fix_id = ?1 AND applied_at IS NOT NULL", [&id], |r| r.get(0))?;
            Ok((load_session(c, &id)?, applied))
        })?;
        if s.status == "committing" {
            return Err(AppError::Invalid("espere o commit em andamento terminar".into()));
        }
        let open = matches!(s.status.as_str(), "analyzing" | "ready");
        let same_tip = s.base_sha.as_deref().is_none_or(|b| b == tip);
        if open && same_tip {
            fix_id = Some(id);
        } else if open && applied > 0 {
            return Err(AppError::Invalid(format!(
                "a branch {branch} recebeu commits depois das correções aprovadas; faça o commit delas ou descarte antes"
            )));
        } else if open {
            if busy(&host, &id) {
                return Err(AppError::Invalid("a correção anterior ainda está rodando; espere ou cancele".into()));
            }
            with_db(&host, |c| Ok(c.execute("UPDATE fix_sessions SET status = 'cancelled', stage = 'substituída' WHERE id = ?1", [&id])?))?;
            remove_worktree(&repo, &s.wt()).await;
        }
    }
    let fix_id = match fix_id {
        Some(id) => id,
        None => {
            check_previous(&host, &None, false)?;
            create_session(&host, &t)?
        }
    };

    with_db(&host, |c| {
        for seq in &seqs {
            let existing: Option<(i64, String)> = c
                .query_row(
                    "SELECT id, status FROM fix_comments WHERE fix_id = ?1 AND finding_run_id = ?2 AND finding_seq = ?3",
                    params![fix_id, run_id, seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match existing {
                // falhou antes: "Corrigir" de novo põe na fila outra vez
                Some((id, st)) if st == "failed" => {
                    c.execute("UPDATE fix_comments SET status = 'queued', error = NULL WHERE id = ?1", [id])?;
                }
                Some(_) => {}
                None => {
                    let (author, body, path, line) = finding_item(c, &run_id, *seq)?;
                    let next: i64 = c.query_row("SELECT ifnull(max(seq), 0) + 1 FROM fix_comments WHERE fix_id = ?1", [&fix_id], |r| r.get(0))?;
                    c.execute(
                        "INSERT INTO fix_comments (fix_id, seq, finding_run_id, finding_seq, author, body, path, line, status)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued')",
                        params![fix_id, next, run_id, seq, author, body, path, line],
                    )?;
                }
            }
        }
        c.execute("UPDATE fix_sessions SET status = 'analyzing', error = NULL WHERE id = ?1 AND status = 'ready'", [&fix_id])?;
        Ok(())
    })?;
    let _ = kick(&host, &fix_id, t, repo);
    Ok(fix_id)
}

/// Processa a fila em segundo plano, se ninguém estiver processando.
fn kick(host: &Host, fix_id: &str, t: Target, repo: PathBuf) -> Option<Running> {
    let ctl = Arc::new(RunCtl::default());
    {
        let st = host.state();
        let mut reg = st.reviews.lock().unwrap();
        if reg.contains_key(&key(fix_id)) {
            emit(host, fix_id, &t, None, true);
            return None; // quem está rodando pega os itens novos ao terminar
        }
        reg.insert(key(fix_id), ctl.clone());
    }
    emit(host, fix_id, &t, None, true);
    let (host, id) = (host.clone(), fix_id.to_owned());
    Some(tauri::async_runtime::spawn(async move {
        let result = async {
            let s = with_db(&host, |c| load_session(c, &id))?;
            if !s.wt().join(".git").exists() && t.kind == "pr" {
                return Err(AppError::Invalid("o worktree desta correção sumiu; busque os comentários de novo".into()));
            }
            if !s.wt().join(".git").exists() {
                set_stage(&host, &id, &t, "criando o worktree na ponta da branch");
                let tip = git::run(&repo, &["rev-parse", &format!("refs/heads/{}^{{commit}}", t.branch)]).await?;
                git::run(&repo, &["worktree", "add", "--detach", &s.wt().display().to_string(), &tip]).await?;
                with_db(&host, |c| Ok(c.execute("UPDATE fix_sessions SET base_sha = ?1 WHERE id = ?2", params![tip, id])?))?;
            }
            run_queue(&host, &id, &t, &repo, ctl.clone()).await
        }
        .await;
        finish_worker(&host, &id, &t, &repo, result, &ctl);
    }))
}

/// Fim do processamento da fila: a correção fica pronta (ou falha, sem worktree).
fn finish_worker(host: &Host, fix_id: &str, t: &Target, repo: &Path, result: AppResult<()>, ctl: &RunCtl) {
    let error = match &result {
        Ok(()) => None,
        Err(_) if ctl.is_cancelled() => Some("cancelado pelo usuário".to_owned()),
        Err(e) => Some(e.to_string()),
    };
    let has_wt = with_db(host, |c| load_session(c, fix_id)).map(|s| s.wt().join(".git").exists()).unwrap_or(false);
    let status = match (&error, has_wt) {
        (None, _) | (Some(_), true) => "ready",
        (Some(_), false) if ctl.is_cancelled() => "cancelled",
        _ => "failed",
    };
    // sem erro, item ainda 'queued' entrou depois da última volta: fica para o `release`
    let stuck = if error.is_some() { "('analyzing', 'queued')" } else { "('analyzing')" };
    let _ = with_db(host, |c| {
        c.execute(
            "UPDATE fix_sessions SET status = ?1, error = ?2, stage = ?3 WHERE id = ?4 AND status = 'analyzing'",
            params![status, error, if status == "ready" { "pronto" } else { status }, fix_id],
        )?;
        c.execute(
            &format!(
                "UPDATE fix_comments SET status = CASE WHEN applied_at IS NOT NULL THEN 'applied' ELSE 'failed' END,
                   error = CASE WHEN applied_at IS NOT NULL THEN error ELSE ifnull(error, ?1) END
                 WHERE fix_id = ?2 AND status IN {stuck}"
            ),
            params![error.clone().unwrap_or_else(|| "o Claude não devolveu este item".into()), fix_id],
        )?;
        Ok(())
    });
    release(host, fix_id, t, repo);
}

/// Libera a correção para a próxima etapa e, se algum item entrou na fila enquanto outra coisa
/// rodava (análise ou conversa), processa. A chave sai do registro **antes** da contagem: item
/// que chegar depois encontra a chave livre e o próprio `add_findings` dispara a fila.
fn release(host: &Host, fix_id: &str, t: &Target, repo: &Path) {
    host.state().reviews.lock().unwrap().remove(&key(fix_id));
    emit(host, fix_id, t, None, false);
    let pending = with_db(host, |c| {
        let queued: i64 =
            c.query_row("SELECT count(*) FROM fix_comments WHERE fix_id = ?1 AND status = 'queued'", [fix_id], |r| r.get(0))?;
        let status: String = c.query_row("SELECT status FROM fix_sessions WHERE id = ?1", [fix_id], |r| r.get(0))?;
        Ok(queued > 0 && matches!(status.as_str(), "ready" | "analyzing"))
    })
    .unwrap_or(false);
    if pending {
        let _ = with_db(host, |c| Ok(c.execute("UPDATE fix_sessions SET status = 'analyzing' WHERE id = ?1", [fix_id])?));
        let _ = kick(host, fix_id, t.clone(), repo.to_path_buf());
    }
}

// ---------------- fila: o agente confere os itens ----------------

fn analyzer_opts(run_dir: &Path, schema: &str, jira: bool) -> AppResult<Vec<String>> {
    let related = related_dir(run_dir);
    let mut allowed = review::READ_ONLY_TOOLS.replace("Agent ", "");
    if jira { allowed.push_str(" ToolSearch mcp__atlassian__getJiraIssue"); }
    let mut opts: Vec<String> = vec![
        "--agent".into(), "comment-fixer".into(), "--agents".into(), review::agents_json(&["comment-fixer"])?,
        "--model".into(), "sonnet".into(), "--effort".into(), "medium".into(),
        "--json-schema".into(), std::fs::read_to_string(review::assets().join("schemas").join(schema))?,
        "--add-dir".into(), run_dir.display().to_string(),
        "--allowedTools".into(), allowed,
        "--disallowedTools".into(), "Edit Write NotebookEdit Agent mcp__claude-in-chrome__*".into(),
    ];
    if related.join(".git").exists() {
        opts.extend(["--add-dir".into(), related.display().to_string()]);
    }
    // sem cache de 5 min: a conversa sobre os itens pode vir bem depois da análise
    opts.extend(review::worker_config(false, jira));
    opts.extend(review::writing_style_opts());
    Ok(opts)
}

fn cost_of(out: &claude::Outcome) -> f64 {
    out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0)
}

fn add_cost(host: &Host, fix_id: &str, cost: f64) {
    let _ = with_db(host, |c| Ok(c.execute("UPDATE fix_sessions SET cost_usd = cost_usd + ?1 WHERE id = ?2", params![cost, fix_id])?));
}

fn limit_error(host: &Host, out: &claude::Outcome) -> Option<AppError> {
    let l = out.limit.as_ref()?;
    host.set_limit(l.clone());
    Some(AppError::Invalid(format!(
        "limite de uso do Claude atingido ({}); tente de novo quando liberar",
        l.reset_text.clone().unwrap_or_else(|| l.message.clone())
    )))
}

fn failure(out: &claude::Outcome, what: &str, timeout: &str) -> AppError {
    match out.status {
        Status::Cancelled => AppError::Invalid("cancelado".into()),
        Status::TimedOut => AppError::Invalid(format!("{what} passou do tempo limite ({timeout})")),
        _ => {
            let text = out.result.as_ref().and_then(|r| r.get("result")).and_then(Value::as_str).unwrap_or("").trim().to_owned();
            AppError::Invalid(format!(
                "{what} falhou (código {:?}), sem saída estruturada válida. {}",
                out.exit_code,
                if text.is_empty() { out.stderr_tail.clone() } else { text }
            ))
        }
    }
}

/// Processa os itens `queued` **um por vez**, na ordem, até a fila esvaziar: cada comentário ou
/// achado tem a própria análise e aparece pronto assim que termina. A primeira rodada abre a
/// sessão do Claude; as seguintes a retomam, para o agente lembrar do que já viu.
async fn run_queue(host: &Host, fix_id: &str, t: &Target, repo: &Path, ctl: Arc<RunCtl>) -> AppResult<()> {
    let mut round = 0;
    loop {
        check(&ctl)?;
        let (items, waiting): (Vec<Value>, i64) = with_db(host, |c| {
            let mut stmt = c.prepare(
                "SELECT seq, author, body, path, line, outdated, replies_json, finding_seq, id FROM fix_comments
                 WHERE fix_id = ?1 AND status = 'queued' ORDER BY seq LIMIT 1",
            )?;
            let mut row_id = None;
            let rows: Vec<Value> = stmt
                .query_map([fix_id], |r| {
                    let finding: Option<i64> = r.get(7)?;
                    row_id = Some(r.get::<_, i64>(8)?);
                    Ok(json!({
                        "id": format!("C{}", r.get::<_, i64>(0)?),
                        // como a tela chama o item: F3 (achado do review) ou C3 (comentário)
                        "label": match finding { Some(f) => format!("F{f}"), None => format!("C{}", r.get::<_, i64>(0)?) },
                        "origin": if finding.is_some() { "review" } else { "comentario" },
                        "author": r.get::<_, Option<String>>(1)?,
                        "body": r.get::<_, String>(2)?,
                        "path": r.get::<_, Option<String>>(3)?,
                        "line": r.get::<_, Option<i64>>(4)?,
                        "outdated": r.get::<_, bool>(5)?,
                        "replies": serde_json::from_str::<Value>(&r.get::<_, String>(6)?).unwrap_or(json!([])),
                    }))
                })?
                .collect::<Result<_, _>>()?;
            if let Some(id) = row_id {
                c.execute("UPDATE fix_comments SET status = 'analyzing', error = NULL WHERE id = ?1", [id])?;
            }
            let waiting: i64 =
                c.query_row("SELECT count(*) FROM fix_comments WHERE fix_id = ?1 AND status = 'queued'", [fix_id], |r| r.get(0))?;
            Ok((rows, waiting))
        })?;
        if items.is_empty() {
            return Ok(());
        }
        emit(host, fix_id, t, None, true);
        round += 1;
        let s = with_db(host, |c| load_session(c, fix_id))?;
        let wt = s.wt();
        let items_file = s.run_dir.join(format!("items-{round}.json"));
        std::fs::write(&items_file, serde_json::to_vec_pretty(&items)?)?;
        let id = items[0]["label"].as_str().unwrap_or("?");
        let more = if waiting > 0 { format!(" ({waiting} na fila)") } else { String::new() };
        set_stage(host, fix_id, t, &format!("Sonnet conferindo {id} no código{more}"));

        let (session, prompt) = match &s.session_id {
            Some(id) => (
                Session::Resume(id.clone()),
                format!(
                    "{}Novos itens para conferir e corrigir: COMMENTS_FILE: {}\n\
                     Releia os arquivos antes: sugestões aprovadas podem ter mudado o código.\n\
                     Devolva um item em `comments` para cada um, com o mesmo id.",
                    late_related(host, t, &s).await,
                    items_file.display()
                ),
            ),
            None => (Session::New(uuid::Uuid::new_v4().to_string()), first_prompt(t, &s, &wt, &items_file, repo).await),
        };
        let mut opts = analyzer_opts(&s.run_dir, "fix-analysis.json", s.card_key.is_some())?;
        opts.extend(["--max-turns".into(), "80".into(), "--max-budget-usd".into(), "3".into()]);
        let spec = Spec {
            session: session.clone(),
            name: format!("fix-rodada{round}"),
            opts,
            prompt,
            cwd: wt.clone(),
            log_dir: s.run_dir.join("logs"),
            env_extra: vec![],
            timeout: ANALYZE_TIMEOUT,
        };
        let out = claude::run(spec, ctl.clone(), |l| emit(host, fix_id, t, Some(&l), true)).await?;
        add_cost(host, fix_id, cost_of(&out));
        if let Some(e) = limit_error(host, &out) {
            return Err(e);
        }
        let results = match (out.status, out.result.as_ref().and_then(|r| r.pointer("/structured_output/comments")).and_then(Value::as_array)) {
            (Status::Completed, Some(items)) => items.clone(),
            _ => return Err(failure(&out, "a análise", "30 min")),
        };
        with_db(host, |c| {
            // só uma sessão que respondeu vale para a conversa e as próximas rodadas
            if let Session::New(id) = &session {
                c.execute("UPDATE fix_sessions SET session_id = ?1 WHERE id = ?2", params![id, fix_id])?;
            }
            for it in &results {
                let Some(seq) = it["id"].as_str().and_then(|x| x.trim().trim_start_matches('C').parse::<i64>().ok()) else { continue };
                let edits: Vec<Edit> = serde_json::from_value(it["edits"].clone()).unwrap_or_default();
                let (status, edits, error) = judge(&wt, it["verdict"].as_str().unwrap_or(""), edits, false);
                c.execute(
                    "UPDATE fix_comments SET status = ?1, explanation = ?2, edits_json = ?3, error = ?4
                     WHERE fix_id = ?5 AND seq = ?6 AND status = 'analyzing'",
                    params![status, it["explanation"].as_str().unwrap_or("").trim(), serde_json::to_string(&edits)?, error, fix_id, seq],
                )?;
            }
            c.execute(
                "UPDATE fix_comments SET status = 'failed', error = 'o Claude não devolveu este item' WHERE fix_id = ?1 AND status = 'analyzing'",
                [fix_id],
            )?;
            Ok(())
        })?;
        emit(host, fix_id, t, None, true);
    }
}

async fn first_prompt(t: &Target, s: &SessionRow, wt: &Path, items_file: &Path, repo: &Path) -> String {
    let base = git::run(repo, &["rev-parse", &format!("refs/remotes/origin/{}^{{commit}}", t.base)]).await.unwrap_or_else(|_| format!("origin/{}", t.base));
    let head = s.base_sha.clone().unwrap_or_default();
    let changed = git::run(wt, &["diff", "--name-only", &format!("{base}...HEAD")]).await.unwrap_or_default();
    let files: Vec<&str> = changed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let conventions = match review::conventions::collect(wt, &files) {
        Some(text) => {
            let p = s.run_dir.join("conventions.md");
            match std::fs::write(&p, text) {
                Ok(()) => p.display().to_string(),
                Err(_) => "nenhum".into(),
            }
        }
        None => "nenhum".into(),
    };
    let card_line = match &s.card_key {
        Some(k) => format!(
            "CARD_KEY: {k} ({})",
            review::card_read_hint(s.jira_site.as_deref())
        ),
        None => "CARD_KEY: (nenhuma)".into(),
    };
    // o outro lado da task (front ↔ back), na mesma pasta dos clones
    let related = match repo.parent() {
        Some(root) => review::related::prepare(root, &t.slug, &t.branch, &t.base, &related_dir(&s.run_dir)).await,
        None => None,
    };
    let related = match related {
        Some(r) => format!("{}\n", r.prompt_line(&t.branch)),
        None => "PROJETO_RELACIONADO: nenhum\n".into(),
    };
    format!(
        "WORKTREE: {wt}\n{target}\nHEAD: {head}\nBASE: {base}\n{card_line}\nCONVENTIONS_FILE: {conventions}\n{related}COMMENTS_FILE: {items}\n\n\
         Confira cada item no código e devolva o veredito e, quando procede, a sugestão de correção.",
        wt = wt.display(),
        target = t.describe(),
        items = items_file.display(),
    )
}

/// Veredito do agente → situação do item. Uma sugestão que não se aplica ao código atual
/// vira `failed` (o usuário pede outra no chat). `was_applied`: o item já teve uma sugestão
/// gravada; sem sugestão nova, ele continua resolvido.
fn judge(wt: &Path, verdict: &str, edits: Vec<Edit>, was_applied: bool) -> (&'static str, Vec<Edit>, Option<String>) {
    match verdict {
        "procede" if edits.is_empty() && was_applied => ("applied", edits, None),
        "procede" if edits.is_empty() => ("failed", edits, Some("o Claude disse que procede, mas não trouxe a sugestão; peça no chat".into())),
        "procede" => match apply::check(wt, &edits) {
            None => ("procede", edits, None),
            Some(e) => ("failed", edits, Some(e)),
        },
        _ if was_applied => ("applied", vec![], None),
        "resposta" => ("resposta", vec![], None),
        _ => ("nao_procede", vec![], None),
    }
}

pub fn cancel(state: &AppState, fix_id: &str) -> AppResult<()> {
    match state.reviews.lock().unwrap().get(&key(fix_id)).cloned() {
        Some(c) => {
            c.cancel();
            Ok(())
        }
        None => Err(AppError::Invalid("nada em andamento nesta correção".into())),
    }
}

/// Descarta a correção (e o que foi aprovado e não commitado): remove o worktree.
pub async fn discard(host: Host, fix_id: String) -> AppResult<()> {
    if busy(&host, &fix_id) {
        return Err(AppError::Invalid("cancele o que está rodando antes de descartar".into()));
    }
    let (s, t, repo) = with_db(&host, |c| {
        let s = load_session(c, &fix_id)?;
        let t = target_of(c, &s)?;
        let repo = repo_of_target(c, &t)?;
        Ok((s, t, repo))
    })?;
    if matches!(s.status.as_str(), "committed" | "pushed") {
        return Err(AppError::Invalid("esta correção já foi commitada".into()));
    }
    with_db(&host, |c| {
        Ok(c.execute("UPDATE fix_sessions SET status = 'cancelled', stage = 'descartada' WHERE id = ?1", [&fix_id])?)
    })?;
    remove_worktree(&repo, &s.wt()).await;
    emit(&host, &fix_id, &t, None, false);
    Ok(())
}

// ---------------- conversa sobre um item ----------------

struct CommentRow {
    fix_id: String,
    seq: i64,
    author: Option<String>,
    path: Option<String>,
    line: Option<i64>,
    status: String,
    applied_at: Option<String>,
    edits: Vec<Edit>,
}

fn load_comment(conn: &Connection, comment_id: i64) -> AppResult<CommentRow> {
    conn.query_row(
        "SELECT fix_id, seq, author, path, line, status, applied_at, edits_json FROM fix_comments WHERE id = ?1",
        [comment_id],
        |r| {
            Ok(CommentRow {
                fix_id: r.get(0)?,
                seq: r.get(1)?,
                author: r.get(2)?,
                path: r.get(3)?,
                line: r.get(4)?,
                status: r.get(5)?,
                applied_at: r.get(6)?,
                edits: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
            })
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid("item não encontrado".into()))
}

fn status_text(status: &str, applied: bool) -> &'static str {
    match (status, applied) {
        ("applied", _) => "a sugestão aprovada já está aplicada no worktree",
        ("procede", true) => "há uma sugestão nova esperando aprovação; uma anterior já foi aplicada",
        ("procede", false) => "a sugestão está esperando a aprovação do autor (ainda não foi aplicada)",
        ("nao_procede", _) => "você concluiu que não procede",
        ("resposta", _) => "você concluiu que não pede mudança e respondeu",
        _ => "a última tentativa falhou ou a sugestão não batia com o código",
    }
}

/// Mensagem do usuário sobre um item; a resposta vem em segundo plano.
pub fn ask(host: Host, comment_id: i64, text: String) -> AppResult<Running> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(AppError::Invalid("escreva a mensagem".into()));
    }
    if let Some(l) = host.active_limit() {
        return Err(AppError::Invalid(format!(
            "limite de uso do Claude atingido ({}); a mensagem fica para quando liberar",
            l.limit.reset_text.unwrap_or(l.limit.message)
        )));
    }
    let state = host.state();
    let (c, s, t) = {
        let conn = state.db.lock().map_err(|_| db_err())?;
        let c = load_comment(&conn, comment_id)?;
        let s = load_session(&conn, &c.fix_id)?;
        let t = target_of(&conn, &s)?;
        (c, s, t)
    };
    claim(&host, &c.fix_id)?;
    if s.status != "ready" {
        return Err(AppError::Invalid(match s.status.as_str() {
            "pushed" | "committed" => "esta correção já foi commitada; comece outra".into(),
            "analyzing" | "committing" => "espere a etapa em andamento terminar".into(),
            _ => "a correção não terminou bem; comece outra".into(),
        }));
    }
    let Some(session_id) = s.session_id.clone() else {
        return Err(AppError::Invalid("a análise ainda não respondeu nenhuma vez; clique em Corrigir de novo".into()));
    };
    let ctl = Arc::new(RunCtl::default());
    {
        let mut reg = state.reviews.lock().unwrap();
        if reg.contains_key(&key(&c.fix_id)) {
            return Err(AppError::Invalid("o Claude ainda está respondendo; espere terminar".into()));
        }
        reg.insert(key(&c.fix_id), ctl.clone());
    }
    {
        let conn = state.db.lock().map_err(|_| db_err())?;
        conn.execute(
            "INSERT INTO fix_messages (fix_id, comment_id, role, content) VALUES (?1, ?2, 'user', ?3)",
            params![c.fix_id, comment_id, text],
        )?;
        conn.execute("UPDATE fix_comments SET status = 'analyzing' WHERE id = ?1", [comment_id])?;
    }
    emit(&host, &c.fix_id, &t, None, true);

    let task = tauri::async_runtime::spawn(async move {
        let result = turn(&host, &c, &s, &t, &session_id, &text, ctl.clone()).await;
        let fix_id = c.fix_id.clone();
        let _ = with_db(&host, |conn| {
            match &result {
                Ok(r) => {
                    let (status, edits, error) = judge(&s.wt(), &r.verdict, r.edits.clone(), c.applied_at.is_some());
                    if status == "applied" {
                        // continua resolvido: a explicação e a sugestão gravada ficam como estavam
                        conn.execute("UPDATE fix_comments SET status = 'applied', error = NULL WHERE id = ?1", [comment_id])?;
                    } else {
                        conn.execute(
                            "UPDATE fix_comments SET status = ?1, explanation = ?2, edits_json = ?3, error = ?4 WHERE id = ?5",
                            params![status, r.explanation, serde_json::to_string(&edits)?, error, comment_id],
                        )?;
                    }
                    conn.execute(
                        "INSERT INTO fix_messages (fix_id, comment_id, role, content, cost_usd) VALUES (?1, ?2, 'assistant', ?3, ?4)",
                        params![fix_id, comment_id, r.reply, r.cost],
                    )?;
                }
                Err(e) => {
                    conn.execute("UPDATE fix_comments SET status = ?1 WHERE id = ?2", params![c.status, comment_id])?;
                    let msg = if ctl.is_cancelled() { "cancelado".to_owned() } else { e.to_string() };
                    conn.execute(
                        "INSERT INTO fix_messages (fix_id, comment_id, role, content) VALUES (?1, ?2, 'error', ?3)",
                        params![fix_id, comment_id, msg],
                    )?;
                }
            }
            Ok(())
        });
        match with_db(&host, |c| repo_of_target(c, &t)) {
            Ok(repo) => release(&host, &fix_id, &t, &repo),
            Err(_) => {
                host.state().reviews.lock().unwrap().remove(&key(&fix_id));
                emit(&host, &fix_id, &t, None, false);
            }
        }
    });
    Ok(task)
}

struct Turn {
    reply: String,
    verdict: String,
    explanation: String,
    edits: Vec<Edit>,
    cost: f64,
}

async fn turn(
    host: &Host,
    c: &CommentRow,
    s: &SessionRow,
    t: &Target,
    session_id: &str,
    text: &str,
    ctl: Arc<RunCtl>,
) -> AppResult<Turn> {
    let wt = s.wt();
    if !wt.join(".git").exists() {
        return Err(AppError::Invalid("o worktree desta correção sumiu; comece outra".into()));
    }
    let place = match (&c.path, c.line) {
        (Some(p), Some(l)) => format!(", em {p}:{l}"),
        (Some(p), None) => format!(", em {p}"),
        _ => String::new(),
    };
    let current = if c.edits.is_empty() || c.status == "applied" {
        String::new()
    } else {
        format!("\nSugestão atual (ainda não aplicada):\n{}\n", serde_json::to_string_pretty(&c.edits).unwrap_or_default())
    };
    let related = late_related(host, t, s).await;
    let prompt = format!(
        "{related}Mensagem do autor sobre o item C{seq} ({author}{place}).\n\
         Situação: {situation}.{current}\n\
         Releia os arquivos antes de responder: sugestões aprovadas de outros itens podem ter mudado o código.\n\
         Devolva `reply`, `verdict`, `explanation` e `edits` deste item.\n\n\
         Mensagem: {text}",
        seq = c.seq,
        author = c.author.clone().unwrap_or_default(),
        situation = status_text(&c.status, c.applied_at.is_some()),
    );
    let mut opts = analyzer_opts(&s.run_dir, "fix-turn.json", s.card_key.is_some())?;
    opts.extend(["--max-turns".into(), "40".into(), "--max-budget-usd".into(), "1.5".into()]);
    let spec = Spec {
        session: Session::Resume(session_id.to_owned()),
        name: format!("fix-c{}", c.seq),
        opts,
        prompt,
        cwd: wt,
        log_dir: s.run_dir.join("logs"),
        env_extra: vec![],
        timeout: TURN_TIMEOUT,
    };
    let out = claude::run(spec, ctl, |l| emit(host, &c.fix_id, t, Some(&l), true)).await?;
    let cost = cost_of(&out);
    add_cost(host, &c.fix_id, cost);
    if let Some(e) = limit_error(host, &out) {
        return Err(e);
    }
    match (out.status, out.result.as_ref().and_then(|r| r.get("structured_output"))) {
        (Status::Completed, Some(o)) if o.is_object() => Ok(Turn {
            reply: o["reply"].as_str().unwrap_or("").trim().to_owned(),
            verdict: o["verdict"].as_str().unwrap_or("").to_owned(),
            explanation: o["explanation"].as_str().unwrap_or("").trim().to_owned(),
            edits: serde_json::from_value(o["edits"].clone()).unwrap_or_default(),
            cost,
        }),
        _ => Err(failure(&out, "a resposta", "15 min")),
    }
}

// ---------------- aprovação ----------------

/// Grava a sugestão do item no worktree da correção.
pub fn approve(host: &Host, comment_id: i64) -> AppResult<Vec<String>> {
    let (c, s, t) = with_db(host, |conn| {
        let c = load_comment(conn, comment_id)?;
        let s = load_session(conn, &c.fix_id)?;
        let t = target_of(conn, &s)?;
        Ok((c, s, t))
    })?;
    if busy(host, &c.fix_id) {
        return Err(AppError::Invalid("espere o Claude terminar de responder antes de aprovar".into()));
    }
    if s.status != "ready" {
        return Err(AppError::Invalid("esta correção não está pronta para receber alterações".into()));
    }
    if c.status != "procede" {
        return Err(AppError::Invalid("este item não tem sugestão esperando aprovação".into()));
    }
    let touched = apply::apply(&s.wt(), &c.edits).map_err(AppError::Invalid)?;
    with_db(host, |conn| {
        Ok(conn.execute(
            "UPDATE fix_comments SET status = 'applied', error = NULL,
               applied_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
            [comment_id],
        )?)
    })?;
    emit(host, &c.fix_id, &t, None, false);
    Ok(touched)
}

// ---------------- commit, push e avanço da branch local ----------------

/// Todos resolvidos → committer (Haiku). No PR, o app faz o push em seguida; na branch local,
/// avança a branch (o push é `push_branch`). Em segundo plano.
pub fn commit(host: Host, fix_id: String) -> AppResult<Running> {
    let state = host.state();
    let (s, t, repo, rows) = {
        let conn = state.db.lock().map_err(|_| db_err())?;
        let s = load_session(&conn, &fix_id)?;
        let t = target_of(&conn, &s)?;
        let repo = repo_of_target(&conn, &t)?;
        let mut stmt = conn.prepare(
            "SELECT seq, ifnull(author, ''), ifnull(path, ''), line, body, ifnull(explanation, ''), status, applied_at IS NOT NULL
             FROM fix_comments WHERE fix_id = ?1 ORDER BY seq",
        )?;
        let rows: Vec<(i64, String, String, Option<i64>, String, String, String, bool)> = stmt
            .query_map([&fix_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
            .collect::<Result<_, _>>()?;
        (s, t, repo, rows)
    };
    if s.status != "ready" {
        return Err(AppError::Invalid("esta correção não está pronta para commit".into()));
    }
    let open: Vec<String> = rows
        .iter()
        .filter(|r| !matches!(r.6.as_str(), "applied" | "nao_procede" | "resposta" | "new"))
        .map(|r| format!("C{}", r.0))
        .collect();
    if !open.is_empty() {
        return Err(AppError::Invalid(format!("ainda há item(ns) sem resolver: {}", open.join(", "))));
    }
    if !rows.iter().any(|r| r.7) {
        return Err(AppError::Invalid("nenhuma sugestão foi aprovada: não há o que commitar".into()));
    }
    if git::is_protected(&t.branch) {
        return Err(AppError::Invalid(format!("recusado: a branch {} é protegida", t.branch)));
    }
    claim(&host, &fix_id)?;
    let ctl = Arc::new(RunCtl::default());
    {
        let mut reg = state.reviews.lock().unwrap();
        if reg.contains_key(&key(&fix_id)) {
            return Err(AppError::Invalid("o Claude ainda está rodando nesta correção".into()));
        }
        reg.insert(key(&fix_id), ctl.clone());
    }
    {
        let conn = state.db.lock().map_err(|_| db_err())?;
        conn.execute("UPDATE fix_sessions SET status = 'committing', error = NULL, stage = 'commit' WHERE id = ?1", [&fix_id])?;
    }
    emit(&host, &fix_id, &t, None, true);

    let summary: Vec<String> = rows
        .iter()
        .filter(|r| r.7)
        .map(|r| {
            let place = if r.2.is_empty() { String::new() } else { format!(", {}{}", r.2, r.3.map(|l| format!(":{l}")).unwrap_or_default()) };
            let body: String = r.4.chars().take(240).collect();
            format!("- C{} ({}{place}): \"{}\" → {}", r.0, r.1, body.replace('\n', " "), r.5.replace('\n', " "))
        })
        .collect();
    let task = tauri::async_runtime::spawn(async move {
        let result = commit_and_finish(&host, &fix_id, &s, &t, &repo, &summary, ctl.clone()).await;
        let done = if t.kind == "pr" { "pushed" } else { "committed" };
        let _ = with_db(&host, |c| {
            match &result {
                Ok((sha, subject)) => c.execute(
                    "UPDATE fix_sessions SET status = ?1, stage = ?2, commit_sha = ?3, commit_subject = ?4,
                       pushed_at = CASE WHEN ?1 = 'pushed' THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') END WHERE id = ?5",
                    params![done, if done == "pushed" { "push concluído" } else { "commit na branch local" }, sha, subject, fix_id],
                )?,
                // volta para pronto: o usuário pode tentar de novo (o commit já feito é reaproveitado)
                Err(e) => c.execute(
                    "UPDATE fix_sessions SET status = 'ready', stage = 'pronto', error = ?1 WHERE id = ?2",
                    params![if ctl.is_cancelled() { "cancelado".to_owned() } else { e.to_string() }, fix_id],
                )?,
            };
            Ok(())
        });
        if result.is_ok() {
            remove_worktree(&repo, &s.wt()).await;
        }
        host.state().reviews.lock().unwrap().remove(&key(&fix_id));
        emit(&host, &fix_id, &t, None, false);
    });
    Ok(task)
}

async fn commit_and_finish(
    host: &Host,
    fix_id: &str,
    s: &SessionRow,
    t: &Target,
    repo: &Path,
    summary: &[String],
    ctl: Arc<RunCtl>,
) -> AppResult<(String, String)> {
    let log = |l: &str| emit(host, fix_id, t, Some(l), true);
    let wt = s.wt();
    let base = s.base_sha.clone().unwrap_or_default();
    let dirty = git::run(&wt, &["status", "--porcelain"]).await?;
    let head = git::run(&wt, &["rev-parse", "HEAD"]).await?;
    if !dirty.is_empty() {
        set_stage(host, fix_id, t, "committer (Haiku) criando o commit");
        run_committer(host, fix_id, s, t, summary, ctl.clone(), &log).await?;
        let new_head = git::run(&wt, &["rev-parse", "HEAD"]).await?;
        if new_head == head {
            return Err(AppError::Invalid("o committer não criou o commit; veja o log".into()));
        }
        let left = git::run(&wt, &["status", "--porcelain"]).await?;
        if !left.is_empty() {
            return Err(AppError::Invalid(format!(
                "o commit foi criado, mas ficaram arquivos de fora; nada foi enviado. Arquivos:\n{left}"
            )));
        }
    } else if head == base {
        return Err(AppError::Invalid("não há alteração no worktree para commitar".into()));
    }
    let sha = git::run(&wt, &["rev-parse", "HEAD"]).await?;
    let subject = git::run(&wt, &["log", "-1", "--format=%s"]).await?;
    let short = &sha[..sha.len().min(10)];
    check(&ctl)?;
    if t.kind == "pr" {
        set_stage(host, fix_id, t, &format!("push para {}", t.branch));
        let rr = t.repo.as_ref().ok_or_else(|| AppError::Invalid("correção de PR sem repositório".into()))?;
        let provider = crate::hosting::Provider::connect(rr.kind, &host.state().db, host.state().secrets.as_ref()).await?;
        let pushed = provider.push(&wt, &t.source_repo, &t.branch).await;
        pushed.map_err(|e| {
            AppError::Invalid(format!(
                "o push foi recusado; o commit {short} continua no worktree e o botão tenta de novo. \
                 Se a branch recebeu commits no remoto, comece uma análise nova. {e}"
            ))
        })?;
        log(&format!("push concluído: {short} {subject}"));
    } else {
        set_stage(host, fix_id, t, &format!("avançando a branch local {}", t.branch));
        advance_local(repo, &t.branch, &base, &sha).await.map_err(|e| {
            AppError::Invalid(format!("o commit {short} continua no worktree e o botão tenta de novo. {e}"))
        })?;
        log(&format!("branch {} agora em {short} {subject}", t.branch));
    }
    Ok((sha, subject))
}

async fn run_committer(
    host: &Host,
    fix_id: &str,
    s: &SessionRow,
    t: &Target,
    summary: &[String],
    ctl: Arc<RunCtl>,
    log: &(dyn Fn(&str) + Send + Sync),
) -> AppResult<()> {
    let wt = s.wt();
    let (dir, _) = review::claude_config_paths();
    let (agent, _) = review::user_file("agents/committer.md")?;
    let settings = json!({"enabledPlugins": review::plugins_off(&review::read_json(&dir.join("settings.json")))}).to_string();
    let opts: Vec<String> = vec![
        "--agent".into(), "committer".into(), "--agents".into(), json!({"committer": review::agent_entry(&agent)}).to_string(),
        "--model".into(), "haiku".into(),
        "--allowedTools".into(), COMMITTER_ALLOW.into(),
        "--disallowedTools".into(), COMMITTER_DENY.into(),
        "--max-turns".into(), "20".into(), "--max-budget-usd".into(), "0.5".into(),
        "--settings".into(), settings, "--setting-sources".into(), "user".into(),
        "--strict-mcp-config".into(), "--mcp-config".into(), json!({"mcpServers": {}}).to_string(),
    ];
    let why = match t.kind {
        "pr" => format!("correções pedidas pelos revisores no PR {}#{} ({})", t.slug, t.bb_id, t.title),
        _ => format!("correções de achados do review automático da branch {} em {}", t.branch, t.slug),
    };
    let prompt = format!(
        "Repo: {wt}\n\
         O worktree está em HEAD destacado, então `git branch --show-current` sai vazio: a branch é `{branch}`. Use `{branch}` no assunto.\n\
         Não faça push: o app cuida disso.\n\
         Todos os arquivos alterados no worktree são desta mudança: inclua todos.\n\n\
         O que mudou e por quê: {why}.\n{list}",
        wt = wt.display(),
        branch = t.branch,
        list = summary.join("\n"),
    );
    let spec = Spec {
        session: Session::Ephemeral,
        name: "committer".into(),
        opts,
        prompt,
        cwd: wt.clone(),
        log_dir: s.run_dir.join("logs"),
        env_extra: vec![review::cache_5m()],
        timeout: COMMIT_TIMEOUT,
    };
    let out = claude::run(spec, ctl, |l| log(&l)).await?;
    add_cost(host, fix_id, cost_of(&out));
    if let Some(e) = limit_error(host, &out) {
        return Err(e);
    }
    if out.status == Status::Cancelled {
        return Err(AppError::Invalid("cancelado".into()));
    }
    let text = out.result.as_ref().and_then(|r| r.get("result")).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    for l in text.lines().take(6) {
        log(&format!("committer: {l}"));
    }
    if out.status != Status::Completed {
        log(&format!("committer terminou com {:?}: {}", out.status, out.stderr_tail));
    }
    Ok(())
}

/// Leva a branch local até `new` (commit feito no worktree da correção, filho de `old`), sem
/// tocar em trabalho do usuário:
/// - se a branch andou desde o início da correção, recusa;
/// - se ela está aberta em algum checkout, exige que ele esteja limpo e avança por fast-forward;
/// - senão, move só a referência, conferindo o valor antigo.
pub async fn advance_local(repo: &Path, branch: &str, old: &str, new: &str) -> AppResult<()> {
    let r = format!("refs/heads/{branch}");
    let tip = git::run(repo, &["rev-parse", &format!("{r}^{{commit}}")]).await?;
    if tip != old {
        return Err(AppError::Invalid(format!(
            "a branch {branch} recebeu commits depois do início da correção ({} → {}); descarte e corrija de novo",
            &old[..old.len().min(10)],
            &tip[..tip.len().min(10)]
        )));
    }
    let list = git::run(repo, &["worktree", "list", "--porcelain"]).await?;
    let checked_out = list.split("\n\n").find_map(|block| {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "))?;
        block.lines().any(|l| l == format!("branch {r}")).then(|| PathBuf::from(path))
    });
    match checked_out {
        Some(dir) => {
            let dirty = git::run(&dir, &["status", "--porcelain", "--untracked-files=no"]).await?;
            if !dirty.is_empty() {
                return Err(AppError::Invalid(format!(
                    "{branch} está aberta em {} com {} arquivo(s) alterado(s) sem commit; faça commit ou stash e tente de novo",
                    dir.display(),
                    dirty.lines().count()
                )));
            }
            git::run(&dir, &["merge", "-q", "--ff-only", new]).await?;
        }
        None => {
            git::run(repo, &["update-ref", &r, new, old]).await?;
        }
    }
    Ok(())
}

/// Branch local já avançada: envia para a origin (sem force; cria a branch remota se não houver).
pub async fn push_branch(host: Host, fix_id: String) -> AppResult<()> {
    let (s, t, repo) = with_db(&host, |c| {
        let s = load_session(c, &fix_id)?;
        let t = target_of(c, &s)?;
        let repo = repo_of_target(c, &t)?;
        Ok((s, t, repo))
    })?;
    if t.kind != "branch" || s.status != "committed" {
        return Err(AppError::Invalid("o push fica disponível depois do commit na branch local".into()));
    }
    if git::is_protected(&t.branch) {
        return Err(AppError::Invalid(format!("recusado: a branch {} é protegida", t.branch)));
    }
    emit(&host, &fix_id, &t, Some(&format!("== push para origin/{}", t.branch)), true);
    let r = format!("refs/heads/{}", t.branch);
    let result = git::run(&repo, &["push", "-u", "origin", &format!("{r}:{r}")]).await;
    match result {
        Ok(_) => {
            with_db(&host, |c| {
                Ok(c.execute(
                    "UPDATE fix_sessions SET status = 'pushed', stage = 'push concluído', error = NULL,
                       pushed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
                    [&fix_id],
                )?)
            })?;
            emit(&host, &fix_id, &t, None, false);
            Ok(())
        }
        Err(e) => {
            let msg = format!("o push foi recusado (a branch local continua com o commit): {e}");
            let _ = with_db(&host, |c| Ok(c.execute("UPDATE fix_sessions SET error = ?1 WHERE id = ?2", params![msg, fix_id])?));
            emit(&host, &fix_id, &t, None, false);
            Err(AppError::Invalid(msg))
        }
    }
}

/// Ao abrir o app: o que ficou rodando foi interrompido, menos o que um `comments` do terminal
/// ainda está processando.
pub fn mark_interrupted(conn: &Connection) -> AppResult<()> {
    mark_interrupted_with(conn, |dir: &Path| review::pid_alive(&dir.join(CLI_PID_FILE)))
}

/// `alive` recebe a `run_dir` da correção.
pub fn mark_interrupted_with(conn: &Connection, alive: impl Fn(&Path) -> bool) -> AppResult<()> {
    let open: Vec<(String, String)> = conn
        .prepare(
            "SELECT s.id, s.run_dir FROM fix_sessions s
             WHERE s.status IN ('analyzing', 'committing')
                OR EXISTS (SELECT 1 FROM fix_comments c WHERE c.fix_id = s.id AND c.status IN ('analyzing', 'queued'))",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, dir) in open {
        if !alive(Path::new(&dir)) {
            recover(conn, &id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Casa como as regras `Bash(...)` do Claude Code: `*` no fim é prefixo, sem ele é exato.
    fn matches(rules: &str, cmd: &str) -> bool {
        rules.split("Bash(").skip(1).filter_map(|r| r.split_once(')')).any(|(r, _)| match r.strip_suffix('*') {
            Some(prefix) => cmd.starts_with(prefix),
            None => cmd == r,
        })
    }

    /// Caso PROJ-1373 (2026-11-26): `Bash(git branch -*)` no deny barrava o `git branch --show-current`
    /// que o `committer.md` manda rodar, e o Haiku desistia sem commitar.
    #[test]
    fn committer_permissions() {
        // comandos que o committer.md manda rodar
        for cmd in [
            "git status --short",
            "git diff --stat",
            "git diff --cached --stat",
            "git branch --show-current",
            "git log --oneline -3",
            "git add -A",
            "git commit -F -",
        ] {
            assert!(matches(COMMITTER_ALLOW, cmd), "não liberado: {cmd}");
            assert!(!matches(COMMITTER_DENY, cmd), "negado: {cmd}");
        }
        for cmd in [
            "git branch -D x",
            "git branch -d x",
            "git branch --delete x",
            "git branch -m a b",
            "git branch -f x HEAD",
            "git branch --force x HEAD",
            "git push origin HEAD",
            "git commit --amend",
            "git checkout x",
            "git reset --hard",
            "git update-ref refs/heads/x HEAD",
        ] {
            assert!(matches(COMMITTER_DENY, cmd), "não negado: {cmd}");
        }
    }

    fn e(file: &str, existing: &str, improved: &str) -> Edit {
        Edit { file: file.into(), line: 1, end_line: 1, existing_code: existing.into(), improved_code: improved.into() }
    }

    #[test]
    fn veredito_vira_situacao_do_item() {
        let wt = std::env::temp_dir().join(format!("prr-judge-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("a.txt"), "x\n").unwrap();
        assert_eq!(judge(&wt, "procede", vec![e("a.txt", "x", "y")], false).0, "procede");
        // sugestão que não bate com o código
        let (st, _, err) = judge(&wt, "procede", vec![e("a.txt", "z", "y")], false);
        assert_eq!(st, "failed");
        assert!(err.unwrap().contains("não está mais"));
        assert_eq!(judge(&wt, "procede", vec![], false).0, "failed");
        // já aplicado e sem sugestão nova: continua resolvido
        assert_eq!(judge(&wt, "procede", vec![], true).0, "applied");
        assert_eq!(judge(&wt, "nao_procede", vec![], true).0, "applied");
        // em nao_procede/resposta a sugestão é ignorada
        let (st, edits, _) = judge(&wt, "nao_procede", vec![e("a.txt", "x", "y")], false);
        assert_eq!((st, edits.len()), ("nao_procede", 0));
        assert_eq!(judge(&wt, "resposta", vec![], false).0, "resposta");
        std::fs::remove_dir_all(&wt).unwrap();
    }

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO repositories (id, workspace, slug) VALUES (1, 'w', 'r');
             INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, reviewers_json, participants_json,
               source_branch, dest_branch, source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 10, 't', 'u', '{a}', '[]', '[]', 'PROJ-1', 'dev', 's', 'd', 'OPEN', 'x', 'x', 'authored');",
        )
        .unwrap();
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("prr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn owner_e_recuperacao() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn);
        let data_dir = tmp_dir("owner");
        let (dir_a, dir_b) = (data_dir.join("a"), data_dir.join("b"));
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join(CLI_PID_FILE), "999999999").unwrap(); // terminal morto com SIGKILL
        std::fs::write(dir_b.join(CLI_PID_FILE), "1").unwrap();
        conn.execute(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('a', 'pr', 1, ?1, 'analyzing')",
            [dir_a.display().to_string()],
        )
        .unwrap();
        // 'b': conversa em andamento (a sessão fica 'ready' e só o item vai para 'analyzing')
        conn.execute(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status, created_at) VALUES ('b', 'pr', 1, ?1, 'ready', '2030-01-01')",
            [dir_b.display().to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('a', 1, 'oi', 'analyzing');
             INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('b', 1, 'oi', 'analyzing');",
        )
        .unwrap();

        assert!(matches!(owner(&conn, &data_dir, "a").unwrap(), Owner::Stale));
        recover(&conn, "a").unwrap();
        assert!(!dir_a.join(CLI_PID_FILE).exists());
        assert!(matches!(owner(&conn, &data_dir, "a").unwrap(), Owner::Free));

        let dead = |_: &Path| false;
        assert!(matches!(owner_with(&conn, &data_dir, "b", dead).unwrap(), Owner::Stale));
        assert!(matches!(owner_with(&conn, &data_dir, "b", |p: &Path| p.ends_with(CLI_PID_FILE)).unwrap(), Owner::Terminal));
        // trabalho do app: ao assumir, o `claim` do app apagou o cli.pid
        std::fs::remove_file(dir_b.join(CLI_PID_FILE)).unwrap();
        assert!(matches!(owner_with(&conn, &data_dir, "b", |p: &Path| p.ends_with(crate::APP_PID_FILE)).unwrap(), Owner::App));
        // terminal morto com SIGKILL e app aberto: o cli.pid morto diz que a etapa era do terminal
        std::fs::write(dir_a.join(CLI_PID_FILE), "999999999").unwrap();
        conn.execute("UPDATE fix_sessions SET status = 'analyzing' WHERE id = 'a'", []).unwrap();
        let app_only = |p: &Path| p.ends_with(crate::APP_PID_FILE);
        assert!(matches!(owner_with(&conn, &data_dir, "a", app_only).unwrap(), Owner::Stale));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn app_nao_mexe_na_correcao_do_terminal() {
        let dir = tmp_dir("claim");
        // sem cli.pid: livre
        assert!(claim_with(&dir, |_: &Path| true).is_ok());
        // cli.pid de terminal vivo: recusa
        std::fs::write(dir.join(CLI_PID_FILE), "1").unwrap();
        assert_eq!(
            claim_with(&dir, |_: &Path| true).unwrap_err().to_string(),
            "o terminal está processando esta correção; espere terminar"
        );
        assert!(dir.join(CLI_PID_FILE).exists());
        // cli.pid de terminal morto: o app assume e apaga o arquivo velho
        assert!(claim_with(&dir, |_: &Path| false).is_ok());
        assert!(!dir.join(CLI_PID_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconhece_o_app_pela_linha_de_comando() {
        assert!(is_app_cmdline(&["/usr/bin/pr-review-orchestrator"]));
        assert!(!is_app_cmdline(&["pr-review-orchestrator", "comments", "PROJ-1"]));
        assert!(!is_app_cmdline(&["pr-review-orchestrator", "-p", "PROJ-1"]));
        assert!(!is_app_cmdline(&["pr-review-orchestrator", "--sync"]));
        assert!(!is_app_cmdline(&["pr-review-orchestrator", "--help"]));
    }

    #[test]
    fn mark_interrupted_pula_correcao_do_terminal() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn);
        conn.execute_batch(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('viva', 'pr', 1, '/x/viva', 'analyzing');
             INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status, created_at) VALUES ('morta', 'pr', 1, '/x/morta', 'analyzing', '2030-01-01');
             INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('viva', 1, 'oi', 'analyzing');",
        )
        .unwrap();
        mark_interrupted_with(&conn, |dir: &Path| dir.ends_with("viva")).unwrap();
        let st = |id: &str| conn.query_row("SELECT status FROM fix_sessions WHERE id = ?1", [id], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!((st("viva"), st("morta")), ("analyzing".to_owned(), "failed".to_owned()));
        assert_eq!(view(&conn, "viva", false).unwrap().comments[0].status, "analyzing");
    }

    #[test]
    fn interrompido_vira_falha_e_commit_volta_para_pronto() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn);
        conn.execute_batch(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('a', 'pr', 1, '/x', 'analyzing');
             INSERT INTO fix_sessions (id, kind, repo_slug, branch, run_dir, status, created_at)
               VALUES ('b', 'branch', 'r', 'PROJ-2', '/y', 'committing', '2030-01-01');
             INSERT INTO fix_comments (fix_id, seq, finding_run_id, finding_seq, body, status) VALUES ('b', 1, 'run', 3, 'oi', 'queued');
             INSERT INTO fix_comments (fix_id, seq, finding_run_id, finding_seq, body, status, applied_at)
               VALUES ('b', 2, 'run', 4, 'já aplicado', 'analyzing', '2030-01-01');",
        )
        .unwrap();
        mark_interrupted(&conn).unwrap();
        let st = |id: &str| conn.query_row("SELECT status FROM fix_sessions WHERE id = ?1", [id], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!((st("a"), st("b")), ("failed".to_owned(), "ready".to_owned()));
        let v = view(&conn, "b", false).unwrap();
        assert_eq!((v.comments[0].status.as_str(), v.comments[0].finding_seq), ("failed", Some(3)));
        // a conversa sobre um item já aplicado foi interrompida: ele continua aplicado (não trava o commit)
        assert_eq!((v.comments[1].status.as_str(), v.comments[1].error.clone()), ("applied", None));
        let sums = summaries(&conn).unwrap();
        assert!(sums.contains_key("pr:1"));
        assert_eq!(sums["branch:r:PROJ-2"].findings[0], ("run".to_owned(), 3, "failed".to_owned()));
        // sessão de branch sem slug é recusada pelo banco
        assert!(conn.execute("INSERT INTO fix_sessions (id, kind, run_dir, status) VALUES ('c', 'branch', '/z', 'ready')", []).is_err());
    }

    #[tokio::test]
    async fn avanca_branch_local_sem_tocar_trabalho_do_usuario() {
        let root = std::env::temp_dir().join(format!("prr-adv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = |dir: &Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=dev"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_owned()
        };
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q"]);
        std::fs::write(repo.join("f.txt"), "1\n").unwrap();
        g(&repo, &["add", "f.txt"]);
        g(&repo, &["commit", "-q", "-m", "base"]);
        g(&repo, &["branch", "PROJ-1"]);
        g(&repo, &["branch", "PROJ-2"]);
        let old = g(&repo, &["rev-parse", "HEAD"]);
        // commit feito num worktree destacado, como o da correção
        let wt = root.join("wt");
        g(&repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), &old]);
        std::fs::write(wt.join("f.txt"), "2\n").unwrap();
        g(&wt, &["commit", "-q", "-am", "fix"]);
        let new = g(&wt, &["rev-parse", "HEAD"]);

        // branch fora de qualquer checkout: só a referência anda
        advance_local(&repo, "PROJ-1", &old, &new).await.unwrap();
        assert_eq!(g(&repo, &["rev-parse", "PROJ-1"]), new);

        // branch aberta no checkout com alteração sem commit: recusa e não mexe
        g(&repo, &["checkout", "-q", "PROJ-2"]);
        std::fs::write(repo.join("f.txt"), "meu trabalho\n").unwrap();
        let err = advance_local(&repo, "PROJ-2", &old, &new).await.unwrap_err().to_string();
        assert!(err.contains("sem commit"), "{err}");
        assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "meu trabalho\n");
        // checkout limpo: fast-forward, e o arquivo do checkout acompanha
        g(&repo, &["checkout", "-q", "--", "f.txt"]);
        advance_local(&repo, "PROJ-2", &old, &new).await.unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "2\n");

        // a branch andou depois do início: recusa
        let err = advance_local(&repo, "PROJ-1", &old, &new).await.unwrap_err().to_string();
        assert!(err.contains("recebeu commits"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
