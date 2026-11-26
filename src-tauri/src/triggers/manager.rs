//! Gerenciador dos processos dos gatilhos:
//! - cada execução é um `/bin/bash -lc <script>` num grupo próprio (`process_group(0)`);
//! - stdout e stderr vão, linha a linha e sem ANSI, para `<id>.log` e, em lotes, para a UI;
//! - Parar roda o comando de parada (se houver), manda SIGINT no grupo e, no prazo, SIGKILL;
//! - `<id>.pid` registra o grupo enquanto roda, para a limpeza depois de uma queda do app.
//!
//! Quem decide entre "saiu" e "parado pelo usuário" é o `stop_requested` da entrada, lido e
//! gravado sob a mesma trava: se o processo terminou antes do pedido de parada, vale "saiu";
//! se o pedido veio antes, quem grava o estado final é o próprio `stop`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::process::{CommandExt as _, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::ansi::strip_ansi;
use super::build::launch;
use super::{Kind, Trigger};
use crate::error::{AppError, AppResult};
use crate::proc::{self, First};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TriggerState {
    Stopped,
    Running { since: String, pgid: i32 },
    Stopping,
    Exited { code: Option<i32>, at: String },
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TriggerEvent {
    Status { id: String, state: TriggerState },
    Log { id: String, lines: Vec<String> },
}

/// Recebe os eventos para a UI. Os de estado são emitidos com a trava do gerenciador
/// presa (para chegarem na ordem certa), então `emit` não pode chamar o `Manager`.
pub type Emit = Arc<dyn Fn(TriggerEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// Parar: SIGINT → SIGKILL (npm, ou Shell com comando de parada).
    pub stop: Duration,
    /// Parar: SIGINT → SIGKILL no Shell sem comando de parada.
    pub stop_shell_no_script: Duration,
    /// Prazo do comando de parada.
    pub stop_script: Duration,
    /// Fechar o app: prazo dos comandos de parada (em paralelo).
    pub exit_scripts: Duration,
    /// Fechar o app: SIGINT → SIGKILL em todos os grupos.
    pub exit_groups: Duration,
    /// Intervalo dos lotes de log para a UI.
    pub batch: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Deadlines {
            stop: Duration::from_secs(10),
            stop_shell_no_script: Duration::from_secs(30),
            stop_script: Duration::from_secs(60),
            exit_scripts: Duration::from_secs(30),
            exit_groups: Duration::from_secs(5),
            batch: Duration::from_millis(100),
        }
    }
}

/// Prazo do comando de parada na limpeza ao abrir o app.
const CLEANUP_STOP_SCRIPT: Duration = Duration::from_secs(60);
/// Prazo SIGINT → SIGKILL na limpeza ao abrir o app.
const CLEANUP_GROUP: Duration = Duration::from_secs(5);
/// Depois do SIGKILL, quanto esperar o grupo sumir.
const AFTER_KILL: Duration = Duration::from_secs(5);
/// O mesmo, ao fechar o app e na limpeza: o SIGKILL não demora, e fechar não deve travar.
const AFTER_KILL_BLOCKING: Duration = Duration::from_secs(1);

const STOPPED_LINE: &str = "parado pelo usuário";
const SHUTDOWN_LINE: &str = "parado ao fechar o app";

/// Conteúdo do `<id>.pid`.
#[derive(Debug, Serialize, Deserialize)]
struct PidFile {
    pgid: i32,
    start_time: u64,
    owner_pid: u32,
    cwd: PathBuf,
    stop: Option<String>,
}

enum Msg {
    Line(String),
    /// Última linha da execução: o lote grava tudo, confirma e termina.
    Final(String, oneshot::Sender<()>),
}

struct Entry {
    /// Número da execução; tarefas de uma execução antiga não mexem na entrada nova.
    run: u64,
    state: TriggerState,
    pgid: i32,
    kind: Kind,
    cwd: PathBuf,
    stop: Option<String>,
    tx: Option<mpsc::UnboundedSender<Msg>>,
    /// O usuário pediu para parar: o estado final é gravado pelo `stop`.
    stop_requested: bool,
    /// O processo terminou sozinho e a tarefa de espera está gravando o estado final.
    ending: bool,
    /// O processo e a saída dele terminaram.
    exited: watch::Sender<bool>,
    /// O estado final foi gravado.
    finished: watch::Sender<bool>,
    /// A limpeza ao abrir o app está parando o que um app anterior deixou rodando.
    cleaning: bool,
}

impl Entry {
    fn new(run: u64, state: TriggerState, kind: Kind, cwd: PathBuf, stop: Option<String>) -> Self {
        Entry {
            run,
            state,
            pgid: 0,
            kind,
            cwd,
            stop,
            tx: None,
            stop_requested: false,
            ending: false,
            exited: watch::channel(false).0,
            finished: watch::channel(false).0,
            cleaning: false,
        }
    }

    fn active(&self) -> bool {
        matches!(self.state, TriggerState::Running { .. } | TriggerState::Stopping)
    }
}

pub struct Manager {
    dir: PathBuf,
    deadlines: Deadlines,
    entries: Mutex<HashMap<String, Entry>>,
    next_run: AtomicU64,
}

/// O que o `stop` faz, decidido sob a trava.
enum StopPlan {
    Nothing,
    /// Outra parada (ou a saída natural) já está gravando o estado final.
    Wait(watch::Receiver<bool>),
    Stop {
        run: u64,
        pgid: i32,
        script: Option<String>,
        cwd: PathBuf,
        grace: Duration,
        tx: Option<mpsc::UnboundedSender<Msg>>,
        exited: watch::Receiver<bool>,
    },
}

