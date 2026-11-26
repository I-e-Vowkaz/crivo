//! Bitbucket Cloud: cliente, escopos do token e validação da conta.

pub mod client;
pub mod comments;
pub mod scopes;
pub mod sync;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{AppError, AppResult};
use client::Client;
use scopes::ScopeCheck;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub uuid: String,
    pub display_name: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// O que a UI mostra da conta conectada. Nunca contém o token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub user: User,
    pub scopes: Vec<String>,
    pub workspace: String,
    /// Repositórios que o token enxerga no workspace (`size` da listagem).
    pub workspace_repos: Option<u64>,
    pub validated_at: Option<String>,
}

/// Confere o token contra a API: só leitura, com os três escopos que o app usa.
/// Não grava nada: quem chama decide se salva.
pub async fn validate(client: &Client, workspace: &str) -> AppResult<Account> {
    let r = client.get_json::<User>("/user", &[]).await?;
    let header = r.headers.get("x-oauth-scopes").and_then(|v| v.to_str().ok());
    let Some(scopes) = scopes::parse(header) else {
        return Err(AppError::Invalid(
            "o Bitbucket não informou os escopos deste token (header X-Oauth-Scopes), então não dá para \
             validar suas permissões. Crie um API token com escopos: read:pullrequest, read:repository e read:user"
                .into(),
        ));
    };
    let check = scopes::check(&scopes);
    if !check.ok() {
        return Err(AppError::Invalid(scope_error(&check)));
    }

    let ws: serde_json::Value = client
        .get_json(&format!("/repositories/{workspace}"), &[("pagelen", "1"), ("fields", "size")])
        .await
        .map_err(|e| AppError::Invalid(format!("o token não acessa o workspace {workspace:?}: {e}")))?
        .body;

    Ok(Account {
        user: r.body,
        scopes,
        workspace: workspace.to_owned(),
        workspace_repos: ws.get("size").and_then(|v| v.as_u64()),
        validated_at: None,
    })
}

fn scope_error(c: &ScopeCheck) -> String {
    let mut parts = Vec::new();
    if !c.forbidden.is_empty() {
        parts.push(format!(
            "o token tem permissões não usadas pelo app ({})",
            c.forbidden.join(", ")
        ));
    }
    if !c.missing.is_empty() {
        parts.push(format!("faltam os escopos {}", c.missing.join(", ")));
    }
    format!("{}. Crie um token só de leitura, com read:pullrequest, read:repository e read:user", parts.join("; "))
}

/// Email e workspace salvos nas configurações; erro claro se faltarem.
pub fn account_settings(conn: &Connection) -> AppResult<(String, String)> {
    let get = |k: &str| -> AppResult<String> {
        Ok(db::get_setting(conn, k)?.and_then(|v| v.as_str().map(str::trim).map(str::to_owned)).unwrap_or_default())
    };
    let (email, workspace) = (get("bitbucket.email")?, get("bitbucket.workspace")?);
    if !email.contains('@') {
        return Err(AppError::Invalid("preencha e salve o email da conta Atlassian".into()));
    }
    let ws_ok = !workspace.is_empty()
        && workspace.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ws_ok {
        return Err(AppError::Invalid("preencha e salve o workspace (ex.: acme)".into()));
    }
    Ok((email, workspace))
}

/// Grava o resultado da validação: escopos e data na credencial, conta nas configurações.
pub fn record_validation(conn: &Connection, account: &Account) -> AppResult<Account> {
    let n = conn.execute(
        "UPDATE credentials SET scopes_json = ?1, validated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE kind = 'bitbucket'",
        params![serde_json::to_string(&account.scopes)?],
    )?;
    if n == 0 {
        return Err(AppError::Invalid("nenhum token do Bitbucket salvo".into()));
    }
    let validated_at: Option<String> = conn
        .query_row("SELECT validated_at FROM credentials WHERE kind = 'bitbucket'", [], |r| r.get(0))
        .optional()?
        .flatten();
    let saved = Account { validated_at, ..account.clone() };
    db::set_setting(conn, "bitbucket.account", &serde_json::to_value(&saved)?)?;
    Ok(saved)
}

/// Conta validada, se houver e se ainda bater com o token salvo (sem validação = `None`).
pub fn stored_account(conn: &Connection) -> AppResult<Option<Account>> {
    let validated: Option<Option<String>> = conn
        .query_row("SELECT validated_at FROM credentials WHERE kind = 'bitbucket'", [], |r| r.get(0))
        .optional()?;
    if !matches!(validated, Some(Some(_))) {
        return Ok(None);
    }
    Ok(db::get_setting(conn, "bitbucket.account")?.map(serde_json::from_value).transpose()?)
}

