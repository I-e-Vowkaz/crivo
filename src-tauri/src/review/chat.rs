//! Chat com o Claude sobre um review: perguntas sobre os achados ou sobre outros bugs da task.
//!
//! - Uma sessão do Claude Code por execução (`--session-id` na primeira pergunta, `--resume`
//!   nas seguintes), então a conversa tem memória.
//! - O código é o do **commit revisado**, num worktree próprio do chat (`<run_dir>/chat-wt`),
//!   criado na primeira pergunta e removido quando o app abre de novo.
//! - Só leitura: mesmas ferramentas e hooks de proteção do review.
//! - O outro lado da task (front ↔ back), quando há clone dele, vai junto em `<run_dir>/chat-related`
//!   (`related.rs`), para o Claude conferir o que vem de lá.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use super::claude::{self, RunCtl, Session, Spec, Status};
use super::related::{self, Related};
use super::{READ_ONLY_TOOLS, git, repos_root, worker_config};
use crate::commands::AppState;
use crate::error::{AppError, AppResult};

const CHAT_TIMEOUT: Duration = Duration::from_secs(20 * 60);

fn key(run_id: &str) -> String {
    format!("chat:{run_id}")
}

#[derive(Serialize)]
pub struct ChatMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub cost_usd: Option<f64>,
    pub created_at: String,
}

