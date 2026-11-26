//! Engine de review de um PR (Fase 0, E4–E6, portado para o app):
//! worktree isolado no commit exato → skill `/review-branch --orquestrado` (Sonnet) →
//! validação determinística dos achados → Opus sobre todos os bugs válidos (se houver algum).

mod browser;
pub(crate) mod conventions;
pub mod chat;
pub mod claude;
pub mod git;
pub(crate) mod related;
pub mod usage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::AppState;
use crate::db;
use crate::error::{AppError, AppResult};
use claude::{RunCtl, Spec, Status};

static ASSETS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Usa `dir` (pasta com `agents/`, `schemas/` e `hooks/`) se ela existir: é o caso do app
/// instalado, em que `claude/` vai nos recursos do pacote.
pub fn set_assets_root(dir: PathBuf) {
    if dir.join("agents").is_dir() && dir.join("hooks").is_dir() {
        let _ = ASSETS.set(dir);
    }
}

/// Pasta `claude/` (agentes, schemas e hooks): a do pacote instalado ou, em dev, a do checkout.
pub(crate) fn assets() -> PathBuf {
    ASSETS
        .get()
        .cloned()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("claude"))
}

pub(crate) const READ_ONLY_TOOLS: &str = "Read Grep Glob Agent Bash(git diff *) Bash(git log *) Bash(git show *) Bash(git blame *) \
Bash(git merge-tree *) Bash(git rev-list *) Bash(git rev-parse *) Bash(git ls-files *) Bash(grep *) \
Bash(rg *) Bash(ls *) Bash(cat *) Bash(head *) Bash(tail *) Bash(wc *) Bash(find *) Bash(sed -n *)";
const REVIEW_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const AUDIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Folga, em linhas, para uma melhoria apontar o início do bloco (assinatura do método, por
/// exemplo) logo antes do trecho alterado pelo PR.
const HUNK_SLACK: u64 = 3;

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Limite de uso do Claude em vigor e quando tentar de novo.
#[derive(Debug, Clone, Serialize)]
pub struct LimitState {
    #[serde(flatten)]
    pub limit: claude::Limit,
    pub since: i64,
    /// Próxima tentativa (epoch): 1 min depois da liberação informada, ou a cada 15 min.
    pub retry_at: i64,
}

/// Registra o limite e avisa a UI.
pub fn set_limit(app: &AppHandle, limit: claude::Limit) {
    Host::App(app.clone()).set_limit(limit)
}

/// Limite ainda em vigor? Passado o `retry_at`, libera (a próxima execução confirma).
pub fn active_limit(app: &AppHandle) -> Option<LimitState> {
    Host::App(app.clone()).active_limit()
}

pub fn clear_limit(app: &AppHandle) {
    *app.state::<AppState>().claude_limit.lock().unwrap() = None;
    let _ = app.emit("claude-limit", None::<LimitState>);
}

/// Quem roda o review: a janela do app (eventos para a UI) ou o terminal
/// (`pr-review-orchestrator -p <branch>`), que escreve o andamento no stderr.
#[derive(Clone)]
pub enum Host {
    App(AppHandle),
    Cli(Arc<CliHost>),
}

/// Estado do modo terminal: o mesmo `AppState` do app, sem janela.
pub struct CliHost {
    pub state: AppState,
    /// Também escreve o log detalhado (eventos dos subagentes) no stderr.
    pub verbose: bool,
    /// Rótulo de cada execução no stderr (`acme-back#2433`), por `run_id`.
    pub labels: Mutex<HashMap<String, String>>,
}

impl CliHost {
    pub(crate) fn print(&self, run_id: &str, line: &str) {
        let label = self.labels.lock().unwrap().get(run_id).cloned().unwrap_or_else(|| run_id.to_owned());
        eprintln!("[{label}] {line}");
    }
}

impl Host {
    pub fn state(&self) -> &AppState {
        match self {
            Host::App(app) => app.state::<AppState>().inner(),
            Host::Cli(cli) => &cli.state,
        }
    }

    pub(crate) fn is_cli(&self) -> bool {
        matches!(self, Host::Cli(_))
    }

    fn emit_update(&self, u: &Update) {
        match self {
            Host::App(app) => {
                let _ = app.emit("review-update", u.clone());
            }
            Host::Cli(cli) => cli.print(u.run_id, &format!("== {}", u.stage)),
        }
    }

    fn emit_log(&self, run_id: &str, line: &str) {
        match self {
            Host::App(app) => {
                let _ = app.emit("review-log", LogLine { run_id, line });
            }
            // a etapa já sai pelo emit_update; o resto só com --verbose
            Host::Cli(cli) if cli.verbose && !line.starts_with("== ") => cli.print(run_id, line),
            Host::Cli(_) => {}
        }
    }

    fn emit_limit(&self, limit: Option<&LimitState>) {
        match self {
            Host::App(app) => {
                let _ = app.emit("claude-limit", limit.cloned());
            }
            Host::Cli(_) => {
                if let Some(l) = limit {
                    eprintln!("limite de uso do Claude: {} (nova tentativa em {} s)", l.limit.message, (l.retry_at - now_secs()).max(0));
                }
            }
        }
    }

    pub(crate) fn set_limit(&self, limit: claude::Limit) {
        let now = now_secs();
        let retry_at = limit.reset_at.map(|r| r.max(now) + 60).unwrap_or(now + 15 * 60);
        let st = LimitState { limit, since: now, retry_at };
        *self.state().claude_limit.lock().unwrap() = Some(st.clone());
        self.emit_limit(Some(&st));
    }

    pub(crate) fn active_limit(&self) -> Option<LimitState> {
        let mut guard = self.state().claude_limit.lock().unwrap();
        if guard.as_ref().is_some_and(|l| now_secs() >= l.retry_at) {
            *guard = None;
            self.emit_limit(None);
        }
        guard.clone()
    }
}

/// Execuções em andamento (para cancelar).
pub type Registry = Mutex<HashMap<String, Arc<RunCtl>>>;

#[derive(Debug, Clone)]
pub struct PrForReview {
    /// Repositório do PR no provedor; `None` na branch local.
    pub repo: Option<crate::hosting::RepoRef>,
    pub slug: String,
    pub bb_id: i64,
    pub title: String,
    pub description: String,
    pub author: String,
    pub url: String,
    pub source_branch: String,
    pub dest_branch: String,
    pub source_sha: String,
    pub dest_sha: String,
    /// Branch local (tela "Minhas branches"): sem PR; a ponta vem de `refs/heads/<branch>` e a
    /// base, de `origin/<dest_branch>`.
    pub local: bool,
}

fn load_pr(conn: &Connection, pr_id: i64) -> AppResult<PrForReview> {
    conn.query_row(
        "SELECT r.slug, p.bb_id, p.title, ifnull(p.description, ''), ifnull(p.author_name, ''),
                p.url, p.source_branch, p.dest_branch, p.source_sha, p.dest_sha, r.provider, r.workspace
         FROM pull_requests p JOIN repositories r ON r.id = p.repository_id WHERE p.id = ?1",
        [pr_id],
        |r| {
            Ok(PrForReview {
                repo: Some(crate::hosting::RepoRef::from_row(&r.get::<_, String>(10)?, r.get(11)?, r.get(0)?)?),
                slug: r.get(0)?,
                bb_id: r.get(1)?,
                title: r.get(2)?,
                description: r.get(3)?,
                author: r.get(4)?,
                url: r.get(5)?,
                source_branch: r.get(6)?,
                dest_branch: r.get(7)?,
                source_sha: r.get(8)?,
                dest_sha: r.get(9)?,
                local: false,
            })
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("PR {pr_id} não encontrado; atualize a lista")))
}

/// Pasta onde ficam os clones locais (`<raiz>/<slug>`). Padrão: `~/Projects`.
pub fn repos_root(conn: &Connection) -> AppResult<PathBuf> {
    let configured = db::get_setting(conn, "repos.root")?.and_then(|v| v.as_str().map(str::trim).map(str::to_owned));
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    Ok(match configured.filter(|s| !s.is_empty()) {
        Some(p) if p == "~" => home,
        Some(p) => match p.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(p),
        },
        None => home.join("Projects"),
    })
}

/// Site do Jira configurado (`jira.site`, ex.: `empresa.atlassian.net`), sem protocolo nem `/` no
/// fim. Sem ele, o link do card some e os agentes acham o site pelo MCP.
pub fn jira_site(conn: &Connection) -> AppResult<Option<String>> {
    Ok(db::get_setting(conn, "jira.site")?
        .and_then(|v| v.as_str().map(normalize_jira_site))
        .filter(|s| !s.is_empty()))
}

fn normalize_jira_site(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    s.trim_end_matches('/').to_owned()
}

/// Como ler o card pelo MCP do Atlassian, para os prompts que recebem `CARD_KEY`.
pub fn card_read_hint(site: Option<&str>) -> String {
    let cloud = match site {
        Some(s) => format!("cloudId \"{s}\""),
        None => "cloudId: o site de mcp__atlassian__getAccessibleAtlassianResources".into(),
    };
    format!("leia com mcp__atlassian__getJiraIssue, carregando via ToolSearch; {cloud}, responseContentFormat \"markdown\"")
}

/// Chave do card: primeiro `[A-Z]+-\d+` no nome da branch, senão no título (`PROJ-1676-ajuste` → `PROJ-1676`).
pub fn card_key(branch: &str, title: &str) -> Option<String> {
    [branch, title].iter().find_map(|text| {
        let b = text.as_bytes();
        (0..b.len()).find_map(|i| {
            if i > 0 && b[i - 1].is_ascii_alphanumeric() {
                return None;
            }
            let letters = b[i..].iter().take_while(|c| c.is_ascii_uppercase()).count();
            if letters == 0 || b.get(i + letters) != Some(&b'-') {
                return None;
            }
            let digits = b[i + letters + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            (digits > 0).then(|| text[i..i + letters + 1 + digits].to_owned())
        })
    })
}

/// O Jira só participa quando foi configurado explicitamente.
pub fn configured_jira_key(conn: &Connection, branch: &str, title: &str) -> AppResult<Option<String>> {
    Ok(jira_site(conn)?.and_then(|_| card_key(branch, title)))
}

fn new_run_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(secs);
    format!("r{secs}-{:06x}", h.finish() & 0xffffff)
}

#[derive(Clone, Serialize)]
struct Update<'a> {
    run_id: &'a str,
    pr_id: i64,
    status: &'a str,
    stage: &'a str,
}

#[derive(Clone, Serialize)]
struct LogLine<'a> {
    run_id: &'a str,
    line: &'a str,
}

