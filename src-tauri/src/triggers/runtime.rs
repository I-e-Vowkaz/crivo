//! Node instalado (nvm, fnm, volta e o do sistema) e scripts do `package.json`.

use std::path::Path;

use serde::Serialize;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize)]
pub struct NodeRuntime {
    /// Caminho do executável; vazio = o do sistema (PATH).
    pub path: String,
    pub version: String,
    /// "sistema" | "nvm" | "fnm" | "volta"
    pub source: String,
}

/// (origem, pasta das versões, subcaminho até o `node` dentro de cada versão)
const MANAGERS: [(&str, &str, &str); 3] = [
    ("nvm", ".nvm/versions/node", "bin/node"),
    ("fnm", ".local/share/fnm/node-versions", "installation/bin/node"),
    ("volta", ".volta/tools/image/node", "bin/node"),
];

fn version_key(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Nodes dos gerenciadores, sem o do sistema, da versão mais nova para a mais antiga.
pub fn detect_managed(home: &Path) -> Vec<NodeRuntime> {
    let mut found = Vec::new();
    for (source, dir, tail) in MANAGERS {
        let Ok(entries) = std::fs::read_dir(home.join(dir)) else {
            continue;
        };
        for e in entries.flatten() {
            let bin = e.path().join(tail);
            if !bin.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let version = if name.starts_with('v') { name } else { format!("v{name}") };
            found.push(NodeRuntime {
                path: bin.to_string_lossy().into_owned(),
                version,
                source: source.to_string(),
            });
        }
    }
    found.sort_by(|a, b| version_key(&b.version).cmp(&version_key(&a.version)));
    found
}

/// Lê a saída de `command -v node && node --version`: as duas últimas linhas não vazias
/// (o profile de login pode imprimir antes) e a versão precisa ser `v<dígito>…`.
fn parse_system_node(stdout: &str) -> Option<NodeRuntime> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let [.., _path, version] = lines.as_slice() else {
        return None;
    };
    let mut ch = version.chars();
    if ch.next() != Some('v') || !ch.next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(NodeRuntime { path: String::new(), version: version.to_string(), source: "sistema".into() })
}

/// O node do PATH de um shell de login (`path` vazio, pois quem roda usa o PATH).
/// Limite de 5 s: o profile do usuário pode travar.
pub async fn system_node() -> Option<NodeRuntime> {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.args(["-lc", "command -v node && node --version"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_system_node(&String::from_utf8_lossy(&out.stdout))
}

/// Nomes dos scripts do `package.json`, em ordem alfabética.
pub fn package_scripts(package_json: &Path) -> AppResult<Vec<String>> {
    let shown = package_json.display();
    let text = std::fs::read_to_string(package_json)
        .map_err(|e| AppError::Invalid(format!("não consegui ler {shown}: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Invalid(format!("JSON inválido em {shown}: {e}")))?;
    let mut names: Vec<String> = v
        .get("scripts")
        .and_then(|s| s.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("trig-rt-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_nvm_and_volta() {
        let h = tmp("det");
        for p in [".nvm/versions/node/v22.17.0/bin", ".volta/tools/image/node/20.1.0/bin"] {
            fs::create_dir_all(h.join(p)).unwrap();
            fs::write(h.join(p).join("node"), "").unwrap();
        }
        let r = detect_managed(&h);
        fs::remove_dir_all(&h).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].version.as_str(), r[0].source.as_str()), ("v22.17.0", "nvm"));
        assert_eq!((r[1].version.as_str(), r[1].source.as_str()), ("v20.1.0", "volta"));
    }

    #[test]
    fn parses_system_node_output() {
        let n = parse_system_node("bem-vindo\n/usr/bin/node\nv22.1.0\n").unwrap();
        assert_eq!((n.version.as_str(), n.path.as_str(), n.source.as_str()), ("v22.1.0", "", "sistema"));
        assert!(parse_system_node("/usr/bin/node\nlixo\n").is_none());
        assert!(parse_system_node("v22.1.0\n").is_none());
    }

    #[test]
    fn no_managers_is_empty() {
        let h = tmp("empty");
        let r = detect_managed(&h);
        fs::remove_dir_all(&h).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn reads_scripts_sorted() {
        let d = tmp("pkg");
        let p = d.join("package.json");
        fs::write(&p, r#"{"scripts":{"serve":"x","build":"y"}}"#).unwrap();
        assert_eq!(package_scripts(&p).unwrap(), vec!["build", "serve"]);
        fs::write(&p, r#"{"name":"a"}"#).unwrap();
        assert!(package_scripts(&p).unwrap().is_empty());
        fs::write(&p, "{nope").unwrap();
        let err = package_scripts(&p).unwrap_err().to_string();
        fs::remove_dir_all(&d).unwrap();
        assert!(err.contains("package.json"), "{err}");
    }
}
