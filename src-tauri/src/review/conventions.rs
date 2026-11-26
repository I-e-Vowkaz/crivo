//! Convenções do time tiradas do próprio repositório (no commit do PR), para o `complexity-reviewer`
//! e o `complexity-judge` não proporem nada que as contrarie. Só o que está versionado: o review
//! roda num worktree, sem os arquivos ignorados do clone do usuário.
//!
//! - `.cursor/rules/**/*.mdc` (regras do Cursor), com
//!   frontmatter `description`, `globs` e `alwaysApply`;
//! - `.claude/rules/**/*.md` (Claude Code), com frontmatter `paths`.
//!
//! Entra a regra com `alwaysApply: true`, a que tem um glob (ou `paths`) que casa com algum
//! arquivo do PR e a que não tem nenhum. `CLAUDE.md`, `.claude/CLAUDE.md`, `AGENTS.md`,
//! `.cursorrules` e `.github/copilot-instructions.md` entram sempre.

use std::path::Path;

/// Arquivos da raiz que, se existirem, entram sempre.
const ROOT_FILES: &[&str] = &["CLAUDE.md", ".claude/CLAUDE.md", "AGENTS.md", ".cursorrules", ".github/copilot-instructions.md"];
/// Pastas de regras e a extensão dos arquivos de regra em cada uma.
const RULE_DIRS: &[(&str, &str)] = &[(".cursor/rules", "mdc"), (".claude/rules", "md")];
/// Teto do texto montado (~15 mil tokens). O que passa disso fica só listado.
const MAX_BYTES: usize = 60_000;

/// Monta o markdown com as convenções que valem para `changed` (caminhos relativos à raiz).
/// `None` quando o repositório não tem nenhuma.
pub fn collect(wt: &Path, changed: &[&str]) -> Option<String> {
    let mut files: Vec<(String, String, String)> = Vec::new(); // (caminho, título, corpo)
    for rel in ROOT_FILES {
        // CLAUDE.md e AGENTS.md costumam ser cópia um do outro
        if let Some(text) = read_inside(wt, rel).map(|t| t.trim().to_owned()).filter(|t| !files.iter().any(|f| &f.2 == t)) {
            files.push((rel.to_string(), String::new(), text));
        }
    }
    let mut rules = Vec::new();
    for (dir, ext) in RULE_DIRS {
        let mut found = Vec::new();
        list_rules(wt, &wt.join(dir), ext, &mut found);
        found.sort();
        rules.extend(found);
    }
    for rel in rules {
        let Some(text) = read_inside(wt, &rel) else { continue };
        let (meta, body) = split_frontmatter(&text);
        if !applies(&meta, changed) {
            continue;
        }
        files.push((rel, meta.description, body.trim().to_owned()));
    }
    if files.is_empty() {
        return None;
    }

    let mut out = String::from(
        "# Convenções do projeto\n\nRegras do time, tiradas do próprio repositório no commit do PR, para os arquivos que ele altera.\n",
    );
    let mut left_out = Vec::new();
    for (rel, title, body) in files {
        let head = if title.is_empty() { format!("\n## `{rel}`\n\n") } else { format!("\n## `{rel}`: {title}\n\n") };
        if out.len() + head.len() + body.len() > MAX_BYTES {
            left_out.push(rel);
            continue;
        }
        out.push_str(&head);
        out.push_str(&body);
        out.push('\n');
    }
    if !left_out.is_empty() {
        out.push_str("\n## Não incluídas por tamanho\n\nLeia no worktree, se o arquivo do PR tiver relação:\n");
        for rel in left_out {
            out.push_str(&format!("- `{rel}`\n"));
        }
    }
    Some(out)
}

/// Arquivos `.<ext>` dentro de `dir`, recursivo, como caminhos relativos à raiz. Ignora links
/// simbólicos: o PR poderia apontar um para fora do repositório.
fn list_rules(wt: &Path, dir: &Path, ext: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        if ft.is_dir() {
            list_rules(wt, &path, ext, out);
        } else if ft.is_file()
            && path.extension().is_some_and(|x| x == ext)
            && let Ok(rel) = path.strip_prefix(wt)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Lê um arquivo do worktree só se for arquivo comum (sem link simbólico no caminho) e de texto.
fn read_inside(wt: &Path, rel: &str) -> Option<String> {
    let mut p = wt.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        if std::fs::symlink_metadata(&p).ok()?.file_type().is_symlink() {
            return None;
        }
    }
    if !std::fs::symlink_metadata(&p).ok()?.is_file() {
        return None;
    }
    String::from_utf8(std::fs::read(&p).ok()?).ok().filter(|t| !t.trim().is_empty())
}