struct Ctx {
    host: Host,
    run_id: String,
    pr_id: i64,
    run_dir: PathBuf,
    ctl: Arc<RunCtl>,
}

impl Ctx {
    fn state(&self) -> &AppState {
        self.host.state()
    }

    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
        let st = self.state();
        let conn = st.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        f(&conn)
    }

    fn stage(&self, stage: &str) {
        let _ = self.with_db(|c| {
            c.execute("UPDATE review_runs SET stage = ?1 WHERE id = ?2", params![stage, self.run_id])?;
            usage::step_start(c, &self.run_id, stage)
        });
        self.host.emit_update(&Update { run_id: &self.run_id, pr_id: self.pr_id, status: "running", stage });
        self.log(&format!("== {stage}"));
    }

    fn log(&self, line: &str) {
        self.host.emit_log(&self.run_id, line);
        let path = self.run_dir.join("run.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    }

    fn check_cancel(&self) -> AppResult<()> {
        if self.ctl.is_cancelled() { Err(AppError::Invalid("cancelado".into())) } else { Ok(()) }
    }

    /// Espera o limite de uso liberar (ou o cancelamento), mantendo a etapa visível.
    async fn wait_limit(&self, resume_stage: &str) -> AppResult<()> {
        let mut paused = false;
        while self.host.active_limit().is_some() {
            self.check_cancel()?;
            if !paused {
                self.stage("pausado: limite de uso do Claude atingido (retoma sozinho quando liberar)");
                paused = true;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        if paused {
            self.stage(resume_stage);
        }
        Ok(())
    }

    /// Grava a chamada (agente, modelo, custo, duração) na etapa atual: log, pipeline e aba Uso.
    fn record_call(&self, name: &str, out: &claude::Outcome, took: Duration) {
        let r = out.result.as_ref();
        let call = usage::NewCall {
            name: name.to_owned(),
            model: usage::model_of(name, r),
            status: match out.status {
                claude::Status::Completed => "completed",
                claude::Status::Failed => "failed",
                claude::Status::Cancelled => "cancelled",
                claude::Status::TimedOut => "timed_out",
            }
            .to_owned(),
            cost_usd: r.and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0),
            duration_ms: Some(took.as_millis() as i64),
            turns: r.and_then(|r| r.get("num_turns")).and_then(Value::as_i64),
        };
        let _ = self.with_db(|c| usage::record_call(c, &self.run_id, &call));
    }

    /// `claude::run` que, ao bater no limite de uso, pausa e repete a mesma etapa ao liberar.
    async fn run_claude(&self, spec: Spec, stage: &str, on_line: impl Fn(String) + Send + Sync) -> AppResult<claude::Outcome> {
        for attempt in 1..=4 {
            self.wait_limit(stage).await?;
            let started = std::time::Instant::now();
            let out = claude::run(spec.clone(), self.ctl.clone(), &on_line).await?;
            self.record_call(&spec.name, &out, started.elapsed());
            match &out.limit {
                Some(l) => {
                    self.log(&format!("limite de uso do Claude (tentativa {attempt}/4): {}", l.message));
                    self.host.set_limit(l.clone());
                }
                None => return Ok(out),
            }
        }
        Err(AppError::Invalid("o limite de uso do Claude continuou depois de 4 tentativas; rode o review de novo mais tarde".into()))
    }
}

/// Valida e registra a execução; o trabalho continua em segundo plano. Devolve o id.
pub fn start(host: Host, pr_id: i64) -> AppResult<String> {
    Ok(start_run(host, pr_id)?.0)
}

/// Como `start`, devolvendo também a tarefa (o modo terminal espera o fim).
pub fn start_run(host: Host, pr_id: i64) -> AppResult<(String, Running)> {
    let state = host.state();
    let (pr, root) = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let running: Option<String> = conn
            .query_row("SELECT id FROM review_runs WHERE pr_id = ?1 AND status = 'running'", [pr_id], |r| r.get(0))
            .optional()?;
        if let Some(id) = running {
            return Err(AppError::Invalid(format!("já existe um review em andamento para este PR ({id})")));
        }
        (load_pr(&conn, pr_id)?, repos_root(&conn)?)
    };
    launch(host.clone(), Some(pr_id), pr, &root)
}

/// Review de uma branch local (tela "Minhas branches"), comparada com a base da origin
/// (`origin/dev`, ou a branch padrão da origin). Só o que está commitado na branch.
pub async fn start_branch(host: Host, slug: String, branch: String) -> AppResult<String> {
    Ok(start_branch_run(host, slug, branch).await?.0)
}

/// Como `start_branch`, devolvendo também a tarefa (o modo terminal espera o fim).
pub async fn start_branch_run(host: Host, slug: String, branch: String) -> AppResult<(String, Running)> {
    let state = host.state();
    let root = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let running: Option<String> = conn
            .query_row(
                "SELECT id FROM review_runs WHERE repo_slug = ?1 AND branch = ?2 AND status = 'running'",
                params![slug, branch],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = running {
            return Err(AppError::Invalid(format!("já existe um review em andamento para esta branch ({id})")));
        }
        repos_root(&conn)?
    };
    let repo = crate::branches::repo_dir(&root, &slug)?;
    let tip = git::run(&repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}^{{commit}}")])
        .await
        .map_err(|_| AppError::Invalid(format!("a branch {branch} não existe em {slug}")))?;
    let base = git::default_base(&repo)
        .await
        .ok_or_else(|| AppError::Invalid(format!("{slug} não tem origin/dev nem branch padrão na origin para comparar")))?;
    if base == branch {
        return Err(AppError::Invalid(format!("{branch} é a própria base do review")));
    }
    let pr = PrForReview {
        repo: None,
        slug,
        bb_id: 0,
        title: branch.clone(),
        description: String::new(),
        author: String::new(),
        url: String::new(),
        source_branch: branch,
        dest_branch: base,
        source_sha: tip,
        dest_sha: String::new(),
        local: true,
    };
    launch(host, None, pr, &root)
}

/// Tarefa de um review em andamento: termina quando a execução é gravada como concluída,
/// falha ou cancelada.
pub type Running = tauri::async_runtime::JoinHandle<()>;

fn launch(host: Host, pr_id: Option<i64>, pr: PrForReview, root: &Path) -> AppResult<(String, Running)> {
    let state = host.state();
    if git::is_protected(&pr.source_branch) {
        return Err(AppError::Invalid(format!("recusado: a branch de origem {} é protegida", pr.source_branch)));
    }
    let repo = match &pr.repo { Some(rr) => rr.clone_dir(root)?, None => root.join(&pr.slug) };
    if !repo.join(".git").exists() {
        return Err(AppError::Invalid(format!(
            "não achei o clone local de {} em {} (ajuste a pasta dos repositórios nas Configurações)",
            pr.slug,
            repo.display()
        )));
    }

    let run_id = new_run_id();
    let run_dir = state.db_path.parent().unwrap().join("runs").join(&run_id);
    std::fs::create_dir_all(&run_dir)?;
    let key = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        configured_jira_key(&conn, &pr.source_branch, &pr.title)?
    };
    {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let (slug, branch, base) = if pr.local {
            (Some(&pr.slug), Some(&pr.source_branch), Some(&pr.dest_branch))
        } else {
            (None, None, None)
        };
        conn.execute(
            "INSERT INTO review_runs (id, pr_id, repo_slug, branch, base_branch, source_sha, dest_sha, card_key, status, stage, run_dir)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running', 'preparando', ?9)",
            params![run_id, pr_id, slug, branch, base, pr.source_sha, pr.dest_sha, key, run_dir.display().to_string()],
        )?;
    }
    let ctl = Arc::new(RunCtl::default());
    state.reviews.lock().unwrap().insert(run_id.clone(), ctl.clone());
    if let Host::Cli(cli) = &host {
        let label = match &pr.repo { Some(rr) => rr.label(pr.bb_id), None => format!("{}:{}", pr.slug, pr.source_branch) };
        cli.labels.lock().unwrap().insert(run_id.clone(), label);
        // o app, ao abrir, não marca como interrompida a execução de um processo vivo
        std::fs::write(run_dir.join(CLI_PID_FILE), std::process::id().to_string())?;
    }

    // branch local: pr_id 0 nos eventos (a lista de branches recarrega por run_id)
    let ctx = Ctx { host: host.clone(), run_id: run_id.clone(), pr_id: pr_id.unwrap_or(0), run_dir, ctl };
    let slots = state.review_slots.clone();
    let task = tauri::async_runtime::spawn(async move {
        let wt = ctx.run_dir.join("wt");
        if slots.available_permits() == 0 {
            ctx.stage("na fila (máximo de 5 reviews ao mesmo tempo)");
        }
        let permit = slots.acquire_owned().await;
        let result = if ctx.ctl.is_cancelled() {
            Err(AppError::Invalid("cancelado".into()))
        } else {
            pipeline(&ctx, &pr, &repo, &wt, key).await
        };
        drop(permit);
        // limpeza do worktree sempre (o repo do usuário fica como estava)
        if wt.exists() {
            let _ = git::raw(&repo, &["worktree", "remove", "--force", &wt.display().to_string()]).await;
        }
        let _ = git::raw(&repo, &["worktree", "prune"]).await;

        let (status, error) = match result {
            Ok(()) => ("completed", None),
            Err(_) if ctx.ctl.is_cancelled() => ("cancelled", Some("cancelado pelo usuário".to_owned())),
            Err(e) => ("failed", Some(e.to_string())),
        };
        if let Some(e) = &error {
            ctx.log(&format!("== {status}: {e}"));
        }
        let stage = if status == "completed" { "concluído" } else { status };
        let _ = ctx.with_db(|c| {
            usage::steps_close(c, &ctx.run_id)?;
            Ok(c.execute(
                "UPDATE review_runs SET status = ?1, stage = ?2, error = ?3,
                   audit_status = CASE WHEN audit_status = 'running' THEN ?1 ELSE audit_status END,
                   finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?4",
                params![status, stage, error, ctx.run_id],
            )?)
        });
        ctx.host.emit_update(&Update { run_id: &ctx.run_id, pr_id: ctx.pr_id, status, stage });
        ctx.state().reviews.lock().unwrap().remove(&ctx.run_id);
        let _ = std::fs::remove_file(ctx.run_dir.join(CLI_PID_FILE));
    });
    Ok((run_id, task))
}

pub fn cancel(state: &AppState, run_id: &str) -> AppResult<()> {
    let ctl = state.reviews.lock().unwrap().get(run_id).cloned();
    match ctl {
        Some(c) => {
            c.cancel();
            Ok(())
        }
        None => Err(AppError::Invalid("essa execução não está em andamento".into())),
    }
}

