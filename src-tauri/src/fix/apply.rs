//! Aplica uma sugestão aprovada (lista de edições) no worktree da correção.
//!
//! Cada edição troca um bloco de linhas. O bloco é achado pelo conteúdo (`existing_code`, sem a
//! indentação comum e sem espaço no fim, como na validação das melhorias): primeiro nas linhas
//! indicadas, depois no arquivo todo, porque uma sugestão aprovada antes pode ter deslocado as
//! linhas. Tudo ou nada: se uma edição não bate, nenhum arquivo é gravado.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edit {
    pub file: String,
    pub line: u64,
    pub end_line: u64,
    pub existing_code: String,
    pub improved_code: String,
}

/// Linhas sem as quebras das pontas e sem espaço no fim, menos a indentação comum.
fn normalize(lines: &[&str]) -> Vec<String> {
    let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| indent_len(l)).min().unwrap_or(0);
    lines.iter().map(|l| l.trim_end().get(indent..).unwrap_or("").to_owned()).collect()
}

fn indent_len(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn block_lines(code: &str) -> Vec<&str> {
    let t = code.trim_matches('\n');
    if t.trim().is_empty() { vec![] } else { t.lines().collect() }
}

/// Caminho relativo e dentro do worktree: sem `..`, sem raiz, sem `.git` e sem passar por link
/// simbólico (um link versionado no repo levaria a escrita para fora do worktree, como no
/// `conventions.rs`).
pub fn safe_path(wt: &Path, file: &str) -> Result<PathBuf, String> {
    let rel = Path::new(file.trim());
    if file.trim().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("caminho inválido na sugestão: {file}"));
    }
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return Err(format!("a sugestão não pode mexer em .git: {file}"));
    }
    let mut cur = wt.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("a sugestão passa por um link simbólico ({}); recusada", cur.strip_prefix(wt).unwrap_or(&cur).display()));
            }
            Ok(_) => {}
            // daqui para a frente o caminho ainda não existe (arquivo ou pasta nova)
            Err(_) => break,
        }
    }
    Ok(wt.join(rel))
}

/// Bloco `[início, fim)` (índices de linha, base 0) onde `existing` está no arquivo.
/// Prefere as linhas indicadas; senão, a ocorrência mais perto delas.
fn locate(file_lines: &[&str], e: &Edit) -> Result<(usize, usize), String> {
    let want = normalize(&block_lines(&e.existing_code));
    if want.is_empty() {
        return Err(format!("{}:{}: a sugestão não traz o trecho atual (existing_code)", e.file, e.line));
    }
    let n = want.len();
    let fits = |start: usize| start + n <= file_lines.len() && normalize(&file_lines[start..start + n]) == want;
    let hint = (e.line as usize).saturating_sub(1);
    if fits(hint) {
        return Ok((hint, hint + n));
    }
    (0..file_lines.len())
        .filter(|&s| fits(s))
        .min_by_key(|&s| s.abs_diff(hint))
        .map(|s| (s, s + n))
        .ok_or_else(|| {
            format!("{}:{}: o trecho atual da sugestão não está mais no arquivo; peça uma nova sugestão no chat", e.file, e.line)
        })
}

/// `improved` com a indentação do bloco achado no arquivo, caso o agente tenha copiado o trecho
/// com outra indentação (a comparação ignora a indentação comum).
fn reindent(improved: &[&str], existing: &[&str], found: &[&str]) -> Vec<String> {
    let min = |ls: &[&str]| ls.iter().filter(|l| !l.trim().is_empty()).map(|l| indent_len(l)).min().unwrap_or(0);
    let (have, want) = (min(existing), min(found));
    if have == want {
        return improved.iter().map(|l| (*l).to_owned()).collect();
    }
    let unit = found
        .iter()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.chars().next())
        .filter(|c| *c == '\t')
        .map(|_| "\t")
        .unwrap_or(" ");
    improved
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else if want > have {
                format!("{}{l}", unit.repeat(want - have))
            } else {
                let cut = (have - want).min(indent_len(l));
                l[cut..].to_owned()
            }
        })
        .collect()
}

