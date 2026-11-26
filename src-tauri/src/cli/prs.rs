//! `pr-review-orchestrator prs`: o painel do dia. O resumo das listas "Para eu revisar" e
//! "Meus PRs" do app, lido do banco (com `--sync`, atualizado antes). Não chama o Claude.

use serde_json::Value;

use super::{cli_host, sync};
use crate::commands;

const USAGE: &str = "\
uso: pr-review-orchestrator prs [opções]

O painel do dia: os PRs para você revisar (com o último review) e os seus (pedidos de mudança,
aprovações, comentários e correção), como nas listas do app. Não chama o Claude.

opções:
  --sync                     atualiza os PRs configurados antes
  --all                      mostra também os PRs que você já aprovou
  --output-format text|json  formato no stdout (padrão: text)
  -h, --help                 mostra esta ajuda

Depois: pr-review-orchestrator -p <branch> revisa um PR; comments <branch> mostra os
comentários de um PR seu.

saída: 0 ok, 2 erro de uso ou banco não encontrado.";

#[derive(Debug, PartialEq)]
struct Args {
    sync: bool,
    all: bool,
    json: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args { sync: false, all: false, json: false };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sync" => a.sync = true,
            "--all" => a.all = true,
            "--output-format" => match it.next().map(String::as_str) {
                Some("text") => a.json = false,
                Some("json") => a.json = true,
                other => return Err(format!("--output-format aceita text ou json, não {}", other.unwrap_or("nada"))),
            },
            other => return Err(format!("argumento desconhecido: {other}")),
        }
    }
    Ok(a)
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
    let cli = match cli_host(false) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if a.sync {
        tauri::async_runtime::block_on(sync(&cli.state));
    }
    let list = match cli.state.db.lock().map_err(|_| "banco indisponível".to_owned()).and_then(|c| {
        commands::pr_list(&c).and_then(|l| Ok(serde_json::to_value(l)?)).map_err(|e| e.to_string())
    }) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&list).unwrap_or_default());
    } else {
        println!("{}", dashboard(&list, a.all));
    }
    0
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn n(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// `2026-11-24T20:23:51.366Z` (UTC) → `24/11 17:23` no fuso local. Texto que não é data volta
/// como veio.
fn local_time(iso: &str) -> String {
    let part = |a: usize, b: usize| iso.get(a..b).and_then(|x| x.parse::<i32>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(sec)) =
        (part(0, 4), part(5, 7), part(8, 10), part(11, 13), part(14, 16), part(17, 19))
    else {
        return iso.to_owned();
    };
    // SAFETY: tm zerado e preenchido aqui; timegm e localtime_r só leem e escrevem nas structs locais
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = y - 1900;
        tm.tm_mon = mo - 1;
        tm.tm_mday = d;
        tm.tm_hour = h;
        tm.tm_min = mi;
        tm.tm_sec = sec;
        let t = libc::timegm(&mut tm);
        let mut local: libc::tm = std::mem::zeroed();
        if t == -1 || libc::localtime_r(&t, &mut local).is_null() {
            return iso.to_owned();
        }
        format!("{:02}/{:02} {:02}:{:02}", local.tm_mday, local.tm_mon + 1, local.tm_hour, local.tm_min)
    }
}

/// O último review de um PR para eu revisar, em uma linha.
fn review_line(run: &Value, outdated: bool) -> String {
    let base = match s(run, "status") {
        "" => "nunca revisado".to_owned(),
        "running" => match s(run, "stage") {
            "" => "rodando".to_owned(),
            st => format!("rodando ({st})"),
        },
        "failed" => "o último falhou".to_owned(),
        "cancelled" => "o último foi cancelado".to_owned(),
        _ => {
            let (f, h, m) = (n(run, "findings"), n(run, "high"), n(run, "improvements"));
            let mut parts = Vec::new();
            if f > 0 {
                parts.push(if h > 0 { format!("{f} achado(s) ({h} alto ou crítico)") } else { format!("{f} achado(s)") });
            }
            if m > 0 {
                parts.push(format!("{m} melhoria(s)"));
            }
            if parts.is_empty() { "sem achados".to_owned() } else { parts.join(", ") }
        }
    };
    let mut out = format!("review: {base}");
    if outdated {
        out.push_str(" · desatualizado: o PR tem commits depois do review");
    }
    out
}

