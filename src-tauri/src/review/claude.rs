//! Executa `claude -p` com stream-json, como o runner da Fase 0 (`spikes/claude_run.py`):
//! - processo em grupo próprio, para cancelar a árvore toda;
//! - leitura linha a linha; cada evento vira uma linha curta para a UI e para o log;
//! - subagentes identificados pelo id do tool `Agent` (`parent_tool_use_id`);
//! - cancelamento: SIGINT → 10 s → SIGTERM no grupo → 5 s → SIGKILL no grupo;
//! - ambiente sem variáveis de credencial;
//! - `--` antes do prompt: `--allowedTools` é variádico e engoliria o prompt (achado 1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::error::{AppError, AppResult};
use crate::proc::{self, First};

const SECRET_ENV: [&str; 6] = ["BITBUCKET", "JIRA", "ATLASSIAN", "TOKEN", "SECRET", "PASSWORD"];

/// Controle de uma execução em andamento: pid do processo atual e pedido de cancelamento.
#[derive(Default)]
pub struct RunCtl {
    pid: AtomicI32,
    cancelled: AtomicBool,
}

impl RunCtl {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Pede o cancelamento e escala os sinais em segundo plano.
    pub fn cancel(self: &Arc<Self>) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid <= 0 {
            return;
        }
        tauri::async_runtime::spawn(proc::escalate(pid, (First::Pid(pid), libc::SIGINT), &CANCEL_STEPS));
    }
}

/// Depois do SIGINT no `claude`: SIGTERM no grupo em 10 s e SIGKILL em mais 5 s.
static CANCEL_STEPS: [(Duration, i32); 2] =
    [(Duration::from_secs(10), libc::SIGTERM), (Duration::from_secs(5), libc::SIGKILL)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// Limite de uso do Claude (janela de 5 h ou semanal) reconhecido numa execução que falhou.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Limit {
    /// Trecho da mensagem do Claude.
    pub message: String,
    /// Liberação (epoch, segundos), quando a mensagem traz (`…|1763485200`).
    pub reset_at: Option<i64>,
    /// Texto de liberação, quando vem por extenso (ex.: "resets 3pm").
    pub reset_text: Option<String>,
}

/// Reconhece a mensagem de limite de uso. Só deve ser usado com execuções que **falharam**:
/// uma resposta normal pode falar de "limit" no código.
pub fn detect_limit(text: &str) -> Option<Limit> {
    let low = text.to_lowercase();
    const KEYS: [&str; 8] = [
        "usage limit",
        "limit reached",
        "hit your limit",
        "5-hour limit",
        "weekly limit",
        "out of extra usage",
        "rate_limit_error",
        "rate limit exceeded",
    ];
    if !KEYS.iter().any(|k| low.contains(k)) {
        return None;
    }
    // formato antigo: "Claude AI usage limit reached|1763485200"
    let reset_at = text.split('|').skip(1).find_map(|p| {
        let d: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        (d.len() == 10).then(|| d.parse::<i64>().ok()).flatten()
    });
    let reset_text = low.find("reset").map(|i| {
        text[i..].chars().take_while(|c| *c != '\n' && *c != '|').take(60).collect::<String>().trim().trim_end_matches('.').to_owned()
    });
    let message = text.lines().find(|l| KEYS.iter().any(|k| l.to_lowercase().contains(k))).unwrap_or(text);
    Some(Limit { message: message.chars().take(200).collect::<String>().trim().to_owned(), reset_at, reset_text })
}

pub struct Outcome {
    /// Preenchido quando a execução falhou por limite de uso do Claude.
    pub limit: Option<Limit>,
    pub status: Status,
    /// Evento `result` final (traz `structured_output`, `total_cost_usd`...).
    pub result: Option<Value>,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
    /// Subagentes lançados pela sessão (tipo ou descrição do tool `Agent`).
    pub subagents: Vec<String>,
}

