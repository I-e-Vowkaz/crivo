//! SQLite: abertura, migrations versionadas por `PRAGMA user_version` e configurações.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::error::{AppError, AppResult};

/// Em ordem. A posição + 1 é a versão do schema depois de aplicar. Nunca editar uma
/// migration já publicada: criar a próxima.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    ("0002_reviews", include_str!("migrations/0002_reviews.sql")),
    ("0003_review_warning", include_str!("migrations/0003_review_warning.sql")),
    ("0004_browser", include_str!("migrations/0004_browser.sql")),
    ("0005_browser_checkout", include_str!("migrations/0005_browser_checkout.sql")),
    ("0006_chat", include_str!("migrations/0006_chat.sql")),
    ("0007_improvements", include_str!("migrations/0007_improvements.sql")),
    ("0008_improvement_scores", include_str!("migrations/0008_improvement_scores.sql")),
    ("0009_fix", include_str!("migrations/0009_fix.sql")),
    ("0010_branch_reviews", include_str!("migrations/0010_branch_reviews.sql")),
    ("0011_fix_new_status", include_str!("migrations/0011_fix_new_status.sql")),
    ("0012_run_steps", include_str!("migrations/0012_run_steps.sql")),
    ("0013_providers", include_str!("migrations/0013_providers.sql")),
    ("0014_request_changes", include_str!("migrations/0014_request_changes.sql")),
];

pub fn open(path: &Path) -> AppResult<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&mut conn)?;
    Ok(conn)
}

#[cfg(test)]
pub fn open_in_memory() -> AppResult<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> AppResult<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

pub fn schema_version(conn: &Connection) -> AppResult<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

pub fn latest_version() -> u32 {
    MIGRATIONS.len() as u32
}

/// Aplica as migrations pendentes, cada uma na sua transação. Retorna a versão final.
pub fn migrate(conn: &mut Connection) -> AppResult<u32> {
    let current = schema_version(conn)?;
    if current > latest_version() {
        return Err(AppError::Invalid(format!(
            "o banco está na versão {current}, mais nova que este app (versão {}); atualize o app",
            latest_version()
        )));
    }
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        // migration que recria tabela: chaves estrangeiras desligadas fora da transação (o
        // PRAGMA não tem efeito dentro dela) e conferidas antes do commit
        let fk_off = sql.starts_with("-- foreign_keys: off");
        if fk_off {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
        }
        let result = apply_migration(conn, name, sql, (i + 1) as u32, fk_off);
        if fk_off {
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        result?;
    }
    schema_version(conn)
}

fn apply_migration(conn: &mut Connection, name: &str, sql: &str, version: u32, check_fk: bool) -> AppResult<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(sql).map_err(|e| AppError::Invalid(format!("migration {name} falhou: {e}")))?;
    if check_fk {
        let broken: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
        if broken > 0 {
            return Err(AppError::Invalid(format!("migration {name} deixaria {broken} referência(s) quebrada(s)")));
        }
    }
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}

/// Chaves de configuração: minúsculas, dígitos, `_` e `.`. Recusa nomes que sugerem
/// segredo, porque segredo vai para o keyring, nunca para o SQLite.
pub fn validate_setting_key(key: &str) -> AppResult<()> {
    let ok_chars = !key.is_empty()
        && key.len() <= 64
        && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    if !ok_chars {
        return Err(AppError::Invalid(format!("chave de configuração inválida: {key:?}")));
    }
    if ["token", "password", "senha", "secret"].iter().any(|w| key.contains(w)) {
        return Err(AppError::Invalid(format!(
            "{key:?} parece um segredo; segredos vão para o cofre do sistema, não para as configurações"
        )));
    }
    Ok(())
}