impl Manager {
    /// Cria a pasta dos logs e dos `.pid`.
    pub fn new(dir: PathBuf, deadlines: Deadlines) -> Self {
        let _ = fs::create_dir_all(&dir);
        Manager { dir, deadlines, entries: Mutex::new(HashMap::new()), next_run: AtomicU64::new(1) }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.log"))
    }

    fn pid_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pid"))
    }

    /// Estado atual; `Stopped` se o gatilho nunca rodou.
    pub fn state(&self, id: &str) -> TriggerState {
        self.lock().get(id).map(|e| e.state.clone()).unwrap_or(TriggerState::Stopped)
    }

    #[cfg(test)]
    /// Gatilhos rodando ou parando.
    pub fn running_count(&self) -> usize {
        self.lock().values().filter(|e| e.active()).count()
    }

    pub async fn start(self: &Arc<Self>, t: &Trigger, emit: Emit) -> AppResult<()> {
        checked_id(&t.id)?;
        let l = launch(t);
        let run = self.next_run.fetch_add(1, Ordering::SeqCst);
        // a trava fica presa do teste de "já rodando" até a entrada nova: dois cliques
        // seguidos não lançam dois processos
        let mut map = self.lock();
        if map.get(&t.id).is_some_and(|e| e.cleaning) {
            return Err(AppError::Invalid("limpando a execução anterior; tente de novo em instantes".into()));
        }
        if map.get(&t.id).is_some_and(Entry::active) {
            return Err(AppError::Invalid("o gatilho já está rodando".into()));
        }
        let node = if t.node.is_empty() { "do sistema".to_owned() } else { t.node.clone() };
        let header = vec![format!("$ {}", l.display), format!("pasta: {}", l.cwd.display()), format!("node: {node}")];
        // zera o log e escreve sempre no fim: o que for anexado por fora (comando de parada ao
        // fechar o app) não é sobrescrito pelo lote
        let log_path = self.log_path(&t.id);
        fs::write(&log_path, format!("{}\n", header.join("\n")))?;
        let mut file = OpenOptions::new().append(true).open(&log_path)?;

        let spawned = Command::new("/bin/bash")
            .arg("-lc")
            .arg(&l.script)
            .current_dir(&l.cwd)
            .process_group(0)
            .kill_on_drop(false)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let message = format!("não foi possível iniciar: {e}");
                let _ = writeln!(file, "{message}");
                let state = TriggerState::Failed { message: message.clone() };
                map.insert(t.id.clone(), Entry::new(run, state.clone(), t.kind.clone(), l.cwd, l.stop));
                let mut lines = header;
                lines.push(message.clone());
                emit(TriggerEvent::Log { id: t.id.clone(), lines });
                emit(TriggerEvent::Status { id: t.id.clone(), state });
                return Err(AppError::Invalid(message));
            }
        };
        let pgid = child.id().map(|p| p as i32).unwrap_or(0);

        let pid_file = PidFile {
            pgid,
            start_time: proc::start_time(pgid).unwrap_or(0),
            owner_pid: std::process::id(),
            cwd: l.cwd.clone(),
            stop: l.stop.clone(),
        };
        if let Err(e) = serde_json::to_vec(&pid_file).map_err(AppError::from).and_then(|b| Ok(fs::write(self.pid_path(&t.id), b)?)) {
            let _ = writeln!(file, "aviso: não foi possível gravar o .pid ({e}); se o app cair, o processo não será limpo");
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let readers = vec![
            tokio::spawn(read_lines(child.stdout.take(), tx.clone())),
            tokio::spawn(read_lines(child.stderr.take(), tx.clone())),
        ];
        tokio::spawn(batch_lines(rx, file, t.id.clone(), emit.clone(), self.deadlines.batch));
        tokio::spawn(Arc::clone(self).watch_exit(t.id.clone(), run, child, readers, pgid, emit.clone()));

        let state = TriggerState::Running { since: now_iso(), pgid };
        let mut entry = Entry::new(run, state.clone(), t.kind.clone(), l.cwd, l.stop);
        entry.pgid = pgid;
        entry.tx = Some(tx);
        map.insert(t.id.clone(), entry);
        emit(TriggerEvent::Status { id: t.id.clone(), state });
        emit(TriggerEvent::Log { id: t.id.clone(), lines: header });
        Ok(())
    }

    /// Espera o processo e a saída dele; se ninguém pediu para parar, grava "saiu".
    async fn watch_exit(
        self: Arc<Self>,
        id: String,
        run: u64,
        mut child: tokio::process::Child,
        readers: Vec<JoinHandle<()>>,
        pgid: i32,
        emit: Emit,
    ) {
        let status = child.wait().await.ok();
        wait_output(readers, pgid).await;
        {
            let mut map = self.lock();
            let Some(e) = map.get_mut(&id).filter(|e| e.run == run) else { return };
            if e.stop_requested {
                e.exited.send_replace(true);
                return;
            }
            e.ending = true;
        }
        // sobrou processo do grupo que não segura a saída (ex.: `cmd >/dev/null &`)
        if proc::group_alive(pgid) {
            proc::escalate(pgid, (First::Group(pgid), libc::SIGINT), &[(self.deadlines.stop, libc::SIGKILL)]).await;
            proc::wait_group_gone(pgid, AFTER_KILL).await;
        }
        let code = status.and_then(|s| s.code());
        let state = TriggerState::Exited { code, at: now_iso() };
        self.finish(&id, run, exit_line(status), state, &emit).await;
    }

    /// Grava a última linha no log, apaga o `.pid` e o estado final.
    async fn finish(&self, id: &str, run: u64, line: String, state: TriggerState, emit: &Emit) {
        let tx = self.lock().get_mut(id).filter(|e| e.run == run).and_then(|e| e.tx.take());
        if let Some(tx) = tx {
            let (ack, done) = oneshot::channel();
            if tx.send(Msg::Final(line, ack)).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(5), done).await;
            }
        }
        let _ = fs::remove_file(self.pid_path(id));
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id).filter(|e| e.run == run) {
            e.state = state.clone();
            e.finished.send_replace(true);
            emit(TriggerEvent::Status { id: id.to_owned(), state });
        }
    }

    /// Para o gatilho e volta quando o grupo sumiu. Parar um gatilho parado é Ok.
    pub async fn stop(self: &Arc<Self>, id: &str, emit: Emit) -> AppResult<()> {
        // numa tarefa própria: se quem chamou desistir no meio, a parada vai até o fim
        let me = Arc::clone(self);
        let id = id.to_owned();
        tokio::spawn(async move { me.stop_inner(&id, &emit).await })
            .await
            .map_err(|e| AppError::Invalid(format!("falha ao parar o gatilho: {e}")))
    }

    async fn stop_inner(&self, id: &str, emit: &Emit) {
        let plan = {
            let mut map = self.lock();
            match map.get_mut(id) {
                Some(e) if matches!(e.state, TriggerState::Running { .. }) && !e.ending => {
                    e.stop_requested = true;
                    e.state = TriggerState::Stopping;
                    emit(TriggerEvent::Status { id: id.to_owned(), state: TriggerState::Stopping });
                    let grace = if e.kind == Kind::Shell && e.stop.is_none() {
                        self.deadlines.stop_shell_no_script
                    } else {
                        self.deadlines.stop
                    };
                    StopPlan::Stop {
                        run: e.run,
                        pgid: e.pgid,
                        script: e.stop.clone(),
                        cwd: e.cwd.clone(),
                        grace,
                        tx: e.tx.clone(),
                        exited: e.exited.subscribe(),
                    }
                }
                Some(e) if e.active() => StopPlan::Wait(e.finished.subscribe()),
                _ => StopPlan::Nothing,
            }
        };
        match plan {
            StopPlan::Nothing => {}
            StopPlan::Wait(mut finished) => {
                let _ = finished.wait_for(|v| *v).await;
            }
            StopPlan::Stop { run, pgid, script, cwd, grace, tx, mut exited } => {
                if let Some(script) = script {
                    run_stop_script(&script, &cwd, tx.as_ref(), self.deadlines.stop_script).await;
                }
                drop(tx);
                proc::escalate(pgid, (First::Group(pgid), libc::SIGINT), &[(grace, libc::SIGKILL)]).await;
                proc::wait_group_gone(pgid, AFTER_KILL).await;
                // erro = a tarefa de espera acabou sem avisar; não há o que esperar
                let _ = exited.wait_for(|v| *v).await;
                self.finish(id, run, STOPPED_LINE.to_owned(), TriggerState::Stopped, emit).await;
            }
        }
    }

    /// Marca os gatilhos que a limpeza ao abrir o app vai parar: ficam "parando" e o `start`
    /// recusa até o `end_cleanup`. Chamado antes de o gerenciador aceitar comandos.
    pub fn begin_cleanup(&self, ids: &[String]) {
        let mut map = self.lock();
        for id in ids {
            if map.get(id).is_some_and(Entry::active) {
                continue;
            }
            let mut e = Entry::new(0, TriggerState::Stopping, Kind::default(), PathBuf::new(), None);
            e.cleaning = true;
            map.insert(id.clone(), e);
        }
    }

    /// Fim da limpeza de um gatilho: volta a "parado" (se nada mudou desde o `begin_cleanup`).
    pub fn end_cleanup(&self, id: &str, emit: Option<&Emit>) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id).filter(|e| e.cleaning) {
            e.cleaning = false;
            e.state = TriggerState::Stopped;
            e.finished.send_replace(true);
            if let Some(emit) = emit {
                emit(TriggerEvent::Status { id: id.to_owned(), state: TriggerState::Stopped });
            }
        }
    }

    /// Ao abrir o app (bloqueante, numa thread): limpa a lista lida por `scan_orphans` e
    /// libera cada gatilho ao terminar.
    pub fn cleanup_orphans(&self, list: Vec<Orphan>, own_pid: u32, is_app: impl Fn(u32) -> bool, emit: Option<Emit>) {
        for o in list {
            cleanup_one(&o, own_pid, &is_app);
            self.end_cleanup(&o.id, emit.as_ref());
        }
    }

    /// Parar e, quando o grupo sumir, rodar de novo.
    pub async fn restart(self: &Arc<Self>, t: &Trigger, emit: Emit) -> AppResult<()> {
        self.stop(&t.id, emit.clone()).await?;
        self.start(t, emit).await
    }

    /// Últimas `n` linhas do log (vazio se o gatilho nunca rodou).
    pub fn tail(&self, id: &str, n: usize) -> AppResult<Vec<String>> {
        checked_id(id)?;
        let bytes = match fs::read(self.log_path(id)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        Ok(lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect())
    }

    /// Esquece o gatilho (ao apagá-lo) e apaga o log. Recusa se estiver rodando.
    pub fn forget(&self, id: &str) -> AppResult<()> {
        checked_id(id)?;
        let mut map = self.lock();
        if map.get(id).is_some_and(Entry::active) {
            return Err(AppError::Invalid("pare o gatilho antes de apagá-lo".into()));
        }
        map.remove(id);
        match fs::remove_file(self.log_path(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Fechar o app (bloqueante): comandos de parada em paralelo, SIGINT em todos os grupos,
    /// espera e SIGKILL no que sobrar.
    pub fn shutdown_all(&self) {
        let targets: Vec<(String, i32, Option<String>, PathBuf)> = {
            let mut map = self.lock();
            map.iter_mut()
                // o que a limpeza ao abrir está parando não tem grupo deste app
                .filter(|(_, e)| e.active() && !e.cleaning)
                .map(|(id, e)| {
                    // a saída que vier depois disto não vira "saiu com código N"
                    e.stop_requested = true;
                    (id.clone(), e.pgid, e.stop.clone(), e.cwd.clone())
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let scripts: Vec<_> = targets
            .iter()
            .filter_map(|(id, _, stop, cwd)| {
                let (stop, cwd, log, max) = (stop.clone()?, cwd.clone(), self.log_path(id), self.deadlines.exit_scripts);
                Some(std::thread::spawn(move || run_stop_script_blocking(&stop, &cwd, &log, max)))
            })
            .collect();
        for h in scripts {
            let _ = h.join();
        }
        let pgids: Vec<i32> = targets.iter().map(|t| t.1).collect();
        stop_groups_blocking(&pgids, self.deadlines.exit_groups);
        let own = std::process::id();
        let mut map = self.lock();
        for (id, ..) in &targets {
            append_line(&self.log_path(id), SHUTDOWN_LINE);
            // o `.pid` pode ser de outro app que rodou o mesmo gatilho depois
            let path = self.pid_path(id);
            let owner = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<PidFile>(&b).ok()).map(|p| p.owner_pid);
            if owner == Some(own) {
                let _ = fs::remove_file(&path);
            }
            if let Some(e) = map.get_mut(id) {
                e.state = TriggerState::Stopped;
                e.finished.send_replace(true);
            }
        }
    }
}

/// O id vira nome de arquivo: só letras, números, `-` e `_`.
fn checked_id(id: &str) -> AppResult<&str> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(AppError::Invalid(format!("id de gatilho inválido: {id:?}")));
    }
    Ok(id)
}

fn exit_line(status: Option<ExitStatus>) -> String {
    match status {
        Some(s) => match (s.code(), s.signal()) {
            (Some(c), _) => format!("saiu com código {c}"),
            (None, Some(sig)) => format!("saiu pelo sinal {sig}"),
            _ => "saiu".into(),
        },
        None => "saiu (não foi possível ler o código de saída)".into(),
    }
}

/// Lê linhas (bytes inválidos viram `�`), tira o ANSI e manda para o canal.
async fn read_lines<R: AsyncRead + Unpin>(r: Option<R>, tx: mpsc::UnboundedSender<Msg>) {
    let Some(r) = r else { return };
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if tx.send(Msg::Line(strip_ansi(&String::from_utf8_lossy(&buf)))).is_err() {
                    return;
                }
            }
        }
    }
}