/// Sessão do Claude Code: as do review não são gravadas; as do chat continuam entre perguntas.
#[derive(Clone)]
pub enum Session {
    /// `--no-session-persistence` (padrão do review)
    Ephemeral,
    /// Primeira pergunta do chat: `--session-id <uuid>`
    New(String),
    /// Perguntas seguintes: `--resume <uuid>`
    Resume(String),
}

#[derive(Clone)]
pub struct Spec {
    pub session: Session,
    pub name: String,
    pub opts: Vec<String>,
    pub prompt: String,
    pub cwd: PathBuf,
    pub log_dir: PathBuf,
    pub env_extra: Vec<(String, String)>,
    pub timeout: Duration,
}

/// `claude` no PATH ou em `~/.local/bin` (o app aberto pelo menu nem sempre herda o PATH do shell).
pub fn claude_bin() -> AppResult<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".local/bin"));
    }
    dirs.into_iter()
        .map(|d| d.join("claude"))
        .find(|p| p.is_file())
        .ok_or_else(|| AppError::Invalid("não achei o executável `claude` (PATH nem ~/.local/bin)".into()))
}

pub fn clean_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            k == "CLAUDE_CODE_OAUTH_TOKEN" || !SECRET_ENV.iter().any(|s| up.contains(s))
        })
        .collect();
    env.extend(extra.iter().cloned());
    env
}

/// Estado de leitura do stream: quem é cada subagente (pelo id do tool `Agent`).
#[derive(Default)]
struct Agents {
    names: HashMap<String, String>,
    /// Subagentes em segundo plano: o `tool_result` deles só confirma o lançamento
    /// ("Async agent launched"); o fim real vem em `system/task_notification` (achado 4).
    background: std::collections::HashSet<String>,
    /// Última ferramenta mostrada por subagente, para não repetir a mesma linha.
    last_tool: HashMap<String, String>,
    /// Subagentes cujas ações já chegam no stream (`parent_tool_use_id`): para eles o
    /// `task_progress` só repetiria o nome da ferramenta, sem detalhe.
    forwarded: std::collections::HashSet<String>,
    /// Subagentes em segundo plano ainda rodando (entre `task_started` e `task_notification`).
    running: std::collections::HashSet<String>,
}

fn short(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>()) } else { s }
}

/// Nome curto da ferramenta: `mcp__claude-in-chrome__navigate` → `chrome navigate`.
fn tool_label(name: &str) -> String {
    if let Some(t) = name.strip_prefix("mcp__claude-in-chrome__") {
        format!("chrome {t}")
    } else if let Some(t) = name.strip_prefix("mcp__atlassian__") {
        format!("jira {t}")
    } else {
        name.to_owned()
    }
}

fn tool_detail(name: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).map(|v| short(v, 140));
    let chrome = name.strip_prefix("mcp__claude-in-chrome__");
    let detail = match (name, chrome) {
        ("Bash", _) => pick("command"),
        ("Read", _) => pick("file_path"),
        ("Grep" | "Glob", _) => pick("pattern"),
        ("Agent" | "Task", _) => pick("description"),
        ("ToolSearch", _) => pick("query"),
        ("mcp__atlassian__getJiraIssue", _) => pick("issueIdOrKey").or_else(|| Some(String::new())),
        (_, Some("navigate")) => pick("url"),
        (_, Some("computer")) => pick("action_summary").or_else(|| pick("action")),
        (_, Some("find")) => pick("query"),
        (_, Some("form_input")) => input.get("ref").map(|r| format!("campo {}", short(r, 20))),
        (_, Some("read_page")) => pick("filter").or_else(|| Some(String::new())),
        (_, Some(_)) => Some(String::new()),
        _ => None,
    };
    detail.unwrap_or_else(|| short(input, 140))
}