/// Conteúdo novo de cada arquivo tocado, sem gravar nada.
pub fn plan(wt: &Path, edits: &[Edit]) -> Result<Vec<(PathBuf, String)>, String> {
    if edits.is_empty() {
        return Err("a sugestão não tem nenhuma edição".into());
    }
    let mut by_file: BTreeMap<String, Vec<&Edit>> = BTreeMap::new();
    for e in edits {
        by_file.entry(e.file.trim().to_owned()).or_default().push(e);
    }
    let mut out = Vec::new();
    for (file, list) in by_file {
        let path = safe_path(wt, &file)?;
        // arquivo novo: linha 0 e trecho atual vazio
        if list.iter().any(|e| e.line == 0 && block_lines(&e.existing_code).is_empty()) {
            if list.len() > 1 || path.exists() {
                return Err(format!("{file}: a sugestão cria um arquivo que já existe ou mistura criação com edição"));
            }
            let mut text = list[0].improved_code.trim_matches('\n').to_owned();
            text.push('\n');
            out.push((path, text));
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{file}: não consegui ler ({e})"))?;
        let crlf = text.contains("\r\n");
        let trailing = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        let mut spans: Vec<(usize, usize, Vec<String>)> = Vec::new();
        for e in list {
            let (s, t) = locate(&lines, e)?;
            let improved = reindent(&block_lines(&e.improved_code), &block_lines(&e.existing_code), &lines[s..t]);
            spans.push((s, t, improved));
        }
        spans.sort_by_key(|x| x.0);
        if spans.windows(2).any(|w| w[1].0 < w[0].1) {
            return Err(format!("{file}: duas edições da sugestão mexem nas mesmas linhas"));
        }
        let mut new_lines: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
        for (s, t, improved) in spans.into_iter().rev() {
            new_lines.splice(s..t, improved);
        }
        let mut new_text = new_lines.join(if crlf { "\r\n" } else { "\n" });
        if trailing && !new_lines.is_empty() {
            new_text.push_str(if crlf { "\r\n" } else { "\n" });
        }
        out.push((path, new_text));
    }
    Ok(out)
}

/// Confere a sugestão sem gravar: `None` se ela se aplica ao código atual.
pub fn check(wt: &Path, edits: &[Edit]) -> Option<String> {
    plan(wt, edits).err()
}

/// Grava a sugestão (tudo ou nada: só grava depois de conferir todas as edições).
pub fn apply(wt: &Path, edits: &[Edit]) -> Result<Vec<String>, String> {
    let files = plan(wt, edits)?;
    let mut touched = Vec::new();
    for (path, text) in files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        touched.push(path.strip_prefix(wt).unwrap_or(&path).display().to_string());
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(file: &str, line: u64, end: u64, existing: &str, improved: &str) -> Edit {
        Edit { file: file.into(), line, end_line: end, existing_code: existing.into(), improved_code: improved.into() }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("prr-fix-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn troca_nas_linhas_indicadas_e_acha_bloco_deslocado() {
        let wt = tmp("desloc");
        std::fs::write(wt.join("a.php"), "<?php\nfunction a() {\n    $x = 1;\n    return $x;\n}\n").unwrap();
        // linha indicada errada (2 em vez de 3): acha pelo conteúdo
        let e = edit("a.php", 2, 2, "    $x = 1;", "    $x = 2;");
        apply(&wt, &[e]).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("a.php")).unwrap(), "<?php\nfunction a() {\n    $x = 2;\n    return $x;\n}\n");
        std::fs::remove_dir_all(&wt).unwrap();
    }

    #[test]
    fn reindenta_quando_o_agente_copiou_sem_indentacao() {
        let wt = tmp("indent");
        std::fs::write(wt.join("a.ts"), "if (a) {\n\tfoo();\n\tbar();\n}\n").unwrap();
        let e = edit("a.ts", 2, 3, "foo();\nbar();", "foo();\nif (b) {\n\tbaz();\n}");
        apply(&wt, &[e]).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("a.ts")).unwrap(), "if (a) {\n\tfoo();\n\tif (b) {\n\t\tbaz();\n\t}\n}\n");
        std::fs::remove_dir_all(&wt).unwrap();
    }

    #[test]
    fn varias_edicoes_no_mesmo_arquivo_apagar_e_criar() {
        let wt = tmp("multi");
        std::fs::write(wt.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let es = vec![edit("f.txt", 1, 1, "a", "A"), edit("f.txt", 3, 3, "c", ""), edit("novo/g.txt", 0, 0, "", "oi")];
        let mut t = apply(&wt, &es).unwrap();
        t.sort();
        assert_eq!(t, vec!["f.txt", "novo/g.txt"]);
        assert_eq!(std::fs::read_to_string(wt.join("f.txt")).unwrap(), "A\nb\nd\n");
        assert_eq!(std::fs::read_to_string(wt.join("novo/g.txt")).unwrap(), "oi\n");
        std::fs::remove_dir_all(&wt).unwrap();
    }

    #[test]
    fn tudo_ou_nada_e_caminho_fora_do_worktree() {
        let wt = tmp("atom");
        std::fs::write(wt.join("f.txt"), "a\nb\n").unwrap();
        let es = vec![edit("f.txt", 1, 1, "a", "A"), edit("f.txt", 2, 2, "nao existe", "x")];
        assert!(apply(&wt, &es).unwrap_err().contains("não está mais no arquivo"));
        assert_eq!(std::fs::read_to_string(wt.join("f.txt")).unwrap(), "a\nb\n");
        assert!(check(&wt, &[edit("../x", 1, 1, "a", "b")]).unwrap().contains("caminho inválido"));
        assert!(check(&wt, &[edit("/etc/passwd", 1, 1, "a", "b")]).unwrap().contains("caminho inválido"));
        assert!(check(&wt, &[edit(".git/config", 1, 1, "a", "b")]).is_some());
        // link simbólico versionado apontando para fora: nem leitura nem escrita
        let fora = tmp("fora");
        std::fs::write(fora.join("alvo.txt"), "a\n").unwrap();
        std::os::unix::fs::symlink(&fora, wt.join("cfg")).unwrap();
        std::os::unix::fs::symlink(fora.join("alvo.txt"), wt.join("link.txt")).unwrap();
        assert!(apply(&wt, &[edit("cfg/alvo.txt", 1, 1, "a", "HACK")]).unwrap_err().contains("link simbólico"));
        assert!(apply(&wt, &[edit("link.txt", 1, 1, "a", "HACK")]).unwrap_err().contains("link simbólico"));
        assert_eq!(std::fs::read_to_string(fora.join("alvo.txt")).unwrap(), "a\n");
        std::fs::remove_dir_all(&fora).unwrap();
        assert!(check(&wt, &[edit("f.txt", 1, 2, "a\nb", "x"), edit("f.txt", 2, 2, "b", "y")]).unwrap().contains("mesmas linhas"));
        std::fs::remove_dir_all(&wt).unwrap();
    }

    #[test]
    fn mantem_crlf() {
        let wt = tmp("crlf");
        std::fs::write(wt.join("w.txt"), "a\r\nb\r\n").unwrap();
        apply(&wt, &[edit("w.txt", 2, 2, "b", "B")]).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("w.txt")).unwrap(), "a\r\nB\r\n");
        std::fs::remove_dir_all(&wt).unwrap();
    }
}
