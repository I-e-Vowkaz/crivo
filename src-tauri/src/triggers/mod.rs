//! Gatilhos: configurações de execução guardadas em `settings` (chave `triggers`),
//! como as run configurations do JetBrains. Um gatilho é um script npm ou um script de shell.

use std::path::{Path, PathBuf};

pub mod ansi;
pub mod build;
pub mod manager;
pub mod runtime;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Npm,
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Trigger {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// npm: caminho do `package.json`.
    #[serde(default)]
    pub package_json: String,
    /// npm: comando (ex.: `run`).
    #[serde(default)]
    pub command: String,
    /// npm: scripts do `package.json` a rodar com `run`.
    #[serde(default)]
    pub scripts: Vec<String>,
    /// npm: `npm`, `yarn` ou `pnpm` (vazio = npm).
    #[serde(default)]
    pub package_manager: String,
    /// Caminho do executável do node (vazio = o do PATH).
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub node_options: String,
    /// shell: pasta de trabalho.
    #[serde(default)]
    pub workdir: String,
    /// shell: script a executar.
    #[serde(default)]
    pub script: String,
    /// shell: script que encerra o processo (opcional).
    #[serde(default)]
    pub stop_script: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

pub const SETTING_KEY: &str = "triggers";

pub fn load(conn: &Connection) -> AppResult<Vec<Trigger>> {
    Ok(db::get_setting(conn, SETTING_KEY)?.map(serde_json::from_value).transpose()?.unwrap_or_default())
}

fn store(conn: &Connection, all: &[Trigger]) -> AppResult<()> {
    db::set_setting(conn, SETTING_KEY, &serde_json::to_value(all)?)
}

/// `~/x` e `~` sozinho viram `$HOME/x` e `$HOME`; o resto (e tudo, sem `HOME`) fica igual.
pub fn expand_tilde(s: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else { return s.to_owned() };
    let home = home.to_string_lossy();
    if s == "~" {
        home.into_owned()
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        s.to_owned()
    }
}

/// Valida e grava. Id vazio vira um uuid v4; id existente substitui o gatilho.
pub fn save(conn: &Connection, mut t: Trigger) -> AppResult<Trigger> {
    t.name = t.name.trim().to_owned();
    t.command = t.command.trim().to_owned();
    t.package_json = expand_tilde(&t.package_json);
    t.workdir = expand_tilde(&t.workdir);
    t.node = expand_tilde(&t.node);
    if t.id.is_empty() {
        t.id = uuid::Uuid::new_v4().to_string();
    }
    let mut all = load(conn)?;
    validate(&t, &all)?;
    match all.iter_mut().find(|x| x.id == t.id) {
        Some(slot) => *slot = t.clone(),
        None => all.push(t.clone()),
    }
    store(conn, &all)?;
    Ok(t)
}

/// Apagar um id que não existe não é erro.
pub fn delete(conn: &Connection, id: &str) -> AppResult<()> {
    let mut all = load(conn)?;
    let before = all.len();
    all.retain(|t| t.id != id);
    if all.len() != before {
        store(conn, &all)?;
    }
    Ok(())
}

fn invalid<T>(msg: String) -> AppResult<T> {
    Err(AppError::Invalid(msg))
}

fn env_key_ok(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn validate(t: &Trigger, all: &[Trigger]) -> AppResult<()> {
    let name = t.name.trim();
    if name.is_empty() {
        return invalid("dê um nome ao gatilho".into());
    }
    if name != t.name {
        return invalid("o nome do gatilho não pode ter espaço no começo ou no fim".into());
    }
    let lower = name.to_lowercase();
    if all.iter().any(|o| o.id != t.id && o.name.trim().to_lowercase() == lower) {
        return invalid(format!("já existe um gatilho chamado {name:?}"));
    }
    for (k, _) in &t.env {
        if !env_key_ok(k) {
            return invalid(format!("nome de variável de ambiente inválido: {k:?}"));
        }
    }
    if !t.node.is_empty() && !Path::new(&t.node).is_file() {
        return invalid(format!("o node não foi encontrado: {}", t.node));
    }
    match t.kind {
        Kind::Npm => {
            let pj = Path::new(&t.package_json);
            if !t.package_json.ends_with("package.json") || !pj.is_file() {
                return invalid(format!("package.json não encontrado: {}", t.package_json));
            }
            if !matches!(t.package_manager.as_str(), "" | "npm" | "yarn" | "pnpm") {
                return invalid(format!("gerenciador de pacotes não suportado: {}", t.package_manager));
            }
            if t.command.trim().is_empty() {
                return invalid("preencha o comando do npm (ex.: run)".into());
            }
            if t.command.trim() == "run" && t.scripts.is_empty() {
                return invalid("escolha ao menos um script do package.json".into());
            }
        }
        Kind::Shell => {
            if !Path::new(&t.workdir).is_dir() {
                return invalid(format!("a pasta de trabalho não existe: {}", t.workdir));
            }
            if t.script.trim().is_empty() {
                return invalid("preencha o script".into());
            }
        }
    }
    Ok(())
}

/// Pasta em que o processo roda: npm, a do `package.json`; shell, a `workdir`.
pub fn cwd(t: &Trigger) -> PathBuf {
    match t.kind {
        Kind::Npm => Path::new(&t.package_json).parent().map(Path::to_path_buf).unwrap_or_default(),
        Kind::Shell => PathBuf::from(&t.workdir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pasta temporária, apagada no fim do teste.
    struct TmpDir(PathBuf);

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Pasta temporária com um `package.json` vazio; devolve a pasta e o caminho do arquivo.
    fn fixture() -> (TmpDir, String) {
        let dir = std::env::temp_dir().join(format!("trig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = dir.join("package.json");
        std::fs::write(&pj, "{}").unwrap();
        (TmpDir(dir), pj.to_string_lossy().into_owned())
    }

    fn npm(name: &str, pj: &str) -> Trigger {
        Trigger {
            name: name.into(),
            kind: Kind::Npm,
            package_json: pj.into(),
            command: "run".into(),
            scripts: vec!["dev".into()],
            ..Default::default()
        }
    }

    #[test]
    fn save_assigns_id_and_roundtrips() {
        let conn = db::open_in_memory().unwrap();
        let (_d, pj) = fixture();
        let saved = save(&conn, npm("Front", &pj)).unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(load(&conn).unwrap(), vec![saved]);
    }

    #[test]
    fn save_replaces_by_id() {
        let conn = db::open_in_memory().unwrap();
        let (_d, pj) = fixture();
        let a = save(&conn, npm("Front", &pj)).unwrap();
        save(&conn, Trigger { name: "Novo".into(), ..a }).unwrap();
        let all = load(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Novo");
    }

    #[test]
    fn delete_removes_and_ignores_missing() {
        let conn = db::open_in_memory().unwrap();
        let (_d, pj) = fixture();
        let a = save(&conn, npm("Front", &pj)).unwrap();
        delete(&conn, "nao-existe").unwrap();
        assert_eq!(load(&conn).unwrap().len(), 1);
        delete(&conn, &a.id).unwrap();
        assert!(load(&conn).unwrap().is_empty());
    }

    #[test]
    fn name_must_be_unique_case_insensitive() {
        let (_d, pj) = fixture();
        let a = Trigger { id: "1".into(), ..npm("Front", &pj) };
        let b = Trigger { id: "2".into(), ..npm("front", &pj) };
        let err = validate(&b, &[a]).unwrap_err().to_string();
        assert!(err.contains("já existe"), "{err}");
    }

    #[test]
    fn npm_requires_package_json_and_script() {
        let (_d, pj) = fixture();
        let err = validate(&npm("A", "/nao/existe/package.json"), &[]).unwrap_err().to_string();
        assert!(err.contains("/nao/existe/package.json"), "{err}");
        let sem_script = Trigger { scripts: vec![], ..npm("A", &pj) };
        assert!(validate(&sem_script, &[]).is_err());
        let bun = Trigger { package_manager: "bun".into(), ..npm("A", &pj) };
        assert!(validate(&bun, &[]).is_err());
        assert!(validate(&npm("A", &pj), &[]).is_ok());
    }

    #[test]
    fn shell_requires_dir_and_script() {
        let (d, _) = fixture();
        let ok = Trigger {
            name: "S".into(),
            kind: Kind::Shell,
            workdir: d.0.to_string_lossy().into_owned(),
            script: "./run.sh".into(),
            ..Default::default()
        };
        assert!(validate(&ok, &[]).is_ok());
        assert!(validate(&Trigger { workdir: "/nao/existe".into(), ..ok.clone() }, &[]).is_err());
        assert!(validate(&Trigger { script: " ".into(), ..ok }, &[]).is_err());
    }

    #[test]
    fn env_keys_are_validated() {
        let (_d, pj) = fixture();
        for k in ["1X", "A-B"] {
            let t = Trigger { env: vec![(k.into(), "v".into())], ..npm("A", &pj) };
            assert!(validate(&t, &[]).is_err(), "{k}");
        }
        let t = Trigger { env: vec![("VUE_APP_X".into(), "v".into())], ..npm("A", &pj) };
        assert!(validate(&t, &[]).is_ok());
    }

    #[test]
    fn stale_paths_are_reported() {
        let (_d, pj) = fixture();
        let t = Trigger { node: "/nao/existe/node".into(), ..npm("A", &pj) };
        let err = validate(&t, &[]).unwrap_err().to_string();
        assert!(err.contains("/nao/existe/node"), "{err}");
    }

    #[test]
    fn cwd_follows_kind() {
        let (d, pj) = fixture();
        assert_eq!(cwd(&npm("A", &pj)), d.0);
        let s = Trigger { kind: Kind::Shell, workdir: "/x".into(), ..Default::default() };
        assert_eq!(cwd(&s), PathBuf::from("/x"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x"), format!("{}/x", home.trim_end_matches('/')));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs"), "/abs");
        assert_eq!(expand_tilde("a~b"), "a~b");
        assert_eq!(expand_tilde("~outro/x"), "~outro/x");
    }

    #[test]
    fn save_expands_tilde_before_validating() {
        let home = std::env::var("HOME").unwrap();
        let conn = db::open_in_memory().unwrap();
        let missing = format!("nao-existe-{}", uuid::Uuid::new_v4());
        let err = save(&conn, npm("A", &format!("~/{missing}/package.json"))).unwrap_err().to_string();
        let expected = format!("{}/{missing}/package.json", home.trim_end_matches('/'));
        assert!(err.contains(&expected), "{err}");
        let shell = Trigger { name: "S".into(), kind: Kind::Shell, workdir: format!("~/{missing}"), script: "true".into(), ..Default::default() };
        let err = save(&conn, shell).unwrap_err().to_string();
        assert!(err.contains(&format!("{}/{missing}", home.trim_end_matches('/'))), "{err}");
    }

    #[test]
    fn npm_command_is_required_and_trimmed() {
        let (_d, pj) = fixture();
        let err = validate(&Trigger { command: "  ".into(), ..npm("A", &pj) }, &[]).unwrap_err().to_string();
        assert!(err.contains("comando"), "{err}");
        // " run" vale como run: sem script, recusa
        let err = validate(&Trigger { command: " run".into(), scripts: vec![], ..npm("A", &pj) }, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("script"), "{err}");
        let conn = db::open_in_memory().unwrap();
        assert_eq!(save(&conn, Trigger { command: " run ".into(), ..npm("A", &pj) }).unwrap().command, "run");
    }
}
