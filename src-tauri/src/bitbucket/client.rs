//! Cliente HTTP do Bitbucket Cloud, com as lições da Fase 0:
//! - happy eyeballs (do `reqwest`/`hyper`) + timeout de conexão curto: o IPv6 da rede
//!   local trava conexões de forma intermitente (achado 9);
//! - retry em 429/5xx/erro de rede, respeitando `Retry-After`;
//! - uma linha de log por chamada (caminho, status, tempo, bytes), sempre mascarada;
//! - a credencial só vai para o host da API: um `next` apontando para outro host é recusado.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::de::DeserializeOwned;

use crate::error::{AppError, AppResult};
use crate::secrets::Redactor;

pub const API: &str = "https://api.bitbucket.org/2.0";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: u32 = 5;
const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct Response<T> {
    pub body: T,
    pub headers: HeaderMap,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    email: String,
    token: String,
    redactor: Redactor,
    retry_base: Duration,
    requests: AtomicU32,
    started: Instant,
}

impl Client {
    pub fn new(email: &str, token: &str) -> AppResult<Self> {
        Self::with_base(API, email, token)
    }

    pub fn with_base(base: &str, email: &str, token: &str) -> AppResult<Self> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("pr-review-orchestrator/", env!("CARGO_PKG_VERSION")));
        if base.starts_with("http://127.0.0.1") {
            b = b.no_proxy(); // testes com servidor local
        }
        let http = b.build().map_err(|e| AppError::Invalid(format!("cliente HTTP: {e}")))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            email: email.to_owned(),
            token: token.to_owned(),
            redactor: Redactor::new([token.to_owned(), format!("{email}:{token}")]),
            retry_base: Duration::from_secs(1),
            requests: AtomicU32::new(0),
            started: Instant::now(),
        })
    }

    #[cfg(test)]
    pub fn with_retry_base(mut self, d: Duration) -> Self {
        self.retry_base = d;
        self
    }

    /// Chamadas feitas por este cliente (vai para `sync_runs.requests` no passo 3).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn redact(&self, text: &str) -> String {
        self.redactor.redact(text)
    }

    fn log(&self, msg: &str) {
        eprintln!("[bitbucket +{:6.1}s] {}", self.started.elapsed().as_secs_f64(), self.redact(msg));
    }

    /// Caminho relativo à API (`/user`) ou URL absoluta do mesmo host (link `next`).
    fn url(&self, path_or_url: &str) -> AppResult<String> {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            if !path_or_url.starts_with(&format!("{}/", self.base)) {
                return Err(AppError::Invalid(format!(
                    "o Bitbucket mandou um link para outro endereço ({}); a credencial não é enviada para fora de {}",
                    self.redact(path_or_url),
                    self.base
                )));
            }
            Ok(path_or_url.to_owned())
        } else {
            Ok(format!("{}/{}", self.base, path_or_url.trim_start_matches('/')))
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        (self.retry_base * 2u32.saturating_pow(attempt)).min(MAX_WAIT)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path_or_url: &str, query: &[(&str, &str)]) -> AppResult<Response<T>> {
        let url = self.url(path_or_url)?;
        let short = url.strip_prefix(&self.base).unwrap_or(&url).to_owned();

        for attempt in 0..MAX_ATTEMPTS {
            let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
            let t = Instant::now();
            let sent = self
                .http
                .get(&url)
                .query(query)
                .basic_auth(&self.email, Some(&self.token))
                .header("Accept", "application/json")
                .send()
                .await;

            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    let wait = self.backoff(attempt);
                    self.log(&format!(
                        "#{n} GET {short}: erro de rede após {:.1}s ({e}); tentativa {}/{MAX_ATTEMPTS}, esperando {:.1}s",
                        t.elapsed().as_secs_f64(),
                        attempt + 1,
                        wait.as_secs_f64()
                    ));
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(wait).await;
                    }
                    continue;
                }
            };

            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let wait = retry_after(resp.headers()).unwrap_or_else(|| self.backoff(attempt));
                self.log(&format!(
                    "#{n} GET {short}: HTTP {} em {:.1}s; tentativa {}/{MAX_ATTEMPTS}, esperando {:.1}s",
                    status.as_u16(),
                    t.elapsed().as_secs_f64(),
                    attempt + 1,
                    wait.as_secs_f64()
                ));
                if attempt + 1 < MAX_ATTEMPTS {
                    tokio::time::sleep(wait).await;
                }
                continue;
            }

            let headers = resp.headers().clone();
            let bytes = resp.bytes().await.map_err(|e| AppError::Invalid(self.redact(&format!("lendo {short}: {e}"))))?;
            self.log(&format!(
                "#{n} GET {short}: HTTP {} em {:.1}s, {} bytes",
                status.as_u16(),
                t.elapsed().as_secs_f64(),
                bytes.len()
            ));

            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(AppError::Invalid(format!(
                    "o Bitbucket recusou a credencial (HTTP {}) em {short}: confira o email e o token, e os escopos read:pullrequest, read:repository e read:user",
                    status.as_u16()
                )));
            }
            if !status.is_success() {
                let snippet: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
                return Err(AppError::Invalid(self.redact(&format!("HTTP {} em {short}: {snippet}", status.as_u16()))));
            }
            let body = serde_json::from_slice(&bytes)
                .map_err(|e| AppError::Invalid(format!("resposta inesperada de {short}: {e}")))?;
            return Ok(Response { body, headers });
        }
        Err(AppError::Invalid(format!("o Bitbucket não respondeu em {MAX_ATTEMPTS} tentativas ({short}); veja o log")))
    }
}

