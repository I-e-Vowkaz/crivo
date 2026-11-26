//! `pr-review-orchestrator comments <branch> [ação]`: a tela "Meus PRs" pelo terminal. Lista
//! os comentários abertos dos PRs que eu abri e, quando pedido, analisa, conversa, aprova e
//! faz commit e push, pelo mesmo `fix/` do app. Tudo fica no banco do app: o que se faz aqui
//! aparece no painel, e vice-versa.
//!
//! Uma trava entre processos (`cli.pid` na pasta da correção, `app.pid` na pasta de dados)
//! impede o app e o terminal de rodarem o Claude na mesma correção.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::{STOP, cli_host, install_signals, set_assets_from_exe, sync};
use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::fix::{self, CliLock, Owner};
use crate::review::{CliHost, Host, Running};

const USAGE: &str = "\
uso: pr-review-orchestrator comments <branch> [ação] [opções]

Os comentários abertos dos seus PRs da branch, como na tela Meus PRs do app. Sem ação, busca
no provedor da PR os comentários novos e lista todos, sem rodar o Claude.

ações:
  analyze <C1 C3 …|all>   o Sonnet confere os comentários no código e propõe a correção
  ask <C3> <mensagem>     conversa sobre um comentário (mesma sessão do Claude)
  approve <C3 …>          grava a sugestão no worktree da correção
  commit                  commit pelo committer e push na branch do PR (sem force)
  discard                 descarta a correção e o que foi aprovado sem commit

opções:
  --repo <slug|owner/repo>   só neste repositório (obrigatório nas ações se houver PR seu
                             no front e no back da mesma branch)
  --sync                     atualiza os PRs configurados antes de procurar
  --output-format text|json  formato no stdout (padrão: text)
  --verbose                  também escreve o log detalhado no stderr
  -h, --help                 mostra esta ajuda

Comentários de review são publicados apenas pela ação explícita no painel do app.

saída: 0 ok, 1 a etapa falhou, 2 erro de uso, PR não encontrado ou estado que não aceita a
ação, 130 se foi cancelado (Ctrl+C ou SIGTERM).";

#[derive(Debug, PartialEq)]
enum Items {
    All,
    Seqs(Vec<i64>),
}

#[derive(Debug, PartialEq)]
enum Action {
    List,
    Analyze(Items),
    Ask(i64, String),
    Approve(Vec<i64>),
    Commit,
    Discard,
}

#[derive(Debug)]
struct Args {
    branch: String,
    repo: Option<String>,
    sync: bool,
    json: bool,
    verbose: bool,
    action: Action,
}

/// `C3`, `c3` ou `3`.
fn parse_item(s: &str) -> Result<i64, String> {
    s.trim()
        .trim_start_matches(['C', 'c'])
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("comentário inválido: {s} (use C3, c3 ou 3)"))
}

fn parse(args: &[String]) -> Result<Args, String> {
    let (mut repo, mut sync, mut json, mut verbose) = (None, false, false, false);
    let mut pos: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} precisa de um valor"));
        match arg.as_str() {
            "--repo" => repo = Some(value(arg)?),
            "--sync" => sync = true,
            "--verbose" => verbose = true,
            "--output-format" => match value(arg)?.as_str() {
                "text" => json = false,
                "json" => json = true,
                other => return Err(format!("--output-format aceita text ou json, não {other}")),
            },
            other if other.starts_with("--") => return Err(format!("opção desconhecida: {other}")),
            other => pos.push(other.to_owned()),
        }
    }
    let mut pos = pos.into_iter();
    let branch = pos.next().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()).ok_or("falta a branch: comments <branch>")?;
    let rest: Vec<String> = pos.collect();
    let action = match rest.first().map(String::as_str) {
        None => Action::List,
        Some("analyze") => match &rest[1..] {
            [] => return Err("analyze precisa dos comentários (C1 C3 …) ou de all".into()),
            [all] if all == "all" => Action::Analyze(Items::All),
            items => Action::Analyze(Items::Seqs(items.iter().map(|i| parse_item(i)).collect::<Result<_, _>>()?)),
        },
        Some("ask") => {
            let item = rest.get(1).ok_or("ask precisa do comentário e da mensagem: ask C3 <mensagem>")?;
            let text = rest[2..].join(" ").trim().to_owned();
            if text.is_empty() {
                return Err("ask precisa da mensagem: ask C3 <mensagem>".into());
            }
            Action::Ask(parse_item(item)?, text)
        }
        Some("approve") => match &rest[1..] {
            [] => return Err("approve precisa dos comentários: approve C3 …".into()),
            items => Action::Approve(items.iter().map(|i| parse_item(i)).collect::<Result<_, _>>()?),
        },
        Some(a @ ("commit" | "discard")) => {
            if rest.len() > 1 {
                return Err(format!("{a} não recebe argumentos"));
            }
            if a == "commit" { Action::Commit } else { Action::Discard }
        }
        Some(other) => return Err(format!("ação desconhecida: {other}")),
    };
    Ok(Args { branch, repo, sync, json, verbose, action })
}