/// Junta as linhas num lote a cada `every` (a primeira linha de um lote abre o prazo),
/// grava no log e emite um evento por lote.
async fn batch_lines(mut rx: mpsc::UnboundedReceiver<Msg>, mut file: File, id: String, emit: Emit, every: Duration) {
    let mut buf: Vec<String> = Vec::new();
    let mut flush_at: Option<tokio::time::Instant> = None;
    loop {
        let msg = match flush_at {
            None => rx.recv().await,
            Some(at) => match tokio::time::timeout_at(at, rx.recv()).await {
                Ok(m) => m,
                Err(_) => {
                    flush(&mut file, &mut buf, &id, &emit);
                    flush_at = None;
                    continue;
                }
            },
        };
        match msg {
            Some(Msg::Line(l)) => {
                buf.push(l);
                flush_at.get_or_insert_with(|| tokio::time::Instant::now() + every);
            }
            Some(Msg::Final(l, ack)) => {
                buf.push(l);
                flush(&mut file, &mut buf, &id, &emit);
                let _ = ack.send(());
                return;
            }
            None => {
                flush(&mut file, &mut buf, &id, &emit);
                return;
            }
        }
    }
}

fn flush(file: &mut File, buf: &mut Vec<String>, id: &str, emit: &Emit) {
    if buf.is_empty() {
        return;
    }
    let mut text = buf.join("\n");
    text.push('\n');
    let _ = file.write_all(text.as_bytes());
    emit(TriggerEvent::Log { id: id.to_owned(), lines: std::mem::take(buf) });
}

