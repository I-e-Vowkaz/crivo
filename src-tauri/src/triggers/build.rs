//! Montagem do comando de um gatilho: o texto que o `bash -lc` recebe.

use std::path::{Path, PathBuf};

use super::{cwd, Kind, Trigger};

pub struct Launch {
    pub cwd: PathBuf,
    pub script: String,
    pub display: String,
    pub stop: Option<String>,
}

/// Aspas simples de shell: `a'b` vira `'a'\''b'`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Exporta dentro do texto, depois do perfil do `bash -l`, para o perfil não
/// sobrescrever o `PATH` e as variáveis do usuário valerem por último.
fn prefix(t: &Trigger) -> String {
    let mut p = String::from("export FORCE_COLOR=0 NO_COLOR=1;");
    if let Some(dir) = node_dir(t) {
        p.push_str(&format!(" export PATH={}:\"$PATH\";", shell_quote(&dir.to_string_lossy())));
    }
    if !t.node_options.is_empty() {
        p.push_str(&format!(" export NODE_OPTIONS={};", shell_quote(&t.node_options)));
    }
    for (k, v) in &t.env {
        p.push_str(&format!(" export {k}={};", shell_quote(v)));
    }
    p.push(' ');
    p
}

fn node_dir(t: &Trigger) -> Option<&Path> {
    if t.node.is_empty() {
        return None;
    }
    Path::new(&t.node).parent()
}

fn package_manager(t: &Trigger) -> String {
    let name = if t.package_manager.is_empty() { "npm" } else { t.package_manager.as_str() };
    if let Some(dir) = node_dir(t) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return shell_quote(&candidate.to_string_lossy());
        }
    }
    // nome simples (npm, yarn, pnpm) não precisa de aspas; o display fica como no terminal
    name.to_owned()
}

/// Monta o comando sem validar caminhos (a prévia usa). `args`, `script` e
/// `stop_script` entram como texto de shell; o resto passa por `shell_quote`.
pub fn launch(t: &Trigger) -> Launch {
    let args = t.args.trim();
    let (display, stop): (String, Option<String>) = match t.kind {
        Kind::Npm => {
            let pm = package_manager(t);
            let command = t.command.trim();
            let display = if command == "run" {
                let tail = if args.is_empty() { String::new() } else { format!(" -- {args}") };
                t.scripts
                    .iter()
                    .map(|s| format!("{pm} run {}{tail}", shell_quote(s)))
                    .collect::<Vec<_>>()
                    .join(" && ")
            } else {
                let tail = if args.is_empty() { String::new() } else { format!(" {args}") };
                format!("{pm} {}{tail}", shell_quote(command))
            };
            (display, None)
        }
        Kind::Shell => {
            let display = if args.is_empty() { t.script.clone() } else { format!("{} {args}", t.script) };
            let stop = (!t.stop_script.trim().is_empty()).then(|| format!("{}{}", prefix(t), t.stop_script));
            (display, stop)
        }
    };
    Launch { cwd: cwd(t), script: format!("{}{display}", prefix(t)), display, stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm(scripts: &[&str], args: &str) -> Trigger {
        Trigger {
            kind: Kind::Npm,
            package_json: "/tmp/x/package.json".into(),
            command: "run".into(),
            scripts: scripts.iter().map(|s| s.to_string()).collect(),
            args: args.into(),
            ..Default::default()
        }
    }

    fn shell(script: &str) -> Trigger {
        Trigger { kind: Kind::Shell, workdir: "/tmp".into(), script: script.into(), ..Default::default() }
    }

    #[test]
    fn quote_escapes_single_quote() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn npm_run_two_scripts_with_args() {
        let l = launch(&npm(&["serve", "build:dev"], "--port 9000"));
        assert_eq!(l.display, "npm run 'serve' -- --port 9000 && npm run 'build:dev' -- --port 9000");
        assert!(l.stop.is_none());
    }

    #[test]
    fn npm_run_without_args_has_no_dashdash() {
        assert_eq!(launch(&npm(&["serve"], "")).display, "npm run 'serve'");
    }

    #[test]
    fn npm_command_is_trimmed() {
        let t = Trigger { command: " run ".into(), ..npm(&["serve"], "") };
        assert_eq!(launch(&t).display, "npm run 'serve'");
    }

    #[test]
    fn npm_install_puts_args_after_command() {
        let t = Trigger {
            command: "install".into(),
            args: "--frozen-lockfile".into(),
            package_manager: "yarn".into(),
            ..npm(&[], "")
        };
        assert_eq!(launch(&t).display, "yarn 'install' --frozen-lockfile");
    }

    #[test]
    fn node_dir_goes_first_in_path() {
        let t = Trigger { node: "/opt/My Node/bin/node".into(), ..npm(&["a"], "") };
        let l = launch(&t);
        assert!(l.script.contains(r#"export PATH='/opt/My Node/bin':"$PATH";"#), "{}", l.script);
    }

    #[test]
    fn user_env_is_quoted_and_after_defaults() {
        let t = Trigger { env: vec![("X".into(), "it's".into())], ..shell("true") };
        let s = launch(&t).script;
        let a = s.find("NO_COLOR=1;").unwrap();
        let b = s.find(r"export X='it'\''s';").unwrap();
        assert!(a < b, "{s}");
    }

    #[test]
    fn shell_stop_script() {
        let t = Trigger { stop_script: "docker compose stop".into(), ..shell("docker compose up") };
        let stop = launch(&t).stop.unwrap();
        assert!(stop.ends_with("docker compose stop"), "{stop}");
        assert!(launch(&shell("x")).stop.is_none());
    }

    #[test]
    fn script_runs_in_bash() {
        let dir = std::env::temp_dir().join(format!("trig-build-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Trigger {
            workdir: dir.to_string_lossy().into_owned(),
            env: vec![("X".into(), "it's ok".into())],
            ..shell("printf '%s' \"$X\"")
        };
        let l = launch(&t);
        let out = std::process::Command::new("bash").args(["-lc", &l.script]).current_dir(&l.cwd).output().unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "it's ok");
    }
}
