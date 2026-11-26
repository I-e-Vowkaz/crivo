//! Browser Tester (Haiku + Claude in Chrome).
//!
//! Decisão de 2026-11-17: só testa se o checkout do usuário **já estiver no commit do PR**
//! (o docker e o `serve` locais rodam a partir dele). Caso contrário fica `blocked` com o
//! motivo; nada é trocado sozinho no repositório do usuário.

use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};

use super::claude::{self, Spec, Status};
use super::{Ctx, PrForReview, agents_json, assets, cache_5m, git, worker_config};
use crate::error::{AppError, AppResult};
use crate::secrets::test_password_account;
use crate::testenv;

const BROWSER_TIMEOUT: Duration = Duration::from_secs(25 * 60);

fn set(ctx: &Ctx, status: &str, note: &str) -> AppResult<()> {
    ctx.with_db(|c| {
        Ok(c.execute(
            "UPDATE review_runs SET browser_status = ?1, browser_note = ?2 WHERE id = ?3",
            params![status, note, ctx.run_id],
        )?)
    })?;
    ctx.log(&format!("navegador: {status} — {note}"));
    Ok(())
}

/// Mascara o usuário e a senha de teste (a partir de 3 caracteres) no log e nos resultados.
/// O usuário também sai: pode ser um CPF.
fn mask(secrets: &[&str], line: &str) -> String {
    secrets
        .iter()
        .filter(|s| s.chars().count() >= 3)
        .fold(line.to_owned(), |acc, s| acc.replace(*s, "***"))
}

