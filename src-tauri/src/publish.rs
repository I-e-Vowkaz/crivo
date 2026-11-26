//! Publicação explícita de achados validados em comentários de PR.
use reqwest::Method;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::State;

use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::github;
use crate::secrets::SecretKind;

#[derive(Serialize)]
pub struct Published {
    pub seq: i64,
    pub url: Option<String>,
}

struct Finding {
    seq: i64,
    file: Option<String>,
    line: Option<i64>,
    title: String,
    description: String,
    markdown: Option<String>,
    steps_json: String,
    pattern_ref: Option<String>,
    in_diff: bool,
}

#[tauri::command]
pub async fn publish_findings(
    state: State<'_, AppState>,
    run_id: String,
    seqs: Vec<i64>,
    request_changes: bool,
) -> AppResult<Vec<Published>> {
    let _lock = state.publish_lock.lock().await;
    if seqs.is_empty() {
        return Err(AppError::Invalid("selecione ao menos um achado".into()));
    }
    let mut seqs = seqs;
    seqs.sort_unstable();
    seqs.dedup();
    let (provider, owner, repo, number, sha, findings, mut already) = {
        let conn = state
            .db
            .lock()
            .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let meta: (String, String, String, i64, String) = conn
            .query_row(
                "SELECT pvd.provider, pvd.workspace, pvd.slug, p.bb_id, rr.source_sha
             FROM review_runs rr JOIN pull_requests p ON p.id = rr.pr_id
             JOIN repositories pvd ON pvd.id = p.repository_id
             WHERE rr.id = ?1 AND rr.status = 'completed'",
                [&run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?
            .ok_or_else(|| AppError::Invalid("review concluído não encontrado".into()))?;
        let mut findings = Vec::new();
        let mut already = Vec::new();
        for seq in &seqs {
            let f: Finding = conn.query_row(
                "SELECT seq, file, line, ifnull(title, ''), ifnull(description, ''), ifnull(in_diff, 0), markdown, steps_json, pattern_ref
                 FROM findings WHERE run_id = ?1 AND seq = ?2 AND validation = 'ok'
                 AND ifnull(audit_decision, '') != 'falso_positivo' AND ifnull(kind, '') != 'melhoria'",
                params![run_id, seq], |r| Ok(Finding { seq: r.get(0)?, file: r.get(1)?, line: r.get(2)?, title: r.get(3)?, description: r.get(4)?, in_diff: r.get(5)?, markdown: r.get(6)?, steps_json: r.get(7)?, pattern_ref: r.get(8)? })).optional()?
                .ok_or_else(|| AppError::Invalid(format!("achado {seq} não está pronto para publicação")))?;
            let sent: Option<Option<String>> = conn.query_row("SELECT remote_url FROM published_findings WHERE run_id = ?1 AND finding_seq = ?2", params![run_id, seq], |r| r.get(0)).optional()?;
            if let Some(url) = sent {
                already.push(Published { seq: *seq, url });
                continue;
            }
            findings.push(f);
        }
        (meta.0, meta.1, meta.2, meta.3, meta.4, findings, already)
    };
    if findings.is_empty() {
        return Ok(already);
    }
    let body = |f: &Finding| {
        f.markdown
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| {
                let steps: Vec<String> = serde_json::from_str(&f.steps_json).unwrap_or_default();
                let steps = steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!("{}. {s}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n");
                let head = format!(
                    "`{}:{}` — {}",
                    f.file.as_deref().unwrap_or(""),
                    f.line.unwrap_or(0),
                    f.title
                );
                [
                    head,
                    f.description.clone(),
                    steps,
                    f.pattern_ref
                        .as_ref()
                        .map(|p| format!("Padrão do projeto: {p}"))
                        .unwrap_or_default(),
                ]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
            })
    };
    if provider == "github" {
        let mode = {
            let conn = state
                .db
                .lock()
                .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
            crate::db::get_setting(&conn, "github.auth_mode")?
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "gh".into())
        };
        let api = github::Api::new(github::token(&mode, state.secrets.as_ref()).await?)?;
        let current: Value = api
            .get(&format!("/repos/{owner}/{repo}/pulls/{number}"))
            .await?;
        if current.get("state").and_then(Value::as_str) != Some("open") {
            return Err(AppError::Invalid("a PR já não está aberta".into()));
        }
        if current.pointer("/head/sha").and_then(Value::as_str) != Some(sha.as_str()) {
            return Err(AppError::Invalid(
                "a PR recebeu novos commits; revise novamente antes de publicar".into(),
            ));
        }
        let inline: Vec<Value> = findings
            .iter()
            .filter_map(|f| match (&f.file, f.line, f.in_diff) {
                (Some(file), Some(line), true) if line > 0 => {
                    Some(json!({"path": file, "line": line, "side": "RIGHT", "body": body(f)}))
                }
                _ => None,
            })
            .collect();
        let summary = findings
            .iter()
            .filter(|f| f.file.is_none() || f.line.is_none() || !f.in_diff)
            .map(|f| format!("- {}: {}", f.title, f.description))
            .collect::<Vec<_>>()
            .join("\n");
        let marker = format!(
            "<!-- crivo:{run_id}:{} -->",
            findings
                .iter()
                .map(|f| f.seq.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let review_body = format!("Review do Crivo (execução {run_id}).\n\n{summary}\n\n{marker}");
        let me: github::User = api.get("/user").await?;
        let mut prior: Option<Value> = None;
        for page in 1..=100 {
            let reviews: Vec<Value> = api
                .get(&format!(
                    "/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100&page={page}"
                ))
                .await?;
            let last = reviews.len() < 100;
            prior = reviews.into_iter().find(|r| {
                r.pointer("/user/login").and_then(Value::as_str) == Some(me.login.as_str())
                    && r.get("body")
                        .and_then(Value::as_str)
                        .is_some_and(|b| b.contains(&marker))
            });
            if prior.is_some() || last {
                break;
            }
        }
        let response: Value = if let Some(prior) = prior {
            prior
        } else {
            api.request(Method::POST, &format!("/repos/{owner}/{repo}/pulls/{number}/reviews"),
                Some(&json!({"commit_id": sha, "event": if request_changes { "REQUEST_CHANGES" } else { "COMMENT" }, "body": review_body, "comments": inline}))).await?
        };
        let remote = response
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| AppError::Invalid("GitHub não devolveu ID do review".into()))?
            .to_string();
        let url = response
            .get("html_url")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let conn = state
            .db
            .lock()
            .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        for f in &findings {
            conn.execute("INSERT OR IGNORE INTO published_findings (run_id, finding_seq, remote_id, remote_url) VALUES (?1,?2,?3,?4)", params![run_id, f.seq, remote, url])?;
        }
        already.extend(findings.into_iter().map(|f| Published {
            seq: f.seq,
            url: url.clone(),
        }));
        return Ok(already);
    }
    if request_changes {
        return Err(AppError::Invalid(
            "Bitbucket aceita apenas comentários neste fluxo".into(),
        ));
    }
    let token = state
        .secrets
        .get(&SecretKind::Bitbucket.account())?
        .ok_or_else(|| AppError::Invalid("token Bitbucket ausente".into()))?;
    let scopes: Vec<String> = {
        let conn = state
            .db
            .lock()
            .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        conn.query_row(
            "SELECT scopes_json FROM credentials WHERE kind = 'bitbucket'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
    };
    if !scopes
        .iter()
        .any(|s| s == "write:pullrequest" || s == "write:pullrequest:bitbucket")
    {
        return Err(AppError::Invalid(
            "o token Bitbucket precisa do escopo write:pullrequest para publicar".into(),
        ));
    }
    let email = {
        let conn = state
            .db
            .lock()
            .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        crate::db::get_setting(&conn, "bitbucket.email")?
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let bb = crate::bitbucket::client::Client::new(&email, &token)?;
    let current: Value = bb
        .get_json(
            &format!("/repositories/{owner}/{repo}/pullrequests/{number}"),
            &[],
        )
        .await?
        .body;
    if current.get("state").and_then(Value::as_str) != Some("OPEN") {
        return Err(AppError::Invalid("a PR já não está aberta".into()));
    }
    if current
        .pointer("/source/commit/hash")
        .and_then(Value::as_str)
        != Some(sha.as_str())
    {
        return Err(AppError::Invalid(
            "a PR recebeu novos commits; revise novamente antes de publicar".into(),
        ));
    }
    let http = reqwest::Client::new();
    let endpoint = format!(
        "https://api.bitbucket.org/2.0/repositories/{owner}/{repo}/pullrequests/{number}/comments"
    );
    let mut remote_comments = Vec::new();
    for page in 1..=100 {
        let res = http
            .get(&endpoint)
            .basic_auth(&email, Some(&token))
            .query(&[("pagelen", "100"), ("page", &page.to_string())])
            .send()
            .await
            .map_err(|e| {
                AppError::Invalid(format!(
                    "não consegui conferir comentários Bitbucket existentes: {e}"
                ))
            })?;
        if !res.status().is_success() {
            return Err(AppError::Invalid(format!(
                "Bitbucket HTTP {} ao conferir duplicatas",
                res.status()
            )));
        }
        let data: Value = res
            .json()
            .await
            .map_err(|e| AppError::Invalid(format!("resposta Bitbucket inválida: {e}")))?;
        let Some(values) = data.get("values").and_then(Value::as_array) else {
            break;
        };
        let last = values.len() < 100;
        remote_comments.extend(values.iter().cloned());
        if last {
            break;
        }
    }
    let mut published = already;
    for f in &findings {
        let marker = format!("<!-- crivo:{run_id}:{} -->", f.seq);
        if let Some(old) = remote_comments.iter().find(|c| {
            c.pointer("/content/raw")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains(&marker))
        }) {
            let id = old
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::Invalid("comentário Bitbucket sem ID".into()))?
                .to_string();
            let url = old
                .pointer("/links/html/href")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?
                .execute("INSERT OR IGNORE INTO published_findings (run_id, finding_seq, remote_id, remote_url) VALUES (?1,?2,?3,?4)", params![run_id, f.seq, id, url])?;
            published.push(Published { seq: f.seq, url });
            continue;
        }
        let mut payload = json!({"content": {"raw": format!("{}\n\n{marker}", body(f))}});
        if let (Some(path), Some(line), true) = (&f.file, f.line, f.in_diff) {
            payload["inline"] = json!({"path": path, "to": line});
        }
        let res = http
            .post(&endpoint)
            .basic_auth(&email, Some(&token))
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                AppError::Invalid(format!(
                    "Bitbucket: publicação interrompida após {} comentário(s): {e}",
                    published.len()
                ))
            })?;
        if !res.status().is_success() {
            return Err(AppError::Invalid(format!(
                "Bitbucket HTTP {} após {} comentário(s)",
                res.status(),
                published.len()
            )));
        }
        let data: Value = res
            .json()
            .await
            .map_err(|e| AppError::Invalid(format!("resposta Bitbucket inválida: {e}")))?;
        let id = data
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| AppError::Invalid("Bitbucket não devolveu ID do comentário".into()))?
            .to_string();
        let url = data
            .pointer("/links/html/href")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let conn = state
            .db
            .lock()
            .map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        conn.execute("INSERT INTO published_findings (run_id, finding_seq, remote_id, remote_url) VALUES (?1,?2,?3,?4)", params![run_id, f.seq, id, url])?;
        published.push(Published { seq: f.seq, url });
    }
    Ok(published)
}