/// Arquivo com o PID do `pr-review-orchestrator -p` dono da execução (na pasta dela).
const CLI_PID_FILE: &str = "cli.pid";

/// O processo do modo terminal que gravou `cli.pid` em `run_dir` ainda está vivo?
fn cli_alive(run_dir: &Path) -> bool {
    pid_alive(&run_dir.join(CLI_PID_FILE))
}

/// O processo deste programa (app ou terminal) cujo PID está em `pid_file` ainda está vivo?
pub(crate) fn pid_alive(pid_file: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(pid_file).ok().and_then(|s| s.trim().parse::<i32>().ok()) else {
        return false;
    };
    // sinal 0 só confere se o processo existe; o PID pode ter sido reaproveitado, então o
    // nome do executável também precisa bater
    let exists = pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
    let ours = std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim().starts_with("pr-review-orch"));
    exists && ours
}

/// Ao abrir o app: execuções que ficaram "running" foram interrompidas, menos as de um
/// `pr-review-orchestrator -p` que ainda está rodando no terminal.
pub fn mark_interrupted(conn: &Connection) -> AppResult<()> {
    let running: Vec<(String, String)> = conn
        .prepare("SELECT id, run_dir FROM review_runs WHERE status = 'running'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, dir) in running {
        if cli_alive(Path::new(&dir)) {
            continue;
        }
        conn.execute(
            "UPDATE review_runs SET status = 'failed', error = 'o app foi fechado durante a execução',
               finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1 AND status = 'running'",
            [&id],
        )?;
    }
    Ok(())
}

/// Busca no remoto e devolve a ponta atual (SHA completo) da branch de origem e da de destino.
pub async fn fresh_tips(repo: &Path, src_branch: &str, dest_branch: &str) -> AppResult<(String, String)> {
    let spec = |b: &str| format!("+refs/heads/{b}:refs/remotes/origin/{b}");
    git::run(repo, &["fetch", "-q", "origin", &spec(src_branch), &spec(dest_branch)])
        .await
        .map_err(|e| AppError::Invalid(format!("não consegui buscar as branches do PR ({src_branch} → {dest_branch}): {e}")))?;
    let tip = |b: &str| format!("refs/remotes/origin/{b}^{{commit}}");
    Ok((git::run(repo, &["rev-parse", &tip(src_branch)]).await?, git::run(repo, &["rev-parse", &tip(dest_branch)]).await?))
}

pub async fn github_tips(repo: &Path, number: i64, dest_branch: &str) -> AppResult<(String, String)> {
    let src_ref = format!("refs/remotes/origin/pr/{number}");
    let dst_ref = format!("refs/remotes/origin/{dest_branch}");
    let src_spec = format!("+refs/pull/{number}/head:{src_ref}");
    let dst_spec = format!("+refs/heads/{dest_branch}:{dst_ref}");
    git::run(repo, &["fetch", "-q", "origin", &src_spec, &dst_spec]).await?;
    Ok((git::run(repo, &["rev-parse", &src_ref]).await?, git::run(repo, &["rev-parse", &dst_ref]).await?))
}

/// `back` se for Laravel (composer.json/artisan na raiz), senão `front` (mesma regra da skill).
pub fn project_kind(dir: &Path) -> &'static str {
    if dir.join("composer.json").exists() || dir.join("artisan").exists() { "back" } else { "front" }
}

/// Aspas simples para o shell que roda o comando do hook.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Cache do prompt com validade de 5 min. O `claude -p` grava com validade de 1 h, que custa
/// US$ 4/M contra US$ 2,50/M e era 51% do custo do Sonnet (medido em 2026-11-19). Um review
/// dura poucos minutos. Fica fora do chat, onde a pergunta seguinte pode vir bem depois.
pub(crate) fn cache_5m() -> (String, String) {
    ("CLAUDE_CODE_PROMPT_CACHE_TTL".into(), "5m".into())
}

/// Pasta de configuração do Claude Code (`$CLAUDE_CONFIG_DIR` ou `~/.claude`) e o `.claude.json`.
pub(crate) fn claude_config_paths() -> (PathBuf, PathBuf) {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => (PathBuf::from(&d), PathBuf::from(&d).join(".claude.json")),
        None => {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            (home.join(".claude"), home.join(".claude.json"))
        }
    }
}

/// Arquivo da skill ou de um agente do usuário (`rel` relativo à pasta do Claude Code, ex.
/// `agents/committer.md`). O do `~/.claude` vale quando existe: é a fonte de verdade. Senão, a
/// cópia que vai no pacote (`claude/user/`, atualizada por `packaging/sync-user-claude.sh`),
/// para quem instala o app sem ter esses arquivos. O `bool` diz se é a cópia do pacote.
pub(crate) fn user_file(rel: &str) -> AppResult<(String, bool)> {
    let own = claude_config_paths().0.join(rel);
    if let Ok(t) = std::fs::read_to_string(&own) {
        return Ok((t, false));
    }
    let bundled = assets().join("user").join(rel);
    std::fs::read_to_string(&bundled)
        .map(|t| (t, true))
        .map_err(|_| AppError::Invalid(format!("não achei {} nem a cópia do app em {}", own.display(), bundled.display())))
}

/// Texto da skill sem o frontmatter, para ir no prompt quando ela não está no `~/.claude`: o
/// `claude -p` só acha skill pelo nome nas pastas do usuário ou do projeto (testado em
/// 2026-11-25: `--add-dir` não serve com `--setting-sources user`).
fn skill_body(text: &str) -> &str {
    match text.strip_prefix("---").and_then(|r| r.split_once("\n---")) {
        Some((_, body)) => body.trim(),
        None => text.trim(),
    }
}

pub(crate) fn read_json(path: &Path) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// Todos os plugins habilitados no `settings.json` do usuário, desligados. Nenhum worker usa
/// plugin (a skill e o `branch-reviewer` são do usuário, não de plugin).
pub(crate) fn plugins_off(user_settings: &Value) -> Value {
    let off: serde_json::Map<String, Value> = user_settings
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .map(|m| m.keys().map(|k| (k.clone(), Value::Bool(false))).collect())
        .unwrap_or_default();
    Value::Object(off)
}

/// O MCP do Atlassian do usuário, copiado do `.claude.json`. Só aceita `type` e `url`: um
/// header ou env poderia levar segredo para a linha de comando (visível no `ps`).
fn atlassian_mcp(claude_json: &Value) -> Option<Value> {
    let s = claude_json.get("mcpServers")?.get("atlassian")?.as_object()?;
    if s.keys().any(|k| k != "type" && k != "url") {
        return None;
    }
    Some(json!({"type": s.get("type")?.as_str()?, "url": s.get("url")?.as_str()?}))
}

/// Hooks de proteção (git sempre; navegador quando o worker usa o Chrome) e plugins desligados.
fn guard_settings_with(browser: bool, user_settings: &Value) -> String {
    // o caminho do app instalado tem espaços (/usr/lib/PR Review Orchestrator/...): aspas simples
    let cmd = |f: &str| format!("python3 -I {}", shell_quote(&assets().join("hooks").join(f).display().to_string()));
    let mut pre = vec![json!({"matcher": "Bash", "hooks": [{"type": "command", "command": cmd("guard-git.py")}]})];
    if browser {
        pre.push(json!({"matcher": "mcp__claude-in-chrome__.*", "hooks": [{"type": "command", "command": cmd("guard-browser.py")}]}));
    }
    json!({"hooks": {"PreToolUse": pre}, "enabledPlugins": plugins_off(user_settings)}).to_string()
}

/// Configuração comum dos workers: hooks de proteção, só as configurações do usuário, nenhum
/// plugin e só o MCP que o worker usa (`--strict-mcp-config`; o Chrome vem do `--chrome`).
/// Plugins e MCP sem uso somavam ~9 mil tokens ao contexto de todo turno (2026-11-19).
/// Se o Atlassian não estiver no `.claude.json` só com `type` e `url`, o worker carrega os MCP
/// do usuário como antes, para o card continuar vindo pelo MCP.
pub(crate) fn worker_config(browser: bool, atlassian: bool) -> Vec<String> {
    let (dir, claude_json) = claude_config_paths();
    let mut opts: Vec<String> = vec![
        "--settings".into(),
        guard_settings_with(browser, &read_json(&dir.join("settings.json"))),
        "--setting-sources".into(),
        "user".into(),
    ];
    let servers = if atlassian {
        atlassian_mcp(&read_json(&claude_json)).map(|a| json!({"atlassian": a}))
    } else {
        Some(json!({}))
    };
    if let Some(s) = servers {
        opts.extend(["--strict-mcp-config".into(), "--mcp-config".into(), json!({"mcpServers": s}).to_string()]);
    }
    opts
}