/// Roda o Browser Tester ou registra por que não rodou. Devolve o custo.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    ctx: &Ctx,
    pr: &PrForReview,
    project: &str,
    repo: &Path,
    src: &str,
    base: &str,
    wt: &Path,
    card_key: Option<&str>,
    card_path: &Path,
    findings: &[Value],
) -> AppResult<f64> {
    // 1. ambiente configurado para o repo
    let (app, password, jira_site) = {
        let st = ctx.state();
        let conn = st.db.lock().map_err(|_| AppError::Invalid("banco indisponível".into()))?;
        match testenv::for_repo(&conn, &pr.slug)? {
            Some(app) => {
                let pw = st.secrets.get(&test_password_account(&app.id))?.unwrap_or_default();
                (app, pw, super::jira_site(&conn)?)
            }
            None => {
                drop(conn);
                set(ctx, "blocked", &format!("nenhum ambiente de teste configurado para {} (Configurações)", pr.slug))?;
                return Ok(0.0);
            }
        }
    };

    // 2. front e back do mesmo card: um teste só, e com os dois lados no lugar certo
    let siblings = siblings(ctx, pr, &app).await?;
    if let Some((reason, checkout)) = sibling_block(project, &pr.source_branch, &siblings) {
        if let Some(id) = checkout {
            set_checkout(ctx, id)?;
        }
        set(ctx, "blocked", &reason)?;
        return Ok(0.0);
    }

    // 3. o checkout do usuário está no commit do PR?
    let head = git::run(repo, &["rev-parse", "HEAD"]).await?;
    if head != src {
        let branch = git::run(repo, &["branch", "--show-current"]).await.unwrap_or_default();
        set_checkout(ctx, ctx.pr_id)?;
        set(
            ctx,
            "blocked",
            &format!(
                "seu checkout de {} está em {} ({}), não no commit do PR; faça o checkout de {} para testar no navegador",
                pr.slug,
                if branch.is_empty() { "HEAD destacado" } else { branch.as_str() },
                &head[..head.len().min(10)],
                pr.source_branch
            ),
        )?;
        return Ok(0.0);
    }

    // 4. o ambiente responde?
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    if let Err(e) = http.get(&app.base_url).send().await {
        set(ctx, "blocked", &format!("o ambiente {} ({}) não respondeu: {e}", app.name, app.base_url))?;
        return Ok(0.0);
    }

    // 5. um navegador só: fila global
    ctx.stage("aguardando o navegador (um teste por vez)");
    let st = ctx.state();
    let _browser = st.browser_lock.lock().await;
    ctx.check_cancel()?;
    set(ctx, "running", &format!("{} em {}", app.name, app.base_url))?;
    ctx.stage("Haiku testando no navegador");

    let bugs: Vec<&Value> = findings.iter().filter(|f| f["validation"] == "ok" && f["kind"] == "bug").collect();
    let findings_path = ctx.run_dir.join("browser-findings.json");
    std::fs::write(&findings_path, serde_json::to_vec_pretty(&bugs)?)?;
    let changed = git::run(wt, &["diff", "--name-only", &format!("{base}...{src}")]).await.unwrap_or_default();

    let mut opts: Vec<String> = vec![
        "--agent".into(), "browser-tester".into(), "--agents".into(), agents_json(&["browser-tester"])?,
        "--model".into(), "haiku".into(), "--effort".into(), "low".into(), "--chrome".into(),
        "--json-schema".into(), std::fs::read_to_string(assets().join("schemas/browser-results.json"))?,
        "--add-dir".into(), ctx.run_dir.display().to_string(),
        "--allowedTools".into(), if card_key.is_some() { "mcp__claude-in-chrome__* ToolSearch mcp__atlassian__getJiraIssue Read" } else { "mcp__claude-in-chrome__* Read" }.into(),
        "--disallowedTools".into(),
        "Edit Write NotebookEdit Agent mcp__claude-in-chrome__file_upload mcp__claude-in-chrome__upload_image".into(),
        "--max-turns".into(), "60".into(), "--max-budget-usd".into(), "3".into(),
    ];
    opts.extend(worker_config(true, card_key.is_some()));
    let spec = Spec {
        session: claude::Session::Ephemeral,
        name: "browser-haiku".into(),
        opts,
        prompt: format!(
            "BASE_URL: {}\nLOGIN_URL: {}\nUSUARIO: {}\nSENHA: {}\nCARD_KEY: {}\nJIRA_SITE: {}\nCARD_FILE: {}\nFINDINGS_FILE: {}\nCHANGED_FILES:\n{}\n\nTeste o PR {}#{} ({}).",
            app.base_url,
            if app.login_url.is_empty() { &app.base_url } else { &app.login_url },
            app.username,
            password,
            card_key.unwrap_or("(nenhuma)"),
            jira_site.as_deref().unwrap_or("(não configurado)"),
            card_path.display(),
            findings_path.display(),
            changed,
            pr.slug,
            pr.bb_id,
            pr.title
        ),
        cwd: ctx.run_dir.clone(),
        log_dir: ctx.run_dir.clone(),
        env_extra: vec![("PRO_ALLOWED_HOSTS".into(), app.hosts().join(",")), cache_5m()],
        timeout: BROWSER_TIMEOUT,
    };
    let hide = [password.as_str(), app.username.as_str()];
    let out = ctx.run_claude(spec, "Haiku testando no navegador", |l| ctx.log(&mask(&hide, &l))).await?;
    let cost = out.result.as_ref().and_then(|r| r.get("total_cost_usd")).and_then(Value::as_f64).unwrap_or(0.0);

    let so = out.result.as_ref().and_then(|r| r.get("structured_output")).cloned();
    match (out.status, so) {
        (Status::Completed, Some(so)) => {
            let mut tests: Vec<Value> = so.get("tests").and_then(Value::as_array).cloned().unwrap_or_default();
            enforce_evidence(&mut tests);
            ctx.with_db(|c| {
                for (i, t) in tests.iter().enumerate() {
                    c.execute(
                        "INSERT INTO browser_tests (run_id, seq, title, url, preconditions, actions_json, expected,
                           observed, evidence, status, finding_ref)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            ctx.run_id,
                            (i + 1) as i64,
                            t["title"].as_str(),
                            t["url"].as_str(),
                            t["preconditions"].as_str(),
                            t.get("actions").map(|a| a.to_string()).unwrap_or_else(|| "[]".into()),
                            t["expected"].as_str(),
                            t["observed"].as_str().map(|s| mask(&hide, s)),
                            t["evidence"].as_str().map(|s| mask(&hide, s)),
                            t["status"].as_str().unwrap_or("nao_executado"),
                            t["finding_ref"].as_str(),
                        ],
                    )?;
                }
                Ok(())
            })?;
            let note = so.get("note").and_then(Value::as_str).unwrap_or("");
            set(ctx, "completed", &mask(&hide, &format!("{} teste(s). {note}", tests.len())))?;
        }
        (Status::Cancelled, _) => return Err(AppError::Invalid("cancelado".into())),
        (status, _) => {
            // o review de código continua valendo; só o teste no navegador falhou
            let why = out.result.as_ref().and_then(|r| r.get("result")).map(|v| v.to_string()).unwrap_or(out.stderr_tail);
            set(ctx, "failed", &mask(&hide, &format!("{status:?}: {}", why.chars().take(400).collect::<String>())))?;
        }
    }
    Ok(cost)
}

