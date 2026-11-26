//! Ambientes de teste do Browser Tester (ex.: Acme local).
//! Configuração em `settings` (`test.apps`); a senha de teste no cofre do sistema.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{AppError, AppResult};
use crate::secrets::{SecretStore, test_password_account};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TestApp {
    /// Minúsculo, sem espaço (ex.: `acme`). Compõe a conta da senha no cofre.
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub login_url: String,
    /// Usuário de **desenvolvimento**, nunca de produção.
    #[serde(default)]
    pub username: String,
    /// Hosts em que o Chrome pode navegar. Vazio = hosts das URLs + localhost/127.0.0.1.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Repositórios cujos PRs são testados neste ambiente.
    #[serde(default)]
    pub repos: Vec<String>,
}

impl TestApp {
    pub fn hosts(&self) -> Vec<String> {
        if !self.allowed_hosts.is_empty() {
            return self.allowed_hosts.clone();
        }
        let mut h = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        for u in [&self.base_url, &self.login_url] {
            if let Some(host) = reqwest::Url::parse(u).ok().and_then(|u| u.host_str().map(str::to_lowercase)) {
                if !h.contains(&host) {
                    h.push(host);
                }
            }
        }
        h
    }

    fn validate(&self) -> AppResult<()> {
        let id_ok = !self.id.is_empty() && self.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !id_ok {
            return Err(AppError::Invalid(format!("id de ambiente inválido: {:?}", self.id)));
        }
        for (label, u) in [("URL da aplicação", &self.base_url), ("URL de login", &self.login_url)] {
            if !u.is_empty() && reqwest::Url::parse(u).map(|u| !matches!(u.scheme(), "http" | "https")).unwrap_or(true) {
                return Err(AppError::Invalid(format!("{label} inválida em {}: {u}", self.name)));
            }
        }
        if self.base_url.is_empty() {
            return Err(AppError::Invalid(format!("preencha a URL da aplicação de {}", self.name)));
        }
        Ok(())
    }
}

pub fn load(conn: &Connection) -> AppResult<Vec<TestApp>> {
    Ok(db::get_setting(conn, "test.apps")?.map(serde_json::from_value).transpose()?.unwrap_or_default())
}

pub fn save_all(conn: &Connection, apps: &[TestApp]) -> AppResult<()> {
    for a in apps {
        a.validate()?;
    }
    db::set_setting(conn, "test.apps", &serde_json::to_value(apps)?)
}

pub fn for_repo(conn: &Connection, slug: &str) -> AppResult<Option<TestApp>> {
    Ok(load(conn)?.into_iter().find(|a| a.repos.iter().any(|r| r == slug)))
}

/// `.env` da tela de configuração da Fase 0: valores entre aspas são JSON.
fn parse_env(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                serde_json::from_str::<String>(v).unwrap_or_default()
            } else {
                v.to_owned()
            };
            (k.trim().to_owned(), v)
        })
        .collect()
}

/// Importa os ambientes de teste do `.env` da Fase 0, uma vez (só se ainda não há nenhum).
/// Ambientes sem URL são ignorados. Devolve quantos foram importados.
pub fn import_phase0_env(conn: &Connection, store: &dyn SecretStore, env_path: &Path) -> AppResult<usize> {
    if db::get_setting(conn, "test.apps")?.is_some() || !env_path.exists() {
        return Ok(0);
    }
    let env = parse_env(&std::fs::read_to_string(env_path)?);
    let ids = env.get("TEST_APPS").cloned().unwrap_or_default();
    let mut apps = Vec::new();
    for raw in ids.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let key = raw.to_ascii_uppercase();
        let get = |suf: &str| env.get(&format!("TEST_{key}_{suf}")).cloned().unwrap_or_default();
        let list = |suf: &str| get(suf).split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let app = TestApp {
            id: raw.to_ascii_lowercase(),
            name: Some(get("NAME")).filter(|s| !s.is_empty()).unwrap_or_else(|| raw.to_owned()),
            base_url: get("BASE_URL"),
            login_url: get("LOGIN_URL"),
            username: get("USERNAME"),
            allowed_hosts: list("ALLOWED_HOSTS"),
            repos: list("REPOS"),
        };
        if app.base_url.is_empty() {
            continue;
        }
        let pw = get("PASSWORD");
        if !pw.is_empty() {
            store.set(&test_password_account(&app.id), &pw)?;
        }
        apps.push(app);
    }
    save_all(conn, &apps)?;
    Ok(apps.len())
}

/// O que a UI mostra (sem a senha).
#[derive(Serialize)]
pub struct TestAppView {
    #[serde(flatten)]
    pub app: TestApp,
    pub password_saved: bool,
    pub effective_hosts: Vec<String>,
}

pub fn views(conn: &Connection, store: &dyn SecretStore) -> AppResult<Vec<TestAppView>> {
    load(conn)?
        .into_iter()
        .map(|app| {
            Ok(TestAppView {
                password_saved: store.get(&test_password_account(&app.id))?.is_some(),
                effective_hosts: app.hosts(),
                app,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryStore;

    #[test]
    fn importa_env_da_fase0_uma_vez() {
        let conn = db::open_in_memory().unwrap();
        let store = MemoryStore::default();
        let dir = std::env::temp_dir().join(format!("prr-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        std::fs::write(
            &env,
            "# x\nTEST_APPS=\"acme,beta\"\nTEST_ACME_NAME=\"Acme\"\n\
             TEST_ACME_BASE_URL=\"http://localhost:8080\"\nTEST_ACME_LOGIN_URL=\"http://localhost:8080/login\"\n\
             TEST_ACME_USERNAME=\"dev@x\"\nTEST_ACME_PASSWORD=\"s3nh4-dev\"\nTEST_ACME_ALLOWED_HOSTS=\"\"\n\
             TEST_ACME_REPOS=\"acme-back,acme-front\"\nTEST_BETA_NAME=\"Beta\"\n\
             TEST_BETA_BASE_URL=\"\"\n",
        )
        .unwrap();

        assert_eq!(import_phase0_env(&conn, &store, &env).unwrap(), 1, "Beta sem URL fica de fora");
        let app = for_repo(&conn, "acme-front").unwrap().unwrap();
        assert_eq!(app.id, "acme");
        assert_eq!(app.hosts(), vec!["localhost", "127.0.0.1"]);
        assert_eq!(store.get("test-acme-password").unwrap().as_deref(), Some("s3nh4-dev"));
        assert!(!db::get_setting(&conn, "test.apps").unwrap().unwrap().to_string().contains("s3nh4"), "senha não vai para o banco");

        // segunda vez não importa de novo
        assert_eq!(import_phase0_env(&conn, &store, &env).unwrap(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hosts_padrao_incluem_o_host_da_url() {
        let a = TestApp { base_url: "http://app.local:8080".into(), ..Default::default() };
        assert_eq!(a.hosts(), vec!["localhost", "127.0.0.1", "app.local"]);
    }

    #[test]
    fn recusa_url_invalida() {
        let conn = db::open_in_memory().unwrap();
        let bad = TestApp { id: "x".into(), name: "X".into(), base_url: "ftp://x".into(), ..Default::default() };
        assert!(save_all(&conn, &[bad]).is_err());
    }
}