async fn pipeline(ctx: &Ctx, pr: &PrForReview, repo: &Path, wt: &Path, key: Option<String>) -> AppResult<()> {
    // ---- workspace ----
    ctx.stage("buscando o código do PR (git fetch)");
    // Sempre a ponta atual das branches, não o commit da última atualização da lista:
    // o PR pode ter recebido commits depois dela (PROJ-1503, 2026-11-18).
    let (src, base) = match &pr.repo {
        None => local_tips(ctx, repo, pr).await?,
        Some(rr) => rr.fetch_tips(repo, pr.bb_id, &pr.source_branch, &pr.dest_branch).await?,
    };
    if !pr.local && !src.starts_with(&pr.source_sha) {
        ctx.log(&format!(
            "o PR recebeu commits depois da última atualização da lista: revisando {} (a lista tinha {})",
            &src[..12],
            pr.source_sha
        ));
    }
    ctx.with_db(|c| {
        c.execute(
            "UPDATE review_runs SET source_sha = ?1, dest_sha = ?2 WHERE id = ?3",
            params![src, base, ctx.run_id],
        )?;
        if !pr.local && (!src.starts_with(&pr.source_sha) || !base.starts_with(&pr.dest_sha)) {
            // mesmo formato curto (12) do Bitbucket, para o resto do app comparar
            c.execute(
                "UPDATE pull_requests SET source_sha = ?1, dest_sha = ?2, changed_since_sync = 1 WHERE id = ?3",
                params![&src[..12], &base[..12], ctx.pr_id],
            )?;
        }
        Ok(())
    })?;
    ctx.check_cancel()?;

    ctx.stage("criando worktree isolado");
    git::run(repo, &["worktree", "add", "--detach", &wt.display().to_string(), &src]).await?;
    let mt = git::raw(repo, &["merge-tree", "--write-tree", "--name-only", "--no-messages", &base, &src]).await?;
    let conflicts: Vec<String> = if mt.code == 1 {
        String::from_utf8_lossy(&mt.stdout).lines().skip(1).filter(|l| !l.is_empty()).map(str::to_owned).collect()
    } else {
        vec![]
    };
    let project = project_kind(wt);
    ctx.log(&format!("worktree no commit {}, projeto {project}, {} conflito(s) com {}", &src[..10.min(src.len())], conflicts.len(), pr.dest_branch));

    let card_path = ctx.run_dir.join("card.md");
    let mut card_text = if pr.local {
        format!(
            "# Contexto (branch local, sem PR)\n\n- Repositório: {}\n- Branch: `{}` → `origin/{}`\n- Chave do card: {}\n\n\
             {}\n",
            pr.slug,
            pr.source_branch,
            pr.dest_branch,
            key.as_deref().unwrap_or("(não encontrada)"),
            if key.is_some() { "O card do Jira é a spec." } else { "Sem card configurado: use o diff e as convenções do repositório como contexto; registre hipóteses quando a intenção da mudança não estiver clara." },
        )
    } else {
        format!(
            "# Contexto do PR ({})\n\n- PR: {}#{} — {}\n- Link: {}\n- Branch: `{}` → `{}`\n- Autor: {}\n- Chave do card: {}\n\n## Descrição do PR\n\n{}\n",
            pr.repo.as_ref().map_or("local", |r| r.kind.as_str()),
            pr.slug,
            pr.bb_id,
            pr.title,
            pr.url,
            pr.source_branch,
            pr.dest_branch,
            pr.author,
            key.as_deref().unwrap_or("(não encontrada)"),
            if pr.description.trim().is_empty() { "(sem descrição)" } else { pr.description.as_str() }
        )
    };
    // contexto que só o provedor tem (as issues citadas no PR do GitHub); sem ele, o review segue
    if let Some(rr) = &pr.repo {
        match crate::hosting::Provider::connect(rr.kind, &ctx.state().db, ctx.state().secrets.as_ref()).await {
            Ok(p) => card_text.push_str(&p.extra_context(rr, &pr.title, &pr.description, &|l| ctx.log(l)).await),
            Err(e) => ctx.log(&format!("AVISO: sem o contexto extra do {}: {e}", rr.kind.name())),
        }
    }
    std::fs::write(&card_path, card_text)?;
    let mut manifest = json!({
        "projects": [{"project": project, "repo_name": pr.slug, "repo_path": repo, "worktree": wt,
                      "branch": pr.source_branch, "source_sha": src, "base_ref": base,
                      "base_branch": pr.dest_branch, "conflicts": conflicts, "obs": ""}],
        "card_file": card_path,
    });
    if let Some(k) = &key {
        manifest["card_key"] = json!(k);
        let site = {
            let st = ctx.state();
            let conn = st.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
            jira_site(&conn)?
        };
        if let Some(site) = site {
            manifest["jira_site"] = json!(site);
        }
    }
    let manifest_path = ctx.run_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    ctx.check_cancel()?;

    // ---- review (Sonnet, skill em modo orquestrado) ----
    ctx.stage("Sonnet revisando (skill /review-branch + branch-reviewer)");
    let schema = std::fs::read_to_string(assets().join("schemas/review-findings.json"))?;
    let mut allowed = READ_ONLY_TOOLS.to_owned();
    if key.is_some() {
        allowed.push_str(" ToolSearch mcp__atlassian__getJiraIssue");
    }
    let mut opts: Vec<String> = vec![
        "--model".into(), "sonnet".into(), "--effort".into(), "medium".into(),
        "--forward-subagent-text".into(),
        "--json-schema".into(), schema,
        "--add-dir".into(), ctx.run_dir.display().to_string(),
        "--allowedTools".into(), allowed,
        "--disallowedTools".into(), "Edit Write NotebookEdit mcp__claude-in-chrome__*".into(),
        "--max-turns".into(), "80".into(), "--max-budget-usd".into(), "8".into(),
    ];
    opts.extend(worker_config(false, key.is_some()));
    let args = format!("--orquestrado {}", manifest_path.display());
    let (skill, skill_bundled) = user_file("skills/review-branch/SKILL.md")?;
    let prompt = if skill_bundled {
        ctx.log("skill review-branch: cópia do app (não está no ~/.claude)");
        format!("{}\n\nARGUMENTS: {args}", skill_body(&skill))
    } else {
        format!("/review-branch {args}")
    };
    let (reviewer, reviewer_bundled) = user_file("agents/branch-reviewer.md")?;
    if reviewer_bundled {
        ctx.log("agente branch-reviewer: cópia do app (não está no ~/.claude)");
        opts.extend(["--agents".into(), json!({"branch-reviewer": agent_entry(&reviewer)}).to_string()]);
    }
    let spec = Spec {
        session: claude::Session::Ephemeral,
        name: "review-sonnet".into(),
        opts,
        prompt,
        cwd: wt.to_path_buf(),
        log_dir: ctx.run_dir.clone(),
        env_extra: vec![("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS".into(), "2400000".into()), cache_5m()],
        timeout: REVIEW_TIMEOUT,
    };
    let out = ctx.run_claude(spec, "Sonnet revisando (skill /review-branch + branch-reviewer)", |l| ctx.log(&l)).await?;
    let mut cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);
    save_cost(ctx, cost)?;
    match out.status {
        Status::Completed => {}
        Status::Cancelled => return Err(AppError::Invalid("cancelado".into())),
        Status::TimedOut => return Err(AppError::Invalid("o review passou do tempo limite (45 min)".into())),
        Status::Failed => {
            return Err(AppError::Invalid(format!(
                "o claude terminou com erro (código {:?}). {}",
                out.exit_code,
                out.result.as_ref().and_then(|r| r.get("result")).map(|v| v.to_string()).unwrap_or(out.stderr_tail)
            )));
        }
    }
    if !out.subagents.iter().any(|s| s == "branch-reviewer") {
        let w = "o review foi feito sem o subagente branch-reviewer (análise mais rasa que o fluxo manual); rode de novo";
        ctx.log(&format!("AVISO: {w}. Subagentes lançados: {:?}", out.subagents));
        ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET warning = ?1 WHERE id = ?2", params![w, ctx.run_id])?))?;
    }
    let so = out
        .result
        .as_ref()
        .and_then(|r| r.get("structured_output"))
        .cloned()
        .ok_or_else(|| AppError::Invalid("o review terminou sem saída estruturada (achado 3 da Fase 0); veja o log".into()))?;
    for p in so.get("projects").and_then(Value::as_array).into_iter().flatten() {
        ctx.log(&format!(
            "projeto {}: {}{}",
            p.get("project").and_then(Value::as_str).unwrap_or("?"),
            p.get("status").and_then(Value::as_str).unwrap_or("?"),
            p.get("note").and_then(Value::as_str).map(|n| format!(" — {n}")).unwrap_or_default()
        ));
    }

    let mut findings: Vec<Value> = so.get("findings").and_then(Value::as_array).cloned().unwrap_or_default();

    // ---- melhorias técnicas (Haiku), depois dos bugs para não repetir o que o Sonnet achou ----
    let changed = git::run(wt, &["diff", "--name-only", &format!("{base}...{src}")]).await.unwrap_or_default();
    let conventions = write_conventions(ctx, wt, &changed)?;
    let (improvements, quality_cost) = improvements(ctx, wt, &src, &base, project, &changed, &conventions, &findings).await?;
    cost += quality_cost;
    save_cost(ctx, cost)?;
    findings.extend(improvements);

    // ---- validação determinística ----
    ctx.stage("validando os achados");
    validate_findings(wt, &base, &src, &mut findings).await?;

    // ---- juiz das melhorias (Sonnet): nota de 0 a 10, como a autorreflexão do Qodo ----
    if findings.iter().any(|f| f["kind"] == "melhoria" && f["validation"] == "ok") {
        cost += judge_improvements(ctx, wt, &src, &base, &conventions, &mut findings).await?;
        save_cost(ctx, cost)?;
    }
    // se o gerador falhou, o status já é 'failed' e fica
    ctx.with_db(|c| {
        Ok(c.execute("UPDATE review_runs SET quality_status = 'completed' WHERE id = ?1 AND quality_status = 'running'", [&ctx.run_id])?)
    })?;
    std::fs::write(ctx.run_dir.join("findings.json"), serde_json::to_vec_pretty(&findings)?)?;
    ctx.with_db(|c| insert_findings(c, &ctx.run_id, &findings))?;
    let valid = findings.iter().filter(|f| f["validation"] == "ok").count();
    ctx.log(&format!("{} achado(s), {valid} válido(s)", findings.len()));

    // ---- Opus condicional (só bugs; melhoria não tem severidade) ----
    let to_audit = findings_to_audit(&findings);
    if to_audit.is_empty() {
        ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET audit_status = 'not_needed' WHERE id = ?1", [&ctx.run_id])?))?;
    } else {
        cost += audit(ctx, wt, &src, &base, &card_path, &to_audit).await?;
        save_cost(ctx, cost)?;
    }

    // ---- navegador (Haiku), só com o ambiente local no commit do PR ----
    ctx.check_cancel()?;
    if pr.local {
        ctx.log("teste no navegador: só roda em review de PR");
    } else if ctx.host.is_cli() {
        ctx.log("teste no navegador: não roda pelo terminal (-p); use o app");
    } else {
        cost += browser::run(ctx, pr, project, repo, &src, &base, wt, key.as_deref(), &card_path, &findings).await?;
        save_cost(ctx, cost)?;
    }
    Ok(())
}

/// Branch local: a ponta é a `refs/heads/<branch>` do clone; a base é buscada na origin (sem
/// rede, fica a que o último fetch trouxe, com aviso no log).
async fn local_tips(ctx: &Ctx, repo: &Path, pr: &PrForReview) -> AppResult<(String, String)> {
    let base_ref = format!("refs/remotes/origin/{}", pr.dest_branch);
    let spec = format!("+refs/heads/{}:{base_ref}", pr.dest_branch);
    if let Err(e) = git::run(repo, &["fetch", "-q", "origin", &spec]).await {
        ctx.log(&format!("AVISO: não consegui atualizar origin/{} ({e}); comparando com a que já está no clone", pr.dest_branch));
    }
    let src = git::run(repo, &["rev-parse", &format!("refs/heads/{}^{{commit}}", pr.source_branch)]).await?;
    let base = git::run(repo, &["rev-parse", &format!("{base_ref}^{{commit}}")]).await?;
    Ok((src, base))
}