/// Resumo do resultado entregue pela sessão (`StructuredOutput`).
fn structured_summary(input: &Value) -> String {
    let count = |k: &str| input.get(k).and_then(Value::as_array).map(Vec::len);
    match (count("findings"), count("tests"), count("audits")) {
        (Some(n), _, _) => format!("{n} achado(s)"),
        (_, Some(n), _) => format!("{n} teste(s)"),
        (_, _, Some(n)) => format!("{n} decisão(ões)"),
        _ => "resultado".into(),
    }
}

/// Linhas curtas (para UI e log) a partir de um evento do stream.
fn describe(ev: &Value, agents: &mut Agents) -> Vec<String> {
    let parent = ev.get("parent_tool_use_id").and_then(Value::as_str);
    let who = parent.and_then(|p| agents.names.get(p).cloned()).unwrap_or_else(|| "principal".into());
    let mut out = Vec::new();
    match ev.get("type").and_then(Value::as_str) {
        Some("system") => {
            let sub = ev.get("subtype").and_then(Value::as_str).unwrap_or("");
            let id = ev.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_owned();
            let name = || {
                agents.names.get(&id).cloned().unwrap_or_else(|| {
                    ev.get("subagent_type").and_then(Value::as_str).unwrap_or("subagente").to_owned()
                })
            };
            match sub {
                "init" => {
                    out.push(format!("sessão iniciada, modelo {}", short(ev.get("model").unwrap_or(&Value::Null), 60)));
                }
                "task_started" => {
                    let n = name();
                    agents.names.entry(id.clone()).or_insert_with(|| n.clone());
                    if ev.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
                        agents.background.insert(id.clone());
                        agents.running.insert(id);
                        out.push(format!("subagente {n} rodando em segundo plano"));
                    }
                }
                "task_progress" if !agents.forwarded.contains(&id) => {
                    if let Some(tool) = ev.get("last_tool_name").and_then(Value::as_str) {
                        if agents.last_tool.get(&id).map(String::as_str) != Some(tool) {
                            agents.last_tool.insert(id.clone(), tool.to_owned());
                            out.push(format!("{}: {}", name(), tool_label(tool)));
                        }
                    }
                }
                "task_notification" => {
                    let status = ev.get("status").and_then(Value::as_str).unwrap_or("?");
                    agents.running.remove(&id);
                    let n = name();
                    out.push(match status {
                        "completed" => format!("subagente {n} terminou"),
                        other => format!("subagente {n} terminou: {other}"),
                    });
                }
                _ => {}
            }
        }
        Some("assistant") => {
            if let Some(p) = parent {
                agents.forwarded.insert(p.to_owned());
            }
            for b in ev.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                    let input = b.get("input").unwrap_or(&Value::Null);
                    if matches!(name, "Agent" | "Task") {
                        let sub = input
                            .get("subagent_type")
                            .and_then(Value::as_str)
                            .or_else(|| input.get("description").and_then(Value::as_str))
                            .unwrap_or("subagente")
                            .to_owned();
                        if let Some(id) = b.get("id").and_then(Value::as_str) {
                            agents.names.insert(id.to_owned(), sub.clone());
                        }
                        out.push(format!("{who}: lançou o subagente {sub}"));
                    } else if name == "StructuredOutput" {
                        let pending: Vec<String> =
                            agents.running.iter().filter_map(|id| agents.names.get(id).cloned()).collect();
                        out.push(if pending.is_empty() {
                            format!("{who}: entregou o resultado ({})", structured_summary(input))
                        } else {
                            format!(
                                "{who}: resultado parcial ({}) com {} ainda rodando; vale o que vier depois",
                                structured_summary(input),
                                pending.join(", ")
                            )
                        });
                    } else {
                        out.push(format!("{who}: {} {}", tool_label(name), tool_detail(name, input)).trim_end().to_owned());
                    }
                }
            }
        }
        Some("user") => {
            for b in ev.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let content = short(b.get("content").unwrap_or(&Value::Null), 300);
                if content.contains("Bloqueado pelo PR Review Orchestrator") {
                    out.push(format!("{who}: comando bloqueado pelo hook de proteção"));
                }
                let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                if let Some(name) = agents.names.get(id) {
                    let err = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    if agents.background.contains(id) || content.contains("Async agent launched") {
                        // só a confirmação do lançamento; o fim vem em task_notification
                        agents.background.insert(id.to_owned());
                    } else {
                        out.push(format!("subagente {name} {}", if err { "terminou com erro" } else { "terminou" }));
                    }
                }
            }
        }
        Some("result") => {
            out.push(format!(
                "turno encerrado: {} turnos, US$ {:.2} acumulado, {:.0}s",
                ev.get("num_turns").and_then(Value::as_u64).unwrap_or(0),
                ev.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                ev.get("duration_ms").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0
            ));
        }
        _ => {}
    }
    out
}