// ---------------- os PRs ----------------

#[derive(Debug, Clone)]
struct MyPr {
    id: i64,
    slug: String,
    /// Como está no PR (a busca não diferencia maiúsculas).
    branch: String,
    label: String,
    title: String,
    url: String,
}

/// PRs abertos da branch que eu abri (os de outras pessoas são do `-p`).
fn my_open_prs(conn: &Connection, branch: &str, repo: Option<&str>) -> AppResult<Vec<MyPr>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, r.slug, p.source_branch, p.bb_id, p.title, p.url, r.provider, r.workspace
         FROM pull_requests p JOIN repositories r ON r.id = p.repository_id
         WHERE p.state = 'OPEN' AND p.category != 'review_requested' AND lower(p.source_branch) = lower(?1)
           AND (?2 IS NULL OR r.slug = ?2 OR (r.provider = 'github' AND r.workspace || '/' || r.slug = ?2))
         ORDER BY r.provider, r.workspace, r.slug",
    )?;
    let rows = stmt.query_map(params![branch, repo], |r| {
        let slug: String = r.get(1)?;
        let provider: String = r.get(6)?;
        let owner: String = r.get(7)?;
        Ok(MyPr {
            id: r.get(0)?,
            label: if provider == "github" { format!("{owner}/{slug}#{}", r.get::<_, i64>(3)?) } else { format!("{slug}#{}", r.get::<_, i64>(3)?) },
            slug,
            branch: r.get(2)?,
            title: r.get(4)?,
            url: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn others_open_prs_count(conn: &Connection, branch: &str, repo: Option<&str>) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM pull_requests p JOIN repositories r ON r.id = p.repository_id
         WHERE p.state = 'OPEN' AND p.category = 'review_requested' AND lower(p.source_branch) = lower(?1)
           AND (?2 IS NULL OR r.slug = ?2 OR (r.provider = 'github' AND r.workspace || '/' || r.slug = ?2))",
        params![branch, repo],
        |r| r.get(0),
    )?)
}

/// Os `C1, C2…` são por PR: as ações precisam de um PR só.
fn check_single(prs: &[MyPr], action: &Action) -> Result<(), String> {
    if prs.len() <= 1 || *action == Action::List {
        return Ok(());
    }
    let slugs: Vec<&str> = prs.iter().map(|p| p.slug.as_str()).collect();
    Err(format!("a branch {} tem PR seu em {}: use --repo", prs[0].branch, slugs.join(" e ")))
}

async fn find_prs(state: &AppState, a: &Args) -> Result<Vec<MyPr>, i32> {
    let query = |f: &dyn Fn(&Connection) -> AppResult<Vec<MyPr>>| -> Result<Vec<MyPr>, i32> {
        let conn = state.db.lock().map_err(|_| {
            eprintln!("banco indisponível");
            2
        })?;
        f(&conn).map_err(|e| {
            eprintln!("{e}");
            2
        })
    };
    let find = |c: &Connection| my_open_prs(c, &a.branch, a.repo.as_deref());
    let mut prs = if a.sync { vec![] } else { query(&find)? };
    if prs.is_empty() {
        sync(state).await;
        prs = query(&find)?;
    }
    if prs.is_empty() {
        let scope = a.repo.as_deref().map(|r| format!(" em {r}")).unwrap_or_default();
        eprintln!("nenhum PR aberto seu na branch {}{scope}", a.branch);
        let others = state.db.lock().ok().and_then(|c| others_open_prs_count(&c, &a.branch, a.repo.as_deref()).ok()).unwrap_or(0);
        if others > 0 {
            eprintln!("o PR dessa branch é de outra pessoa: para revisar, use pr-review-orchestrator -p {}", a.branch);
        }
        return Err(2);
    }
    Ok(prs)
}

// ---------------- execução ----------------

pub fn run(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return 0;
    }
    let a = match parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return 2;
        }
    };
    set_assets_from_exe();
    let cli = match cli_host(a.verbose) {
        Ok(c) => c,
        Err(code) => return code,
    };
    install_signals();
    tauri::async_runtime::block_on(main(cli, a))
}

fn db<T>(state: &AppState, f: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
    let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
    f(&conn)
}