/// Convenções do repositório que valem para os arquivos do PR (`conventions::collect`), gravadas
/// em `<run_dir>/conventions.md`. Devolve o caminho para o prompt, ou `nenhum`.
fn write_conventions(ctx: &Ctx, wt: &Path, changed: &str) -> AppResult<String> {
    let files: Vec<&str> = changed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let Some(text) = conventions::collect(wt, &files) else {
        ctx.log("o repositório não tem convenções versionadas (.cursor/rules, .claude/rules, CLAUDE.md, AGENTS.md)");
        return Ok("nenhum".into());
    };
    let path = ctx.run_dir.join("conventions.md");
    std::fs::write(&path, &text)?;
    ctx.log(&format!("convenções do repositório para as melhorias: {} regra(s)", text.matches("\n## `").count()));
    Ok(path.display().to_string())
}

/// Haiku (`complexity-reviewer`): complexidade desnecessária, desempenho (consulta ou trabalho
/// repetido dentro de laço) e teste quebrado ou instável no código alterado pelo PR.
/// Devolve as melhorias já no formato de achado (`kind = "melhoria"`, sem severidade) e o
/// custo. Se falhar, o review continua só com os bugs (como na auditoria).
#[allow(clippy::too_many_arguments)]
async fn improvements(
    ctx: &Ctx,
    wt: &Path,
    src: &str,
    base: &str,
    project: &str,
    changed: &str,
    conventions: &str,
    bugs: &[Value],
) -> AppResult<(Vec<Value>, f64)> {
    ctx.check_cancel()?;
    let label = "Haiku procurando complexidade, desempenho e testes quebrados";
    ctx.stage(label);
    ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET quality_status = 'running' WHERE id = ?1", [&ctx.run_id])?))?;
    let existing = ctx.run_dir.join("quality-existing.json");
    std::fs::write(&existing, serde_json::to_vec_pretty(bugs)?)?;
    let mut opts: Vec<String> = vec![
        "--agent".into(), "complexity-reviewer".into(), "--agents".into(), agents_json(&["complexity-reviewer"])?,
        "--model".into(), "haiku".into(), "--effort".into(), "medium".into(),
        "--json-schema".into(), std::fs::read_to_string(assets().join("schemas/code-improvements.json"))?,
        "--add-dir".into(), ctx.run_dir.display().to_string(),
        "--allowedTools".into(), READ_ONLY_TOOLS.replace("Agent ", ""),
        "--disallowedTools".into(), "Edit Write NotebookEdit Agent mcp__atlassian__* mcp__claude-in-chrome__*".into(),
        "--max-turns".into(), "40".into(), "--max-budget-usd".into(), "1.5".into(),
    ];
    opts.extend(worker_config(false, false));
    let spec = Spec {
        session: claude::Session::Ephemeral,
        name: "quality-haiku".into(),
        opts,
        prompt: format!(
            "WORKTREE: {}\nSOURCE_SHA: {src}\nBASE: {base}\nPROJECT: {project}\nEXISTING_FINDINGS: {}\nCONVENTIONS_FILE: {conventions}\nCHANGED_FILES:\n{changed}\n\n\
             Aponte a complexidade desnecessária, os problemas de desempenho e os testes quebrados ou instáveis do código alterado por este PR.",
            wt.display(),
            existing.display()
        ),
        cwd: wt.to_path_buf(),
        log_dir: ctx.run_dir.clone(),
        env_extra: vec![cache_5m()],
        timeout: QUALITY_TIMEOUT,
    };
    let out = ctx.run_claude(spec, label, |l| ctx.log(&l)).await?;
    let cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);
    let items = out.result.as_ref().and_then(|r| r.pointer("/structured_output/improvements")).and_then(Value::as_array).cloned();
    let status = match (out.status, items) {
        (Status::Completed, Some(items)) => {
            let found = dedup_improvements(items.iter().map(|i| improvement_as_finding(i, project)).collect());
            ctx.log(&format!("melhorias técnicas propostas: {} ({} sem repetição)", items.len(), found.len()));
            // quality_status fica 'running' até o juiz terminar
            return Ok((found, cost));
        }
        (Status::Cancelled, _) => return Err(AppError::Invalid("cancelado".into())),
        (status, _) => status,
    };
    ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET quality_status = 'failed' WHERE id = ?1", [&ctx.run_id])?))?;
    ctx.log(&format!("análise de complexidade falhou ({status:?}); o review segue só com os bugs"));
    Ok((vec![], cost))
}

/// Melhoria do `complexity-reviewer` → achado (`kind = "melhoria"`), com o texto para colar no PR.
/// `existing_code` e `improved_code` guardam a indentação: só perdem as quebras de linha das pontas.
/// Em `desempenho` e `teste`, `improved_code` vazio quer dizer que a correção fica só na descrição
/// (a de desempenho costuma mexer em mais de um lugar). Nas categorias de complexidade, vazio
/// quer dizer apagar as linhas.
fn improvement_as_finding(i: &Value, project: &str) -> Value {
    let s = |k: &str| i[k].as_str().unwrap_or("").trim().to_owned();
    let code = |k: &str| i[k].as_str().unwrap_or("").trim_matches('\n').trim_end().to_owned();
    let line = i["line"].as_u64().unwrap_or(0);
    let end_line = i["end_line"].as_u64().filter(|e| *e >= line).unwrap_or(line);
    let range = if end_line > line { format!("{line}-{end_line}") } else { line.to_string() };
    let improved = code("improved_code");
    let (tag, label) = match s("category").as_str() {
        "desempenho" => ("desempenho", "Mais eficiente"),
        "teste" => ("teste", "Corrigido"),
        _ => ("melhoria técnica", "Mais simples"),
    };
    let proposal = match (improved.is_empty(), tag == "melhoria técnica") {
        (true, true) => "\n\nMais simples: remover essas linhas.".to_owned(),
        (true, false) => String::new(),
        (false, _) => format!("\n\n{label}:\n```\n{improved}\n```"),
    };
    let markdown = format!(
        "`{}:{range}` — {} ({})\n\n{}\n\nPor quê: {}\n\nAtual:\n```\n{}\n```{proposal}",
        s("file"),
        s("title"),
        tag,
        s("description"),
        s("evidence"),
        code("existing_code"),
    );
    json!({
        "project": project,
        "kind": "melhoria",
        "category": s("category"),
        "file": s("file"),
        "line": line,
        "end_line": end_line,
        "title": s("title"),
        "description": s("description"),
        "evidence": s("evidence"),
        "existing_code": code("existing_code"),
        "improved_code": improved,
        "steps": [],
        "markdown": markdown,
    })
}

/// Tira melhorias repetidas: mesmo arquivo e mesmo título (sem diferença de caixa), como o Qodo.
fn dedup_improvements(items: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|f| {
            let key = (f["file"].as_str().unwrap_or("").to_owned(), f["title"].as_str().unwrap_or("").trim().to_lowercase());
            seen.insert(key)
        })
        .collect()
}

/// O `existing_code` bate com as linhas `line..=end_line` do arquivo? Compara sem a indentação
/// comum e sem espaço no fim de linha, como o `_validate_suggestion` do PR-Agent.
fn existing_code_matches(file_text: &str, line: u64, end_line: u64, existing: &str) -> bool {
    let lines: Vec<&str> = file_text.lines().collect();
    if line == 0 || end_line < line || end_line as usize > lines.len() {
        return false;
    }
    normalize_block(&lines[line as usize - 1..end_line as usize].join("\n")) == normalize_block(existing)
}

fn normalize_block(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.trim_matches('\n').lines().map(|l| l.trim_end()).collect();
    let indent = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines.iter().map(|l| if l.len() >= indent { l[indent..].to_owned() } else { String::new() }).collect()
}

/// Nota → importância, decidida em código (no Qodo: ≥ 9 alta, ≥ 7 média). Melhoria nunca é bug
/// grave, então as faixas descem: ≥ 7 alta, ≥ 4 média, 1 a 3 baixa.
fn importance(score: u64) -> &'static str {
    match score {
        7.. => "alta",
        4..=6 => "media",
        _ => "baixa",
    }
}

/// Sonnet (`complexity-judge`): nota de 0 a 10 para cada melhoria válida, conferindo no código se
/// a forma simples é correta e equivalente. Nota 0 ou `equivalent = false` descarta a melhoria.
/// Se o juiz falhar, as melhorias ficam sem nota (não somem). Devolve o custo.
async fn judge_improvements(ctx: &Ctx, wt: &Path, src: &str, base: &str, conventions: &str, findings: &mut [Value]) -> AppResult<f64> {
    ctx.check_cancel()?;
    let label = "Sonnet dando nota às melhorias";
    ctx.stage(label);
    let to_judge: Vec<Value> = findings
        .iter()
        .filter(|f| f["kind"] == "melhoria" && f["validation"] == "ok")
        .map(|f| {
            json!({
                "id": f["id"], "category": f["category"], "file": f["file"], "line": f["line"], "end_line": f["end_line"],
                "title": f["title"], "description": f["description"], "evidence": f["evidence"],
                "existing_code": f["existing_code"], "improved_code": f["improved_code"],
            })
        })
        .collect();
    let input = ctx.run_dir.join("judge-input.json");
    std::fs::write(&input, serde_json::to_vec_pretty(&to_judge)?)?;
    let mut opts: Vec<String> = vec![
        "--agent".into(), "complexity-judge".into(), "--agents".into(), agents_json(&["complexity-judge"])?,
        "--model".into(), "sonnet".into(), "--effort".into(), "low".into(),
        "--json-schema".into(), std::fs::read_to_string(assets().join("schemas/complexity-judgments.json"))?,
        "--add-dir".into(), ctx.run_dir.display().to_string(),
        "--allowedTools".into(), READ_ONLY_TOOLS.replace("Agent ", ""),
        "--disallowedTools".into(), "Edit Write NotebookEdit Agent mcp__atlassian__* mcp__claude-in-chrome__*".into(),
        "--max-turns".into(), "30".into(), "--max-budget-usd".into(), "1".into(),
    ];
    opts.extend(worker_config(false, false));
    let spec = Spec {
        session: claude::Session::Ephemeral,
        name: "judge-sonnet".into(),
        opts,
        prompt: format!(
            "WORKTREE: {}\nSOURCE_SHA: {src}\nBASE: {base}\nCONVENTIONS_FILE: {conventions}\nIMPROVEMENTS_FILE: {}\nDê a nota de cada melhoria do IMPROVEMENTS_FILE.",
            wt.display(),
            input.display()
        ),
        cwd: wt.to_path_buf(),
        log_dir: ctx.run_dir.clone(),
        env_extra: vec![cache_5m()],
        timeout: QUALITY_TIMEOUT,
    };
    let out = ctx.run_claude(spec, label, |l| ctx.log(&l)).await?;
    let cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);
    let judgments = out.result.as_ref().and_then(|r| r.pointer("/structured_output/judgments")).and_then(Value::as_array).cloned();
    match (out.status, judgments) {
        (Status::Completed, Some(judgments)) => {
            let (kept, dropped) = apply_judgments(findings, &judgments);
            ctx.log(&format!("juiz das melhorias: {kept} mantida(s), {dropped} descartada(s)"));
        }
        (Status::Cancelled, _) => return Err(AppError::Invalid("cancelado".into())),
        (status, _) => {
            for f in findings.iter_mut().filter(|f| f["kind"] == "melhoria" && f["validation"] == "ok") {
                f["score_why"] = json!("sem nota: o juiz falhou");
            }
            ctx.log(&format!("juiz das melhorias falhou ({status:?}); as melhorias ficam sem nota"));
        }
    }
    Ok(cost)
}