pub async fn run(spec: Spec, ctl: Arc<RunCtl>, on_line: impl Fn(String) + Send + Sync) -> AppResult<Outcome> {
    tokio::fs::create_dir_all(&spec.log_dir).await?;
    let events_path = spec.log_dir.join(format!("{}.log", spec.name));
    let stderr_path = spec.log_dir.join(format!("{}.stderr.txt", spec.name));
    let result_path = spec.log_dir.join(format!("{}.result.json", spec.name));

    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"].iter().map(|s| s.to_string()).collect();
    match &spec.session {
        Session::Ephemeral => args.push("--no-session-persistence".into()),
        Session::New(id) => args.extend(["--session-id".into(), id.clone()]),
        Session::Resume(id) => args.extend(["--resume".into(), id.clone()]),
    }
    args.extend(spec.opts.iter().cloned());
    args.push("--".into());
    args.push(spec.prompt.clone());

    let mut cmd = Command::new(claude_bin()?);
    cmd.args(&args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(clean_env(&spec.env_extra))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&stderr_path)?)
        .process_group(0)
        .kill_on_drop(false);
    if ctl.is_cancelled() {
        return Ok(Outcome { limit: None, status: Status::Cancelled, result: None, exit_code: None, stderr_tail: String::new(), subagents: vec![] });
    }
    let mut child = cmd.spawn().map_err(|e| AppError::Invalid(format!("não consegui iniciar o claude: {e}")))?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    ctl.pid.store(pid, Ordering::SeqCst);

    let started = Instant::now();
    let mut log = tokio::fs::File::create(&events_path).await?;
    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut agents = Agents::default();
    let mut result: Option<Value> = None;

    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
            for text in describe(&ev, &mut agents) {
                let stamped = format!("+{:5.0}s {text}", started.elapsed().as_secs_f64());
                let _ = log.write_all(format!("{stamped}\n").as_bytes()).await;
                on_line(stamped);
            }
            if ev.get("type").and_then(Value::as_str) == Some("result") {
                let _ = tokio::fs::write(&result_path, serde_json::to_vec_pretty(&ev).unwrap_or_default()).await;
                result = Some(ev);
            }
        }
    };
    let timed_out = tokio::time::timeout(spec.timeout, read).await.is_err();
    if timed_out {
        on_line(format!("tempo limite de {} min atingido; cancelando", spec.timeout.as_secs() / 60));
        ctl.cancel();
    }
    let exit = child.wait().await.ok().and_then(|s| s.code());
    ctl.pid.store(0, Ordering::SeqCst);
    let _ = log.flush().await;

    let stderr = tokio::fs::read_to_string(&stderr_path).await.unwrap_or_default();
    let stderr_tail: String = stderr.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let ok = result
        .as_ref()
        .map(|r| r.get("subtype").and_then(Value::as_str) == Some("success") && r.get("is_error") != Some(&Value::Bool(true)))
        .unwrap_or(false);
    let status = if timed_out {
        Status::TimedOut
    } else if ctl.is_cancelled() {
        Status::Cancelled
    } else if ok && exit == Some(0) {
        Status::Completed
    } else {
        Status::Failed
    };
    let subagents = agents.names.into_values().collect();
    let limit = if status == Status::Failed {
        let said = result.as_ref().and_then(|r| r.get("result")).and_then(Value::as_str).unwrap_or("");
        detect_limit(&format!("{said}\n{stderr_tail}"))
    } else {
        None
    };
    Ok(Outcome { limit, status, result, exit_code: exit, stderr_tail, subagents })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descreve_ferramentas_subagentes_e_fim() {
        let mut a = Agents::default();
        let ev = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Agent","input":{"subagent_type":"branch-reviewer","description":"x"}},
            {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git diff a...b"}}]}});
        assert_eq!(describe(&ev, &mut a), vec!["principal: lançou o subagente branch-reviewer", "principal: Bash git diff a...b"]);

        let sub = json!({"type":"assistant","parent_tool_use_id":"t1","message":{"content":[
            {"type":"tool_use","id":"t3","name":"Read","input":{"file_path":"/w/app.php"}}]}});
        assert_eq!(describe(&sub, &mut a), vec!["branch-reviewer: Read /w/app.php"]);

        let done = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}});
        assert_eq!(describe(&done, &mut a), vec!["subagente branch-reviewer terminou"]);

        let res = json!({"type":"result","num_turns":7,"total_cost_usd":0.281,"duration_ms":34000});
        assert_eq!(describe(&res, &mut a), vec!["turno encerrado: 7 turnos, US$ 0.28 acumulado, 34s"]);
    }

    /// Sequência real (Fase 1, 2026-11-17) de um subagente em segundo plano.
    #[test]
    fn subagente_em_segundo_plano_nao_termina_no_lancamento() {
        let mut a = Agents::default();
        let launch = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Agent","input":{"subagent_type":"branch-reviewer"}}]}});
        assert_eq!(describe(&launch, &mut a), vec!["principal: lançou o subagente branch-reviewer"]);

        let started = json!({"type":"system","subtype":"task_started","tool_use_id":"t1",
            "subagent_type":"branch-reviewer","is_backgrounded":true});
        assert_eq!(describe(&started, &mut a), vec!["subagente branch-reviewer rodando em segundo plano"]);

        let ack = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1",
            "content":"Async agent launched successfully."}]}});
        assert!(describe(&ack, &mut a).is_empty(), "o lançamento não é o fim");

        // resultado entregue com o subagente ainda rodando é marcado como parcial
        let early = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"s1","name":"StructuredOutput","input":{"findings":[],"projects":[]}}]}});
        assert_eq!(
            describe(&early, &mut a),
            vec!["principal: resultado parcial (0 achado(s)) com branch-reviewer ainda rodando; vale o que vier depois"]
        );

        let p1 = json!({"type":"system","subtype":"task_progress","tool_use_id":"t1","last_tool_name":"Read"});
        let p2 = json!({"type":"system","subtype":"task_progress","tool_use_id":"t1","last_tool_name":"Read"});
        let p3 = json!({"type":"system","subtype":"task_progress","tool_use_id":"t1","last_tool_name":"Bash"});
        assert_eq!(describe(&p1, &mut a), vec!["branch-reviewer: Read"]);
        assert!(describe(&p2, &mut a).is_empty(), "não repete a mesma ferramenta");
        assert_eq!(describe(&p3, &mut a), vec!["branch-reviewer: Bash"]);

        let done = json!({"type":"system","subtype":"task_notification","tool_use_id":"t1","status":"completed"});
        assert_eq!(describe(&done, &mut a), vec!["subagente branch-reviewer terminou"]);
        let failed = json!({"type":"system","subtype":"task_notification","tool_use_id":"t1","status":"failed"});
        assert_eq!(describe(&failed, &mut a), vec!["subagente branch-reviewer terminou: failed"]);

        let late = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"s2","name":"StructuredOutput","input":{"findings":[{"x":1}]}}]}});
        assert_eq!(describe(&late, &mut a), vec!["principal: entregou o resultado (1 achado(s))"]);
    }

    /// Quando as ações do subagente chegam no stream, o `task_progress` não repete a ferramenta.
    #[test]
    fn progresso_nao_duplica_acoes_encaminhadas() {
        let mut a = Agents::default();
        describe(&json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Agent","input":{"subagent_type":"branch-reviewer"}}]}}), &mut a);
        let action = json!({"type":"assistant","parent_tool_use_id":"t1","message":{"content":[
            {"type":"tool_use","id":"x","name":"Bash","input":{"command":"git diff a...b"}}]}});
        assert_eq!(describe(&action, &mut a), vec!["branch-reviewer: Bash git diff a...b"]);
        let progress = json!({"type":"system","subtype":"task_progress","tool_use_id":"t1","last_tool_name":"Bash"});
        assert!(describe(&progress, &mut a).is_empty());
    }

    #[test]
    fn ferramentas_do_chrome_e_do_jira_legiveis() {
        let mut a = Agents::default();
        let ev = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"1","name":"mcp__claude-in-chrome__computer","input":{"action":"left_click","action_summary":"Clica em Entrar","tabId":1}},
            {"type":"tool_use","id":"2","name":"mcp__claude-in-chrome__navigate","input":{"tabId":1,"url":"http://localhost:8080/login"}},
            {"type":"tool_use","id":"3","name":"mcp__claude-in-chrome__form_input","input":{"ref":"ref_3","tabId":1,"value":"x"}},
            {"type":"tool_use","id":"4","name":"mcp__claude-in-chrome__tabs_create_mcp","input":{}},
            {"type":"tool_use","id":"5","name":"mcp__atlassian__getJiraIssue","input":{"cloudId":"c","issueIdOrKey":"PROJ-1694"}}]}});
        assert_eq!(
            describe(&ev, &mut a),
            vec![
                "principal: chrome computer Clica em Entrar",
                "principal: chrome navigate http://localhost:8080/login",
                "principal: chrome form_input campo ref_3",
                "principal: chrome tabs_create_mcp",
                "principal: jira getJiraIssue PROJ-1694",
            ]
        );
    }

    #[test]
    fn reconhece_limite_de_uso() {
        let l = detect_limit("Claude AI usage limit reached|1763485200").unwrap();
        assert_eq!(l.reset_at, Some(1763485200));
        let l = detect_limit("You've hit your limit · resets 3pm (America/Sao_Paulo)").unwrap();
        assert_eq!(l.reset_at, None);
        assert_eq!(l.reset_text.as_deref(), Some("resets 3pm (America/Sao_Paulo)"));
        assert!(detect_limit("5-hour limit reached ∙ resets 11am").is_some());
        assert!(detect_limit("API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}").is_some());
        assert_eq!(detect_limit("erro de parse no arquivo app.php"), None);
        assert_eq!(detect_limit("Error: max turns reached"), None);
    }

    #[test]
    fn ambiente_sem_credenciais() {
        // SAFETY: teste single-thread quanto a estas variáveis
        unsafe {
            std::env::set_var("BITBUCKET_TOKEN_TESTE", "x");
            std::env::set_var("MY_SECRET_TESTE", "x");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "token-de-teste");
        }
        let env = clean_env(&[("EXTRA".into(), "1".into())]);
        assert!(!env.iter().any(|(k, _)| k == "BITBUCKET_TOKEN_TESTE" || k == "MY_SECRET_TESTE"));
        assert!(env.iter().any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == "token-de-teste"));
        assert!(env.iter().any(|(k, v)| k == "EXTRA" && v == "1"));
        unsafe { std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN"); }
    }
}
