//! `pr-review-orchestrator -p <branch>`: o mesmo review do app, pelo terminal, como o
//! `claude -p`. O andamento vai para o stderr e o relatório, para o stdout. O review fica
//! gravado no banco do app e aparece na lista como qualquer outro.
//!
//! A branch é procurada nos PRs abertos (com um sync antes, se nenhum tiver essa branch) e,
//! sem PR, nas branches locais dos clones. Front e back do mesmo card rodam juntos.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::commands::{self, AppState};
use crate::error::{AppError, AppResult};
use crate::review::{self, CliHost, Host};
use crate::{branches, db, secrets};

pub mod comments;
pub mod prs;

const USAGE: &str = "\
uso: pr-review-orchestrator -p <branch> [opções]

Revisa o PR aberto da branch (ou a branch local, se não houver PR) com o mesmo
pipeline do app: Sonnet, validação, melhorias do Haiku com o juiz e auditoria do Opus.
O teste no navegador não roda pelo terminal.

opções:
  --repo <slug|owner/repo>   só neste repositório (GitHub: owner/repo)
  --sync                     atualiza os PRs do Bitbucket antes de procurar
  --output-format text|json  formato do relatório no stdout (padrão: text)
  --verbose                  também escreve o log detalhado no stderr
  -h, --help                 mostra esta ajuda

Para os comentários dos seus PRs: pr-review-orchestrator comments --help
Painel do dia (o que revisar, como estão os seus PRs): pr-review-orchestrator prs --help

saída: 0 se todos os reviews concluíram, 1 se algum falhou, 2 erro de uso ou branch
não encontrada, 130 se foi cancelado (Ctrl+C ou SIGTERM).";

#[derive(Debug, Default, PartialEq)]
struct Args {
    branch: String,
    repo: Option<String>,
    sync: bool,
    json: bool,
    verbose: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} precisa de um valor"));
        match arg.as_str() {
            "-p" | "--print" => a.branch = value(arg)?,
            "--repo" => a.repo = Some(value(arg)?),
            "--sync" => a.sync = true,
            "--verbose" => a.verbose = true,
            "--output-format" => match value(arg)?.as_str() {
                "text" => a.json = false,
                "json" => a.json = true,
                other => return Err(format!("--output-format aceita text ou json, não {other}")),
            },
            other => return Err(format!("argumento desconhecido: {other}")),
        }
    }
    a.branch = a.branch.trim().to_owned();
    if a.branch.is_empty() {
        return Err("falta a branch: -p <branch>".into());
    }
    Ok(a)
}

/// O que revisar: um PR aberto ou uma branch local.
#[derive(Debug, Clone)]
enum Target {
    Pr { id: i64, label: String, title: String, url: String },
    Local { slug: String, branch: String },
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Pr { label, .. } => label.clone(),
            Target::Local { slug, branch } => format!("{slug}:{branch} (branch local)"),
        }
    }
}