/// Aplica as notas do juiz. Nota 0, `equivalent = false` ou melhoria sem nota viram descartadas.
/// Devolve (mantidas, descartadas).
fn apply_judgments(findings: &mut [Value], judgments: &[Value]) -> (usize, usize) {
    let (mut kept, mut dropped) = (0, 0);
    for f in findings.iter_mut().filter(|f| f["kind"] == "melhoria" && f["validation"] == "ok") {
        let Some(j) = judgments.iter().find(|j| j["id"] == f["id"]) else {
            f["validation"] = json!("rejected");
            f["reject_reason"] = json!("o juiz não deu nota");
            dropped += 1;
            continue;
        };
        let score = j["score"].as_u64().unwrap_or(0).min(10);
        let equivalent = j["equivalent"].as_bool().unwrap_or(false);
        let why = j["why"].as_str().unwrap_or("").trim().to_owned();
        f["score"] = json!(score);
        f["score_why"] = json!(why);
        if score == 0 || !equivalent {
            f["validation"] = json!("rejected");
            f["reject_reason"] = json!(format!("juiz: {why}"));
            dropped += 1;
        } else {
            f["importance"] = json!(importance(score));
            kept += 1;
        }
    }
    (kept, dropped)
}

/// Achados que o Opus audita: todos os bugs válidos, de qualquer severidade (regra em código).
/// Sem nenhum, o Opus não roda. Melhoria não tem severidade e nunca entra; achado com
/// `arquivo:linha` inválido também não.
fn findings_to_audit(findings: &[Value]) -> Vec<&Value> {
    findings.iter().filter(|f| f["validation"] == "ok" && f["kind"] != "melhoria").collect()
}

/// Opus (`review-auditor`) sobre todos os bugs válidos (ver `findings_to_audit`). Devolve o custo.
async fn audit(ctx: &Ctx, wt: &Path, src: &str, base: &str, card_path: &Path, to_audit: &[&Value]) -> AppResult<f64> {
    ctx.check_cancel()?;
    ctx.stage(&format!("Opus auditando {} achado(s)", to_audit.len()));
    ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET audit_status = 'running' WHERE id = ?1", [&ctx.run_id])?))?;
    let audit_input = ctx.run_dir.join("audit-input.json");
    std::fs::write(&audit_input, serde_json::to_vec_pretty(&to_audit)?)?;
    let mut opts: Vec<String> = vec![
        "--agent".into(), "review-auditor".into(), "--agents".into(), agents_json(&["review-auditor"])?,
        "--model".into(), "opus".into(), "--effort".into(), "high".into(),
        "--json-schema".into(), std::fs::read_to_string(assets().join("schemas/audit.json"))?,
        "--add-dir".into(), ctx.run_dir.display().to_string(),
        "--allowedTools".into(), READ_ONLY_TOOLS.replace("Agent ", ""),
        "--disallowedTools".into(), "Edit Write NotebookEdit Agent mcp__atlassian__* mcp__claude-in-chrome__*".into(),
        "--max-turns".into(), "40".into(), "--max-budget-usd".into(), "6".into(),
    ];
    // o auditor lê o card do CARD_FILE: nenhum MCP
    opts.extend(worker_config(false, false));
    let spec = Spec {
        session: claude::Session::Ephemeral,
        name: "audit-opus".into(),
        opts,
        prompt: format!(
            "WORKTREE: {}\nSOURCE_SHA: {src}\nBASE: {base}\nCARD_FILE: {}\nFINDINGS_FILE: {}\nAudite cada achado do FINDINGS_FILE.",
            wt.display(),
            card_path.display(),
            audit_input.display()
        ),
        cwd: wt.to_path_buf(),
        log_dir: ctx.run_dir.clone(),
        env_extra: vec![cache_5m()],
        timeout: AUDIT_TIMEOUT,
    };
    let out = ctx.run_claude(spec, &format!("Opus auditando {} achado(s)", to_audit.len()), |l| ctx.log(&l)).await?;
    let cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);
    let audits = out.result.as_ref().and_then(|r| r.pointer("/structured_output/audits")).and_then(Value::as_array).cloned();
    match (out.status, audits) {
        (Status::Completed, Some(audits)) => {
            ctx.with_db(|c| {
                for a in &audits {
                    c.execute(
                        "UPDATE findings SET audit_decision = ?1, audit_new_severity = ?2, audit_rationale = ?3
                         WHERE run_id = ?4 AND seq = ?5",
                        params![
                            a["decision"].as_str(),
                            a["new_severity"].as_str(),
                            a["rationale"].as_str(),
                            ctx.run_id,
                            a["id"].as_str().and_then(|s| s.trim_start_matches('F').parse::<i64>().ok()),
                        ],
                    )?;
                }
                c.execute("UPDATE review_runs SET audit_status = 'completed' WHERE id = ?1", [&ctx.run_id])?;
                Ok(())
            })?;
            ctx.log(&format!("auditoria: {} decisão(ões)", audits.len()));
        }
        (Status::Cancelled, _) => return Err(AppError::Invalid("cancelado".into())),
        (status, _) => {
            // o review do Sonnet continua valendo; só a auditoria falhou
            ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET audit_status = 'failed' WHERE id = ?1", [&ctx.run_id])?))?;
            ctx.log(&format!("auditoria do Opus falhou ({status:?}); os achados do Sonnet ficam sem auditoria"));
        }
    }
    Ok(cost)
}

fn save_cost(ctx: &Ctx, cost: f64) -> AppResult<()> {
    ctx.with_db(|c| Ok(c.execute("UPDATE review_runs SET cost_usd = ?1 WHERE id = ?2", params![cost, ctx.run_id])?))?;
    Ok(())
}

/// Cada achado precisa apontar `arquivo:linha` existente no commit do PR; marca se está no diff.
async fn validate_findings(wt: &Path, base: &str, src: &str, findings: &mut [Value]) -> AppResult<()> {
    let changed = git::run(wt, &["diff", "--name-only", &format!("{base}...{src}")]).await?;
    let changed: Vec<&str> = changed.lines().collect();
    for (i, f) in findings.iter_mut().enumerate() {
        f["id"] = json!(format!("F{}", i + 1));
        let file = f["file"].as_str().unwrap_or("").trim_start_matches("./").to_owned();
        let line = f["line"].as_u64().unwrap_or(0);
        let blob = git::raw(wt, &["show", &format!("{src}:{file}")]).await?;
        if file.is_empty() || blob.code != 0 {
            f["validation"] = json!("rejected");
            f["reject_reason"] = json!("arquivo não existe no commit do PR");
        } else if line == 0 || line > blob.stdout.iter().filter(|b| **b == b'\n').count() as u64 + 1 {
            f["validation"] = json!("rejected");
            f["reject_reason"] = json!("linha além do fim do arquivo");
        } else if let Some(reason) = improvement_problem(f, line, &blob.stdout, &new_hunks_if_improvement(wt, base, src, &file, f).await?) {
            f["validation"] = json!("rejected");
            f["reject_reason"] = json!(reason);
        } else {
            f["validation"] = json!("ok");
            f["in_diff"] = json!(changed.contains(&file.as_str()));
        }
    }
    Ok(())
}

async fn new_hunks_if_improvement(wt: &Path, base: &str, src: &str, file: &str, f: &Value) -> AppResult<Vec<(u64, u64)>> {
    if f["kind"] == "melhoria" { new_hunks(wt, base, src, file).await } else { Ok(vec![]) }
}

/// Por que uma melhoria não vale (`None` se vale, ou se não é melhoria): ela só trata do código
/// que o PR escreveu, e o `existing_code` tem que ser cópia literal das linhas apontadas.
fn improvement_problem(f: &Value, line: u64, file_bytes: &[u8], hunks: &[(u64, u64)]) -> Option<&'static str> {
    if f["kind"] != "melhoria" {
        return None;
    }
    let end_line = f["end_line"].as_u64().unwrap_or(line);
    if !(line..=end_line).any(|l| line_in_hunks(l, hunks)) {
        return Some("linha fora do trecho alterado pelo PR");
    }
    let existing = f["existing_code"].as_str().unwrap_or("");
    if !existing_code_matches(&String::from_utf8_lossy(file_bytes), line, end_line, existing) {
        return Some("o trecho citado não bate com o arquivo no commit do PR");
    }
    None
}

/// Trechos `(início, fim)` do arquivo no `src` que o PR adicionou ou alterou (`git diff -U0`).
async fn new_hunks(wt: &Path, base: &str, src: &str, file: &str) -> AppResult<Vec<(u64, u64)>> {
    let diff = git::run(wt, &["diff", "-U0", &format!("{base}...{src}"), "--", file]).await?;
    Ok(parse_new_hunks(&diff))
}

fn parse_new_hunks(diff: &str) -> Vec<(u64, u64)> {
    diff.lines()
        .filter_map(|l| l.strip_prefix("@@ "))
        .filter_map(|l| l.split_whitespace().find(|p| p.starts_with('+')))
        .filter_map(|p| {
            let mut it = p[1..].splitn(2, ',');
            let start: u64 = it.next()?.parse().ok()?;
            let count: u64 = it.next().map_or(Some(1), |c| c.parse().ok())?;
            // count 0: só remoção; a linha seguinte é a que ficou no lugar
            Some(if count == 0 { (start.max(1), start.max(1)) } else { (start, start + count - 1) })
        })
        .collect()
}

fn line_in_hunks(line: u64, hunks: &[(u64, u64)]) -> bool {
    hunks.iter().any(|&(a, b)| line + HUNK_SLACK >= a && line <= b + HUNK_SLACK)
}