/// O outro lado (front ou back) do mesmo card, em outro repositório do mesmo ambiente.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Sibling {
    /// PR aberto na mesma branch. `at_commit`: o checkout do usuário está no commit desse PR.
    Pr { kind: &'static str, slug: String, bb_id: i64, pr_id: i64, at_commit: bool },
    /// Só a branch no remoto, sem PR aberto que o app conheça.
    Branch { kind: &'static str, slug: String },
}

/// Outros repositórios do ambiente com a mesma branch (PR aberto no banco ou branch no remoto).
async fn siblings(ctx: &Ctx, pr: &PrForReview, app: &testenv::TestApp) -> AppResult<Vec<Sibling>> {
    let (root, prs) = ctx.with_db(|c| {
        let mut stmt = c.prepare(
            "SELECT r.slug, p.bb_id, p.source_sha, p.id FROM pull_requests p JOIN repositories r ON r.id = p.repository_id
             WHERE p.state = 'OPEN' AND p.source_branch = ?1 AND r.slug != ?2",
        )?;
        let prs: Vec<(String, i64, String, i64)> = stmt
            .query_map(params![pr.source_branch, pr.slug], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        Ok((super::repos_root(c)?, prs))
    })?;
    let mut out = Vec::new();
    for slug in app.repos.iter().filter(|s| **s != pr.slug) {
        let clone = root.join(slug);
        if !clone.join(".git").exists() {
            continue;
        }
        let kind = super::project_kind(&clone);
        if let Some((_, bb_id, sha, pr_id)) = prs.iter().find(|(s, _, _, _)| s == slug) {
            let head = git::run(&clone, &["rev-parse", "HEAD"]).await.unwrap_or_default();
            let at_commit = !head.is_empty() && (head.starts_with(sha.as_str()) || sha.starts_with(head.as_str()));
            out.push(Sibling::Pr { kind, slug: slug.clone(), bb_id: *bb_id, pr_id: *pr_id, at_commit });
        } else {
            let ls = git::raw(&clone, &["ls-remote", "--heads", "origin", &pr.source_branch]).await?;
            if ls.code == 0 && !ls.stdout.is_empty() {
                out.push(Sibling::Branch { kind, slug: slug.clone() });
            }
        }
    }
    Ok(out)
}

/// Decide se o teste no navegador deste PR deve ficar de fora (devolve o motivo):
/// - PR do back com o front tendo a mesma branch: o teste fica com o front (evita duplicar);
/// - PR do front com PR do back na mesma branch: o checkout do back também precisa estar no PR.
/// Devolve o motivo e, quando um checkout resolve, o id do PR para o botão Checkout.
pub(super) fn sibling_block(project: &str, branch: &str, siblings: &[Sibling]) -> Option<(String, Option<i64>)> {
    match project {
        "back" => siblings.iter().find_map(|s| match s {
            Sibling::Pr { kind: "front", slug, bb_id, .. } => Some((format!(
                "o teste no navegador fica com o PR do front {slug}#{bb_id} (mesma branch {branch}), para não testar o mesmo fluxo duas vezes"
            ), None)),
            Sibling::Branch { kind: "front", slug } => Some((format!(
                "o front {slug} também tem a branch {branch}; o teste no navegador fica com o review do front, para não testar o mesmo fluxo duas vezes"
            ), None)),
            _ => None,
        }),
        "front" => siblings.iter().find_map(|s| match s {
            Sibling::Pr { kind: "back", slug, bb_id, pr_id, at_commit: false } => Some((format!(
                "o back {slug}#{bb_id} tem a mesma branch {branch}, mas o seu checkout do back não está no commit desse PR; faça o checkout dele também para testar o fluxo completo"
            ), Some(*pr_id))),
            _ => None,
        }),
        _ => None,
    }
}

fn set_checkout(ctx: &Ctx, pr_id: i64) -> AppResult<()> {
    ctx.with_db(|c| {
        Ok(c.execute("UPDATE review_runs SET browser_checkout_pr_id = ?1 WHERE id = ?2", params![pr_id, ctx.run_id])?)
    })?;
    Ok(())
}

/// "Aprovado" sem evidência registrada vira "não executado" (regra da seção 5.5 do plano).
fn enforce_evidence(tests: &mut [Value]) {
    for t in tests.iter_mut() {
        let empty = t["evidence"].as_str().map(|s| s.trim().is_empty()).unwrap_or(true);
        if t["status"] == "aprovado" && empty {
            t["status"] = json!("nao_executado");
            t["observed"] = json!(format!("{} (sem evidência registrada)", t["observed"].as_str().unwrap_or("")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aprovado_sem_evidencia_nao_vale() {
        let mut t = vec![
            json!({"status": "aprovado", "evidence": "  ", "observed": "ok"}),
            json!({"status": "aprovado", "evidence": "Salvo com sucesso"}),
            json!({"status": "reprovado", "evidence": ""}),
        ];
        enforce_evidence(&mut t);
        assert_eq!(t[0]["status"], "nao_executado");
        assert_eq!(t[1]["status"], "aprovado");
        assert_eq!(t[2]["status"], "reprovado");
    }

    #[test]
    fn front_e_back_do_mesmo_card() {
        let front_pr = Sibling::Pr { kind: "front", slug: "f".into(), bb_id: 7, pr_id: 70, at_commit: false };
        let front_branch = Sibling::Branch { kind: "front", slug: "f".into() };
        let back_ok = Sibling::Pr { kind: "back", slug: "b".into(), bb_id: 9, pr_id: 90, at_commit: true };
        let back_off = Sibling::Pr { kind: "back", slug: "b".into(), bb_id: 9, pr_id: 90, at_commit: false };

        // back: fica de fora se o front tem PR ou branch; roda se o front não tem
        let (why, checkout) = sibling_block("back", "PROJ-1", &[front_pr]).unwrap();
        assert!(why.contains("f#7") && checkout.is_none(), "não é checkout que resolve: o teste é do front");
        assert!(sibling_block("back", "PROJ-1", &[front_branch.clone()]).unwrap().0.contains("também tem a branch"));
        assert_eq!(sibling_block("back", "PROJ-1", &[]), None);

        // front: roda, mas exige o back no commit do PR quando existe PR do back
        assert_eq!(sibling_block("front", "PROJ-1", &[back_ok]), None);
        let (why, checkout) = sibling_block("front", "PROJ-1", &[back_off]).unwrap();
        assert!(why.contains("b#9"));
        assert_eq!(checkout, Some(90), "o botão faz checkout do PR do back");
        assert_eq!(sibling_block("front", "PROJ-1", &[front_branch]), None);
    }

    #[test]
    fn senha_mascarada_no_log() {
        assert_eq!(mask(&["abc123", "05600000000"], "login 05600000000 senha abc123"), "login *** senha ***");
        assert_eq!(mask(&["ab"], "ab cd"), "ab cd");
    }
}