/// Esquece a conta validada (ao trocar ou remover o token).
pub fn forget_account(conn: &Connection) -> AppResult<()> {
    conn.execute("DELETE FROM settings WHERE key = 'bitbucket.account'", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::client::testserver::{Canned, ok, start};
    use super::*;

    #[test]
    fn erro_de_escopo_pede_so_leitura() {
        let c = scopes::check(&["read:pullrequest", "read:repository", "read:user", "write:pullrequest:bitbucket"].map(String::from));
        let msg = scope_error(&c);
        assert!(msg.contains("write:pullrequest:bitbucket"), "{msg}");
        assert!(msg.ends_with("Crie um token só de leitura, com read:pullrequest, read:repository e read:user"), "{msg}");
        assert!(!msg.contains("opcional"), "{msg}");
    }
    use crate::secrets::{self, MemoryStore, SecretKind};
    use serde_json::json;

    const USER: &str = r#"{"uuid":"{aa-bb}","display_name":"Fulano","account_id":"123"}"#;
    const READ_SCOPES: &str = "read:repository:bitbucket, read:pullrequest:bitbucket, read:user:bitbucket";

    fn user_resp(scopes: Option<&str>) -> Canned {
        Canned {
            status: 200,
            headers: scopes.map(|s| vec![("X-Oauth-Scopes", s.to_owned())]).unwrap_or_default(),
            body: USER.into(),
        }
    }

    fn client(base: &str) -> Client {
        Client::with_base(base, "eu@empresa.com", "tok-secreto-123456").unwrap()
    }

    #[tokio::test]
    async fn token_so_leitura_valida_usuario_e_workspace() {
        let srv = start(vec![user_resp(Some(READ_SCOPES)), ok(r#"{"size":42}"#)]);
        let acc = validate(&client(&srv.base), "acme").await.unwrap();
        assert_eq!(acc.user.uuid, "{aa-bb}");
        assert_eq!(acc.workspace_repos, Some(42));
        let reqs = srv.requests.lock().unwrap();
        assert!(reqs[1].starts_with("GET /2.0/repositories/acme?pagelen=1&fields=size "), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn token_com_escrita_e_recusado() {
        let srv = start(vec![user_resp(Some(&format!("{READ_SCOPES}, write:repository:bitbucket")))]);
        let err = validate(&client(&srv.base), "acme").await.unwrap_err().to_string();
        assert!(err.contains("permissões não usadas pelo app (write:repository:bitbucket)"), "{err}");
    }

    #[tokio::test]
    async fn sem_header_de_escopos_e_recusado() {
        let srv = start(vec![user_resp(None)]);
        let err = validate(&client(&srv.base), "acme").await.unwrap_err().to_string();
        assert!(err.contains("X-Oauth-Scopes"), "{err}");
    }

    #[tokio::test]
    async fn workspace_inacessivel_e_recusado() {
        let srv = start(vec![user_resp(Some(READ_SCOPES)), Canned { status: 404, headers: vec![], body: "{}".into() }]);
        let err = validate(&client(&srv.base), "outro").await.unwrap_err().to_string();
        assert!(err.contains("não acessa o workspace \"outro\""), "{err}");
    }

    #[test]
    fn configuracoes_obrigatorias() {
        let conn = db::open_in_memory().unwrap();
        assert!(account_settings(&conn).unwrap_err().to_string().contains("email"));
        db::set_setting(&conn, "bitbucket.email", &json!("eu@empresa.com")).unwrap();
        assert!(account_settings(&conn).unwrap_err().to_string().contains("workspace"));
        db::set_setting(&conn, "bitbucket.workspace", &json!("start gov")).unwrap();
        assert!(account_settings(&conn).is_err());
        db::set_setting(&conn, "bitbucket.workspace", &json!(" acme ")).unwrap();
        assert_eq!(account_settings(&conn).unwrap(), ("eu@empresa.com".into(), "acme".into()));
    }

    #[test]
    fn registro_da_validacao_e_esquecimento() {
        let conn = db::open_in_memory().unwrap();
        let acc = Account {
            user: serde_json::from_str(USER).unwrap(),
            scopes: vec!["read:user:bitbucket".into()],
            workspace: "acme".into(),
            workspace_repos: Some(3),
            validated_at: None,
        };
        assert!(record_validation(&conn, &acc).is_err(), "sem token salvo não registra");

        secrets::save(&conn, &MemoryStore::default(), SecretKind::Bitbucket, "tok-secreto-123456").unwrap();
        assert_eq!(stored_account(&conn).unwrap(), None, "token salvo mas não validado");
        let saved = record_validation(&conn, &acc).unwrap();
        assert!(saved.validated_at.is_some());
        assert_eq!(stored_account(&conn).unwrap(), Some(saved));

        // salvar outro token zera a validação
        secrets::save(&conn, &MemoryStore::default(), SecretKind::Bitbucket, "outro-token-7890").unwrap();
        assert_eq!(stored_account(&conn).unwrap(), None);
        forget_account(&conn).unwrap();
        assert_eq!(db::get_setting(&conn, "bitbucket.account").unwrap(), None);
    }
}