#[derive(Debug, Default, PartialEq)]
struct Meta {
    description: String,
    globs: Vec<String>,
    always: bool,
}

/// Frontmatter `---` … `---` do `.mdc` → metadados e corpo. Sem frontmatter, tudo é corpo.
fn split_frontmatter(text: &str) -> (Meta, &str) {
    let mut meta = Meta::default();
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (meta, text);
    };
    let Some(end) = rest.find("\n---") else { return (meta, text) };
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['-', '\r']).trim_start_matches('\n');
    let lines: Vec<&str> = header.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(v) = line.strip_prefix("description:") {
            meta.description = unquote(v.trim()).to_owned();
        } else if let Some(v) = line.strip_prefix("alwaysApply:") {
            meta.always = v.trim() == "true";
        } else if let Some(v) = line.strip_prefix("globs:").or_else(|| line.strip_prefix("paths:")) {
            let v = v.trim();
            if v.is_empty() {
                // lista YAML nas linhas seguintes: "  - app/**/*.php"
                while i + 1 < lines.len() && lines[i + 1].trim_start().starts_with('-') {
                    i += 1;
                    meta.globs.push(unquote(lines[i].trim_start()[1..].trim()).to_owned());
                }
            } else {
                // "['a', 'b']", "a, b" ou "a"
                let v = v.trim_start_matches('[').trim_end_matches(']');
                meta.globs.extend(split_outside_braces(v).into_iter().map(|g| unquote(g.trim()).to_owned()).filter(|g| !g.is_empty()));
            }
        }
        i += 1;
    }
    (meta, body)
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '\'' || c == '"')
}

/// Separa por vírgula, menos dentro de `{…}`: `**/*.{ts,vue}, app/**` → 2 globs.
fn split_outside_braces(v: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, c) in v.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(&v[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&v[start..]);
    out
}

/// `src/*.{ts,vue}` → `src/*.ts` e `src/*.vue` (grupos aninhados ou repetidos também).
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_owned()] };
    let mut depth = 0;
    let close = pattern[open..].char_indices().find_map(|(i, c)| {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        (depth == 0).then_some(open + i)
    });
    let Some(close) = close else { return vec![pattern.to_owned()] };
    let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
    split_outside_braces(&pattern[open + 1..close])
        .into_iter()
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}")))
        .collect()
}

fn applies(meta: &Meta, changed: &[&str]) -> bool {
    meta.always
        || meta.globs.is_empty()
        || meta.globs.iter().flat_map(|g| expand_braces(g)).any(|g| changed.iter().any(|f| glob_match(&g, f)))
}

/// Glob com `**` (zero ou mais pastas), `*` e `?` (dentro de um nome). Padrão sem `/` casa com
/// o nome do arquivo em qualquer pasta, como no `.gitignore`.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    if !pattern.contains('/') {
        return path.rsplit('/').next().is_some_and(|name| seg_match(pattern.as_bytes(), name.as_bytes()));
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    segs_match(&p, &s)
}

fn segs_match(p: &[&str], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => segs_match(&p[1..], s) || (!s.is_empty() && segs_match(p, &s[1..])),
        Some(seg) => !s.is_empty() && seg_match(seg.as_bytes(), s[0].as_bytes()) && segs_match(&p[1..], &s[1..]),
    }
}