fn head(p: &Value) -> String {
    let (branch, title) = (s(p, "source_branch"), s(p, "title").trim());
    let mut h = format!("- {}#{} · {branch}", s(p, "repo"), n(p, "bb_id"));
    // muitos PRs têm o nome da branch como título
    if !title.is_empty() && !title.eq_ignore_ascii_case(branch) {
        h.push_str(&format!(" · {title}"));
    }
    h
}

fn to_review(p: &Value) -> String {
    let mut h = head(p);
    if p["approved_by_me"].as_bool() == Some(true) {
        h.push_str(" · você já aprovou");
    }
    let mut lines = vec![h];
    if !s(p, "author").is_empty() {
        lines.push(format!("  autor: {}", s(p, "author")));
    }
    lines.push(format!("  {}", review_line(&p["last_run"], p["needs_rereview"].as_bool() == Some(true))));
    lines.join("\n")
}

fn mine(p: &Value) -> String {
    let mut lines = vec![head(p)];
    let names = |pred: &dyn Fn(&Value) -> bool| -> Vec<String> {
        p["reviewers"].as_array().map(|a| a.iter().filter(|r| pred(r)).map(|r| s(r, "name").to_owned()).collect()).unwrap_or_default()
    };
    let changes = names(&|r| s(r, "state") == "changes_requested");
    let approved = names(&|r| r["approved"].as_bool() == Some(true));
    let mut who = Vec::new();
    if !changes.is_empty() {
        who.push(format!("pediram mudanças: {}", changes.join(", ")));
    }
    if !approved.is_empty() {
        who.push(format!("aprovaram: {}", approved.join(", ")));
    }
    if !who.is_empty() {
        lines.push(format!("  {}", who.join(" · ")));
    }
    let mut status = vec![format!("{} comentário(s)", n(p, "comment_count"))];
    let fix = &p["fix"];
    if fix.is_object() {
        let (pending, applied) = (n(fix, "pending"), n(fix, "applied"));
        let mut f = Vec::new();
        if pending > 0 {
            f.push(format!("{pending} com sugestão esperando aprovação"));
        }
        if applied > 0 {
            f.push(format!("{applied} aplicada(s)"));
        }
        if !f.is_empty() {
            status.push(format!("correção: {}", f.join(", ")));
        }
    }
    lines.push(format!("  {}", status.join(" · ")));
    lines.join("\n")
}