pub fn get_setting(conn: &Connection, key: &str) -> AppResult<Option<Value>> {
    validate_setting_key(key)?;
    let raw: Option<String> = conn
        .query_row("SELECT value_json FROM settings WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &Value) -> AppResult<()> {
    validate_setting_key(key)?;
    conn.execute(
        "INSERT INTO settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json",
        params![key, value.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migra_do_zero_ate_a_ultima_versao() {
        let conn = open_in_memory().unwrap();
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for t in ["browser_tests", "credentials", "findings", "pull_requests", "repositories", "review_runs", "settings", "sync_runs"] {
            assert!(tables.iter().any(|x| x == t), "falta a tabela {t}: {tables:?}");
        }
    }

    #[test]
    fn recriar_review_runs_mantem_achados_conversa_e_cascata() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        // banco na versão 9, com um review de PR, achado e conversa
        for (i, (name, sql)) in MIGRATIONS.iter().enumerate().take(9) {
            apply_migration(&mut conn, name, sql, (i + 1) as u32, false).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO repositories (id, workspace, slug) VALUES (1, 'w', 'r');
             INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, source_branch, dest_branch,
               source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 10, 't', 'u', 'a', 'PROJ-1', 'dev', 's', 'd', 'OPEN', 'c', 'u', 'authored');
             INSERT INTO review_runs (id, pr_id, status, run_dir, chat_session_id, quality_status)
             VALUES ('r1', 1, 'completed', '/x', 'sess', 'completed');
             INSERT INTO findings (run_id, seq, validation) VALUES ('r1', 1, 'ok');
             INSERT INTO review_chat (run_id, role, content) VALUES ('r1', 'user', 'oi');
             INSERT INTO fix_sessions (id, kind, pr_id, run_dir, status) VALUES ('f1', 'pr', 1, '/f', 'ready');
             INSERT INTO fix_comments (id, fix_id, seq, bb_comment_id, body, status, applied_at)
               VALUES (5, 'f1', 1, 77, 'use firstOrFail', 'applied', '2026-11-23');
             INSERT INTO fix_messages (fix_id, comment_id, role, content) VALUES ('f1', 5, 'user', 'e o null?');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), latest_version());
        // 0011: correção, item e conversa preservados; o status 'new' passa a valer
        assert_eq!(conn.query_row("SELECT status FROM fix_comments WHERE id = 5", [], |r| r.get::<_, String>(0)).unwrap(), "applied");
        conn.execute("INSERT INTO fix_comments (fix_id, seq, bb_comment_id, body, status) VALUES ('f1', 2, 78, 'x', 'new')", []).unwrap();
        assert!(conn.execute("INSERT INTO fix_comments (fix_id, seq, body, status) VALUES ('f1', 3, 'x', 'bogus')", []).is_err());
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert!(fk, "as chaves estrangeiras voltam a ficar ligadas");
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT count(*) FROM review_runs WHERE chat_session_id = 'sess' AND pr_id = 1"), 1);
        assert_eq!(count("SELECT count(*) FROM findings"), 1);
        assert_eq!(count("SELECT count(*) FROM review_chat"), 1);
        assert_eq!(count("SELECT count(*) FROM fix_messages WHERE comment_id = 5"), 1);
        // review de branch local, sem PR
        conn.execute(
            "INSERT INTO review_runs (id, repo_slug, branch, base_branch, status, run_dir) VALUES ('r2', 'r', 'PROJ-2', 'dev', 'running', '/y')",
            [],
        )
        .unwrap();
        assert!(conn.execute("INSERT INTO review_runs (id, status, run_dir) VALUES ('r3', 'running', '/z')", []).is_err());
        // a cascata continua apontando para a tabela nova
        conn.execute("DELETE FROM review_runs WHERE id = 'r1'", []).unwrap();
        assert_eq!(count("SELECT count(*) FROM findings") + count("SELECT count(*) FROM review_chat"), 0);
    }

    #[test]
    fn migrar_de_novo_nao_faz_nada() {
        let mut conn = open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), latest_version());
    }

    #[test]
    fn recusa_banco_mais_novo_que_o_app() {
        let mut conn = open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", latest_version() + 1).unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("mais nova que este app"), "{err}");
    }

    #[test]
    fn arquivo_reaberto_mantem_dados_e_versao() {
        let dir = std::env::temp_dir().join(format!("prr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.db");
        {
            let conn = open(&path).unwrap();
            set_setting(&conn, "bitbucket.workspace", &json!("acme")).unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        assert_eq!(get_setting(&conn, "bitbucket.workspace").unwrap(), Some(json!("acme")));
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn foreign_keys_ligadas() {
        let conn = open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO pull_requests (repository_id, bb_id, title, url, author_uuid, source_branch,
               dest_branch, source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (999, 1, 't', 'u', 'a', 's', 'd', 'x', 'y', 'OPEN', 'c', 'u', 'authored')",
            [],
        );
        assert!(err.is_err(), "PR sem repositório deveria falhar");
    }

    #[test]
    fn configuracao_ida_e_volta_e_sobrescrita() {
        let conn = open_in_memory().unwrap();
        assert_eq!(get_setting(&conn, "bitbucket.email").unwrap(), None);
        set_setting(&conn, "bitbucket.email", &json!("a@b.c")).unwrap();
        set_setting(&conn, "bitbucket.email", &json!("x@y.z")).unwrap();
        assert_eq!(get_setting(&conn, "bitbucket.email").unwrap(), Some(json!("x@y.z")));
    }

    #[test]
    fn configuracao_recusa_chave_de_segredo_e_invalida() {
        let conn = open_in_memory().unwrap();
        for key in ["bitbucket.token", "jira_password", "Bitbucket.Email", "", "a b", "senha"] {
            assert!(set_setting(&conn, key, &json!("x")).is_err(), "deveria recusar {key:?}");
        }
    }
}
