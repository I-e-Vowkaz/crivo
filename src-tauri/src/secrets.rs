//! Segredos (tokens do Bitbucket e do Jira) no cofre do sistema via `keyring`:
//! Secret Service no Linux, Keychain no macOS, Credential Manager no Windows.
//!
//! Regras:
//! - o segredo nunca vai para o SQLite: lá fica só a referência (`credentials`);
//! - nenhum comando devolve o segredo para a UI, só se ele está salvo;
//! - todo texto que pode conter um segredo passa pelo `Redactor` antes de ir para log ou UI.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub const SERVICE: &str = "pr-review-orchestrator";
const MAX_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    Bitbucket,
    Jira,
    Github,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::Bitbucket => "bitbucket",
            SecretKind::Jira => "jira",
            SecretKind::Github => "github",
        }
    }
    /// Conta no keyring. Fixa por tipo: há um token de cada.
    pub fn account(self) -> String {
        format!("{}-token", self.as_str())
    }
}

/// Cofre por conta (`bitbucket-token`, `test-acme-password`...).
pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, value: &str) -> AppResult<()>;
    fn get(&self, account: &str) -> AppResult<Option<String>>;
    /// Apagar o que não existe não é erro.
    fn delete(&self, account: &str) -> AppResult<()>;
}

/// Conta da senha de login de um ambiente de teste.
pub fn test_password_account(app_id: &str) -> String {
    format!("test-{app_id}-password")
}

/// Cofre real do sistema.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(account: &str) -> AppResult<keyring::Entry> {
        keyring::Entry::new(SERVICE, account).map_err(|e| AppError::Secret(e.to_string()))
    }

    /// `Ok(())` se o cofre do sistema está acessível.
    pub fn status() -> Result<(), String> {
        keyring::Entry::store_status().as_ref().map(|_| ()).map_err(|e| e.to_string())
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, account: &str, value: &str) -> AppResult<()> {
        Self::entry(account)?.set_password(value).map_err(|e| AppError::Secret(e.to_string()))
    }
    fn get(&self, account: &str) -> AppResult<Option<String>> {
        match Self::entry(account)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AppError::Secret(e.to_string())),
        }
    }
    fn delete(&self, account: &str) -> AppResult<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::Secret(e.to_string())),
        }
    }
}

/// Cofre em memória, para testes.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn set(&self, account: &str, value: &str) -> AppResult<()> {
        self.0.lock().unwrap().insert(account.to_owned(), value.to_owned());
        Ok(())
    }
    fn get(&self, account: &str) -> AppResult<Option<String>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }
    fn delete(&self, account: &str) -> AppResult<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

/// Valida e normaliza um token colado na UI: tira espaços das pontas; recusa vazio,
/// espaço interno, quebra de linha e tamanho absurdo.
pub fn normalize_secret(raw: &str) -> AppResult<String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(AppError::Invalid("o token está vazio".into()));
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AppError::Invalid("o token tem espaço ou quebra de linha no meio; cole de novo".into()));
    }
    if v.len() > MAX_LEN {
        return Err(AppError::Invalid(format!("o token passa de {MAX_LEN} caracteres")));
    }
    Ok(v.to_owned())
}