fn insert_findings(conn: &Connection, run_id: &str, findings: &[Value]) -> AppResult<()> {
    for (i, f) in findings.iter().enumerate() {
        conn.execute(
            "INSERT INTO findings (run_id, seq, project, kind, severity, severity_reason, impact, file, line, title,
               description, steps_json, pattern_ref, confidence, markdown, validation, reject_reason, in_diff,
               category, evidence, suggestion, end_line, existing_code, improved_code, score, score_why, importance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                     ?22, ?23, ?24, ?25, ?26, ?27)",
            params![
                run_id,
                (i + 1) as i64,
                f["project"].as_str(),
                f["kind"].as_str(),
                f["severity"].as_str(),
                f["severity_reason"].as_str(),
                f["impact"].as_str(),
                f["file"].as_str(),
                f["line"].as_i64(),
                f["title"].as_str(),
                f["description"].as_str(),
                f.get("steps").map(|s| s.to_string()).unwrap_or_else(|| "[]".into()),
                f["pattern_ref"].as_str(),
                f["confidence"].as_str(),
                f["markdown"].as_str(),
                f["validation"].as_str().unwrap_or("rejected"),
                f["reject_reason"].as_str(),
                f["in_diff"].as_bool(),
                f["category"].as_str(),
                f["evidence"].as_str(),
                f["suggestion"].as_str(),
                f["end_line"].as_i64(),
                f["existing_code"].as_str(),
                f["improved_code"].as_str(),
                f["score"].as_i64(),
                f["score_why"].as_str(),
                f["importance"].as_str(),
            ],
        )?;
    }
    Ok(())
}

/// Regras de escrita das respostas que o usuário lê (chat do review e correções), resumidas da skill
/// `humanizer` em `claude/prompts/escrita.md`. Vai no prompt de sistema, então vale em todo turno,
/// inclusive depois do `--resume`.
pub(crate) fn writing_style_opts() -> Vec<String> {
    vec!["--append-system-prompt-file".into(), assets().join("prompts").join("escrita.md").display().to_string()]
}

/// `claude/agents/<nome>.md` (frontmatter + corpo) → JSON de `--agents`.
pub(crate) fn agents_json(names: &[&str]) -> AppResult<String> {
    let mut out = serde_json::Map::new();
    for n in names {
        let text = std::fs::read_to_string(assets().join("agents").join(format!("{n}.md")))?;
        out.insert((*n).to_owned(), agent_entry(&text));
    }
    Ok(Value::Object(out).to_string())
}

