//! Projeto relacionado (front ↔ back do mesmo card) para o chat do review e a correção de
//! comentários: um worktree `--detach`, só leitura, do outro repositório, para o agente conferir
//! o que vem do outro lado (resposta da API, validação, permissão) em vez de concluir que não dá
//! para saber.
//!
//! - O par sai do nome do repo: `<x>-front` ↔ `<x>-back`, na mesma pasta dos clones.
//! - Ref: a branch do card em `origin` (sem diferenciar maiúsculas), senão a base do PR em
//!   `origin`, senão o `origin/HEAD`. Sem rede: vale o que o último sync trouxe.
//! - O checkout do usuário no outro repo não é tocado (pode estar em outra branch ou sujo).

use std::path::{Path, PathBuf};

use super::git;

pub struct Related {
    pub slug: String,
    pub dir: PathBuf,
    pub sha: String,
    /// `true` quando é a branch do próprio card; `false` quando caiu na base.
    pub same_branch: bool,
    /// `origin/<branch>` usado.
    pub reference: String,
}

impl Related {
    /// Linha do prompt que apresenta o projeto ao agente.
    pub fn prompt_line(&self, branch: &str) -> String {
        let r#where = if self.same_branch {
            format!("na branch {} do mesmo card", self.reference)
        } else {
            format!(
                "em {} (o clone não tem a branch {branch}: é o código da base, sem as mudanças desta task do outro lado)",
                self.reference
            )
        };
        format!(
            "PROJETO_RELACIONADO: {slug} em {dir}, {where}, commit {sha}. É o outro lado da task (front ↔ back), \
só para leitura: quando a resposta depende do que vem de lá (resposta da API, validação, regra de negócio, \
permissão, dado filtrado), confira no código dele antes de concluir que não dá para saber, citando `arquivo:linha` \
com o caminho do projeto. O que o PR mudou continua sendo só o do WORKTREE.",
            slug = self.slug,
            dir = self.dir.display(),
            sha = &self.sha[..self.sha.len().min(12)],
        )
    }
}

/// `acme-front` ↔ `acme-back`.
pub fn peer_slug(slug: &str) -> Option<String> {
    if let Some(p) = slug.strip_suffix("-front") {
        Some(format!("{p}-back"))
    } else {
        slug.strip_suffix("-back").map(|p| format!("{p}-front"))
    }
}

/// Ref do outro repo: a branch do card, senão a base, senão o `origin/HEAD`.
async fn pick(repo: &Path, branch: &str, base: &str) -> Option<(String, bool)> {
    let refs = git::run(repo, &["for-each-ref", "--format=%(refname)", "refs/remotes/origin/"]).await.ok()?;
    let find = |name: &str| {
        let want = format!("refs/remotes/origin/{name}");
        refs.lines().map(str::trim).find(|r| r.eq_ignore_ascii_case(&want)).map(str::to_owned)
    };
    if let Some(r) = find(branch) {
        return Some((r, true));
    }
    if let Some(r) = find(base) {
        return Some((r, false));
    }
    let head = git::run(repo, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]).await.ok()?;
    (!head.is_empty()).then_some((head, false))
}

/// Cria (ou move para a ponta atual) o worktree do outro projeto em `dir`. `None` quando o repo
/// não tem par, o clone do par não existe ou o git falhou: o agente segue só com o próprio repo.
pub async fn prepare(root: &Path, slug: &str, branch: &str, base: &str, dir: &Path) -> Option<Related> {
    let peer = peer_slug(slug)?;
    let repo = root.join(&peer);
    if !repo.join(".git").exists() {
        return None;
    }
    let (refname, same_branch) = pick(&repo, branch, base).await?;
    let sha = git::run(&repo, &["rev-parse", &format!("{refname}^{{commit}}")]).await.ok()?;
    if dir.join(".git").exists() {
        let head = git::run(dir, &["rev-parse", "HEAD"]).await.unwrap_or_default();
        // só leitura para o agente; o --force só descarta o que nunca deveria estar lá
        if head != sha {
            git::run(dir, &["checkout", "-q", "--force", "--detach", &sha]).await.ok()?;
        }
    } else {
        git::run(&repo, &["worktree", "add", "-q", "--detach", &dir.display().to_string(), &sha]).await.ok()?;
    }
    Some(Related {
        slug: peer,
        dir: dir.to_path_buf(),
        sha,
        same_branch,
        reference: refname.trim_start_matches("refs/remotes/").to_owned(),
    })
}