/// O painel em markdown: "Para eu revisar" (sem os que já aprovei, salvo `all`) e "Meus PRs".
fn dashboard(list: &Value, all: bool) -> String {
    let empty = vec![];
    let prs = list["prs"].as_array().unwrap_or(&empty);
    let review: Vec<&Value> = prs.iter().filter(|p| s(p, "category") == "review_requested").collect();
    let shown: Vec<&&Value> = review.iter().filter(|p| all || p["approved_by_me"].as_bool() != Some(true)).collect();
    let hidden = review.len() - shown.len();
    let my: Vec<&Value> = prs.iter().filter(|p| s(p, "category") != "review_requested").collect();

    let sync = match list["last_sync"].as_str() {
        Some(t) => local_time(t),
        None => "nunca".into(),
    };
    let mut out = vec![format!("# Painel do dia · último sync: {sync}")];
    let mut title = format!("## Para eu revisar ({})", shown.len());
    if hidden > 0 {
        title.push_str(&format!(" · {hidden} que você já aprovou fora (--all mostra)"));
    }
    out.push(title);
    if shown.is_empty() {
        out.push("Nada para revisar.".into());
    } else {
        out.push(shown.iter().map(|p| to_review(p)).collect::<Vec<_>>().join("\n"));
    }
    out.push(format!("## Meus PRs ({})", my.len()));
    if my.is_empty() {
        out.push("Nenhum PR seu aberto.".into());
    } else {
        out.push(my.iter().map(|p| mine(p)).collect::<Vec<_>>().join("\n"));
    }
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
        assert_eq!(parse(&args(&[])).unwrap(), Args { sync: false, all: false, json: false });
        assert_eq!(parse(&args(&["--sync", "--all", "--output-format", "json"])).unwrap(), Args { sync: true, all: true, json: true });
        assert!(parse(&args(&["PROJ-1"])).is_err());
        assert!(parse(&args(&["--output-format", "xml"])).is_err());
    }

    fn list() -> Value {
        json!({"last_sync": "x", "prs": [
            {"repo": "back", "bb_id": 1, "title": "Nunca revisado", "source_branch": "PROJ-1", "author": "Ana",
             "category": "review_requested", "approved_by_me": false, "needs_rereview": false, "last_run": null,
             "reviewers": [], "comment_count": 0, "fix": null, "updated_on": "x"},
            {"repo": "front", "bb_id": 2, "title": "Revisado", "source_branch": "PROJ-2", "author": "Bia",
             "category": "review_requested", "approved_by_me": false, "needs_rereview": true,
             "last_run": {"status": "completed", "findings": 3, "high": 1, "improvements": 2, "stage": null},
             "reviewers": [], "comment_count": 4, "fix": null, "updated_on": "x"},
            {"repo": "back", "bb_id": 3, "title": "Já aprovei", "source_branch": "PROJ-3", "author": "Caio",
             "category": "review_requested", "approved_by_me": true, "needs_rereview": false, "last_run": null,
             "reviewers": [], "comment_count": 0, "fix": null, "updated_on": "x"},
            {"repo": "back", "bb_id": 4, "title": "Meu", "source_branch": "PROJ-4", "author": "Eu",
             "category": "authored", "approved_by_me": false, "needs_rereview": false, "last_run": null,
             "reviewers": [{"name": "Dani", "approved": false, "state": "changes_requested"},
                           {"name": "Edu", "approved": true, "state": "approved"}],
             "comment_count": 5, "fix": {"pending": 1, "applied": 2, "total": 4, "status": "ready"}, "updated_on": "x"}
        ]})
    }

    #[test]
    fn painel_separa_e_resume() {
        let t = dashboard(&list(), false);
        assert!(t.contains("## Para eu revisar (2) · 1 que você já aprovou fora (--all mostra)"), "{t}");
        assert!(t.contains("- back#1 · PROJ-1 · Nunca revisado\n  autor: Ana\n  review: nunca revisado"), "{t}");
        assert!(t.contains("review: 3 achado(s) (1 alto ou crítico), 2 melhoria(s) · desatualizado: o PR tem commits depois do review"), "{t}");
        assert!(!t.contains("Já aprovei"), "{t}");
        assert!(t.contains("## Meus PRs (1)"), "{t}");
        assert!(t.contains("pediram mudanças: Dani · aprovaram: Edu"), "{t}");
        assert!(t.contains("5 comentário(s) · correção: 1 com sugestão esperando aprovação, 2 aplicada(s)"), "{t}");
        let all = dashboard(&list(), true);
        assert!(all.contains("## Para eu revisar (3)\n"), "{all}");
        assert!(all.contains("- back#3 · PROJ-3 · Já aprovei · você já aprovou\n"), "{all}");
        // título igual à branch não se repete
        let t = dashboard(&json!({"last_sync": null, "prs": [{"repo": "back", "bb_id": 5, "title": "proj-5", "source_branch": "PROJ-5",
            "category": "authored", "reviewers": [], "comment_count": 0, "fix": null}]}), false);
        assert!(t.contains("- back#5 · PROJ-5\n"), "{t}");
    }

    #[test]
    fn review_rodando_falhou_e_listas_vazias() {
        let run = |st: &str| json!({"status": st, "stage": "Sonnet revisando", "findings": 0, "high": 0, "improvements": 0});
        assert_eq!(review_line(&run("running"), false), "review: rodando (Sonnet revisando)");
        assert_eq!(review_line(&run("failed"), false), "review: o último falhou");
        assert_eq!(review_line(&run("completed"), false), "review: sem achados");
        let empty = dashboard(&json!({"last_sync": null, "prs": []}), false);
        assert!(empty.contains("Nada para revisar."), "{empty}");
        assert!(empty.contains("Nenhum PR seu aberto."), "{empty}");
        assert!(empty.contains("último sync: nunca"), "{empty}");
    }

    #[test]
    fn hora_local_ou_o_texto_original() {
        assert_eq!(local_time("lixo"), "lixo");
        let t = local_time("2026-11-24T20:23:51.366Z");
        assert_eq!(t.len(), "24/11 17:23".len(), "{t}");
        assert_eq!(&t[2..3], "/");
    }
}