/// Salva o segredo no cofre e registra a referência no SQLite. A validação contra a API
/// (usuário, escopos) é feita por quem chama, antes ou depois, e grava `validated_at`.
pub fn save(conn: &Connection, store: &dyn SecretStore, kind: SecretKind, raw: &str) -> AppResult<()> {
    let value = normalize_secret(raw)?;
    store.set(&kind.account(), &value)?;
    conn.execute(
        "INSERT INTO credentials (kind, keyring_service, keyring_account) VALUES (?1, ?2, ?3)
         ON CONFLICT (kind) DO UPDATE SET keyring_service = excluded.keyring_service,
           keyring_account = excluded.keyring_account, scopes_json = NULL, validated_at = NULL,
           created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        params![kind.as_str(), SERVICE, kind.account()],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, store: &dyn SecretStore, kind: SecretKind) -> AppResult<()> {
    store.delete(&kind.account())?;
    conn.execute("DELETE FROM credentials WHERE kind = ?1", [kind.as_str()])?;
    Ok(())
}

pub fn is_saved(store: &dyn SecretStore, kind: SecretKind) -> AppResult<bool> {
    Ok(store.get(&kind.account())?.is_some())
}

/// Mascara segredos conhecidos em qualquer texto (mensagens de erro, logs).
#[derive(Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Ignora valores curtos demais: mascarar "a" estragaria qualquer texto.
    pub fn new<I: IntoIterator<Item = S>, S: Into<String>>(secrets: I) -> Self {
        let mut secrets: Vec<String> = secrets.into_iter().map(Into::into).filter(|s| s.len() >= 8).collect();
        // os mais longos primeiro, para um segredo que contém outro ser mascarado inteiro
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |acc, s| acc.replace(s.as_str(), "***"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn salva_no_cofre_e_so_a_referencia_no_banco() {
        let conn = db::open_in_memory().unwrap();
        let store = MemoryStore::default();
        save(&conn, &store, SecretKind::Bitbucket, "  ATATT3xFfGF0-segredo-de-teste  ").unwrap();

        assert_eq!(store.get("bitbucket-token").unwrap().as_deref(), Some("ATATT3xFfGF0-segredo-de-teste"));
        let (svc, acc): (String, String) = conn
            .query_row("SELECT keyring_service, keyring_account FROM credentials WHERE kind = 'bitbucket'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((svc.as_str(), acc.as_str()), (SERVICE, "bitbucket-token"));

        // o segredo não aparece em nenhuma tabela
        let dump: String = conn
            .query_row(
                "SELECT group_concat(kind || keyring_service || keyring_account || ifnull(scopes_json,''), '|')
                 FROM credentials",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!dump.contains("segredo"));
    }

    #[test]
    fn remover_apaga_cofre_e_referencia_e_e_idempotente() {
        let conn = db::open_in_memory().unwrap();
        let store = MemoryStore::default();
        save(&conn, &store, SecretKind::Jira, "token-do-jira-123").unwrap();
        remove(&conn, &store, SecretKind::Jira).unwrap();
        remove(&conn, &store, SecretKind::Jira).unwrap();
        assert!(!is_saved(&store, SecretKind::Jira).unwrap());
        let n: i64 = conn.query_row("SELECT count(*) FROM credentials", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn recusa_token_vazio_ou_com_espaco_interno() {
        for bad in ["", "   ", "abc def", "abc\ndef", "abc\tdef"] {
            assert!(normalize_secret(bad).is_err(), "deveria recusar {bad:?}");
        }
        assert!(normalize_secret(&"x".repeat(MAX_LEN + 1)).is_err());
        assert_eq!(normalize_secret("\n abc123 \n").unwrap(), "abc123");
    }

    #[test]
    fn redactor_mascara_segredos_e_ignora_curtos() {
        let r = Redactor::new(["segredo-longo-1", "abc", "segredo-longo-1-ainda-maior"]);
        assert_eq!(
            r.redact("x segredo-longo-1-ainda-maior y segredo-longo-1 z abc"),
            "x *** y *** z abc"
        );
    }

    /// Usa o cofre real do sistema. Rodar à mão: `cargo test -- --ignored keyring_real`.
    #[test]
    #[ignore]
    fn keyring_real_ida_e_volta() {
        KeyringStore::status().expect("cofre do sistema indisponível");
        let store = KeyringStore;
        let kind = "jira-token";
        let before = store.get(kind).unwrap();
        assert!(before.is_none(), "já existe um token do Jira salvo; o teste não sobrescreve");
        store.set(kind, "teste-keyring-real-123").unwrap();
        assert_eq!(store.get(kind).unwrap().as_deref(), Some("teste-keyring-real-123"));
        store.delete(kind).unwrap();
        assert_eq!(store.get(kind).unwrap(), None);
    }
}