/// Espera os leitores chegarem ao fim da saída. Um processo que saiu do grupo (`setsid`)
/// pode segurar o pipe para sempre: com o grupo vazio, espera só mais um pouco.
async fn wait_output(readers: Vec<JoinHandle<()>>, pgid: i32) {
    let mut all = std::pin::pin!(async {
        for r in readers {
            let _ = r.await;
        }
    });
    loop {
        if tokio::time::timeout(Duration::from_millis(200), all.as_mut()).await.is_ok() {
            return;
        }
        if !proc::group_alive(pgid) {
            let _ = tokio::time::timeout(Duration::from_millis(500), all.as_mut()).await;
            return;
        }
    }
}

/// Roda o comando de parada no mesmo `cwd`, com a saída no log da execução.
async fn run_stop_script(script: &str, cwd: &Path, tx: Option<&mpsc::UnboundedSender<Msg>>, max: Duration) {
    let say = |line: String| {
        if let Some(tx) = tx {
            let _ = tx.send(Msg::Line(line));
        }
    };
    say("parando: rodando o comando de parada".into());
    let spawned = Command::new("/bin/bash")
        .arg("-lc")
        .arg(script)
        .current_dir(cwd)
        .process_group(0)
        .kill_on_drop(false)
        .stdin(Stdio::null())
        .stdout(if tx.is_some() { Stdio::piped() } else { Stdio::null() })
        .stderr(if tx.is_some() { Stdio::piped() } else { Stdio::null() })
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return say(format!("não foi possível rodar o comando de parada: {e}")),
    };
    let pgid = child.id().map(|p| p as i32).unwrap_or(0);
    let readers: Vec<_> = match tx {
        Some(tx) => vec![
            tokio::spawn(read_lines(child.stdout.take(), tx.clone())),
            tokio::spawn(read_lines(child.stderr.take(), tx.clone())),
        ],
        None => Vec::new(),
    };
    match tokio::time::timeout(max, child.wait()).await {
        Ok(Ok(s)) if !s.success() => say(format!("o comando de parada {}", exit_line(Some(s)))),
        Ok(_) => {}
        Err(_) => {
            say(format!("o comando de parada passou do prazo de {} s e foi encerrado", max.as_secs()));
            proc::signal_group(pgid, libc::SIGKILL);
            let _ = child.wait().await;
        }
    }
    // a saída do comando de parada entra no log antes das linhas seguintes; com prazo, porque
    // um processo que ele deixou em segundo plano pode segurar o pipe
    let _ = tokio::time::timeout(Duration::from_secs(2), wait_output(readers, pgid)).await;
}