fn open_prs(conn: &Connection, branch: &str, repo: Option<&str>) -> AppResult<Vec<Target>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, r.slug, p.bb_id, p.title, p.url, r.provider, r.workspace FROM pull_requests p JOIN repositories r ON r.id = p.repository_id
         WHERE p.state = 'OPEN' AND lower(p.source_branch) = lower(?1)
           AND (?2 IS NULL OR r.slug = ?2 OR (r.provider = 'github' AND r.workspace || '/' || r.slug = ?2))
         ORDER BY r.provider, r.workspace, r.slug",
    )?;
    let rows = stmt.query_map(params![branch, repo], |r| {
        let slug: String = r.get(1)?;
        let bb_id: i64 = r.get(2)?;
        let provider: String = r.get(5)?;
        let owner: String = r.get(6)?;
        let label = if provider == "github" { format!("{owner}/{slug}#{bb_id}") } else { format!("{slug}#{bb_id}") };
        Ok(Target::Pr { id: r.get(0)?, label, title: r.get(3)?, url: r.get(4)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Branch local com esse nome (sem diferenciar maiúsculas) nos clones da pasta configurada.
async fn local_branches(root: &Path, branch: &str, repo: Option<&str>) -> AppResult<Vec<Target>> {
    let wanted = branch.to_lowercase();
    let mut out = Vec::new();
    for p in branches::list(root, &[]).await? {
        if repo.is_some_and(|r| r != p.slug) || p.base.is_none() {
            continue;
        }
        if let Some(b) = p.branches.iter().find(|b| b.name.to_lowercase() == wanted) {
            out.push(Target::Local { slug: p.slug.clone(), branch: b.name.clone() });
        }
    }
    Ok(out)
}

async fn resolve(state: &AppState, a: &Args) -> AppResult<Vec<Target>> {
    let lock = || state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()));
    let mut prs = if a.sync { vec![] } else { open_prs(&*lock()?, &a.branch, a.repo.as_deref())? };
    if prs.is_empty() {
        sync(state).await;
        prs = open_prs(&*lock()?, &a.branch, a.repo.as_deref())?;
    }
    if !prs.is_empty() {
        return Ok(prs);
    }
    let root = review::repos_root(&*lock()?)?;
    local_branches(&root, &a.branch, a.repo.as_deref()).await
}

/// Sync do Bitbucket antes de procurar a branch; falha vira aviso no stderr.
pub(super) async fn sync(state: &AppState) {
    eprintln!("atualizando os PRs do Bitbucket…");
    match commands::sync_all(&state.db, &secrets::KeyringStore).await {
        Ok(r) => {
            for repo in r.repos.iter().filter(|x| x.error.is_some() || x.fetch_error.is_some()) {
                eprintln!("AVISO {}: {}", repo.slug, repo.error.as_deref().or(repo.fetch_error.as_deref()).unwrap_or(""));
            }
        }
        Err(e) => eprintln!("AVISO: sync falhou ({e}); procurando no que já está no banco"),
    }
}

pub(super) static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    // o segundo Ctrl+C sai na hora (o primeiro cancela e espera a limpeza dos worktrees)
    if STOP.swap(true, Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
}

pub(super) fn install_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Pasta `claude/` do pacote instalado: `/usr/bin/<exe>` → `/usr/lib/<productName>/claude`.
/// Em dev, `review::assets()` cai no checkout.
pub(super) fn set_assets_from_exe() {
    if let Some(prefix) = std::env::current_exe().ok().and_then(|e| e.parent()?.parent().map(Path::to_path_buf)) {
        review::set_assets_root(prefix.join("lib").join("PR Review Orchestrator").join("claude"));
    }
}

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

/// Abre o banco do app e monta o estado do modo terminal. `Err` traz o código de saída (a
/// mensagem já foi para o stderr).
pub(super) fn cli_host(verbose: bool) -> Result<Arc<CliHost>, i32> {
    let db_path = crate::data_dir_headless().join("app.db");
    if !db_path.exists() {
        eprintln!("banco não encontrado em {}; abra o app uma vez e configure um provedor", db_path.display());
        return Err(2);
    }
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("erro ao abrir o banco: {e}");
            return Err(2);
        }
    };
    Ok(Arc::new(CliHost {
        state: AppState {
            triggers: Arc::new(crate::triggers::manager::Manager::new(
                crate::data_dir_headless().join("triggers"),
                crate::triggers::manager::Deadlines::default(),
            )),
            db: Mutex::new(conn),
            secrets: Box::new(secrets::KeyringStore),
            db_path,
            reviews: Default::default(),
            review_slots: Arc::new(tokio::sync::Semaphore::new(5)),
            browser_lock: tokio::sync::Mutex::new(()),
            publish_lock: tokio::sync::Mutex::new(()),
            claude_limit: Mutex::new(None),
        },
        verbose,
        labels: Mutex::new(HashMap::new()),
    }))
}