fn seg_match(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => seg_match(&p[1..], s) || (!s.is_empty() && seg_match(p, &s[1..])),
        Some(b'?') => !s.is_empty() && seg_match(&p[1..], &s[1..]),
        Some(c) => s.first() == Some(c) && seg_match(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_com_chaves() {
        let (meta, _) = split_frontmatter("---\nglobs: src/**/*.{vue,ts}, app/{Http,Models}/**\nalwaysApply: false\n---\ncorpo");
        assert_eq!(meta.globs, vec!["src/**/*.{vue,ts}", "app/{Http,Models}/**"]);
        assert!(applies(&meta, &["src/App.vue"]));
        assert!(applies(&meta, &["src/a/b.ts"]));
        assert!(applies(&meta, &["app/Models/User.php"]));
        assert!(!applies(&meta, &["src/a.php"]));
    }

    #[test]
    fn globs_do_cursor() {
        assert!(glob_match("**/*.php", "app/Http/Kernel.php"));
        assert!(glob_match("**/*.php", "Kernel.php"));
        assert!(!glob_match("**/*.php", "src/App.vue"));
        assert!(glob_match("app/Jobs/**/*.php", "app/Jobs/Invoice/SendJob.php"));
        assert!(glob_match("app/Jobs/**/*.php", "app/Jobs/SendJob.php"));
        assert!(!glob_match("app/Jobs/**/*.php", "app/Services/SendJob.php"));
        assert!(glob_match("app/Repositories/ContractInvoiceRepository.php", "app/Repositories/ContractInvoiceRepository.php"));
        assert!(glob_match("*.vue", "src/views/Home.vue"));
        assert!(glob_match("src/?pp.ts", "src/app.ts"));
    }

    #[test]
    fn frontmatter_do_mdc() {
        let (m, body) = split_frontmatter(
            "---\ndescription: Jobs com queue explícita\nglobs: ['app/Jobs/**/*.php', \"x.php\"]\nalwaysApply: false\n---\n\n# Jobs\n\ntexto\n",
        );
        assert_eq!(m.description, "Jobs com queue explícita");
        assert_eq!(m.globs, vec!["app/Jobs/**/*.php", "x.php"]);
        assert!(!m.always);
        assert_eq!(body, "# Jobs\n\ntexto\n");

        let (m, _) = split_frontmatter("---\nglobs:\n  - 'a/**'\n  - b.php\nalwaysApply: true\n---\nx");
        assert_eq!(m.globs, vec!["a/**", "b.php"]);
        assert!(m.always);

        // .claude/rules do Claude Code: `paths` no lugar de `globs`
        let (m, _) = split_frontmatter("---\npaths:\n  - \"**/*.php\"\n  - \"**/composer.json\"\n---\n# PHP");
        assert_eq!(m.globs, vec!["**/*.php", "**/composer.json"]);
        let (m, _) = split_frontmatter("---\npaths: \"src/**/*.vue\"\n---\nx");
        assert_eq!(m.globs, vec!["src/**/*.vue"]);

        let (m, body) = split_frontmatter("# sem frontmatter");
        assert_eq!(m, Meta::default());
        assert_eq!(body, "# sem frontmatter");
    }

    #[test]
    fn so_as_regras_que_valem_para_o_pr() {
        let dir = std::env::temp_dir().join(format!("conv-test-{}", std::process::id()));
        let rules = dir.join(".cursor/rules/sub");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "Use Pest.").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Use Pest.\n").unwrap();
        std::fs::write(dir.join(".cursor/rules/README.md"), "índice").unwrap();
        std::fs::write(dir.join(".cursor/rules/always.mdc"), "---\ndescription: Sempre\nglobs: ['**/*.php']\nalwaysApply: true\n---\nregra A").unwrap();
        std::fs::write(dir.join(".cursor/rules/jobs.mdc"), "---\ndescription: Jobs\nglobs: ['app/Jobs/**/*.php']\nalwaysApply: false\n---\nregra B").unwrap();
        std::fs::write(rules.join("vue.mdc"), "---\ndescription: Vue\nglobs: ['**/*.vue']\nalwaysApply: false\n---\nregra C").unwrap();
        std::fs::create_dir_all(dir.join(".claude/rules/php")).unwrap();
        std::fs::write(dir.join(".claude/CLAUDE.md"), "Use Sail.").unwrap();
        std::fs::write(dir.join(".claude/rules/geral.md"), "regra D").unwrap();
        std::fs::write(dir.join(".claude/rules/php/estilo.md"), "---\npaths:\n  - \"app/Jobs/**\"\n---\nregra E").unwrap();
        std::fs::write(dir.join(".claude/rules/vue.md"), "---\npaths: \"**/*.vue\"\n---\nregra F").unwrap();
        std::fs::write(dir.join(".claude/rules/nota.txt"), "regra G").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", dir.join(".cursor/rules/link.mdc")).unwrap();

        let out = collect(&dir, &["app/Jobs/SendJob.php"]).unwrap();
        assert!(out.contains("Use Pest."));
        assert!(!out.contains("AGENTS.md"), "corpo igual ao do CLAUDE.md não se repete");
        assert!(out.contains("## `.cursor/rules/always.mdc`: Sempre\n\nregra A"));
        assert!(out.contains("regra B"));
        assert!(!out.contains("regra C"));
        assert!(!out.contains("índice"));
        assert!(!out.contains("link.mdc"));
        assert!(out.contains("Use Sail."));
        assert!(out.contains("## `.claude/rules/geral.md`\n\nregra D"));
        assert!(out.contains("## `.claude/rules/php/estilo.md`\n\nregra E"));
        assert!(!out.contains("regra F") && !out.contains("regra G"));
        // Cursor antes do Claude Code
        assert!(out.find("regra A").unwrap() < out.find("regra D").unwrap());

        let out = collect(&dir, &["app/Models/User.php"]).unwrap();
        assert!(out.contains("regra A") && !out.contains("regra B"));

        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(collect(&dir, &["a.php"]), None);
    }
}
