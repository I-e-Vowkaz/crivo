//! Publicação explícita de achados validados num review do PR. Só no GitHub: o Crivo não
//! escreve no Bitbucket (`hosting::Kind::can_write`).
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::State;

use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::hosting::{Provider, RepoRef};

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

/// O PR de um review concluído (repositório, número e commit revisado), se o provedor aceita
/// escrita. Recusa antes de qualquer chamada à API.
pub(crate) fn writable_target(conn: &Connection, run_id: &str) -> AppResult<(RepoRef, i64, String)> {
    let (rr, number, sha) = conn
        .query_row(
            "SELECT pvd.provider, pvd.workspace, pvd.slug, p.bb_id, rr.source_sha
             FROM review_runs rr JOIN pull_requests p ON p.id = rr.pr_id
             JOIN repositories pvd ON pvd.id = p.repository_id
             WHERE rr.id = ?1 AND rr.status = 'completed'",
            [run_id],
            |r| Ok((RepoRef::from_row(&r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid("review concluído não encontrado".into()))?;
    if !rr.kind.can_write() {
        return Err(AppError::Invalid(format!("o Crivo não escreve no {}", rr.kind.name())));
    }
    Ok((rr, number, sha))
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
    let (rr, number, sha, findings, mut already) = {
        let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        let (rr, number, sha) = writable_target(&conn, &run_id)?;
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
        (rr, number, sha, findings, already)
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
    let inline: Vec<Value> = findings
        .iter()
        .filter_map(|f| match (&f.file, f.line, f.in_diff) {
            (Some(file), Some(line), true) if line > 0 => Some(json!({"path": file, "line": line, "side": "RIGHT", "body": body(f)})),
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
        findings.iter().map(|f| f.seq.to_string()).collect::<Vec<_>>().join(",")
    );
    let review_body = format!("Review do Crivo (execução {run_id}).\n\n{summary}\n\n{marker}");
    let provider = Provider::connect(rr.kind, &state.db, state.secrets.as_ref()).await?;
    let (remote, url) = provider.publish(&rr, number, &sha, &review_body, &marker, &inline, request_changes).await?;
    let conn = state.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
    for f in &findings {
        conn.execute(
            "INSERT OR IGNORE INTO published_findings (run_id, finding_seq, remote_id, remote_url) VALUES (?1,?2,?3,?4)",
            params![run_id, f.seq, remote, url],
        )?;
    }
    already.extend(findings.into_iter().map(|f| Published { seq: f.seq, url: url.clone() }));
    Ok(already)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(conn: &rusqlite::Connection, provider: &str) {
        conn.execute(
            "INSERT INTO repositories (id, provider, workspace, slug) VALUES (1, ?1, 'acme', 'app')",
            [provider],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, reviewers_json, participants_json,
               source_branch, dest_branch, source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 7, 't', 'u', 'a', '[]', '[]', 'PROJ-1', 'main', 'abc', 'd', 'OPEN', 'x', 'x', 'review_requested');
             INSERT INTO review_runs (id, pr_id, source_sha, status, run_dir) VALUES ('r1', 1, 'abc', 'completed', '/r');",
        )
        .unwrap();
    }

    #[test]
    fn bitbucket_nao_recebe_publicacao() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "bitbucket");
        assert_eq!(writable_target(&conn, "r1").unwrap_err().to_string(), "o Crivo não escreve no Bitbucket");
    }

    #[test]
    fn github_recebe_publicacao() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "github");
        let (rr, number, sha) = writable_target(&conn, "r1").unwrap();
        assert_eq!((rr.label(number), sha.as_str()), ("acme/app#7".to_owned(), "abc"));
    }
}
