//! Tela "Minhas branches": os repositórios git da pasta configurada e as branches locais de
//! cada um, sem rede (só o que o clone já tem). Branches protegidas ficam de fora: nenhum
//! review nem correção roda nelas.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::review::git;

#[derive(Debug, Serialize)]
pub struct LocalBranch {
    pub name: String,
    /// Data do último commit (ISO 8601).
    pub date: String,
    pub sha: String,
    pub subject: String,
    /// Branch remota que ela acompanha (`origin/PROJ-1`), se houver.
    pub upstream: Option<String>,
    /// Diferença para a remota, como o git escreve: `[ahead 2]`, `[behind 1]`, `[gone]`.
    pub track: String,
    /// Está aberta no checkout principal do clone.
    pub head: bool,
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub slug: String,
    /// `back` (Laravel), `front` (package.json) ou `outro`.
    pub kind: &'static str,
    /// Base do review (`dev`, ou a branch padrão da origin); `None` sem origin.
    pub base: Option<String>,
    /// Data do commit mais recente entre as branches locais.
    pub last_commit: Option<String>,
    pub branches: Vec<LocalBranch>,
    pub error: Option<String>,
}

/// `<raiz>/<slug>`, recusando nome que saia da pasta ou que não seja um clone git.
pub fn repo_dir(root: &Path, slug: &str) -> AppResult<PathBuf> {
    let mut comps = Path::new(slug).components();
    let single = matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None));
    if !single || slug.starts_with('.') {
        return Err(AppError::Invalid(format!("nome de projeto inválido: {slug}")));
    }
    let dir = root.join(slug);
    if !dir.join(".git").exists() {
        return Err(AppError::Invalid(format!("{} não é um clone git", dir.display())));
    }
    Ok(dir)
}

fn kind(dir: &Path) -> &'static str {
    if dir.join("composer.json").exists() || dir.join("artisan").exists() {
        "back"
    } else if dir.join("package.json").exists() {
        "front"
    } else {
        "outro"
    }
}

const FORMAT: &str = "%(refname:short)%00%(committerdate:iso-strict-local)%00%(objectname)%00%(upstream:short)%00%(upstream:track)%00%(HEAD)%00%(contents:subject)";

fn parse_refs(out: &str) -> Vec<LocalBranch> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\0').collect();
            if f.len() < 7 {
                return None;
            }
            Some(LocalBranch {
                name: f[0].to_owned(),
                date: f[1].to_owned(),
                sha: f[2].to_owned(),
                upstream: Some(f[3].to_owned()).filter(|u| !u.is_empty()),
                track: f[4].to_owned(),
                head: f[5] == "*",
                subject: f[6..].join(" "),
            })
        })
        .filter(|b| !git::is_protected(&b.name))
        .collect()
}

async fn project(root: &Path, slug: String) -> Project {
    let dir = root.join(&slug);
    let out = git::run(&dir, &["for-each-ref", "refs/heads", "--sort=-committerdate", &format!("--format={FORMAT}")]).await;
    let (branches, error) = match out {
        Ok(text) => (parse_refs(&text), None),
        Err(e) => (vec![], Some(e.to_string())),
    };
    Project {
        kind: kind(&dir),
        base: git::default_base(&dir).await,
        last_commit: branches.iter().map(|b| b.date.clone()).max(),
        branches,
        error,
        slug,
    }
}

/// Os clones da pasta, do que teve commit mais recente para o mais antigo. Os de `hidden`
/// (ocultados pelo usuário; voltam pelas Configurações) nem são lidos.
pub async fn list(root: &Path, hidden: &[String]) -> AppResult<Vec<Project>> {
    let entries = std::fs::read_dir(root).map_err(|e| AppError::Invalid(format!("não consegui ler {}: {e}", root.display())))?;
    let mut slugs: Vec<String> = entries
        .flatten()
        // só clones: worktree (`.git` arquivo) tem as branches do clone principal, e um worktree
        // órfão (gitdir apontando para pasta que não existe) quebraria o git
        .filter(|e| e.path().join(".git").is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.') && !hidden.contains(n))
        .collect();
    slugs.sort();
    let mut out = Vec::new();
    for slug in slugs {
        out.push(project(root, slug).await);
    }
    // datas ISO no fuso local (`iso-strict-local`): dá para comparar como texto
    out.sort_by(|a, b| b.last_commit.cmp(&a.last_commit));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_refs_e_tira_protegidas() {
        let out = "PROJ-2\x002026-11-20T10:00:00-03:00\x00aaa\x00origin/PROJ-2\x00[ahead 1]\x00*\x00fix: algo\n\
                   dev\x002026-11-21T10:00:00-03:00\x00bbb\x00origin/dev\x00\x00 \x00merge\n\
                   local\x002026-11-19T10:00:00-03:00\x00ccc\x00\x00\x00 \x00wip";
        let b = parse_refs(out);
        assert_eq!(b.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["PROJ-2", "local"]);
        assert!(b[0].head && !b[1].head);
        assert_eq!(b[0].upstream.as_deref(), Some("origin/PROJ-2"));
        assert_eq!((b[0].track.as_str(), b[1].upstream.clone()), ("[ahead 1]", None));
        assert_eq!(b[0].subject, "fix: algo");
    }

    #[test]
    fn nome_de_projeto_nao_sai_da_pasta() {
        let root = std::env::temp_dir();
        for bad in ["..", "../x", "a/b", ".hidden", "/etc"] {
            assert!(repo_dir(&root, bad).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod real {
    /// `cargo test -- --ignored branches_reais --nocapture`: lê a ~/Projects de verdade (só leitura).
    #[tokio::test]
    #[ignore]
    async fn branches_reais() {
        let root = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Projects");
        let t = std::time::Instant::now();
        let ps = super::list(&root, &[]).await.unwrap();
        println!("{} projetos em {:?}", ps.len(), t.elapsed());
        for p in ps.iter().take(6) {
            println!("{} {} base={:?} {} branches, último {:?}, 1ª={:?}", p.slug, p.kind, p.base, p.branches.len(), p.last_commit, p.branches.first().map(|b| (&b.name, &b.track, b.head)));
        }
        assert!(ps.iter().all(|p| p.error.is_none()), "{:?}", ps.iter().filter_map(|p| p.error.as_ref()).collect::<Vec<_>>());
    }
}