async fn main(cli: Arc<CliHost>, a: Args) -> i32 {
    let host = Host::Cli(cli.clone());
    let targets = match resolve(&cli.state, &a).await {
        Ok(t) if t.is_empty() => {
            let scope = a.repo.as_deref().map(|r| format!(" em {r}")).unwrap_or_default();
            eprintln!("nenhum PR aberto nem branch local chamada {}{scope}", a.branch);
            return 2;
        }
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    // Ctrl+C ou SIGTERM: cancela as execuções (o review mata os processos do claude e remove
    // o worktree) e espera o fim delas
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

    let started = Instant::now();
    let mut runs: Vec<(Target, String)> = Vec::new();
    let mut tasks = Vec::new();
    let mut start_errors = 0;
    for t in &targets {
        let launched = match t {
            Target::Pr { id, .. } => review::start_run(host.clone(), *id),
            Target::Local { slug, branch } => review::start_branch_run(host.clone(), slug.clone(), branch.clone()).await,
        };
        match launched {
            Ok((run_id, task)) => {
                eprintln!("[{}] review {run_id} iniciado", t.label());
                runs.push((t.clone(), run_id));
                tasks.push(task);
            }
            Err(e) => {
                eprintln!("[{}] não iniciou: {e}", t.label());
                start_errors += 1;
            }
        }
    }
    for task in tasks {
        let _ = task.await;
    }
    watcher.abort();

    let (views, jira_site): (Vec<(Target, Value)>, Option<String>) = {
        let Ok(conn) = cli.state.db.lock() else {
            eprintln!("banco indisponível ao montar o relatório");
            return 1;
        };
        let site = crate::review::jira_site(&conn).unwrap_or_default();
        let views = runs.iter()
            .filter_map(|(t, id)| match commands::run_view(&conn, id).and_then(|v| Ok(serde_json::to_value(v)?)) {
                Ok(mut v) => {
                    // o log fica no run.log da pasta da execução
                    if let Some(o) = v.as_object_mut() {
                        o.remove("log");
                    }
                    Some((t.clone(), v))
                }
                Err(e) => {
                    eprintln!("[{}] não consegui ler o resultado: {e}", t.label());
                    None
                }
            })
            .collect();
        (views, site)
    };

    if a.json {
        let out: Vec<Value> = views.iter().map(|(t, v)| json!({"target": target_json(t), "run": v})).collect();
        println!("{}", serde_json::to_string_pretty(&json!({"runs": out})).unwrap_or_default());
    } else {
        let text: Vec<String> = views.iter().map(|(t, v)| report(t, v, jira_site.as_deref())).collect();
        println!("{}", text.join("\n\n---\n\n"));
        eprintln!("tempo total: {:.1} min", started.elapsed().as_secs_f64() / 60.0);
    }

    let statuses: Vec<&str> = views.iter().map(|(_, v)| v["status"].as_str().unwrap_or("")).collect();
    if STOP.load(Ordering::SeqCst) || statuses.contains(&"cancelled") {
        130
    } else if start_errors > 0 || statuses.len() < runs.len() || statuses.iter().any(|s| *s != "completed") {
        1
    } else {
        0
    }
}

fn target_json(t: &Target) -> Value {
    match t {
        Target::Pr { id, label, title, url } => json!({"kind": "pr", "pr_id": id, "label": label, "title": title, "url": url}),
        Target::Local { slug, branch } => json!({"kind": "branch", "repo_slug": slug, "branch": branch}),
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn sev_rank(sev: &str) -> u8 {
    match sev {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        _ => 3,
    }
}

/// Severidade depois da auditoria (o Opus pode ajustar).
fn effective_severity(f: &Value) -> &str {
    Some(s(f, "audit_new_severity")).filter(|x| !x.is_empty()).unwrap_or(s(f, "severity"))
}

/// O mesmo texto do botão Copiar do painel: o bloco no formato do `branch-reviewer`.
fn finding_text(f: &Value) -> String {
    let md = s(f, "markdown").trim();
    if !md.is_empty() {
        return md.to_owned();
    }
    let steps: Vec<String> = f["steps"]
        .as_array()
        .map(|a| a.iter().enumerate().map(|(i, x)| format!("{}. {}", i + 1, x.as_str().unwrap_or(""))).collect())
        .unwrap_or_default();
    let line = f["line"].as_i64().map(|l| l.to_string()).unwrap_or_default();
    let pattern = Some(s(f, "pattern_ref")).filter(|p| !p.is_empty()).map(|p| format!("Padrão do projeto: {p}")).unwrap_or_default();
    [format!("`{}:{line}` — {}", s(f, "file"), s(f, "title")), s(f, "description").to_owned(), steps.join("\n"), pattern]
        .into_iter()
        .filter(|x| !x.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Relatório em markdown de uma execução, na ordem do painel: achados pela severidade e
/// melhorias pela nota do juiz.
fn report(t: &Target, run: &Value, jira_site: Option<&str>) -> String {
    let mut out = Vec::new();
    let card = s(run, "card_key");
    out.push(format!("# {}{}", t.label(), if card.is_empty() { String::new() } else { format!(" · {card}") }));
    if let Target::Pr { title, url, .. } = t {
        out.push(format!("{title}\n{url}"));
    }
    if let (false, Some(site)) = (card.is_empty(), jira_site) {
        out.push(format!("Card: https://{site}/browse/{card}"));
    }
    let status = match s(run, "status") {
        "completed" => "concluído",
        "failed" => "falhou",
        "cancelled" => "cancelado",
        other => other,
    };
    let cost = run["cost_usd"].as_f64().map(|c| format!(" · US$ {c:.2}")).unwrap_or_default();
    out.push(format!("Status: {status}{cost} · arquivos em {}", s(run, "run_dir")));
    for (k, label) in [("error", "Erro"), ("warning", "Aviso")] {
        if !s(run, k).is_empty() {
            out.push(format!("{label}: {}", s(run, k)));
        }
    }

    let empty = vec![];
    let findings = run["findings"].as_array().unwrap_or(&empty);
    let mut main: Vec<&Value> = findings
        .iter()
        .filter(|f| s(f, "kind") != "melhoria" && s(f, "validation") == "ok" && s(f, "audit_decision") != "falso_positivo")
        .collect();
    main.sort_by_key(|f| sev_rank(effective_severity(f)));
    let mut improvements: Vec<&Value> = findings.iter().filter(|f| s(f, "kind") == "melhoria" && s(f, "validation") == "ok").collect();
    improvements.sort_by_key(|f| std::cmp::Reverse(f["score"].as_i64().unwrap_or(-1)));
    let discarded = findings.iter().filter(|f| s(f, "validation") == "rejected" || s(f, "audit_decision") == "falso_positivo").count();

    out.push(format!("## Achados ({})", main.len()));
    if main.is_empty() {
        out.push("Nenhum achado.".into());
    }
    for f in &main {
        let mut head = format!("### F{} · {}", f["seq"], effective_severity(f).to_uppercase());
        if !matches!(s(f, "kind"), "" | "bug") {
            head.push_str(&format!(" · {}", s(f, "kind")));
        }
        match s(f, "audit_decision") {
            "confirmado" => head.push_str(" · Opus: confirmado"),
            "severidade_ajustada" => head.push_str(&format!(" · Opus: severidade ajustada (era {})", s(f, "severity").to_uppercase())),
            "evidencia_insuficiente" => head.push_str(" · Opus: evidência insuficiente"),
            _ => {}
        }
        out.push(format!("{head}\n\n{}", finding_text(f)));
    }

    if !improvements.is_empty() {
        out.push(format!("## Melhorias de código ({})", improvements.len()));
        for f in &improvements {
            let mut head = format!("### F{}", f["seq"]);
            if let Some(score) = f["score"].as_i64() {
                head.push_str(&format!(" · nota {score}"));
                if !s(f, "importance").is_empty() {
                    head.push_str(&format!(" ({})", s(f, "importance")));
                }
            }
            if !s(f, "category").is_empty() {
                head.push_str(&format!(" · {}", s(f, "category")));
            }
            out.push(format!("{head}\n\n{}", finding_text(f)));
        }
    }
    if discarded > 0 {
        out.push(format!("{discarded} achado(s) descartado(s) pela validação do arquivo:linha ou pelo Opus (estão no app)."));
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn le_os_argumentos() {
        let a = parse(&args(&["-p", " PROJ-1000 ", "--repo", "acme-back", "--output-format", "json", "--verbose"])).unwrap();
        assert_eq!(a, Args { branch: "PROJ-1000".into(), repo: Some("acme-back".into()), sync: false, json: true, verbose: true });
        assert!(parse(&args(&["--sync"])).is_err());
        assert!(parse(&args(&["-p"])).is_err());
        assert!(parse(&args(&["-p", "x", "--output-format", "xml"])).is_err());
        assert!(parse(&args(&["-p", "x", "--outro"])).is_err());
    }

    #[test]
    fn relatorio_na_ordem_do_painel() {
        let run = json!({
            "status": "completed", "cost_usd": 0.4, "card_key": "PROJ-1", "run_dir": "/tmp/r",
            "findings": [
                {"seq": 1, "kind": "bug", "severity": "low", "validation": "ok", "markdown": "baixo"},
                {"seq": 2, "kind": "bug", "severity": "medium", "audit_new_severity": "critical",
                 "audit_decision": "severidade_ajustada", "validation": "ok", "markdown": "virou crítico"},
                {"seq": 3, "kind": "bug", "severity": "high", "audit_decision": "falso_positivo", "validation": "ok", "markdown": "fp"},
                {"seq": 4, "kind": "melhoria", "validation": "ok", "score": 3, "category": "duplicado", "markdown": "m3"},
                {"seq": 5, "kind": "melhoria", "validation": "ok", "score": 7, "importance": "alta", "category": "desempenho", "markdown": "m7"},
                {"seq": 6, "kind": "bug", "severity": "high", "validation": "rejected", "file": "a.php", "line": 3, "title": "x"}
            ]
        });
        let t = Target::Pr { id: 1, label: "repo#9".into(), title: "Título".into(), url: "https://bb/9".into() };
        let r = report(&t, &run, Some("acme.atlassian.net"));
        let pos = |needle: &str| r.find(needle).unwrap_or_else(|| panic!("faltou {needle} em:\n{r}"));
        assert!(pos("F2 · CRITICAL · Opus: severidade ajustada (era MEDIUM)") < pos("F1 · LOW"));
        assert!(pos("F5 · nota 7 (alta) · desempenho") < pos("F4 · nota 3"));
        assert!(!r.contains("\nfp"));
        assert!(r.contains("## Achados (2)") && r.contains("2 achado(s) descartado(s)"));
        assert!(r.contains("Card: https://acme.atlassian.net/browse/PROJ-1"));
        // sem site do Jira configurado, sem link do card
        assert!(!report(&t, &run, None).contains("Card:"));
    }

    #[test]
    fn texto_sem_markdown_segue_o_copiar() {
        let f = json!({"file": "a.php", "line": 7, "title": "T", "description": "D", "steps": ["um", "dois"], "pattern_ref": ""});
        assert_eq!(finding_text(&f), "`a.php:7` — T\n\nD\n\n1. um\n2. dois");
    }
}