fn data_dir(state: &AppState) -> std::path::PathBuf {
    state.db_path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Espera a etapa e as voltas da fila que ela disparar.
async fn wait_idle(state: &AppState, fix_id: &str, task: Option<Running>) {
    if let Some(t) = task {
        let _ = t.await;
    }
    while fix::running_here(state, fix_id) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// O que um PR deu: a correção (como o painel a vê), aviso e erro.
struct Outcome {
    pr: MyPr,
    fix: Option<Value>,
    warning: Option<String>,
    error: Option<String>,
    /// Este terminal buscou os comentários (um `error` na correção é desta execução).
    ran: bool,
}

/// Código de saída da listagem de um PR.
fn list_code(o: &Outcome) -> i32 {
    let fix = o.fix.as_ref();
    let failed = fix.is_some_and(|f| s(f, "status") == "failed");
    let errored = o.ran && fix.is_some_and(|f| !s(f, "error").is_empty());
    if o.error.is_some() || failed || errored { 1 } else { 0 }
}

/// `approve` só para itens com sugestão esperando aprovação; os outros saem com 2 antes de gravar.
fn approvable(conn: &Connection, fix_id: &str, seqs: &[i64]) -> Result<(), String> {
    for seq in seqs {
        let (status, error): (String, Option<String>) = conn
            .query_row("SELECT status, error FROM fix_comments WHERE fix_id = ?1 AND seq = ?2", params![fix_id, seq], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|_| format!("C{seq} não existe neste PR"))?;
        if status != "procede" {
            return Err(format!(
                "C{seq} não tem sugestão esperando aprovação ({})",
                item_status(&json!({"status": status, "error": error}))
            ));
        }
    }
    Ok(())
}

fn fix_value(state: &AppState, fix_id: &str) -> AppResult<Value> {
    let v = db(state, |c| fix::view(c, fix_id, false))?;
    Ok(serde_json::to_value(v)?)
}

/// Dono da última correção do PR, já recuperando a que ficou travada sem dono.
fn latest(state: &AppState, pr_id: i64) -> AppResult<Option<(String, String, Owner)>> {
    db(state, |c| {
        let Some(id) = fix::latest_pr_id(c, pr_id)? else { return Ok(None) };
        let mut owner = fix::owner(c, &data_dir(state), &id)?;
        if owner == Owner::Stale {
            fix::recover(c, &id)?;
            owner = Owner::Free;
        }
        let status: String = c.query_row("SELECT status FROM fix_sessions WHERE id = ?1", [&id], |r| r.get(0))?;
        Ok(Some((id, status, owner)))
    })
}

fn busy_text(owner: Owner) -> &'static str {
    match owner {
        Owner::App => "o app está processando esta correção",
        _ => "outro terminal está processando esta correção",
    }
}

/// Listagem: começa a correção (ou busca os comentários novos) e devolve o estado.
async fn list_one(host: &Host, state: &AppState, pr: &MyPr) -> AppResult<Outcome> {
    let mut out = Outcome { pr: pr.clone(), fix: None, warning: None, error: None, ran: false };
    let found = latest(state, pr.id)?;
    let fix_id = match &found {
        Some((id, _, owner @ (Owner::App | Owner::Terminal))) => {
            out.warning = Some(format!("{}; não busquei comentários novos", busy_text(*owner)));
            id.clone()
        }
        Some((id, status, _)) if matches!(status.as_str(), "ready" | "analyzing") => {
            let _lock = CliLock::take(&data_dir(state).join("fixes").join(id))?;
            out.ran = true;
            match fix::refresh(host.clone(), id.clone()) {
                Ok(task) => wait_idle(state, id, Some(task)).await,
                Err(e) => out.error = Some(e.to_string()),
            }
            id.clone()
        }
        _ => match {
            out.ran = true;
            fix::start(host.clone(), pr.id, false)
        } {
            Ok((id, task)) => {
                if let Host::Cli(cli) = host {
                    cli.labels.lock().unwrap().insert(id.clone(), pr.label.clone());
                }
                let _lock = CliLock::take(&data_dir(state).join("fixes").join(&id))?;
                wait_idle(state, &id, Some(task)).await;
                id
            }
            Err(e) => {
                out.error = Some(e.to_string());
                match found {
                    Some((id, ..)) => id,
                    None => return Ok(out),
                }
            }
        },
    };
    out.fix = Some(fix_value(state, &fix_id)?);
    Ok(out)
}

async fn main(cli: Arc<CliHost>, a: Args) -> i32 {
    let host = Host::Cli(cli.clone());
    let state = &cli.state;
    let prs = match find_prs(state, &a).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    if let Err(e) = check_single(&prs, &a.action) {
        eprintln!("{e}");
        return 2;
    }
    // Ctrl+C ou SIGTERM: cancela o Claude (o fix grava o estado) e espera
    let watcher_cli = cli.clone();
    let watcher = tauri::async_runtime::spawn(async move {
        loop {
            if STOP.load(Ordering::SeqCst) {
                eprintln!("cancelando…");
                for ctl in watcher_cli.state.reviews.lock().unwrap().values() {
                    ctl.cancel();
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    let mut outcomes = Vec::new();
    let mut code = 0;
    match &a.action {
        Action::List => {
            for p in &prs {
                label_fix(&cli, state, p);
                match list_one(&host, state, p).await {
                    Ok(o) => {
                        code = code.max(list_code(&o));
                        outcomes.push(o);
                    }
                    Err(e) => {
                        eprintln!("[{}] {e}", p.label);
                        code = 1;
                    }
                }
            }
        }
        action => {
            let p = &prs[0];
            label_fix(&cli, state, p);
            let (o, c, only) = act(&host, state, p, action).await;
            code = c;
            if let Some(o) = o {
                watcher.abort();
                print_outcomes(&[o], a.json, only.as_deref(), matches!(action, Action::Ask(..)));
            }
            return if STOP.load(Ordering::SeqCst) { 130 } else { code };
        }
    }
    watcher.abort();
    print_outcomes(&outcomes, a.json, None, false);
    if STOP.load(Ordering::SeqCst) { 130 } else { code }
}

/// `(seq, id)` dos itens pedidos. `All`: os que esperam análise (não analisados ou que falharam).
fn comment_ids(conn: &Connection, fix_id: &str, items: &Items) -> Result<Vec<(i64, i64)>, String> {
    let rows: Vec<(i64, i64, String)> = conn
        .prepare("SELECT seq, id, status FROM fix_comments WHERE fix_id = ?1 ORDER BY seq")
        .and_then(|mut st| st.query_map([fix_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
        .map_err(|e| e.to_string())?;
    match items {
        Items::All => {
            let out: Vec<(i64, i64)> =
                rows.iter().filter(|r| matches!(r.2.as_str(), "new" | "failed")).map(|r| (r.0, r.1)).collect();
            if out.is_empty() { Err("nenhum comentário esperando análise".into()) } else { Ok(out) }
        }
        Items::Seqs(seqs) => seqs
            .iter()
            .map(|seq| rows.iter().find(|r| r.0 == *seq).map(|r| (r.0, r.1)).ok_or_else(|| format!("C{seq} não existe neste PR")))
            .collect(),
    }
}

/// A correção aberta do PR, livre para uma ação, já travada para este terminal.
fn open_fix(state: &AppState, pr: &MyPr) -> Result<(String, CliLock), String> {
    let (id, status, owner) = latest(state, pr.id)
        .map_err(|e| e.to_string())?
        .ok_or("este PR ainda não tem correção: rode `comments <branch>` antes")?;
    if matches!(owner, Owner::App | Owner::Terminal) {
        return Err(format!("{}; espere terminar", busy_text(owner)));
    }
    if !matches!(status.as_str(), "ready" | "analyzing") {
        return Err(format!(
            "a última correção deste PR está {}: rode `comments <branch>` para começar outra",
            session_status(&status)
        ));
    }
    let lock = CliLock::take(&data_dir(state).join("fixes").join(&id)).map_err(|e| e.to_string())?;
    Ok((id, lock))
}

/// Uma ação sobre a correção do PR. Devolve o que imprimir, o código de saída e, quando a
/// ação é sobre alguns itens, quais mostrar.
async fn act(host: &Host, state: &AppState, pr: &MyPr, action: &Action) -> (Option<Outcome>, i32, Option<Vec<i64>>) {
    let fail = |msg: String| {
        eprintln!("{msg}");
        (None, 2, None)
    };
    let (fix_id, _lock) = match open_fix(state, pr) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let ids = |items: &Items| db(state, |c| Ok(comment_ids(c, &fix_id, items))).map_err(|e| e.to_string()).and_then(|r| r);
    let mut code = 0;
    let only: Option<Vec<i64>> = match action {
        Action::Analyze(items) => {
            let pairs = match ids(items) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            match fix::analyze(host.clone(), pairs.iter().map(|p| p.1).collect()) {
                Ok(task) => wait_idle(state, &fix_id, task).await,
                Err(e) => return fail(e.to_string()),
            }
            Some(pairs.iter().map(|p| p.0).collect())
        }
        Action::Ask(seq, text) => {
            let pairs = match ids(&Items::Seqs(vec![*seq])) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            match fix::ask(host.clone(), pairs[0].1, text.clone()) {
                Ok(task) => wait_idle(state, &fix_id, Some(task)).await,
                Err(e) => return fail(e.to_string()),
            }
            Some(vec![*seq])
        }
        Action::Approve(seqs) => {
            let pairs = match ids(&Items::Seqs(seqs.clone())).and_then(|p| {
                db(state, |c| Ok(approvable(c, &fix_id, seqs))).map_err(|e| e.to_string()).and_then(|r| r).map(|_| p)
            }) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            for (seq, id) in &pairs {
                match fix::approve(host, *id) {
                    Ok(files) => eprintln!("C{seq}: aplicado ({})", files.join(", ")),
                    Err(e) => {
                        eprintln!("C{seq}: {e}");
                        code = 1;
                        break;
                    }
                }
            }
            Some(seqs.clone())
        }
        Action::Commit => {
            match fix::commit(host.clone(), fix_id.clone()) {
                Ok(task) => wait_idle(state, &fix_id, Some(task)).await,
                Err(e) => return fail(e.to_string()),
            }
            // no PR, o commit já vem com o push; sem push, o erro fica na correção
            let pushed = db(state, |c| {
                Ok(c.query_row("SELECT status = 'pushed' FROM fix_sessions WHERE id = ?1", [&fix_id], |r| r.get::<_, bool>(0))?)
            });
            if !pushed.unwrap_or(false) {
                code = 1;
            }
            Some(vec![])
        }
        Action::Discard => {
            if let Err(e) = fix::discard(host.clone(), fix_id.clone()).await {
                return fail(e.to_string());
            }
            Some(vec![])
        }
        Action::List => return fail("listar não é uma ação".into()),
    };
    let fix = match fix_value(state, &fix_id) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    if let (Some(seqs), Some(items)) = (&only, fix["comments"].as_array()) {
        let mine = items.iter().filter(|c| c["seq"].as_i64().is_some_and(|s| seqs.contains(&s)));
        let failed = match action {
            Action::Analyze(_) => mine.clone().any(|c| s(c, "status") == "failed"),
            Action::Ask(..) => mine.clone().any(|c| c["messages"].as_array().and_then(|m| m.last()).is_some_and(|m| s(m, "role") == "error")),
            _ => false,
        };
        if failed {
            code = 1;
        }
    }
    (Some(Outcome { pr: pr.clone(), fix: Some(fix), warning: None, error: None, ran: true }), code, only)
}

/// O stderr do fix usa o `fix_id` como chave: põe o rótulo do PR na correção atual dele.
fn label_fix(cli: &CliHost, state: &AppState, pr: &MyPr) {
    if let Ok(Some(id)) = db(state, |c| fix::latest_pr_id(c, pr.id)) {
        cli.labels.lock().unwrap().insert(id, pr.label.clone());
    }
}

fn print_outcomes(outcomes: &[Outcome], as_json: bool, only: Option<&[i64]>, chat: bool) {
    if as_json {
        let prs: Vec<Value> = outcomes
            .iter()
            .map(|o| {
                json!({
                    "target": {"pr_id": o.pr.id, "repo_slug": o.pr.slug, "label": o.pr.label, "title": o.pr.title, "url": o.pr.url, "branch": o.pr.branch},
                    "warning": o.warning, "error": o.error, "fix": o.fix,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!({"prs": prs})).unwrap_or_default());
        return;
    }
    let blocks: Vec<String> = outcomes
        .iter()
        .map(|o| {
            let mut fix = o.fix.clone().unwrap_or(Value::Null);
            match (only, fix.as_object_mut()) {
                // commit e descarte: só o cabeçalho da correção
                (Some([]), Some(o)) => {
                    o.remove("comments");
                }
                (Some(seqs), Some(o)) => {
                    if let Some(items) = o.get_mut("comments").and_then(Value::as_array_mut) {
                        items.retain(|c| c["seq"].as_i64().is_some_and(|s| seqs.contains(&s)));
                    }
                }
                _ => {}
            }
            let mut text = report_with(&o.pr, &fix, o.warning.as_deref(), chat);
            if let Some(e) = &o.error {
                text = text.replacen('\n', &format!("\nErro: {}\n", one_line(e)), 1);
            }
            text
        })
        .collect();
    println!("{}", blocks.join("\n\n---\n\n"));
}

// ---------------- relatório ----------------

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn one_line(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn item_status(c: &Value) -> String {
    match s(c, "status") {
        "new" => "não analisado".into(),
        "queued" => "na fila".into(),
        "analyzing" => "analisando".into(),
        "procede" => "procede, sugestão esperando aprovação".into(),
        "nao_procede" => "não procede".into(),
        "resposta" => "resposta, sem mudança no código".into(),
        "applied" => "aplicado no worktree".into(),
        "failed" if !s(c, "error").is_empty() => format!("falhou: {}", one_line(s(c, "error"))),
        "failed" => "falhou".into(),
        other => other.into(),
    }
}

fn session_status(st: &str) -> &str {
    match st {
        "analyzing" => "analisando",
        "ready" => "pronta",
        "committing" => "fazendo commit",
        "committed" => "commit feito",
        "pushed" => "push feito",
        "failed" => "falhou",
        "cancelled" => "descartada",
        other => other,
    }
}

/// A sugestão em formato de diff: `-` o código atual, `+` o novo (vazio apaga).
fn diff_text(edits: &Value) -> String {
    let empty = vec![];
    edits
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|e| {
            let mut lines = vec!["```diff".to_owned(), format!("--- {}:{}", s(e, "file"), e["line"].as_i64().unwrap_or(0))];
            lines.extend(s(e, "existing_code").lines().map(|l| format!("-{l}")));
            lines.extend(s(e, "improved_code").lines().map(|l| format!("+{l}")));
            lines.push("```".into());
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn quote(body: &str) -> String {
    body.trim().lines().map(|l| if l.trim().is_empty() { ">".to_owned() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

fn item_text(c: &Value, chat: bool) -> String {
    let mut head = vec![format!("### C{}", c["seq"])];
    if !s(c, "author").is_empty() {
        head.push(s(c, "author").to_owned());
    }
    if !s(c, "path").is_empty() {
        head.push(match c["line"].as_i64() {
            Some(l) => format!("{}:{l}", s(c, "path")),
            None => s(c, "path").to_owned(),
        });
    }
    if c["outdated"].as_bool() == Some(true) {
        head.push("desatualizado".into());
    }
    head.push(item_status(c));
    let mut out = vec![head.join(" · "), quote(s(c, "body"))];
    let empty = vec![];
    let replies: Vec<String> = c["replies"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|r| format!("- {}: {}", s(r, "author"), one_line(s(r, "body"))))
        .collect();
    if !replies.is_empty() {
        out.push(format!("Respostas:\n{}", replies.join("\n")));
    }
    if !s(c, "explanation").is_empty() {
        out.push(format!("Análise: {}", s(c, "explanation").trim()));
    }
    let diff = diff_text(&c["edits"]);
    if !diff.is_empty() {
        out.push(diff);
    }
    if chat {
        // a última troca da conversa: a mensagem do usuário e a resposta (ou o erro)
        let msgs = c["messages"].as_array().cloned().unwrap_or_default();
        let last: Vec<String> = msgs
            .iter()
            .skip(msgs.len().saturating_sub(2))
            .map(|m| {
                let who = match s(m, "role") {
                    "user" => "Você",
                    "error" => "Erro",
                    _ => "Claude",
                };
                format!("{who}: {}", s(m, "content").trim())
            })
            .collect();
        if !last.is_empty() {
            out.push(format!("## Conversa\n\n{}", last.join("\n\n")));
        }
    }
    out.join("\n\n")
}

/// Bloco de um PR: cabeçalho, a correção e os comentários.
#[cfg(test)]
fn report(pr: &MyPr, fix: &Value, warning: Option<&str>) -> String {
    report_with(pr, fix, warning, false)
}

/// Com `chat`, cada item traz a última troca da conversa (saída do `ask`).
fn report_with(pr: &MyPr, fix: &Value, warning: Option<&str>, chat: bool) -> String {
    let card = s(fix, "card_key");
    let mut head = vec![format!("# {}{}", pr.label, if card.is_empty() { String::new() } else { format!(" · {card}") })];
    head.push(pr.title.clone());
    head.push(pr.url.clone());
    if fix.is_object() {
        let cost = fix["cost_usd"].as_f64().unwrap_or(0.0);
        head.push(format!(
            "Correção {}: {} · US$ {cost:.2} · worktree em {}/wt",
            s(fix, "id"),
            session_status(s(fix, "status")),
            s(fix, "run_dir")
        ));
        if !s(fix, "error").is_empty() {
            head.push(format!("Erro: {}", one_line(s(fix, "error"))));
        }
    }
    if let Some(w) = warning {
        head.push(format!("Aviso: {w}"));
    }
    if !s(fix, "commit_sha").is_empty() {
        let sha: String = s(fix, "commit_sha").chars().take(9).collect();
        let pushed = if s(fix, "pushed_at").is_empty() { "sem push" } else { "push feito" };
        head.push(format!("Commit: {} ({sha}) · {pushed}", s(fix, "commit_subject")));
    }
    let mut out = vec![head.join("\n")];
    if fix.get("comments").is_none() {
        return out.join("\n\n");
    }
    let empty = vec![];
    let items = fix["comments"].as_array().unwrap_or(&empty);
    let new = items.iter().filter(|c| s(c, "status") == "new").count();
    let pending = items.iter().filter(|c| s(c, "status") == "procede").count();
    out.push(format!(
        "## Comentários ({}) · {new} não analisado(s) · {pending} com sugestão esperando aprovação",
        items.len()
    ));
    if items.is_empty() {
        out.push("Nenhum comentário aberto de outra pessoa neste PR.".into());
    }
    out.extend(items.iter().map(|c| item_text(c, chat)));
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn le_os_argumentos() {
        let a = parse(&args(&["PROJ-1", "ask", "c3", "por", "que?", "--repo", "r"])).unwrap();
        assert_eq!(a.action, Action::Ask(3, "por que?".into()));
        assert_eq!(a.repo.as_deref(), Some("r"));
        assert_eq!(parse(&args(&["PROJ-1", "analyze", "all"])).unwrap().action, Action::Analyze(Items::All));
        assert_eq!(parse(&args(&["PROJ-1", "approve", "C2", "4"])).unwrap().action, Action::Approve(vec![2, 4]));
        let a = parse(&args(&["--sync", "proj-1", "--output-format", "json"])).unwrap();
        assert_eq!((a.action, a.branch.as_str(), a.sync, a.json), (Action::List, "proj-1", true, true));
        assert_eq!(parse(&args(&["PROJ-1", "commit"])).unwrap().action, Action::Commit);
        assert_eq!(parse(&args(&["PROJ-1", "discard"])).unwrap().action, Action::Discard);
        assert!(parse(&args(&["PROJ-1", "ask", "C3"])).is_err()); // sem mensagem
        assert!(parse(&args(&["PROJ-1", "approve"])).is_err()); // sem item
        assert!(parse(&args(&["PROJ-1", "analyze", "Cx"])).is_err());
        assert!(parse(&args(&["PROJ-1", "merge"])).is_err());
        assert!(parse(&args(&["PROJ-1", "commit", "C1"])).is_err());
        assert!(parse(&args(&[])).is_err());
    }

    fn seed_prs(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "INSERT INTO repositories (id, workspace, slug) VALUES (1, 'w', 'front'), (2, 'w', 'back'), (3, 'w', 'outro');
             INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, reviewers_json, participants_json,
               source_branch, dest_branch, source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 10, 'f', 'u1', '{a}', '[]', '[]', 'PROJ-1', 'dev', 's', 'd', 'OPEN', 'x', 'x', 'authored'),
                    (2, 2, 20, 'b', 'u2', '{a}', '[]', '[]', 'PROJ-1', 'dev', 's', 'd', 'OPEN', 'x', 'x', 'authored'),
                    (3, 3, 30, 'o', 'u3', '{b}', '[]', '[]', 'PROJ-1', 'dev', 's', 'd', 'OPEN', 'x', 'x', 'review_requested'),
                    (4, 2, 21, 'velho', 'u4', '{a}', '[]', '[]', 'PROJ-1', 'dev', 's', 'd', 'MERGED', 'x', 'x', 'authored');",
        )
        .unwrap();
    }

    #[test]
    fn acao_exige_repo_com_dois_prs() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_prs(&conn);
        let prs = my_open_prs(&conn, "proj-1", None).unwrap();
        assert_eq!(prs.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), vec!["back#20", "front#10"]);
        assert_eq!(my_open_prs(&conn, "PROJ-1", Some("back")).unwrap().len(), 1);
        assert!(my_open_prs(&conn, "PROJ-1", Some("outro")).unwrap().is_empty());
        assert_eq!(check_single(&prs, &Action::Commit).unwrap_err(), "a branch PROJ-1 tem PR seu em back e front: use --repo");
        assert!(check_single(&prs, &Action::List).is_ok());
        assert!(check_single(&prs[..1], &Action::Commit).is_ok());
        assert_eq!(others_open_prs_count(&conn, "PROJ-1", Some("outro")).unwrap(), 1);
        assert_eq!(others_open_prs_count(&conn, "PROJ-1", Some("back")).unwrap(), 0);
    }

    #[test]
    fn item_inexistente_e_estado_errado() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_prs(&conn);
        conn.execute_batch(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('f', 'pr', 2, '/r', 'ready');
             INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('f', 1, 'a', 'new'), ('f', 2, 'b', 'procede'), ('f', 3, 'c', 'failed');",
        )
        .unwrap();
        let id_c2: i64 = conn.query_row("SELECT id FROM fix_comments WHERE fix_id = 'f' AND seq = 2", [], |r| r.get(0)).unwrap();
        assert_eq!(comment_ids(&conn, "f", &Items::All).unwrap().iter().map(|p| p.0).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(comment_ids(&conn, "f", &Items::Seqs(vec![9])).unwrap_err(), "C9 não existe neste PR");
        assert_eq!(comment_ids(&conn, "f", &Items::Seqs(vec![2])).unwrap(), vec![(2, id_c2)]);
        conn.execute("UPDATE fix_comments SET status = 'applied' WHERE fix_id = 'f'", []).unwrap();
        assert_eq!(comment_ids(&conn, "f", &Items::All).unwrap_err(), "nenhum comentário esperando análise");
    }

    #[test]
    fn approve_so_com_sugestao_esperando() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_prs(&conn);
        conn.execute_batch(
            "INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('f', 'pr', 2, '/r', 'ready');
             INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('f', 1, 'a', 'new'), ('f', 2, 'b', 'procede');",
        )
        .unwrap();
        assert!(approvable(&conn, "f", &[2]).is_ok());
        assert_eq!(approvable(&conn, "f", &[2, 1]).unwrap_err(), "C1 não tem sugestão esperando aprovação (não analisado)");
    }

    #[test]
    fn listagem_que_falhou_sai_com_1() {
        let o = |fix: Value, ran: bool| Outcome { pr: pr(), fix: Some(fix), warning: None, error: None, ran };
        assert_eq!(list_code(&o(json!({"status": "ready", "error": "Bitbucket fora"}), true)), 1);
        // erro antigo numa correção que este terminal não tocou (ocupada pelo app)
        assert_eq!(list_code(&o(json!({"status": "ready", "error": "antigo"}), false)), 0);
        assert_eq!(list_code(&o(json!({"status": "failed", "error": null}), true)), 1);
        assert_eq!(list_code(&o(json!({"status": "ready", "error": null}), true)), 0);
    }

    fn pr() -> MyPr {
        MyPr { id: 1, slug: "back".into(), branch: "PROJ-1".into(), label: "back#20".into(), title: "Título".into(), url: "https://bb/20".into() }
    }

    #[test]
    fn diff_de_remocao_e_corpo_multilinha() {
        let edits = json!([{"file": "a.php", "line": 3, "end_line": 4, "existing_code": "x\ny", "improved_code": ""}]);
        assert_eq!(diff_text(&edits), "```diff\n--- a.php:3\n-x\n-y\n```");
        let edits = json!([{"file": "b.php", "line": 7, "end_line": 7, "existing_code": "a", "improved_code": "b\nc"}]);
        assert_eq!(diff_text(&edits), "```diff\n--- b.php:7\n-a\n+b\n+c\n```");
        let fix = json!({"id": "f1", "status": "ready", "cost_usd": 0.0, "run_dir": "/r", "card_key": "PROJ-1", "comments": [
            {"seq": 1, "author": "Ana", "body": "linha 1\n# não é título", "status": "new", "replies": [], "edits": [],
             "outdated": false, "path": null, "line": null, "messages": []}]});
        let r = report(&pr(), &fix, None);
        assert!(r.contains("### C1 · Ana · não analisado"), "{r}");
        assert!(r.contains("> linha 1\n> # não é título"), "{r}");
        assert!(r.starts_with("# back#20 · PROJ-1"), "{r}");
    }

    #[test]
    fn relatorio_mostra_analise_respostas_e_aviso() {
        let fix = json!({"id": "f1", "status": "ready", "cost_usd": 0.12, "run_dir": "/r", "card_key": null, "comments": [
            {"seq": 2, "author": "Bia", "body": "troque", "status": "procede", "path": "a.php", "line": 40, "outdated": true,
             "replies": [{"author": "Caio", "body": "concordo"}], "explanation": "Procede porque X.",
             "edits": [{"file": "a.php", "line": 40, "end_line": 40, "existing_code": "old", "improved_code": "new"}], "messages": []},
            {"seq": 3, "author": "Bia", "body": "e isso?", "status": "failed", "error": "não bateu", "replies": [], "edits": [],
             "outdated": false, "path": null, "line": null, "messages": []}]});
        let r = report(&pr(), &fix, Some("ocupada pelo app; não busquei comentários novos"));
        assert!(r.contains("Aviso: ocupada pelo app; não busquei comentários novos"), "{r}");
        assert!(r.contains("## Comentários (2) · 0 não analisado(s) · 1 com sugestão esperando aprovação"), "{r}");
        assert!(r.contains("### C2 · Bia · a.php:40 · desatualizado · procede, sugestão esperando aprovação"), "{r}");
        assert!(r.contains("- Caio: concordo"), "{r}");
        assert!(r.contains("Análise: Procede porque X."), "{r}");
        assert!(r.contains("```diff\n--- a.php:40\n-old\n+new\n```"), "{r}");
        assert!(r.contains("### C3 · Bia · falhou: não bateu"), "{r}");
        let empty = json!({"id": "f1", "status": "ready", "cost_usd": 0.0, "run_dir": "/r", "comments": []});
        assert!(report(&pr(), &empty, None).contains("Nenhum comentário aberto de outra pessoa neste PR."));
    }
}
