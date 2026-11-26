//! Etapas e chamadas ao Claude de cada review: o log de terminal e o pipeline do painel, e o
//! custo por modelo da aba Uso.
//!
//! - `run_steps`: uma etapa por `Ctx::stage` (início e fim);
//! - `run_calls`: um processo `claude -p` por linha (`Ctx::run_claude`), com modelo, custo,
//!   duração e como terminou. Execuções anteriores à migration 0012 são importadas dos
//!   `*.result.json` da pasta delas (`backfill_run`), sem as etapas.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::error::AppResult;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Fecha a etapa aberta e abre outra. Devolve o número da nova.
pub fn step_start(conn: &Connection, run_id: &str, label: &str) -> AppResult<i64> {
    steps_close(conn, run_id)?;
    let seq: i64 = conn.query_row("SELECT ifnull(max(seq), 0) + 1 FROM run_steps WHERE run_id = ?1", [run_id], |r| r.get(0))?;
    conn.execute(
        &format!("INSERT INTO run_steps (run_id, seq, label, started_at) VALUES (?1, ?2, ?3, {NOW})"),
        params![run_id, seq, label],
    )?;
    Ok(seq)
}

/// Fecha a etapa aberta (fim do review, ou antes de abrir outra).
pub fn steps_close(conn: &Connection, run_id: &str) -> AppResult<()> {
    conn.execute(&format!("UPDATE run_steps SET finished_at = {NOW} WHERE run_id = ?1 AND finished_at IS NULL"), [run_id])?;
    Ok(())
}

/// Uma chamada ao Claude que terminou.
#[derive(Debug, Clone)]
pub struct NewCall {
    pub name: String,
    pub model: Option<String>,
    /// completed, failed, cancelled ou timed_out
    pub status: String,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub turns: Option<i64>,
}

/// `sonnet`, `haiku` ou `opus`: pelo `modelUsage` do resultado ou, sem ele, pelo nome do agente.
pub fn model_of(name: &str, result: Option<&Value>) -> Option<String> {
    let family = |s: &str| ["opus", "sonnet", "haiku"].into_iter().find(|m| s.contains(m)).map(str::to_owned);
    result
        .and_then(|r| r.get("modelUsage")?.as_object()?.keys().find_map(|k| family(k)))
        .or_else(|| family(name))
}

/// Grava a chamada na etapa aberta (ou na última) do review.
pub fn record_call(conn: &Connection, run_id: &str, c: &NewCall) -> AppResult<()> {
    let step: Option<i64> = conn.query_row("SELECT max(seq) FROM run_steps WHERE run_id = ?1", [run_id], |r| r.get(0))?;
    conn.execute(
        &format!(
            "INSERT INTO run_calls (run_id, step_seq, name, model, status, cost_usd, duration_ms, turns, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, {NOW})"
        ),
        params![run_id, step, c.name, c.model, c.status, c.cost_usd, c.duration_ms, c.turns],
    )?;
    Ok(())
}

/// Execução de antes das etapas: lê os `*.result.json` da pasta e grava as chamadas, uma vez.
/// Só a antiga e terminada: uma em andamento já tem o `result.json` antes de a chamada ser
/// gravada (contaria duas vezes), e toda execução nova tem etapa. Devolve quantas entraram.
pub fn backfill_run(conn: &Connection, run_id: &str) -> AppResult<usize> {
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT r.run_dir,
                    r.status != 'running'
                    AND NOT EXISTS (SELECT 1 FROM run_calls c WHERE c.run_id = r.id)
                    AND NOT EXISTS (SELECT 1 FROM run_steps s WHERE s.run_id = r.id)
             FROM review_runs r WHERE r.id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((dir, old)) = row else { return Ok(0) };
    if !old {
        return Ok(0);
    }
    let Ok(entries) = std::fs::read_dir(Path::new(&dir)) else { return Ok(0) };
    let mut files: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".result.json")).collect();
    files.sort();
    let mut n = 0;
    for f in files {
        let Some(name) = f.file_name().and_then(|x| x.to_str()).and_then(|x| x.strip_suffix(".result.json")).map(str::to_owned) else {
            continue;
        };
        let Some(r) = std::fs::read_to_string(&f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
        let failed = r["is_error"].as_bool() == Some(true) || r["subtype"].as_str().is_some_and(|s| s != "success");
        conn.execute(
            "INSERT INTO run_calls (run_id, name, model, status, cost_usd, duration_ms, turns, imported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
            params![
                run_id,
                name,
                model_of(&name, Some(&r)),
                if failed { "failed" } else { "completed" },
                r["total_cost_usd"].as_f64().unwrap_or(0.0),
                r["duration_ms"].as_i64(),
                r["num_turns"].as_i64(),
            ],
        )?;
        n += 1;
    }
    Ok(n)
}

