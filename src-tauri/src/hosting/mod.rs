//! Provedores de git hosting (Bitbucket e GitHub).
//!
//! `Kind` e `RepoRef` resolvem o que depende do provedor sem falar com a API (caminho do clone,
//! rótulo, filtro da CLI, refs do git). Os chamadores não comparam o texto do provedor: perguntam
//! a estes tipos.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::review::{self, git};

/// Provedor de um repositório; o texto é o gravado em `repositories.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Bitbucket,
    Github,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "bitbucket" => Some(Kind::Bitbucket),
            "github" => Some(Kind::Github),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Bitbucket => "bitbucket",
            Kind::Github => "github",
        }
    }

    /// O Crivo só escreve (comentários, pedido de mudança) onde o provedor permite: o Bitbucket é
    /// só leitura por decisão do projeto.
    pub fn can_write(self) -> bool {
        matches!(self, Kind::Github)
    }
}

/// Um repositório de um provedor: `owner` é o workspace no Bitbucket e o dono no GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub kind: Kind,
    pub owner: String,
    pub slug: String,
}

impl RepoRef {
    pub fn new(provider: &str, owner: impl Into<String>, slug: impl Into<String>) -> AppResult<RepoRef> {
        let kind = Kind::parse(provider).ok_or_else(|| AppError::Invalid(format!("provedor desconhecido: {provider}")))?;
        Ok(RepoRef { kind, owner: owner.into(), slug: slug.into() })
    }

    /// Clone local: `<root>/<slug>`; no GitHub, `<root>/<owner>/<slug>` quando existe.
    pub fn clone_path(&self, root: &Path) -> PathBuf {
        if self.kind == Kind::Github {
            let nested = root.join(&self.owner).join(&self.slug);
            if nested.join(".git").exists() {
                return nested;
            }
        }
        root.join(&self.slug)
    }

    /// `slug#N` no Bitbucket, `owner/slug#N` no GitHub.
    pub fn label(&self, number: i64) -> String {
        match self.kind {
            Kind::Bitbucket => format!("{}#{number}", self.slug),
            Kind::Github => format!("{}/{}#{number}", self.owner, self.slug),
        }
    }

    /// Filtro `--repo` da CLI: o slug, ou `owner/slug` no GitHub.
    pub fn matches(&self, filter: &str) -> bool {
        filter == self.slug || (self.kind == Kind::Github && filter == format!("{}/{}", self.owner, self.slug))
    }

    /// Ref da ponta do PR no clone, depois do último sync (aba Diff).
    pub fn head_ref(&self, number: i64, src_branch: &str) -> String {
        match self.kind {
            Kind::Bitbucket => format!("origin/{src_branch}"),
            Kind::Github => format!("origin/pr/{number}"),
        }
    }

    /// Busca agora as pontas de origem e destino do PR e devolve os dois SHAs.
    pub async fn fetch_tips(&self, repo: &Path, number: i64, src: &str, dest: &str) -> AppResult<(String, String)> {
        match self.kind {
            Kind::Bitbucket => review::fresh_tips(repo, src, dest).await,
            Kind::Github => review::github_tips(repo, number, dest).await,
        }
    }

    /// Checkout do PR no clone do usuário, sem rede no Bitbucket. Devolve a branch local e o SHA.
    pub async fn checkout(&self, repo: &Path, number: i64, branch: &str) -> AppResult<(String, String)> {
        match self.kind {
            Kind::Bitbucket => Ok((branch.to_owned(), git::checkout_synced(repo, &self.slug, branch).await?)),
            Kind::Github => git::checkout_github(repo, number).await,
        }
    }
}

/// Resultado do sync de um repositório.
#[derive(Debug, Serialize)]
pub struct RepoSync {
    pub slug: String,
    pub open: usize,
    pub error: Option<String>,
    /// `git fetch --prune` no clone local (traz as branches novas para o Checkout).
    pub fetch_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub user: String,
    pub repos: Vec<RepoSync>,
    pub requests: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    pub author: String,
    pub body: String,
    pub created_on: Option<String>,
}

/// Uma thread de comentário que ainda pede resposta.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Thread {
    pub id: i64,
    pub author: String,
    pub body: String,
    /// Comentário em linha: arquivo e linha (do lado novo; do antigo se a linha foi removida).
    pub path: Option<String>,
    pub line: Option<i64>,
    /// O código comentado mudou depois do comentário.
    pub outdated: bool,
    pub url: Option<String>,
    pub created_on: Option<String>,
    pub replies: Vec<Reply>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_ida_e_volta() {
        assert_eq!(Kind::parse("github"), Some(Kind::Github));
        assert_eq!(Kind::parse("bitbucket"), Some(Kind::Bitbucket));
        assert_eq!(Kind::Bitbucket.as_str(), "bitbucket");
        assert_eq!(Kind::parse("gitlab"), None);
        assert!(!Kind::Bitbucket.can_write() && Kind::Github.can_write());
        assert_eq!(serde_json::to_value(Kind::Github).unwrap(), "github");
    }

    #[test]
    fn rotulo_e_filtro() {
        let bb = RepoRef::new("bitbucket", "acme", "app").unwrap();
        let gh = RepoRef::new("github", "acme", "app").unwrap();
        assert_eq!(bb.label(7), "app#7");
        assert_eq!(gh.label(7), "acme/app#7");
        assert!(bb.matches("app") && !bb.matches("acme/app"));
        assert!(gh.matches("app") && gh.matches("acme/app") && !gh.matches("outro/app"));
    }

    #[test]
    fn head_ref_por_provedor() {
        assert_eq!(RepoRef::new("bitbucket", "a", "r").unwrap().head_ref(3, "PROJ-1"), "origin/PROJ-1");
        assert_eq!(RepoRef::new("github", "a", "r").unwrap().head_ref(3, "PROJ-1"), "origin/pr/3");
    }

    #[test]
    fn clone_do_github_prefere_owner_repo() {
        let root = std::env::temp_dir().join(format!("crivo-hosting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let gh = RepoRef::new("github", "acme", "app").unwrap();
        let bb = RepoRef::new("bitbucket", "acme", "app").unwrap();
        assert_eq!(gh.clone_path(&root), root.join("app"));
        std::fs::create_dir_all(root.join("acme/app/.git")).unwrap();
        assert_eq!(gh.clone_path(&root), root.join("acme/app"));
        assert_eq!(bb.clone_path(&root), root.join("app"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn provedor_desconhecido_e_erro() {
        assert!(RepoRef::new("gitlab", "a", "r").is_err());
    }
}