pub fn messages(conn: &Connection, run_id: &str) -> AppResult<Vec<ChatMessage>> {
    let mut stmt =
        conn.prepare("SELECT id, role, content, cost_usd, created_at FROM review_chat WHERE run_id = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map([run_id], |r| {
            Ok(ChatMessage { id: r.get(0)?, role: r.get(1)?, content: r.get(2)?, cost_usd: r.get(3)?, created_at: r.get(4)? })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

struct RunInfo {
    slug: String,
    provider: String,
    owner: String,
    bb_id: i64,
    title: String,
    source_branch: String,
    dest_branch: String,
    source_sha: String,
    dest_sha: String,
    card_key: Option<String>,
    jira_site: Option<String>,
    run_dir: PathBuf,
    session: Option<String>,
    browser: Vec<String>,
}

fn load(conn: &Connection, run_id: &str) -> AppResult<RunInfo> {
    let mut info = conn
        .query_row(
            "SELECT coalesce(r.slug, rr.repo_slug), ifnull(p.bb_id, 0), coalesce(p.title, rr.branch),
                    coalesce(p.source_branch, rr.branch), coalesce(p.dest_branch, rr.base_branch), rr.source_sha,
                    rr.dest_sha, rr.card_key, rr.run_dir, rr.chat_session_id, rr.status,
                    ifnull(r.provider, 'local'), ifnull(r.workspace, '')
             FROM review_runs rr LEFT JOIN pull_requests p ON p.id = rr.pr_id LEFT JOIN repositories r ON r.id = p.repository_id
             WHERE rr.id = ?1",
            [run_id],
            |r| {
                let status: String = r.get(10)?;
                Ok((
                    RunInfo {
                        slug: r.get(0)?,
                        provider: r.get(11)?,
                        owner: r.get(12)?,
                        bb_id: r.get(1)?,
                        title: r.get(2)?,
                        source_branch: r.get(3)?,
                        dest_branch: r.get(4)?,
                        source_sha: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        dest_sha: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        card_key: r.get(7)?,
                        jira_site: None,
                        run_dir: PathBuf::from(r.get::<_, String>(8)?),
                        session: r.get(9)?,
                        browser: vec![],
                    },
                    status,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid("review não encontrado".into()))
        .and_then(|(info, status)| {
            if status == "running" {
                Err(AppError::Invalid("espere o review terminar para conversar sobre ele".into()))
            } else {
                Ok(info)
            }
        })?;
    let mut stmt = conn.prepare(
        "SELECT status, ifnull(title, ''), ifnull(observed, '') FROM browser_tests WHERE run_id = ?1 ORDER BY seq",
    )?;
    info.browser = stmt
        .query_map([run_id], |r| Ok(format!("- [{}] {}: {}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        .collect::<Result<_, _>>()?;
    info.jira_site = super::jira_site(conn)?;
    if info.jira_site.is_none() { info.card_key = None; }
    Ok(info)
}

fn first_prompt(info: &RunInfo, wt: &Path, related: Option<&Related>, question: &str) -> String {
    let card = match &info.card_key {
        Some(k) => format!(
            "CARD_KEY: {k} ({}; se falhar, use o CARD_FILE)",
            super::card_read_hint(info.jira_site.as_deref())
        ),
        None => "CARD_KEY: (nenhuma; use o CARD_FILE)".into(),
    };
    let browser = if info.browser.is_empty() { "(não rodou)".to_owned() } else { info.browser.join("\n") };
    // review de branch local (Minhas branches): não há PR nem descrição
    let (what, target) = if info.bb_id == 0 {
        (
            "uma branch local",
            format!("BRANCH local: {} em {}, ainda sem PR (comparada com origin/{})", info.source_branch, info.slug, info.dest_branch),
        )
    } else {
        ("um PR", format!("PR: {}#{} — {} ({} → {})", info.slug, info.bb_id, info.title, info.source_branch, info.dest_branch))
    };
    let related = related.map(|r| format!("{}\n", r.prompt_line(&info.source_branch))).unwrap_or_default();
    format!(
        "Você responde perguntas do usuário sobre o review de código de {what}. Você só lê: não altera arquivos, \
não faz commit nem checkout.\n\n\
{target}\n\
WORKTREE (diretório atual): {wt}, no commit revisado {sha}\n\
BASE: {base} (o diff do PR é `git diff BASE...HEAD`)\n\
{card}\n\
CARD_FILE: {card_file}\n\
FINDINGS_FILE: {findings} (achados do review: id F1, F2…, severidade, validação do app e decisão do Opus em audit_*)\n\
{related}\
TESTES NO NAVEGADOR:\n{browser}\n\n\
Como responder:\n\
- Em português, direto, citando `arquivo:linha` do que você leu.\n\
- Sobre um achado: confira no código e diga se ele procede, com a evidência.\n\
- Se pedirem outros bugs da task: investigue o diff e o card com os critérios do review (só o que o PR mudou ou quebrou, \
fluxo normal do card, sem edge cases exceto segurança) e diga o que verificou e o que não verificou.\n\
- Só sugira correção de código se o usuário pedir.\n\n\
Pergunta: {question}",
        wt = wt.display(),
        sha = info.source_sha,
        base = info.dest_sha,
        card_file = info.run_dir.join("card.md").display(),
        findings = info.run_dir.join("findings.json").display(),
    )
}

#[derive(Clone, Serialize)]
struct ChatEvent<'a> {
    run_id: &'a str,
    line: Option<&'a str>,
    busy: bool,
}

/// Registra a pergunta e responde em segundo plano. Uma pergunta por vez em cada review.
pub fn ask(app: AppHandle, run_id: String, question: String) -> AppResult<()> {
    let question = question.trim().to_owned();
    if question.is_empty() {
        return Err(AppError::Invalid("escreva a pergunta".into()));
    }
    if let Some(l) = super::active_limit(&app) {
        return Err(AppError::Invalid(format!(
            "limite de uso do Claude atingido ({}); a pergunta fica para quando liberar",
            l.limit.reset_text.unwrap_or(l.limit.message)
        )));
    }
    let state = app.state::<AppState>();
    let (info, root) = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        (load(&conn, &run_id)?, repos_root(&conn)?)
    };
    if info.source_sha.is_empty() {
        return Err(AppError::Invalid("este review não tem o commit registrado; rode o review de novo".into()));
    }
    let ctl = Arc::new(RunCtl::default());
    {
        let mut reg = state.reviews.lock().unwrap();
        if reg.contains_key(&key(&run_id)) {
            return Err(AppError::Invalid("o Claude ainda está respondendo a pergunta anterior".into()));
        }
        reg.insert(key(&run_id), ctl.clone());
    }
    {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        conn.execute("INSERT INTO review_chat (run_id, role, content) VALUES (?1, 'user', ?2)", params![run_id, question])?;
    }
    let _ = app.emit("chat-update", ChatEvent { run_id: &run_id, line: None, busy: true });

    tauri::async_runtime::spawn(async move {
        let result = answer(&app, &run_id, &info, &root, &question, ctl.clone()).await;
        let st = app.state::<AppState>();
        st.reviews.lock().unwrap().remove(&key(&run_id));
        let (role, content, cost) = match result {
            Ok((text, cost)) => ("assistant", text, Some(cost)),
            Err(_) if ctl.is_cancelled() => ("error", "cancelado".to_owned(), None),
            Err(e) => ("error", e.to_string(), None),
        };
        if let Ok(conn) = st.db.lock() {
            let _ = conn.execute(
                "INSERT INTO review_chat (run_id, role, content, cost_usd) VALUES (?1, ?2, ?3, ?4)",
                params![run_id, role, content, cost],
            );
        }
        let _ = app.emit("chat-update", ChatEvent { run_id: &run_id, line: None, busy: false });
    });
    Ok(())
}

pub fn cancel(state: &AppState, run_id: &str) -> AppResult<()> {
    match state.reviews.lock().unwrap().get(&key(run_id)).cloned() {
        Some(c) => {
            c.cancel();
            Ok(())
        }
        None => Err(AppError::Invalid("nenhuma pergunta em andamento".into())),
    }
}

async fn answer(
    app: &AppHandle,
    run_id: &str,
    info: &RunInfo,
    root: &Path,
    question: &str,
    ctl: Arc<RunCtl>,
) -> AppResult<(String, f64)> {
    let log = |line: &str| {
        let _ = app.emit("chat-update", ChatEvent { run_id, line: Some(line), busy: true });
    };
    // worktree do chat no commit revisado (o do review já foi removido)
    let repo = if info.provider == "github" { crate::github::clone_path(root, &info.owner, &info.slug) } else { root.join(&info.slug) };
    let wt = info.run_dir.join("chat-wt");
    if !wt.join(".git").exists() {
        log("preparando o código do commit revisado");
        git::run(&repo, &["fetch", "-q", "origin", &info.source_branch]).await.ok();
        git::run(&repo, &["worktree", "add", "--detach", &wt.display().to_string(), &info.source_sha]).await?;
    }
    // o outro lado da task (front ↔ back); a cada pergunta, porque o app remove ao abrir
    let rel = related::prepare(root, &info.slug, &info.source_branch, &info.dest_branch, &info.run_dir.join("chat-related")).await;
    if let Some(r) = &rel {
        log(&format!("lendo também {} ({})", r.slug, r.reference));
    }

    let (session, prompt) = match &info.session {
        Some(id) => (Session::Resume(id.clone()), question.to_owned()),
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let st = app.state::<AppState>();
            let conn = st.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
            conn.execute("UPDATE review_runs SET chat_session_id = ?1 WHERE id = ?2", params![id, run_id])?;
            (Session::New(id), first_prompt(info, &wt, rel.as_ref(), question))
        }
    };
    let mut allowed = READ_ONLY_TOOLS.replace("Agent ", "");
    if info.card_key.is_some() { allowed.push_str(" ToolSearch mcp__atlassian__getJiraIssue"); }
    let mut opts: Vec<String> = vec![
        "--model".into(), "sonnet".into(), "--effort".into(), "medium".into(),
        "--add-dir".into(), info.run_dir.display().to_string(),
        "--allowedTools".into(), allowed,
        "--disallowedTools".into(), "Edit Write NotebookEdit Agent mcp__claude-in-chrome__*".into(),
        "--max-turns".into(), "40".into(), "--max-budget-usd".into(), "2".into(),
    ];
    if let Some(r) = &rel {
        opts.extend(["--add-dir".into(), r.dir.display().to_string()]);
    }
    // sem cache de 5 min: a próxima pergunta pode vir bem depois e reaproveita o de 1 h
    opts.extend(worker_config(false, info.card_key.is_some()));
    opts.extend(super::writing_style_opts());
    let spec = Spec {
        session,
        name: "chat".into(),
        opts,
        prompt,
        cwd: wt.clone(),
        log_dir: info.run_dir.join("chat-logs"),
        env_extra: vec![],
        timeout: CHAT_TIMEOUT,
    };
    let out = claude::run(spec, ctl, |l| log(&l)).await?;
    if let Some(l) = &out.limit {
        super::set_limit(app, l.clone());
        return Err(AppError::Invalid(format!(
            "limite de uso do Claude atingido ({}). Pergunte de novo quando liberar.",
            l.reset_text.clone().unwrap_or_else(|| l.message.clone())
        )));
    }
    let cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);
    let text = out.result.as_ref().and_then(|r| r.get("result")).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    match out.status {
        Status::Completed if !text.is_empty() => Ok((text, cost)),
        Status::Cancelled => Err(AppError::Invalid("cancelado".into())),
        Status::TimedOut => Err(AppError::Invalid("a resposta passou do tempo limite (20 min)".into())),
        _ => Err(AppError::Invalid(format!(
            "o Claude não respondeu (código {:?}). {}",
            out.exit_code,
            if text.is_empty() { out.stderr_tail } else { text }
        ))),
    }
}

/// Ao abrir o app: remove os worktrees de chat (e do projeto relacionado) de execuções anteriores.
pub async fn cleanup_worktrees(runs_dir: PathBuf, root: PathBuf) {
    let Ok(entries) = std::fs::read_dir(&runs_dir) else { return };
    let mut repos = std::collections::HashSet::new();
    for e in entries.flatten() {
        for wt in [e.path().join("chat-wt"), e.path().join("chat-related")] {
            if let Some(repo) = related::owner_repo(&wt) {
                let _ = git::raw(&repo, &["worktree", "remove", "--force", &wt.display().to_string()]).await;
                repos.insert(repo);
            }
        }
    }
    for r in repos.into_iter().chain(std::iter::once(root)) {
        if r.join(".git").exists() {
            let _ = git::raw(&r, &["worktree", "prune"]).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primeira_pergunta_leva_o_contexto() {
        let info = RunInfo {
            slug: "acme-back".into(),
            provider: "bitbucket".into(),
            owner: "acme".into(),
            bb_id: 2397,
            title: "PROJ-1503".into(),
            source_branch: "PROJ-1503".into(),
            dest_branch: "dev".into(),
            source_sha: "6e09391c2031aaaa".into(),
            dest_sha: "c8be00e3f953bbbb".into(),
            card_key: Some("PROJ-1503".into()),
            jira_site: Some("acme.atlassian.net".into()),
            run_dir: PathBuf::from("/runs/r1"),
            session: None,
            browser: vec!["- [aprovado] Login: ok".into()],
        };
        let p = first_prompt(&info, Path::new("/runs/r1/chat-wt"), None, "o F1 procede?");
        assert!(!p.contains("PROJETO_RELACIONADO"), "{p}");
        let rel = Related {
            slug: "acme-front".into(),
            dir: PathBuf::from("/runs/r1/chat-related"),
            sha: "abcdef1234567890".into(),
            same_branch: true,
            reference: "origin/PROJ-1503".into(),
        };
        let with = first_prompt(&info, Path::new("/runs/r1/chat-wt"), Some(&rel), "oi");
        assert!(with.contains("PROJETO_RELACIONADO: acme-front em /runs/r1/chat-related, na branch origin/PROJ-1503"), "{with}");
        for needle in [
            "acme-back#2397",
            "/runs/r1/chat-wt",
            "6e09391c2031aaaa",
            "git diff BASE...HEAD",
            "CARD_KEY: PROJ-1503",
            "cloudId \"acme.atlassian.net\"",
            "/runs/r1/findings.json",
            "[aprovado] Login: ok",
            "Pergunta: o F1 procede?",
        ] {
            assert!(p.contains(needle), "falta {needle:?} no prompt");
        }
        // review de branch local: sem PR #0
        let local = RunInfo { bb_id: 0, title: "PROJ-9".into(), source_branch: "PROJ-9".into(), jira_site: None, ..info };
        let p = first_prompt(&local, Path::new("/runs/r1/chat-wt"), None, "oi");
        assert!(p.contains("getAccessibleAtlassianResources"), "sem site, o agente acha pelo MCP: {p}");
        assert!(p.contains("BRANCH local: PROJ-9 em acme-back, ainda sem PR (comparada com origin/dev)"), "{p}");
        assert!(!p.contains("#0"), "{p}");
    }
}