/// Repo dono de um worktree, pelo `.git` dele (`gitdir: <repo>/.git/worktrees/<nome>`).
pub fn owner_repo(wt: &Path) -> Option<PathBuf> {
    let gitfile = std::fs::read_to_string(wt.join(".git")).ok()?;
    gitfile.trim().strip_prefix("gitdir: ").and_then(|p| Path::new(p).ancestors().nth(3)).map(Path::to_path_buf)
}

/// Remove o worktree do projeto relacionado, se existir.
pub async fn remove(dir: &Path) {
    let Some(repo) = owner_repo(dir) else { return };
    let _ = git::raw(&repo, &["worktree", "remove", "--force", &dir.display().to_string()]).await;
    let _ = git::raw(&repo, &["worktree", "prune"]).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn g(dir: &Path, args: &[&str]) {
        let o = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn par_pelo_sufixo() {
        assert_eq!(peer_slug("acme-front").as_deref(), Some("acme-back"));
        assert_eq!(peer_slug("acme-back").as_deref(), Some("acme-front"));
        assert_eq!(peer_slug("acme-site"), None);
    }

    /// Clone do back com `origin/dev` e `origin/PROJ-7`; o front não precisa existir.
    fn setup() -> (PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("prr-related-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let origin = tmp.join("origin");
        std::fs::create_dir(&origin).unwrap();
        g(&origin, &["init", "-q", "-b", "dev"]);
        g(&origin, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "base"]);
        g(&origin, &["branch", "PROJ-7"]);
        g(&origin, &["checkout", "-q", "PROJ-7"]);
        std::fs::write(origin.join("api.php"), "<?php // PROJ-7\n").unwrap();
        g(&origin, &["add", "."]);
        g(&origin, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "PROJ-7"]);
        g(&origin, &["checkout", "-q", "dev"]);
        let root = tmp.join("projects");
        std::fs::create_dir(&root).unwrap();
        g(&root, &["clone", "-q", origin.to_str().unwrap(), "x-back"]);
        (tmp, root)
    }

    #[tokio::test]
    async fn worktree_na_branch_do_card_ou_na_base() {
        let (tmp, root) = setup();
        let dir = tmp.join("run").join("related");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();

        // mesma branch, sem diferenciar maiúsculas
        let r = prepare(&root, "x-front", "proj-7", "dev", &dir).await.expect("relacionado");
        assert_eq!((r.slug.as_str(), r.same_branch, r.reference.as_str()), ("x-back", true, "origin/PROJ-7"));
        assert!(dir.join("api.php").exists());
        let line = r.prompt_line("proj-7");
        assert!(line.contains("PROJETO_RELACIONADO: x-back em") && line.contains("na branch origin/PROJ-7 do mesmo card"), "{line}");
        assert_eq!(owner_repo(&dir).as_deref(), Some(root.join("x-back").as_path()));

        // sem a branch: cai na base e move o worktree que já existe
        let r = prepare(&root, "x-front", "PROJ-8", "dev", &dir).await.expect("relacionado");
        assert_eq!((r.same_branch, r.reference.as_str()), (false, "origin/dev"));
        assert!(!dir.join("api.php").exists());
        assert!(r.prompt_line("PROJ-8").contains("em origin/dev (o clone não tem a branch PROJ-8"));

        // o checkout do usuário fica onde estava
        let head = Command::new("git").args(["branch", "--show-current"]).current_dir(root.join("x-back")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "dev");

        remove(&dir).await;
        assert!(!dir.exists());

        // sem par ou sem o clone do par
        assert!(prepare(&root, "x-back", "PROJ-7", "dev", &dir).await.is_none());
        assert!(prepare(&root, "outro", "PROJ-7", "dev", &dir).await.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
