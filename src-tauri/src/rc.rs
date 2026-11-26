//! Envio do pedido de mudanças (RC) que o agente `rc-validator` montou no fim do review.
//!
//! Só GitHub: um review com um comentário por linha e o resumo do agente no corpo. Com algum item
//! bloqueante o evento é `REQUEST_CHANGES`; só com sugestões, `COMMENT`. O revisor pode tirar itens
//! antes de enviar (`skip`). Nada sai sem o clique do revisor.
use reqwest::Method;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::State;

use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::github;

#[derive(Serialize)]
pub struct RcSent {
    pub url: Option<String>,
    /// `REQUEST_CHANGES` ou `COMMENT`.
    pub event: String,
    pub comments: usize,
}

#[derive(Debug)]
struct Plan {
    owner: String,
    repo: String,
    number: i64,
    sha: String,
    event: &'static str,
    body: String,
    inline: Vec<Value>,
    seqs: Vec<i64>,
    marker: String,
}

/// Item do RC como o agente deixou no banco.
struct Item {
    seq: i64,
    decision: String,
    body: String,
    file: Option<String>,
    line: Option<i64>,
    in_diff: bool,
    title: String,
}

/// Texto de um item do RC, com o rótulo que diz se ele trava o merge.
fn item_text(decision: &str, body: &str) -> String {
    let label = if decision == "bloqueante" { "**Bloqueante.**" } else { "**Sugestão** (não bloqueia o merge)." };
    format!("{label} {body}")
}