/// Um arquivo de agente (frontmatter + corpo) → entrada do JSON de `--agents`.
pub(crate) fn agent_entry(text: &str) -> Value {
    let mut parts = text.splitn(3, "---");
    let (_, fm, body) = (parts.next(), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut meta = serde_json::Map::new();
    for line in fm.lines().filter(|l| l.contains(':')) {
        let (k, v) = line.split_once(':').unwrap();
        let (k, v) = (k.trim(), v.trim());
        let val = match k {
            "name" => continue,
            "tools" | "disallowedTools" => json!(v.split(',').map(str::trim).collect::<Vec<_>>()),
            "maxTurns" => json!(v.parse::<u64>().unwrap_or(40)),
            _ => json!(v),
        };
        meta.insert(k.to_owned(), val);
    }
    meta.insert("prompt".into(), json!(body.trim()));
    Value::Object(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_body_tira_o_frontmatter() {
        assert_eq!(skill_body("---\nname: x\ndescription: a: b\n---\n\n# Título\n\ntexto\n---\nfim\n"), "# Título\n\ntexto\n---\nfim");
        assert_eq!(skill_body("# sem frontmatter\n"), "# sem frontmatter");
    }

    /// As cópias que vão no pacote existem e viram skill e agentes válidos.
    #[test]
    fn copias_do_usuario_no_pacote() {
        let dir = assets().join("user");
        let skill = std::fs::read_to_string(dir.join("skills/review-branch/SKILL.md")).unwrap();
        assert!(skill_body(&skill).contains("Modo orquestrado"));
        for (f, tool) in [("branch-reviewer", "Grep"), ("committer", "Bash")] {
            let e = agent_entry(&std::fs::read_to_string(dir.join(format!("agents/{f}.md"))).unwrap());
            assert!(e["tools"].as_array().unwrap().iter().any(|t| t == tool), "{f}: {e}");
            assert!(!e["prompt"].as_str().unwrap().is_empty(), "{f}");
        }
    }

    /// Execução sem processo do terminal vivo vira falha ao abrir o app; o `cli.pid` de um
    /// processo que não é o orquestrador (PID reaproveitado) não a protege.
    #[test]
    fn app_marca_interrompida_so_sem_terminal_vivo() {
        let conn = db::open_in_memory().unwrap();
        let base = std::env::temp_dir().join(format!("prr-mi-{}", std::process::id()));
        let dirs: Vec<PathBuf> = (0..3).map(|i| base.join(i.to_string())).collect();
        for (i, d) in dirs.iter().enumerate() {
            std::fs::create_dir_all(d).unwrap();
            conn.execute(
                "INSERT INTO review_runs (id, repo_slug, branch, status, run_dir) VALUES (?1, 'repo', 'PROJ-1', 'running', ?2)",
                params![format!("r{i}"), d.display().to_string()],
            )
            .unwrap();
        }
        std::fs::write(dirs[1].join(CLI_PID_FILE), "999999999").unwrap(); // processo morto
        std::fs::write(dirs[2].join(CLI_PID_FILE), std::process::id().to_string()).unwrap(); // vivo, mas é o teste
        mark_interrupted(&conn).unwrap();
        let failed: i64 = conn.query_row("SELECT count(*) FROM review_runs WHERE status = 'failed'", [], |r| r.get(0)).unwrap();
        assert_eq!(failed, 3);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hook_com_espaco_no_caminho_roda_no_shell() {
        let dir = std::env::temp_dir().join(format!("prr dir com espaço {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.py");
        std::fs::write(&script, "import sys; sys.exit(7)").unwrap();
        let cmd = format!("python3 -I {}", shell_quote(&script.display().to_string()));
        let st = std::process::Command::new("sh").arg("-c").arg(&cmd).status().unwrap();
        assert_eq!(st.code(), Some(7), "{cmd}");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// O review usa a ponta atual da branch, mesmo que o clone e a lista estejam atrasados.
    #[tokio::test]
    async fn ponta_atual_da_branch_apos_fetch() {
        let root = std::env::temp_dir().join(format!("prr-tips-{}", std::process::id()));
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
        let origin = root.join("origin.git");
        let dev = root.join("dev");
        let user = root.join("user");
        g(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), dev.to_str().unwrap()]);
        g(&dev, &["commit", "-q", "--allow-empty", "-m", "base"]);
        g(&dev, &["push", "-q", "origin", "HEAD:dev"]);
        g(&dev, &["commit", "-q", "--allow-empty", "-m", "pr v1"]);
        g(&dev, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        let v1 = g(&dev, &["rev-parse", "HEAD"]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), user.to_str().unwrap()]);
        // o autor empurra mais um commit depois que o usuário clonou
        g(&dev, &["commit", "-q", "--allow-empty", "-m", "pr v2"]);
        g(&dev, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        let v2 = g(&dev, &["rev-parse", "HEAD"]);
        assert_ne!(v1, v2);

        let (src, base) = fresh_tips(&user, "PROJ-1", "dev").await.unwrap();
        assert_eq!(src, v2, "deve revisar a ponta atual, não a v1");
        assert_eq!(base, g(&dev, &["rev-parse", "origin/dev"]));
        assert!(fresh_tips(&user, "nao-existe", "dev").await.unwrap_err().to_string().contains("não consegui buscar"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn chave_do_card() {
        assert_eq!(card_key("PROJ-1676-ajuste", "x").as_deref(), Some("PROJ-1676"));
        assert_eq!(card_key("feature/PROJ-12", "x").as_deref(), Some("PROJ-12"));
        assert_eq!(card_key("ajuste", "Corrige PROJ-99 e PROJ-100").as_deref(), Some("PROJ-99"));
        assert_eq!(card_key("xPROJ-1", "sem chave").as_deref(), None);
        assert_eq!(card_key("dev", "").as_deref(), None);
        let conn = db::open_in_memory().unwrap();
        assert_eq!(configured_jira_key(&conn, "PROJ-12", "x").unwrap(), None);
        db::set_setting(&conn, "jira.site", &json!("acme.atlassian.net")).unwrap();
        assert_eq!(configured_jira_key(&conn, "PROJ-12", "x").unwrap().as_deref(), Some("PROJ-12"));
    }

    #[test]
    fn jira_site_normalizado() {
        let conn = db::open_in_memory().unwrap();
        assert_eq!(jira_site(&conn).unwrap(), None);
        for (raw, want) in [(" https://acme.atlassian.net/ ", Some("acme.atlassian.net")), ("acme.atlassian.net", Some("acme.atlassian.net")), ("  ", None)] {
            db::set_setting(&conn, "jira.site", &json!(raw)).unwrap();
            assert_eq!(jira_site(&conn).unwrap().as_deref(), want, "{raw:?}");
        }
    }

    #[test]
    fn agents_json_do_auditor() {
        let v: Value = serde_json::from_str(&agents_json(&["review-auditor"]).unwrap()).unwrap();
        let a = &v["review-auditor"];
        assert_eq!(a["model"], "opus");
        assert!(a["tools"].as_array().unwrap().iter().any(|t| t == "StructuredOutput"), "achado 3 da Fase 0");
        assert!(a["prompt"].as_str().unwrap().contains("auditor independente"));
        assert!(a.get("name").is_none());
    }

    #[test]
    fn trechos_alterados_do_diff() {
        let diff = "diff --git a/x b/x\n@@ -15,0 +16 @@ class X\n+\n@@ -18,2 +20,7 @@ fn\n+a\n@@ -40,3 +47,0 @@\n";
        let h = parse_new_hunks(diff);
        assert_eq!(h, vec![(16, 16), (20, 26), (47, 47)]);
        assert!(line_in_hunks(20, &h));
        assert!(line_in_hunks(17, &h), "início do bloco logo antes do trecho");
        assert!(line_in_hunks(29, &h));
        assert!(!line_in_hunks(35, &h));
        assert!(!line_in_hunks(1, &[]));
    }

    #[test]
    fn opus_audita_todos_os_bugs_validos() {
        let f = |seq: i64, sev: &str, kind: &str, val: &str| json!({"seq": seq, "severity": sev, "kind": kind, "validation": val});
        let seqs = |v: Vec<&Value>| v.iter().map(|f| f["seq"].as_i64().unwrap()).collect::<Vec<_>>();
        // sem bug válido (só inválido ou melhoria): Opus não roda
        assert!(findings_to_audit(&[]).is_empty());
        assert!(findings_to_audit(&[f(1, "high", "bug", "invalid_line"), json!({"seq": 2, "kind": "melhoria", "validation": "ok"})]).is_empty());
        // só medium/low também roda
        assert_eq!(seqs(findings_to_audit(&[f(1, "medium", "bug", "ok"), f(2, "low", "bug", "ok")])), vec![1, 2]);
        // todos os bugs válidos, sem melhoria nem inválido
        let all = [
            f(1, "low", "bug", "ok"),
            f(2, "critical", "bug", "ok"),
            f(3, "medium", "bug", "invalid_line"),
            json!({"seq": 4, "kind": "melhoria", "validation": "ok"}),
            f(5, "medium", "bug", "ok"),
        ];
        assert_eq!(seqs(findings_to_audit(&all)), vec![1, 2, 5]);
    }

    #[test]
    fn melhoria_vira_achado_sem_severidade() {
        let i = json!({"category": "redundante", "file": "app/X.php", "line": 32, "end_line": 33, "title": "t",
                       "description": "d", "evidence": "app/Http/Kernel.php:25",
                       "existing_code": "\n        'email' => filled($this->email) ? $this->email : null,\n",
                       "improved_code": "        'email' => $this->email,"});
        let f = improvement_as_finding(&i, "back");
        assert_eq!(f["kind"], "melhoria");
        assert!(f.get("severity").is_none(), "melhoria nunca aciona o Opus");
        assert_eq!((f["line"].as_u64(), f["end_line"].as_u64()), (Some(32), Some(33)));
        assert_eq!(f["existing_code"], "        'email' => filled($this->email) ? $this->email : null,", "guarda a indentação");
        let md = f["markdown"].as_str().unwrap();
        assert!(md.starts_with("`app/X.php:32-33` — t"));
        assert!(md.contains("Kernel.php:25") && md.contains("'email' => $this->email,"));
        // end_line ausente ou antes de line vira line
        let f = improvement_as_finding(&json!({"line": 5, "end_line": 2}), "back");
        assert_eq!(f["end_line"], 5);
    }

    /// Em desempenho, a correção costuma mexer em mais de um método: `improved_code` vazio não é
    /// "remover as linhas", e a correção fica só na descrição.
    #[test]
    fn melhoria_de_desempenho_sem_codigo_nao_manda_apagar() {
        let i = json!({"category": "desempenho", "file": "app/S.php", "line": 40, "end_line": 40, "title": "consulta por preço",
                       "description": "carregar uma vez no apply()", "evidence": "app/S.php:12",
                       "existing_code": "        $month = $this->latestPublishedMonth();", "improved_code": ""});
        let md = improvement_as_finding(&i, "back")["markdown"].as_str().unwrap().to_owned();
        assert!(md.starts_with("`app/S.php:40` — consulta por preço (desempenho)"), "{md}");
        assert!(!md.contains("remover essas linhas") && !md.contains("Mais simples"), "{md}");
        assert!(md.trim_end().ends_with("```"), "termina no trecho atual: {md}");
        let i = json!({"category": "desempenho", "line": 1, "end_line": 1, "existing_code": "a", "improved_code": "b"});
        assert!(improvement_as_finding(&i, "back")["markdown"].as_str().unwrap().contains("Mais eficiente:\n```\nb\n```"));
    }

    #[test]
    fn melhoria_de_teste_traz_a_linha_corrigida() {
        let i = json!({"category": "teste", "file": "tests/XTest.php", "line": 40, "end_line": 40, "title": "float por identidade",
                       "description": "d", "evidence": "app/S.php:186",
                       "existing_code": "expect($r)->toBe(3.1);", "improved_code": "expect(round($r, 4))->toBe(3.1);"});
        let md = improvement_as_finding(&i, "back")["markdown"].as_str().unwrap().to_owned();
        assert!(md.starts_with("`tests/XTest.php:40` — float por identidade (teste)"), "{md}");
        assert!(md.contains("Corrigido:\n```\nexpect(round($r, 4))->toBe(3.1);\n```"), "{md}");
        let i = json!({"category": "teste", "line": 1, "end_line": 1, "existing_code": "a", "improved_code": ""});
        assert!(!improvement_as_finding(&i, "back")["markdown"].as_str().unwrap().contains("remover"), "teste sem código não manda apagar");
    }

    #[test]
    fn melhorias_repetidas_saem() {
        let a = json!({"file": "a.php", "title": "Filled redundante"});
        let b = json!({"file": "a.php", "title": "  filled redundante "});
        let c = json!({"file": "b.php", "title": "Filled redundante"});
        assert_eq!(dedup_improvements(vec![a, b, c]).len(), 2);
    }

    #[test]
    fn trecho_citado_confere_com_o_arquivo() {
        let file = "<?php\nclass X {\n    public function a()\n    {\n        $x = 1;   \n        return $x;\n    }\n}\n";
        // sem a indentação comum e sem espaço no fim de linha, como o PR-Agent
        assert!(existing_code_matches(file, 5, 6, "$x = 1;\nreturn $x;"));
        assert!(existing_code_matches(file, 3, 4, "    public function a()\n    {\n"));
        assert!(!existing_code_matches(file, 5, 6, "$x = 2;\nreturn $x;"), "alucinação");
        assert!(!existing_code_matches(file, 5, 5, "$x = 1;\nreturn $x;"), "faixa menor que o trecho");
        assert!(!existing_code_matches(file, 8, 12, "}"), "além do fim");
        assert!(!existing_code_matches(file, 5, 6, ""));
    }

    #[test]
    fn juiz_decide_o_que_fica() {
        let mut fs = vec![
            json!({"id": "F1", "kind": "bug", "validation": "ok"}),
            json!({"id": "F2", "kind": "melhoria", "validation": "ok"}),
            json!({"id": "F3", "kind": "melhoria", "validation": "ok"}),
            json!({"id": "F4", "kind": "melhoria", "validation": "ok"}),
            json!({"id": "F5", "kind": "melhoria", "validation": "ok"}),
            json!({"id": "F6", "kind": "melhoria", "validation": "rejected"}),
        ];
        let js = vec![
            json!({"id": "F2", "score": 8, "equivalent": true, "why": "a"}),
            json!({"id": "F3", "score": 0, "equivalent": true, "why": "errada"}),
            json!({"id": "F4", "score": 6, "equivalent": false, "why": "muda o caso \"0\""}),
            json!({"id": "F1", "score": 0, "equivalent": false, "why": "bug não é com o juiz"}),
        ];
        assert_eq!(apply_judgments(&mut fs, &js), (1, 3));
        assert_eq!(fs[0]["validation"], "ok", "bug não passa pelo juiz");
        assert_eq!((fs[1]["importance"].as_str(), fs[1]["score"].as_u64()), (Some("alta"), Some(8)));
        assert_eq!(fs[2]["reject_reason"], "juiz: errada");
        assert_eq!(fs[3]["validation"], "rejected", "não equivalente cai mesmo com nota");
        assert_eq!(fs[4]["reject_reason"], "o juiz não deu nota");
        assert!(fs[5].get("score").is_none());
        assert_eq!((importance(10), importance(4), importance(3), importance(1)), ("alta", "media", "baixa", "baixa"));
    }

    #[test]
    fn regras_de_escrita_no_prompt_de_sistema() {
        let o = writing_style_opts();
        assert_eq!(o[0], "--append-system-prompt-file");
        let text = std::fs::read_to_string(&o[1]).expect("claude/prompts/escrita.md");
        assert!(text.contains("Comece pela conclusão"));
    }

    #[test]
    fn agents_json_do_complexity_judge() {
        let v: Value = serde_json::from_str(&agents_json(&["complexity-judge"]).unwrap()).unwrap();
        let a = &v["complexity-judge"];
        assert_eq!((a["model"].as_str(), a["effort"].as_str()), (Some("sonnet"), Some("low")));
        assert!(a["tools"].as_array().unwrap().iter().any(|t| t == "StructuredOutput"), "achado 3 da Fase 0");
    }

    #[test]
    fn agents_json_do_complexity_reviewer() {
        let v: Value = serde_json::from_str(&agents_json(&["complexity-reviewer"]).unwrap()).unwrap();
        let a = &v["complexity-reviewer"];
        assert_eq!(a["model"], "haiku");
        assert_eq!(a["effort"], "medium");
        assert!(a["tools"].as_array().unwrap().iter().any(|t| t == "StructuredOutput"), "achado 3 da Fase 0");
    }

    #[test]
    fn workers_sem_plugins() {
        let s = json!({"enabledPlugins": {"sentry@x": true, "postman@x": false}});
        assert_eq!(plugins_off(&s), json!({"sentry@x": false, "postman@x": false}));
        assert_eq!(plugins_off(&Value::Null), json!({}));
        let g: Value = serde_json::from_str(&guard_settings_with(true, &s)).unwrap();
        assert_eq!(g["enabledPlugins"]["sentry@x"], false);
        assert_eq!(g["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn mcp_do_atlassian_so_com_type_e_url() {
        let ok = json!({"mcpServers": {"atlassian": {"type": "http", "url": "https://mcp.atlassian.com/v1/mcp"}, "outro": {}}});
        assert_eq!(atlassian_mcp(&ok), Some(json!({"type": "http", "url": "https://mcp.atlassian.com/v1/mcp"})));
        let com_header = json!({"mcpServers": {"atlassian": {"type": "http", "url": "u", "headers": {"Authorization": "x"}}}});
        assert_eq!(atlassian_mcp(&com_header), None, "header pode ter segredo: não vai para a linha de comando");
        assert_eq!(atlassian_mcp(&json!({"mcpServers": {}})), None);
        assert_eq!(atlassian_mcp(&Value::Null), None);
    }

    #[tokio::test]
    async fn valida_achados_contra_o_commit() {
        let dir = std::env::temp_dir().join(format!("prr-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            let o = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_owned()
        };
        g(&["init", "-q"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base"]);
        let base = g(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("a.php"), "1\n2\n3\n").unwrap();
        g(&["add", "a.php"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "pr"]);
        let src = g(&["rev-parse", "HEAD"]);

        let mut f = vec![
            json!({"file": "a.php", "line": 2}),
            json!({"file": "a.php", "line": 99}),
            json!({"file": "nao/existe.php", "line": 1}),
        ];
        validate_findings(&dir, &base, &src, &mut f).await.unwrap();
        assert_eq!((f[0]["validation"].as_str(), f[0]["in_diff"].as_bool()), (Some("ok"), Some(true)));
        assert_eq!(f[1]["reject_reason"], "linha além do fim do arquivo");
        assert_eq!(f[2]["reject_reason"], "arquivo não existe no commit do PR");
        assert_eq!(f[2]["id"], "F3");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