/// `Retry-After` em segundos (o formato de data não é usado pelo Bitbucket), com teto.
fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let secs: u64 = h.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_WAIT))
}

#[cfg(test)]
pub(crate) mod testserver {
    //! Servidor HTTP/1.1 mínimo: responde, em ordem, a lista de respostas dada e
    //! guarda as requisições recebidas. Sem rede externa.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct Canned {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: String,
    }

    pub fn ok(body: &str) -> Canned {
        Canned { status: 200, headers: vec![], body: body.to_owned() }
    }

    pub struct Server {
        pub base: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    pub fn start(responses: Vec<Canned>) -> Server {
        start_with(|_| responses)
    }

    /// Como `start`, mas as respostas podem citar o endereço do servidor (links `next`).
    pub fn start_with(make: impl FnOnce(&str) -> Vec<Canned>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/2.0", listener.local_addr().unwrap());
        let responses = make(&base);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for canned in responses {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; len];
                let _ = reader.read_exact(&mut body);
                seen.lock().unwrap().push(head);
                let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", canned.status, canned.body.len());
                for (k, v) in &canned.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&canned.body);
                let _ = stream.write_all(out.as_bytes());
            }
        });
        Server { base, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::{Canned, ok, start};
    use super::*;
    use serde_json::Value;

    fn client(base: &str) -> Client {
        Client::with_base(base, "eu@empresa.com", "tok-secreto-123456").unwrap().with_retry_base(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn envia_basic_auth_e_le_json_e_headers() {
        let srv = start(vec![Canned {
            status: 200,
            headers: vec![("X-Oauth-Scopes", "read:user:bitbucket".into())],
            body: r#"{"uuid":"{u}"}"#.into(),
        }]);
        let c = client(&srv.base);
        let r: Response<Value> = c.get_json("/user", &[("fields", "uuid")]).await.unwrap();
        assert_eq!(r.body["uuid"], "{u}");
        assert_eq!(r.headers.get("x-oauth-scopes").unwrap(), "read:user:bitbucket");
        let req = srv.requests.lock().unwrap()[0].clone();
        assert!(req.starts_with("GET /2.0/user?fields=uuid "), "{req}");
        assert!(req.to_ascii_lowercase().contains("authorization: basic "), "{req}");
    }

    #[tokio::test]
    async fn repete_em_429_e_5xx_e_depois_responde() {
        let srv = start(vec![
            Canned { status: 429, headers: vec![("Retry-After", "0".into())], body: String::new() },
            Canned { status: 503, headers: vec![], body: String::new() },
            ok(r#"{"ok":true}"#),
        ]);
        let c = client(&srv.base);
        let r: Response<Value> = c.get_json("/user", &[]).await.unwrap();
        assert_eq!(r.body["ok"], true);
        assert_eq!(c.requests(), 3);
    }

    #[tokio::test]
    async fn desiste_depois_do_maximo_de_tentativas() {
        let srv = start((0..MAX_ATTEMPTS).map(|_| Canned { status: 500, headers: vec![], body: String::new() }).collect());
        let c = client(&srv.base);
        let err = c.get_json::<Value>("/user", &[]).await.unwrap_err().to_string();
        assert!(err.contains("não respondeu"), "{err}");
        assert_eq!(c.requests(), MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn credencial_recusada_vira_mensagem_clara_sem_retry() {
        let srv = start(vec![Canned { status: 401, headers: vec![], body: "nope".into() }]);
        let c = client(&srv.base);
        let err = c.get_json::<Value>("/user", &[]).await.unwrap_err().to_string();
        assert!(err.contains("recusou a credencial (HTTP 401)"), "{err}");
        assert_eq!(c.requests(), 1);
    }

    #[tokio::test]
    async fn erro_com_o_token_no_corpo_sai_mascarado() {
        let srv = start(vec![Canned { status: 400, headers: vec![], body: "token tok-secreto-123456 inválido".into() }]);
        let err = client(&srv.base).get_json::<Value>("/user", &[]).await.unwrap_err().to_string();
        assert!(!err.contains("tok-secreto-123456"), "{err}");
        assert!(err.contains("***"), "{err}");
    }

    #[tokio::test]
    async fn recusa_link_para_outro_host() {
        let c = client("http://127.0.0.1:1/2.0");
        let err = c.get_json::<Value>("https://evil.example.com/2.0/x", &[]).await.unwrap_err().to_string();
        assert!(err.contains("outro endereço"), "{err}");
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn retry_after_tem_teto_e_ignora_lixo() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(retry_after(&h), Some(MAX_WAIT));
        h.insert(RETRY_AFTER, "amanhã".parse().unwrap_or_else(|_| "x".parse().unwrap()));
        assert_eq!(retry_after(&h), None);
    }
}