/// Monta o review a partir do que o agente decidiu. Erro quando não há o que enviar.
fn plan(conn: &Connection, run_id: &str, skip: &[i64]) -> AppResult<Plan> {
    let (provider, owner, repo, number, sha, rc_status, summary): (String, String, String, i64, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT pvd.provider, pvd.workspace, pvd.slug, p.bb_id, rr.source_sha, rr.rc_status, rr.rc_summary
             FROM review_runs rr JOIN pull_requests p ON p.id = rr.pr_id
             JOIN repositories pvd ON pvd.id = p.repository_id
             WHERE rr.id = ?1 AND rr.status = 'completed'",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid("review concluído não encontrado".into()))?;
    if provider != "github" {
        return Err(AppError::Invalid("o pedido de mudanças só vai para PRs do GitHub".into()));
    }
    if rc_status.as_deref() != Some("completed") {
        return Err(AppError::Invalid("este review não tem pedido de mudanças montado".into()));
    }
    let mut stmt = conn.prepare(
        "SELECT seq, rc_decision, rc_body, file, line, ifnull(in_diff, 0), ifnull(title, '')
         FROM findings
         WHERE run_id = ?1 AND rc_decision IN ('bloqueante', 'sugestao') AND ifnull(rc_body, '') != ''
         ORDER BY CASE rc_decision WHEN 'bloqueante' THEN 0 ELSE 1 END, seq",
    )?;
    let items: Vec<Item> = stmt
        .query_map([run_id], |r| {
            Ok(Item { seq: r.get(0)?, decision: r.get(1)?, body: r.get(2)?, file: r.get(3)?, line: r.get(4)?, in_diff: r.get(5)?, title: r.get(6)? })
        })?
        .collect::<Result<_, _>>()?;
    let items: Vec<Item> = items.into_iter().filter(|i| !skip.contains(&i.seq)).collect();
    if items.is_empty() {
        return Err(AppError::Invalid("nenhum item selecionado para o pedido de mudanças".into()));
    }
    let mut inline = Vec::new();
    let mut outside = Vec::new();
    for i in &items {
        match (&i.file, i.line) {
            // comentário de linha só no lado novo do diff; o GitHub recusa linha fora dele
            (Some(file), Some(line)) if i.in_diff && line > 0 => {
                inline.push(json!({"path": file, "line": line, "side": "RIGHT", "body": item_text(&i.decision, &i.body)}))
            }
            (file, line) => {
                let place = file.as_ref().map(|f| format!("`{f}:{}` ", line.unwrap_or(0))).unwrap_or_default();
                outside.push(format!("- {place}{}\n\n  {}", i.title, item_text(&i.decision, &i.body).replace('\n', "\n  ")));
            }
        }
    }
    let seqs: Vec<i64> = items.iter().map(|i| i.seq).collect();
    let blocking = items.iter().any(|i| i.decision == "bloqueante");
    let marker = format!("<!-- crivo-rc:{run_id}:{} -->", seqs.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
    let body = [
        summary.unwrap_or_default().trim().to_owned(),
        if outside.is_empty() { String::new() } else { format!("Fora das linhas do diff:\n\n{}", outside.join("\n")) },
        marker.clone(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    Ok(Plan { owner, repo, number, sha, event: if blocking { "REQUEST_CHANGES" } else { "COMMENT" }, body, inline, seqs, marker })
}

#[tauri::command]
pub async fn send_request_changes(state: State<'_, AppState>, run_id: String, skip: Vec<i64>) -> AppResult<RcSent> {
    let _lock = state.publish_lock.lock().await;
    let (plan, mode, sent) = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let sent: Option<String> = conn
            .query_row("SELECT rc_sent_url FROM review_runs WHERE id = ?1", [&run_id], |r| r.get(0))
            .optional()?
            .flatten();
        let mode = crate::db::get_setting(&conn, "github.auth_mode")?
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "gh".into());
        (plan(&conn, &run_id, &skip)?, mode, sent)
    };
    if let Some(url) = sent {
        return Err(AppError::Invalid(format!("o pedido de mudanças deste review já foi enviado: {url}")));
    }
    let api = github::Api::new(github::token(&mode, state.secrets.as_ref()).await?)?;
    let (owner, repo, number) = (&plan.owner, &plan.repo, plan.number);
    let current: Value = api.get(&format!("/repos/{owner}/{repo}/pulls/{number}")).await?;
    if current.get("state").and_then(Value::as_str) != Some("open") {
        return Err(AppError::Invalid("a PR já não está aberta".into()));
    }
    if current.pointer("/head/sha").and_then(Value::as_str) != Some(plan.sha.as_str()) {
        return Err(AppError::Invalid("a PR recebeu novos commits; revise novamente antes de pedir mudanças".into()));
    }
    // um envio anterior que chegou ao GitHub mas não foi gravado aqui: reaproveita, sem duplicar
    let me: github::User = api.get("/user").await?;
    let mut prior: Option<Value> = None;
    for page in 1..=100 {
        let reviews: Vec<Value> = api.get(&format!("/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100&page={page}")).await?;
        let last = reviews.len() < 100;
        prior = reviews.into_iter().find(|r| {
            r.pointer("/user/login").and_then(Value::as_str) == Some(me.login.as_str())
                && r.get("body").and_then(Value::as_str).is_some_and(|b| b.contains(&plan.marker))
        });
        if prior.is_some() || last {
            break;
        }
    }
    let response: Value = match prior {
        Some(p) => p,
        None => {
            api.request(
                Method::POST,
                &format!("/repos/{owner}/{repo}/pulls/{number}/reviews"),
                Some(&json!({"commit_id": plan.sha, "event": plan.event, "body": plan.body, "comments": plan.inline})),
            )
            .await?
        }
    };
    let remote = response
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| AppError::Invalid("GitHub não devolveu ID do review".into()))?
        .to_string();
    let url = response.get("html_url").and_then(Value::as_str).map(str::to_owned);
    let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
    for seq in &plan.seqs {
        conn.execute(
            "INSERT OR IGNORE INTO published_findings (run_id, finding_seq, remote_id, remote_url) VALUES (?1,?2,?3,?4)",
            params![run_id, seq, remote, url],
        )?;
    }
    conn.execute("UPDATE review_runs SET rc_sent_url = ?1 WHERE id = ?2", params![url.as_deref().unwrap_or(&remote), run_id])?;
    Ok(RcSent { url, event: plan.event.to_owned(), comments: plan.seqs.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(provider: &str, rc_status: &str) -> Connection {
        let conn = crate::db::open_in_memory().unwrap();
        conn.execute("INSERT INTO repositories (id, provider, workspace, slug) VALUES (1, ?1, 'acme', 'app')", [provider]).unwrap();
        conn.execute_batch(
            "INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, reviewers_json, participants_json,
               source_branch, dest_branch, source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 7, 't', 'u', 'a', '[]', '[]', 'PROJ-1', 'main', 'abc', 'd', 'OPEN', 'x', 'x', 'review_requested');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO review_runs (id, pr_id, source_sha, status, run_dir, rc_status, rc_summary)
             VALUES ('r1', 1, 'abc', 'completed', '/r', ?1, 'Falta tratar o tenant.')",
            [rc_status],
        )
        .unwrap();
        let f = |seq: i64, decision: &str, file: &str, line: i64, in_diff: bool, body: &str| {
            conn.execute(
                "INSERT INTO findings (run_id, seq, kind, file, line, in_diff, title, validation, rc_decision, rc_body)
                 VALUES ('r1', ?1, 'bug', ?2, ?3, ?4, ?5, 'ok', ?6, nullif(?7, ''))",
                params![seq, file, line, in_diff, format!("achado {seq}"), decision, body],
            )
            .unwrap();
        };
        f(1, "sugestao", "a.php", 10, true, "renomeie");
        f(2, "bloqueante", "b.php", 20, true, "filtra pelo tenant");
        f(3, "fora", "c.php", 30, true, "");
        f(4, "bloqueante", "d.php", 40, false, "quebra o job\nem produção");
        conn
    }

    #[test]
    fn bloqueante_vira_request_changes_e_fora_nao_sai() {
        let p = plan(&seed("github", "completed"), "r1", &[]).unwrap();
        assert_eq!(p.event, "REQUEST_CHANGES");
        assert_eq!(p.seqs, vec![2, 4, 1], "bloqueantes primeiro, sem o item fora");
        assert_eq!((p.owner.as_str(), p.repo.as_str(), p.number, p.sha.as_str()), ("acme", "app", 7, "abc"));
        let paths: Vec<&str> = p.inline.iter().map(|c| c["path"].as_str().unwrap()).collect();
        assert_eq!(paths, vec!["b.php", "a.php"], "fora do diff não vira comentário de linha");
        assert_eq!(p.inline[0]["body"], "**Bloqueante.** filtra pelo tenant");
        assert!(p.inline[1]["body"].as_str().unwrap().starts_with("**Sugestão**"));
        assert!(p.body.starts_with("Falta tratar o tenant."));
        assert!(p.body.contains("- `d.php:40` achado 4\n\n  **Bloqueante.** quebra o job\n  em produção"), "{}", p.body);
        assert!(p.body.ends_with("<!-- crivo-rc:r1:2,4,1 -->"));
    }

    #[test]
    fn so_sugestoes_vira_comentario() {
        let p = plan(&seed("github", "completed"), "r1", &[2, 4]).unwrap();
        assert_eq!(p.event, "COMMENT");
        assert_eq!(p.seqs, vec![1]);
    }

    #[test]
    fn sem_itens_nao_envia() {
        let e = plan(&seed("github", "completed"), "r1", &[1, 2, 4]).unwrap_err();
        assert_eq!(e.to_string(), "nenhum item selecionado para o pedido de mudanças");
    }

    #[test]
    fn so_github_e_so_com_rc_montado() {
        assert_eq!(plan(&seed("bitbucket", "completed"), "r1", &[]).unwrap_err().to_string(), "o pedido de mudanças só vai para PRs do GitHub");
        assert_eq!(plan(&seed("github", "failed"), "r1", &[]).unwrap_err().to_string(), "este review não tem pedido de mudanças montado");
    }
}
