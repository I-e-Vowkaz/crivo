//! Sinais em grupo de processos: o cancelamento do review e a parada dos gatilhos.

use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(200);

/// Alvo do primeiro sinal: só o processo (o `claude` trata o Ctrl+C e encerra os filhos) ou o
/// grupo inteiro (como o Ctrl+C do terminal).
pub enum First {
    Pid(i32),
    Group(i32),
}

/// O processo existe? Mesma conferência do cancelamento original do review (`kill(pid, 0)`).
pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Algum processo do grupo ainda existe (zumbi conta até ser recolhido).
pub fn group_alive(pgid: i32) -> bool {
    pgid > 1 && unsafe { libc::killpg(pgid, 0) } == 0
}

/// Sinal no grupo inteiro. Recusa 0 e 1: `killpg(0)` atingiria o grupo do próprio app.
pub fn signal_group(pgid: i32, sig: i32) {
    if pgid > 1 {
        unsafe { libc::killpg(pgid, sig) };
    }
}

/// Espera o grupo sumir, conferindo a cada 200 ms. Devolve `false` se o prazo acabou antes.
pub async fn wait_group_gone(pgid: i32, max: Duration) -> bool {
    let end = Instant::now() + max;
    loop {
        if !group_alive(pgid) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Versão bloqueante, para vários grupos (fechar o app e limpeza ao abrir).
pub fn wait_groups_gone_blocking(pgids: &[i32], max: Duration) -> bool {
    let end = Instant::now() + max;
    loop {
        if !pgids.iter().any(|&g| group_alive(g)) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Tempo de início do processo (campo 22 de `/proc/<pid>/stat`, em ticks desde o boot).
/// Junto com o PID, identifica o processo: um PID reaproveitado tem outro tempo de início.
pub fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // o campo 2 (nome) vem entre parênteses e pode ter espaço; conta a partir do último ')'
    let rest = &stat[stat.rfind(')')? + 1..];
    // depois do ')' vem o campo 3; o 22 é o índice 19
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Escala os sinais: o primeiro no PID ou no grupo, e cada passo seguinte no grupo `pgid`
/// depois da espera, parando assim que o alvo do primeiro sinal sumir (conferido a cada
/// 200 ms; volta na hora em vez de dormir o prazo inteiro, com os mesmos sinais).
///
/// O review usa `escalate(pid, (First::Pid(pid), SIGINT), &[(10 s, SIGTERM), (5 s, SIGKILL)])`;
/// os gatilhos, `escalate(pgid, (First::Group(pgid), SIGINT), &[(prazo, SIGKILL)])`.
pub async fn escalate(pgid: i32, first: (First, i32), steps: &[(Duration, i32)]) {
    let (target, sig) = first;
    let alive = || match target {
        First::Pid(pid) => pid_alive(pid),
        First::Group(g) => group_alive(g),
    };
    match target {
        First::Pid(pid) if pid > 0 => unsafe {
            libc::kill(pid, sig);
        },
        First::Pid(_) => return,
        First::Group(g) => signal_group(g, sig),
    }
    for &(wait, sig) in steps {
        let end = Instant::now() + wait;
        loop {
            if !alive() {
                return; // já saiu
            }
            let now = Instant::now();
            if now >= end {
                break;
            }
            tokio::time::sleep(POLL.min(end - now)).await;
        }
        signal_group(pgid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    #[tokio::test]
    async fn review_cancel_unchanged() {
        let mut child = std::process::Command::new("bash")
            .args(["-c", "trap \"\" INT; sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // dá tempo de o trap ser instalado
        tokio::time::sleep(Duration::from_millis(300)).await;
        escalate(pid, (First::Pid(pid), libc::SIGINT), &[(Duration::from_millis(200), libc::SIGTERM)]).await;
        let st = child.wait().unwrap();
        assert_eq!(st.signal(), Some(libc::SIGTERM), "{st:?}");
        assert!(wait_group_gone(pid, Duration::from_secs(2)).await);
        assert!(!group_alive(pid));
    }
}