/// Versão bloqueante, com a saída anexada ao log (fechar o app e limpeza ao abrir).
fn run_stop_script_blocking(script: &str, cwd: &Path, log: &Path, max: Duration) {
    let out = OpenOptions::new().append(true).create(true).open(log);
    let (stdout, stderr) = match out.and_then(|f| Ok((f.try_clone()?, f))) {
        Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let spawned = std::process::Command::new("/bin/bash")
        .arg("-lc")
        .arg(script)
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn();
    let Ok(mut child) = spawned else { return };
    let end = Instant::now() + max;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= end => {
                proc::signal_group(child.id() as i32, libc::SIGKILL);
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// SIGINT nos grupos, espera até `max` e SIGKILL no que sobrar (bloqueante).
fn stop_groups_blocking(pgids: &[i32], max: Duration) {
    for &g in pgids {
        proc::signal_group(g, libc::SIGINT);
    }
    if !proc::wait_groups_gone_blocking(pgids, max) {
        for &g in pgids {
            proc::signal_group(g, libc::SIGKILL);
        }
        proc::wait_groups_gone_blocking(pgids, AFTER_KILL_BLOCKING);
    }
}

/// `.pid` lido ao abrir o app, antes de o gerenciador aceitar comandos.
pub struct Orphan {
    pub id: String,
    path: PathBuf,
    /// Conteúdo lido: o arquivo só é apagado se ainda for este.
    raw: Vec<u8>,
}

/// Ao abrir o app (rápido, antes do `manage`): lê os `.pid` da pasta. Pula os deste processo.
pub fn scan_orphans(dir: &Path, own_pid: u32) -> Vec<Orphan> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for path in rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pid")) {
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        let Ok(raw) = fs::read(&path) else { continue };
        if serde_json::from_slice::<PidFile>(&raw).is_ok_and(|pf| pf.owner_pid == own_pid) {
            continue;
        }
        out.push(Orphan { id, path, raw });
    }
    out
}

/// Limpa um gatilho que um app que caiu deixou rodando. Pula o `.pid` deste processo e o de
/// dono ainda vivo (outra janela, ou dev junto com o instalado). Nos demais, roda o comando de
/// parada e mata o grupo, se ele ainda for o mesmo (tempo de início igual ao gravado: um PID
/// reaproveitado não é morto).
fn cleanup_one(o: &Orphan, own_pid: u32, is_app: &impl Fn(u32) -> bool) {
    // apaga só se ninguém regravou o arquivo depois da leitura
    let remove = || {
        if fs::read(&o.path).is_ok_and(|now| now == o.raw) {
            let _ = fs::remove_file(&o.path);
        }
    };
    let Ok(pf) = serde_json::from_slice::<PidFile>(&o.raw) else { return remove() };
    if pf.owner_pid == own_pid || is_app(pf.owner_pid) {
        return;
    }
    // confere antes do comando de parada: ele pode encerrar o líder e o grupo continuar
    let ours = proc::group_alive(pf.pgid) && proc::start_time(pf.pgid) == Some(pf.start_time);
    let log = o.path.with_extension("log");
    if pf.stop.is_some() || ours {
        append_line(&log, "o app fechou sem parar o gatilho; encerrando ao abrir de novo");
    }
    if let Some(stop) = &pf.stop {
        run_stop_script_blocking(stop, &pf.cwd, &log, CLEANUP_STOP_SCRIPT);
    }
    if ours && proc::group_alive(pf.pgid) {
        stop_groups_blocking(&[pf.pgid], CLEANUP_GROUP);
    }
    remove();
}

fn append_line(log: &Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// O processo `pid` existe e é este programa (app ou terminal)?
pub fn is_app_process(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    proc::pid_alive(pid)
        && fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim().starts_with("pr-review-orch"))
}

/// Agora, em UTC, como as datas do banco (`2026-11-24T13:05:09.123Z`).
fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, m, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Dias desde 1970-01-01 → (ano, mês, dia), pelo algoritmo de Howard Hinnant.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    use crate::proc::group_alive;
    use crate::triggers::Kind;

    /// Pasta temporária apagada no fim do teste.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("trig-mgr-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(p.join("work")).unwrap();
            Tmp(p)
        }
        fn work(&self) -> PathBuf {
            self.0.join("work")
        }
        fn dir(&self) -> PathBuf {
            self.0.join("triggers")
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn deadlines() -> Deadlines {
        Deadlines {
            stop: Duration::from_secs(1),
            stop_shell_no_script: Duration::from_secs(2),
            stop_script: Duration::from_secs(10),
            exit_scripts: Duration::from_secs(10),
            exit_groups: Duration::from_secs(1),
            batch: Duration::from_millis(50),
        }
    }

    fn shell(tmp: &Tmp, script: &str) -> Trigger {
        Trigger {
            id: uuid::Uuid::new_v4().to_string(),
            name: "teste".into(),
            kind: Kind::Shell,
            workdir: tmp.work().to_string_lossy().into_owned(),
            script: script.into(),
            ..Default::default()
        }
    }

    fn recorder() -> (Emit, Arc<Mutex<Vec<TriggerEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        (Arc::new(move |e| sink.lock().unwrap().push(e)), events)
    }

    fn setup(tmp: &Tmp) -> Arc<Manager> {
        Arc::new(Manager::new(tmp.dir(), deadlines()))
    }

    fn log(tmp: &Tmp, id: &str) -> String {
        std::fs::read_to_string(tmp.dir().join(format!("{id}.log"))).unwrap()
    }

    fn pgid(m: &Manager, id: &str) -> i32 {
        match m.state(id) {
            TriggerState::Running { pgid, .. } => pgid,
            s => panic!("esperava rodando: {s:?}"),
        }
    }

    /// Espera o estado final (saiu) do gatilho.
    async fn wait_exit(m: &Manager, id: &str) -> Option<i32> {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let TriggerState::Exited { code, .. } = m.state(id) {
                return code;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("o gatilho não terminou: {:?}", m.state(id));
    }

    #[tokio::test]
    async fn start_and_stop_leaves_no_process() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, events) = recorder();
        let t = shell(&tmp, "sleep 30 & sleep 30; wait");
        m.start(&t, emit.clone()).await.unwrap();
        let pg = pgid(&m, &t.id);
        assert!(tmp.dir().join(format!("{}.pid", t.id)).exists());
        assert_eq!(m.running_count(), 1);
        m.stop(&t.id, emit).await.unwrap();
        assert_eq!(m.state(&t.id), TriggerState::Stopped);
        assert!(!group_alive(pg));
        assert!(log(&tmp, &t.id).trim_end().ends_with("parado pelo usuário"), "{}", log(&tmp, &t.id));
        assert!(!tmp.dir().join(format!("{}.pid", t.id)).exists());
        assert_eq!(m.running_count(), 0);
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| matches!(e, TriggerEvent::Status { state: TriggerState::Stopping, .. })));
        assert!(matches!(ev.last(), Some(TriggerEvent::Status { state: TriggerState::Stopped, .. })), "{ev:?}");
    }

    #[tokio::test]
    async fn exits_immediately_records_code() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, events) = recorder();
        let t = shell(&tmp, "echo erro >&2; exit 3");
        m.start(&t, emit.clone()).await.unwrap();
        assert_eq!(wait_exit(&m, &t.id).await, Some(3));
        let text = log(&tmp, &t.id);
        assert!(text.contains("$ echo erro >&2; exit 3"), "{text}");
        assert!(text.contains("erro"), "{text}");
        assert!(text.trim_end().ends_with("saiu com código 3"), "{text}");
        assert!(!tmp.dir().join(format!("{}.pid", t.id)).exists());
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            TriggerEvent::Status { state: TriggerState::Exited { code: Some(3), .. }, .. }
        )));
        // roda de novo
        m.start(&t, emit).await.unwrap();
        assert_eq!(wait_exit(&m, &t.id).await, Some(3));
        let tail = m.tail(&t.id, 1).unwrap();
        assert_eq!(tail, vec!["saiu com código 3".to_string()]);
    }

    #[tokio::test]
    async fn stop_kills_group_that_ignores_sigint() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let t = shell(&tmp, "trap '' INT; sleep 30 & wait");
        m.start(&t, emit.clone()).await.unwrap();
        let pg = pgid(&m, &t.id);
        // dá tempo de o perfil carregar e o trap ser instalado
        tokio::time::sleep(Duration::from_millis(500)).await;
        let t0 = Instant::now();
        m.stop(&t.id, emit).await.unwrap();
        assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
        assert!(!group_alive(pg));
        assert_eq!(m.state(&t.id), TriggerState::Stopped);
    }

    #[tokio::test]
    async fn stop_script_runs_before_signal() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let mut t = shell(&tmp, "sleep 30");
        t.stop_script = "echo parando > stop.txt; echo saida-da-parada".into();
        m.start(&t, emit.clone()).await.unwrap();
        let pg = pgid(&m, &t.id);
        m.stop(&t.id, emit).await.unwrap();
        assert!(tmp.work().join("stop.txt").exists());
        let text = log(&tmp, &t.id);
        assert!(text.contains("saida-da-parada"), "{text}");
        assert!(text.trim_end().ends_with("parado pelo usuário"), "{text}");
        assert!(!group_alive(pg));
    }

    #[tokio::test]
    async fn burst_is_batched() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, events) = recorder();
        let t = shell(&tmp, "seq 1 10000");
        m.start(&t, emit).await.unwrap();
        assert_eq!(wait_exit(&m, &t.id).await, Some(0));
        let text = log(&tmp, &t.id);
        let nums: Vec<u32> = text.lines().filter_map(|l| l.parse().ok()).collect();
        assert_eq!(nums, (1..=10000).collect::<Vec<_>>());
        let ev = events.lock().unwrap();
        let logs = ev.iter().filter(|e| matches!(e, TriggerEvent::Log { .. })).count();
        assert!(logs < 200, "{logs} eventos de log");
        let streamed: usize = ev
            .iter()
            .filter_map(|e| match e {
                TriggerEvent::Log { lines, .. } => Some(lines.iter().filter(|l| l.parse::<u32>().is_ok()).count()),
                _ => None,
            })
            .sum();
        assert_eq!(streamed, 10000);
    }

    #[tokio::test]
    async fn double_start_is_refused() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let t = shell(&tmp, "sleep 30");
        m.start(&t, emit.clone()).await.unwrap();
        let err = m.start(&t, emit.clone()).await.unwrap_err();
        assert!(err.to_string().contains("já está rodando"), "{err}");
        assert!(m.forget(&t.id).is_err());
        m.stop(&t.id, emit.clone()).await.unwrap();
        // parar de novo não é erro; apagar o log, depois de parado, funciona
        m.stop(&t.id, emit).await.unwrap();
        m.forget(&t.id).unwrap();
        assert!(!tmp.dir().join(format!("{}.log", t.id)).exists());
        assert_eq!(m.state(&t.id), TriggerState::Stopped);
    }

    #[tokio::test]
    async fn failed_spawn_is_reported() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let mut t = shell(&tmp, "true");
        t.workdir = tmp.0.join("nao-existe").to_string_lossy().into_owned();
        assert!(m.start(&t, emit).await.is_err());
        assert!(matches!(m.state(&t.id), TriggerState::Failed { .. }));
        assert_eq!(m.running_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_all_stops_everything() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let a = shell(&tmp, "trap '' INT; sleep 30 & wait");
        let mut b = shell(&tmp, "sleep 30");
        b.stop_script = "echo parou > shutdown.txt".into();
        m.start(&a, emit.clone()).await.unwrap();
        m.start(&b, emit).await.unwrap();
        let (pa, pb) = (pgid(&m, &a.id), pgid(&m, &b.id));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let m2 = Arc::clone(&m);
        tokio::task::spawn_blocking(move || m2.shutdown_all()).await.unwrap();
        assert!(!group_alive(pa) && !group_alive(pb));
        assert!(tmp.work().join("shutdown.txt").exists());
        assert!(!tmp.dir().join(format!("{}.pid", a.id)).exists());
        assert!(!tmp.dir().join(format!("{}.pid", b.id)).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_keeps_stop_output_and_foreign_pid() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        // ao receber o SIGINT escreve mais uma linha, que o lote grava depois do comando de parada
        let mut a = shell(&tmp, "echo antes; trap 'echo depois-do-int; exit 0' INT; sleep 30");
        a.stop_script = "echo saida-da-parada".into();
        let b = shell(&tmp, "sleep 30");
        m.start(&a, emit.clone()).await.unwrap();
        m.start(&b, emit).await.unwrap();
        let (pa, pb) = (pgid(&m, &a.id), pgid(&m, &b.id));
        // o `.pid` de b passa a ser de outro app
        let b_pid = tmp.dir().join(format!("{}.pid", b.id));
        let foreign = pid_json(pb, 1, std::process::id() + 1, &tmp.work());
        std::fs::write(&b_pid, &foreign).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let m2 = Arc::clone(&m);
        tokio::task::spawn_blocking(move || m2.shutdown_all()).await.unwrap();
        assert!(!group_alive(pa) && !group_alive(pb));
        // dá tempo de o lote gravar a última linha do processo
        tokio::time::sleep(Duration::from_millis(300)).await;
        let text = log(&tmp, &a.id);
        for want in ["antes", "saida-da-parada", "depois-do-int", "parado ao fechar o app"] {
            assert!(text.contains(want), "faltou {want:?}: {text}");
        }
        assert!(log(&tmp, &b.id).trim_end().ends_with("parado ao fechar o app"), "{}", log(&tmp, &b.id));
        assert!(!tmp.dir().join(format!("{}.pid", a.id)).exists());
        assert_eq!(std::fs::read_to_string(&b_pid).unwrap(), foreign);
    }

    /// `sleep 30` num grupo próprio, filho do init (não fica zumbi deste teste). Sem o `&` do
    /// bash, que faria o processo ignorar o SIGINT.
    async fn detached_sleep(tmp: &Tmp) -> i32 {
        let pid_out = tmp.0.join(format!("sleep-{}.pid", uuid::Uuid::new_v4()));
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_out.display());
        let st = std::process::Command::new("setsid")
            .args(["-f", "sh", "-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let pid = std::fs::read_to_string(&pid_out).ok().and_then(|s| s.trim().parse::<i32>().ok());
            if let Some(pid) = pid.filter(|&p| group_alive(p)) {
                return pid;
            }
            assert!(Instant::now() < end, "o setsid não criou o grupo");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// O que o app faz ao abrir: lê os `.pid`, marca e limpa.
    fn clean(tmp: &Tmp, own: u32, is_app: impl Fn(u32) -> bool) -> Arc<Manager> {
        let m = setup(tmp);
        let list = scan_orphans(&tmp.dir(), own);
        m.begin_cleanup(&list.iter().map(|o| o.id.clone()).collect::<Vec<_>>());
        m.cleanup_orphans(list, own, is_app, None);
        m
    }

    fn pid_json(pgid: i32, start_time: u64, owner: u32, cwd: &Path) -> String {
        serde_json::json!({ "pgid": pgid, "start_time": start_time, "owner_pid": owner, "cwd": cwd, "stop": null })
            .to_string()
    }

    #[tokio::test]
    async fn cleanup_skips_own_process() {
        let tmp = Tmp::new();
        std::fs::create_dir_all(tmp.dir()).unwrap();
        let pid = detached_sleep(&tmp).await;
        let pid_file = tmp.dir().join("meu.pid");
        let me = std::process::id();
        std::fs::write(&pid_file, pid_json(pid, crate::proc::start_time(pid).unwrap(), me, &tmp.work())).unwrap();
        // o dono "não é o app" para o teste: só o owner_pid igual ao deste processo o protege
        let m = clean(&tmp, me, |_| false);
        assert!(pid_file.exists());
        assert!(group_alive(pid), "a limpeza matou um gatilho deste processo");
        assert_eq!(m.state("meu"), TriggerState::Stopped);
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        assert!(crate::proc::wait_group_gone(pid, Duration::from_secs(2)).await);
    }

    #[test]
    fn cleanup_keeps_file_rewritten_after_scan() {
        let tmp = Tmp::new();
        std::fs::create_dir_all(tmp.dir()).unwrap();
        let pid_file = tmp.dir().join("z.pid");
        // grupo que não existe: a limpeza só apagaria o arquivo
        std::fs::write(&pid_file, pid_json(i32::MAX - 1, 1, 0, &tmp.work())).unwrap();
        let m = setup(&tmp);
        let list = scan_orphans(&tmp.dir(), std::process::id());
        assert_eq!(list.len(), 1);
        let novo = pid_json(i32::MAX - 2, 2, 0, &tmp.work());
        std::fs::write(&pid_file, &novo).unwrap();
        m.cleanup_orphans(list, std::process::id(), |_| false, None);
        assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), novo);
    }

    #[tokio::test]
    async fn start_refused_while_cleaning() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, events) = recorder();
        let t = shell(&tmp, "sleep 30");
        m.begin_cleanup(&[t.id.clone()]);
        assert_eq!(m.state(&t.id), TriggerState::Stopping);
        let err = m.start(&t, emit.clone()).await.unwrap_err();
        assert!(err.to_string().contains("limpando a execução anterior"), "{err}");
        assert!(m.forget(&t.id).is_err());
        // o fechar do app não mexe no que a limpeza está parando
        m.shutdown_all();
        assert_eq!(m.state(&t.id), TriggerState::Stopping);
        m.end_cleanup(&t.id, Some(&emit));
        assert_eq!(m.state(&t.id), TriggerState::Stopped);
        assert!(matches!(
            events.lock().unwrap().last(),
            Some(TriggerEvent::Status { state: TriggerState::Stopped, .. })
        ));
        m.start(&t, emit.clone()).await.unwrap();
        m.stop(&t.id, emit).await.unwrap();
    }

    #[tokio::test]
    async fn restart_runs_a_new_process() {
        let tmp = Tmp::new();
        let m = setup(&tmp);
        let (emit, _) = recorder();
        let t = shell(&tmp, "echo rodou; sleep 30");
        m.start(&t, emit.clone()).await.unwrap();
        let first = pgid(&m, &t.id);
        m.restart(&t, emit.clone()).await.unwrap();
        let second = pgid(&m, &t.id);
        assert_ne!(first, second);
        assert!(!group_alive(first));
        assert!(group_alive(second));
        assert!(tmp.dir().join(format!("{}.pid", t.id)).exists());
        m.stop(&t.id, emit).await.unwrap();
        assert!(!group_alive(second));
        assert_eq!(m.state(&t.id), TriggerState::Stopped);
    }

    #[tokio::test]
    async fn cleanup_skips_live_owner() {
        let tmp = Tmp::new();
        std::fs::create_dir_all(tmp.dir()).unwrap();
        let pid = detached_sleep(&tmp).await;
        let pid_file = tmp.dir().join("x.pid");
        let me = std::process::id();
        let content = serde_json::json!({
            "pgid": pid,
            "start_time": crate::proc::start_time(pid).unwrap(),
            "owner_pid": me,
            "cwd": tmp.work(),
            "stop": null,
        });
        std::fs::write(&pid_file, content.to_string()).unwrap();

        clean(&tmp, me + 1, |_| true);
        assert!(pid_file.exists());
        assert!(group_alive(pid));

        clean(&tmp, me + 1, |_| false);
        assert!(!pid_file.exists());
        assert!(crate::proc::wait_group_gone(pid, Duration::from_secs(2)).await, "o sleep sobreviveu");
    }

    #[tokio::test]
    async fn cleanup_spares_reused_pid() {
        let tmp = Tmp::new();
        std::fs::create_dir_all(tmp.dir()).unwrap();
        let pid = detached_sleep(&tmp).await;
        let pid_file = tmp.dir().join("y.pid");
        let content = serde_json::json!({
            "pgid": pid,
            "start_time": crate::proc::start_time(pid).unwrap() + 1,
            "owner_pid": 0,
            "cwd": tmp.work(),
            "stop": null,
        });
        std::fs::write(&pid_file, content.to_string()).unwrap();
        clean(&tmp, std::process::id(), |_| false);
        assert!(!pid_file.exists());
        assert!(group_alive(pid), "matou um grupo com outro tempo de início");
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        assert!(crate::proc::wait_group_gone(pid, Duration::from_secs(2)).await);
    }

    #[test]
    fn iso_time_from_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
        assert_eq!(civil_from_days(19723 + 59), (2024, 2, 29));
        assert!(is_app_process(std::process::id()) == false);
    }
}