#[derive(Debug, Serialize)]
pub struct StepView {
    pub seq: i64,
    pub label: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// Soma das chamadas ao Claude desta etapa.
    pub cost_usd: f64,
    /// Agentes que rodaram nela (`review-sonnet`, `audit-opus`…).
    pub calls: Vec<String>,
    /// Alguma chamada desta etapa não terminou bem.
    pub failed: bool,
}

pub fn steps(conn: &Connection, run_id: &str) -> AppResult<Vec<StepView>> {
    let mut stmt = conn.prepare(
        "SELECT s.seq, s.label, s.started_at, s.finished_at, ifnull(sum(c.cost_usd), 0),
                ifnull(group_concat(c.name, ','), ''), ifnull(max(c.status != 'completed'), 0)
         FROM run_steps s LEFT JOIN run_calls c ON c.run_id = s.run_id AND c.step_seq = s.seq
         WHERE s.run_id = ?1 GROUP BY s.seq ORDER BY s.seq",
    )?;
    let rows = stmt.query_map([run_id], |r| {
        let names: String = r.get(5)?;
        Ok(StepView {
            seq: r.get(0)?,
            label: r.get(1)?,
            started_at: r.get(2)?,
            finished_at: r.get(3)?,
            cost_usd: r.get(4)?,
            calls: names.split(',').filter(|x| !x.is_empty()).map(str::to_owned).collect(),
            failed: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Debug, Serialize)]
pub struct CallView {
    pub name: String,
    pub model: Option<String>,
    pub status: String,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub turns: Option<i64>,
    pub step_seq: Option<i64>,
    /// Lida dos arquivos de uma execução antiga (sem etapa nem hora).
    pub imported: bool,
}

pub fn calls(conn: &Connection, run_id: &str) -> AppResult<Vec<CallView>> {
    let mut stmt = conn.prepare(
        "SELECT name, model, status, cost_usd, duration_ms, turns, step_seq, imported FROM run_calls WHERE run_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([run_id], |r| {
        Ok(CallView {
            name: r.get(0)?,
            model: r.get(1)?,
            status: r.get(2)?,
            cost_usd: r.get(3)?,
            duration_ms: r.get(4)?,
            turns: r.get(5)?,
            step_seq: r.get(6)?,
            imported: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Debug, Serialize)]
pub struct AgentUsage {
    pub name: String,
    pub model: Option<String>,
    pub calls: i64,
    pub failed: i64,
    pub cost_total: f64,
    pub cost_avg: f64,
    pub duration_avg_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct RunCost {
    pub id: String,
    pub started_at: String,
    pub cost_usd: f64,
    /// `repo#123` ou `repo:branch`.
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub runs_completed: i64,
    pub cost_avg: f64,
    pub cost_min: f64,
    pub cost_max: f64,
    pub cost_total: f64,
    /// Por agente, do mais caro (no total) ao mais barato.
    pub agents: Vec<AgentUsage>,
    /// Os 30 reviews concluídos mais recentes, do mais antigo ao mais novo.
    pub recent: Vec<RunCost>,
}

/// Custo dos reviews e de cada agente. Importa antes as chamadas das execuções antigas.
pub fn usage(conn: &Connection) -> AppResult<Usage> {
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM review_runs r WHERE r.status != 'running'
               AND NOT EXISTS (SELECT 1 FROM run_calls c WHERE c.run_id = r.id)
               AND NOT EXISTS (SELECT 1 FROM run_steps s WHERE s.run_id = r.id)",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !ids.is_empty() {
        // uma transação só: na primeira abertura são uma centena de chamadas importadas
        let tx = conn.unchecked_transaction()?;
        for id in ids {
            backfill_run(&tx, &id)?;
        }
        tx.commit()?;
    }
    let (runs_completed, cost_avg, cost_min, cost_max, cost_total) = conn.query_row(
        "SELECT count(*), ifnull(avg(cost_usd), 0), ifnull(min(cost_usd), 0), ifnull(max(cost_usd), 0), ifnull(sum(cost_usd), 0)
         FROM review_runs WHERE status = 'completed' AND cost_usd > 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let agents = conn
        .prepare(
            "SELECT name, max(model), count(*), sum(status != 'completed'), sum(cost_usd), avg(cost_usd), avg(duration_ms)
             FROM run_calls GROUP BY name ORDER BY sum(cost_usd) DESC",
        )?
        .query_map([], |r| {
            Ok(AgentUsage {
                name: r.get(0)?,
                model: r.get(1)?,
                calls: r.get(2)?,
                failed: r.get(3)?,
                cost_total: r.get(4)?,
                cost_avg: r.get(5)?,
                duration_avg_ms: r.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut recent: Vec<RunCost> = conn
        .prepare(
            "SELECT rr.id, rr.started_at, rr.cost_usd,
                    CASE WHEN rr.pr_id IS NOT NULL THEN ifnull(r.slug, '?') || '#' || ifnull(p.bb_id, '?')
                         ELSE rr.repo_slug || ':' || rr.branch END
             FROM review_runs rr
             LEFT JOIN pull_requests p ON p.id = rr.pr_id
             LEFT JOIN repositories r ON r.id = p.repository_id
             WHERE rr.status = 'completed' AND rr.cost_usd > 0
             ORDER BY rr.started_at DESC LIMIT 30",
        )?
        .query_map([], |r| Ok(RunCost { id: r.get(0)?, started_at: r.get(1)?, cost_usd: r.get(2)?, label: r.get(3)? }))?
        .collect::<Result<_, _>>()?;
    recent.reverse();
    Ok(Usage { runs_completed, cost_avg, cost_min, cost_max, cost_total, agents, recent })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(conn: &Connection, id: &str, dir: &str, status: &str, cost: Option<f64>, started: &str) {
        conn.execute(
            "INSERT INTO review_runs (id, repo_slug, branch, status, run_dir, cost_usd, started_at) VALUES (?1, 'back', 'PROJ-1', ?2, ?3, ?4, ?5)",
            params![id, status, dir, cost, started],
        )
        .unwrap();
    }

    fn call(name: &str, status: &str, cost: f64) -> NewCall {
        NewCall { name: name.into(), model: model_of(name, None), status: status.into(), cost_usd: cost, duration_ms: Some(1500), turns: Some(3) }
    }

    #[test]
    fn etapas_e_chamadas_de_um_review() {
        let conn = crate::db::open_in_memory().unwrap();
        run(&conn, "r1", "/r1", "running", None, "2026-11-24T10:00:00Z");
        assert_eq!(step_start(&conn, "r1", "buscando o código do PR (git fetch)").unwrap(), 1);
        assert_eq!(step_start(&conn, "r1", "Sonnet revisando").unwrap(), 2);
        record_call(&conn, "r1", &call("review-sonnet", "completed", 0.28)).unwrap();
        step_start(&conn, "r1", "Opus auditando 1 achado(s)").unwrap();
        record_call(&conn, "r1", &call("audit-opus", "failed", 0.10)).unwrap();
        steps_close(&conn, "r1").unwrap();

        let st = steps(&conn, "r1").unwrap();
        assert_eq!(st.len(), 3);
        assert!(st.iter().all(|s| s.finished_at.is_some()), "{st:?}");
        assert_eq!((st[1].cost_usd, st[1].calls.clone(), st[1].failed), (0.28, vec!["review-sonnet".to_owned()], false));
        assert!(st[2].failed);
        let cs = calls(&conn, "r1").unwrap();
        assert_eq!(cs.iter().map(|c| (c.name.as_str(), c.step_seq, c.model.as_deref())).collect::<Vec<_>>(),
            vec![("review-sonnet", Some(2), Some("sonnet")), ("audit-opus", Some(3), Some("opus"))]);
    }

    #[test]
    fn modelo_pelo_resultado_ou_pelo_nome() {
        assert_eq!(model_of("x", Some(&json!({"modelUsage": {"claude-opus-5-5": {}}}))).as_deref(), Some("opus"));
        assert_eq!(model_of("quality-haiku", None).as_deref(), Some("haiku"));
        assert_eq!(model_of("judge-sonnet", Some(&json!({}))).as_deref(), Some("sonnet"));
        assert_eq!(model_of("outro", None), None);
    }

    #[test]
    fn importa_as_chamadas_de_um_review_antigo() {
        let conn = crate::db::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("prr-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("review-sonnet.result.json"),
            json!({"total_cost_usd": 0.27, "duration_ms": 1000, "num_turns": 2, "is_error": false, "subtype": "success",
                   "modelUsage": {"claude-sonnet-5-5": {}}}).to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("audit-opus.result.json"), json!({"total_cost_usd": 0.1, "is_error": true}).to_string()).unwrap();
        std::fs::write(dir.join("lixo.result.json"), "{não é json").unwrap();
        run(&conn, "r1", &dir.display().to_string(), "completed", Some(0.37), "2026-11-20T10:00:00Z");

        assert_eq!(backfill_run(&conn, "r1").unwrap(), 2);
        assert_eq!(backfill_run(&conn, "r1").unwrap(), 0, "não importa duas vezes");
        let cs = calls(&conn, "r1").unwrap();
        let by: Vec<(&str, &str, bool)> = cs.iter().map(|c| (c.name.as_str(), c.status.as_str(), c.imported)).collect();
        assert!(by.contains(&("review-sonnet", "completed", true)), "{by:?}");
        assert!(by.contains(&("audit-opus", "failed", true)), "{by:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uso_so_importa_execucao_antiga_e_terminada() {
        let conn = crate::db::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("prr-usage-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review-sonnet.result.json"), r#"{"total_cost_usd": 0.3}"#).unwrap();
        let d = dir.display().to_string();
        // em andamento: o result.json já existe, mas a chamada ainda vai ser gravada pelo review
        run(&conn, "rodando", &d, "running", None, "2026-11-24T10:00:00Z");
        // nova (tem etapas) e falhou antes de gravar chamada: não é antiga
        run(&conn, "nova", &d, "failed", None, "2026-11-24T11:00:00Z");
        step_start(&conn, "nova", "buscando o código do PR (git fetch)").unwrap();
        run(&conn, "antiga", &d, "completed", Some(0.3), "2026-11-20T10:00:00Z");
        usage(&conn).unwrap();
        let n = |id: &str| conn.query_row("SELECT count(*) FROM run_calls WHERE run_id = ?1", [id], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!((n("rodando"), n("nova"), n("antiga")), (0, 0, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uso_agrega_reviews_e_agentes() {
        let conn = crate::db::open_in_memory().unwrap();
        run(&conn, "a", "/nada/a", "completed", Some(0.2), "2026-11-20T10:00:00Z");
        run(&conn, "b", "/nada/b", "completed", Some(0.4), "2026-11-21T10:00:00Z");
        run(&conn, "c", "/nada/c", "failed", Some(0.05), "2026-11-22T10:00:00Z");
        record_call(&conn, "a", &call("review-sonnet", "completed", 0.2)).unwrap();
        record_call(&conn, "b", &call("review-sonnet", "completed", 0.25)).unwrap();
        record_call(&conn, "b", &call("audit-opus", "completed", 0.15)).unwrap();
        record_call(&conn, "c", &call("quality-haiku", "failed", 0.01)).unwrap();

        let u = usage(&conn).unwrap();
        assert_eq!(u.runs_completed, 2);
        assert!((u.cost_avg - 0.3).abs() < 1e-9 && u.cost_min == 0.2 && u.cost_max == 0.4, "{u:?}");
        assert_eq!(u.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["review-sonnet", "audit-opus", "quality-haiku"]);
        let sonnet = &u.agents[0];
        assert_eq!((sonnet.calls, sonnet.failed), (2, 0));
        assert!((sonnet.cost_avg - 0.225).abs() < 1e-9);
        assert_eq!(u.agents[2].failed, 1);
        // os mais recentes, em ordem de data, só os concluídos
        assert_eq!(u.recent.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }
}
