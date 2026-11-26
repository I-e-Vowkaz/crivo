//! Git do app (fetch, worktree, verificações). Os agentes nunca escrevem em git:
//! isso fica com o hook `guard-git.py`.

use std::path::Path;

use tokio::process::Command;

use crate::error::{AppError, AppResult};

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

pub async fn raw(cwd: &Path, args: &[&str]) -> AppResult<Output> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        // nunca travar esperando senha: o fetch usa o SSH/credencial do usuário ou falha
        .env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        // no timer do systemd não há terminal: sem BatchMode o ssh poderia esperar uma senha
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| AppError::Invalid(format!("não consegui rodar git: {e}")))?;
    Ok(Output {
        code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    })
}

/// Roda e exige sucesso; devolve o stdout sem espaços nas pontas.
pub async fn run(cwd: &Path, args: &[&str]) -> AppResult<String> {
    let o = raw(cwd, args).await?;
    if o.code != 0 {
        return Err(AppError::Invalid(format!("git {}: {}", args.join(" "), o.stderr)));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Checkout pedido pelo usuário, sem rede: usa o `origin/<branch>` que o último sync trouxe.
/// Recusa com alteração não commitada; avança só por fast-forward. Devolve o HEAD.
pub async fn checkout_synced(repo: &Path, slug: &str, branch: &str) -> AppResult<String> {
    let dirty = run(repo, &["status", "--porcelain", "--untracked-files=no"]).await?;
    if !dirty.is_empty() {
        let n = dirty.lines().count();
        return Err(AppError::Invalid(format!(
            "{slug} tem {n} arquivo(s) alterado(s) sem commit; faça commit ou stash antes de trocar para {branch}"
        )));
    }
    let remote = format!("refs/remotes/origin/{branch}");
    if raw(repo, &["rev-parse", "--verify", "-q", &remote]).await?.code != 0 {
        return Err(AppError::Invalid(format!(
            "a branch {branch} ainda não está no seu clone de {slug}; clique em Atualizar (o sync faz o fetch) e tente de novo"
        )));
    }
    run(repo, &["checkout", "-q", branch]).await?;
    run(repo, &["merge", "-q", "--ff-only", &remote]).await.map_err(|e| {
        AppError::Invalid(format!("{branch} local tem commits que não estão no remoto, então não dá para avançar sozinho: {e}"))
    })?;
    run(repo, &["rev-parse", "HEAD"]).await
}

/// Checkout local de uma PR GitHub, inclusive quando a origem é um fork.
pub async fn checkout_github(repo: &Path, number: i64) -> AppResult<(String, String)> {
    let dirty = run(repo, &["status", "--porcelain", "--untracked-files=no"]).await?;
    if !dirty.is_empty() { return Err(AppError::Invalid("há alterações locais sem commit; faça commit ou stash".into())); }
    let branch = format!("crivo/pr-{number}");
    let source = format!("refs/pull/{number}/head");
    let remote = format!("refs/remotes/origin/pr/{number}");
    run(repo, &["fetch", "-q", "origin", &format!("+{source}:{remote}")]).await?;
    let exists = raw(repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).await?.code == 0;
    if exists {
        run(repo, &["switch", "-q", &branch]).await?;
        run(repo, &["merge", "-q", "--ff-only", &remote]).await?;
    } else {
        run(repo, &["switch", "-q", "-c", &branch, &remote]).await?;
    }
    Ok((branch, run(repo, &["rev-parse", "HEAD"]).await?))
}

/// Base do review de uma branch local: `dev` se a origin tiver, senão a branch padrão da origin
/// (`origin/HEAD`), senão `main` ou `master`. Sem rede: olha os refs que o clone já tem.
pub async fn default_base(repo: &Path) -> Option<String> {
    let has = |b: String| async move {
        raw(repo, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{b}")]).await.is_ok_and(|o| o.code == 0)
    };
    if has("dev".into()).await {
        return Some("dev".into());
    }
    if let Ok(o) = raw(repo, &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]).await {
        let head = String::from_utf8_lossy(&o.stdout).trim().to_owned();
        if let Some(b) = head.strip_prefix("origin/").filter(|b| o.code == 0 && !b.is_empty()) {
            return Some(b.to_owned());
        }
    }
    for b in ["main", "master"] {
        if has(b.into()).await {
            return Some(b.into());
        }
    }
    None
}

/// Branches em que nenhum review roda (o app recusa PR que sai delas).
pub fn is_protected(branch: &str) -> bool {
    let b = branch.to_ascii_lowercase();
    matches!(
        b.as_str(),
        "master" | "main" | "dev" | "develop" | "development" | "homol" | "homolog" | "homologacao"
            | "homologation" | "hml" | "staging" | "stage" | "prod" | "production" | "producao" | "hotfix"
    ) || b == "release"
        || b.starts_with("release/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkout_sem_rede_com_o_que_o_sync_trouxe() {
        let root = std::env::temp_dir().join(format!("prr-co-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = |dir: &Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=dev"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_owned()
        };
        let (origin, author, user) = (root.join("o.git"), root.join("a"), root.join("u"));
        g(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), author.to_str().unwrap()]);
        std::fs::write(author.join("f.txt"), "1\n").unwrap();
        g(&author, &["add", "f.txt"]);
        g(&author, &["commit", "-q", "-m", "base"]);
        g(&author, &["push", "-q", "origin", "HEAD:dev"]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), user.to_str().unwrap()]);

        // branch nova no remoto, ainda sem sync: recusa e pede Atualizar
        g(&author, &["commit", "-q", "--allow-empty", "-m", "pr v1"]);
        g(&author, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        let err = checkout_synced(&user, "repo", "PROJ-1").await.unwrap_err().to_string();
        assert!(err.contains("clique em Atualizar"), "{err}");

        // depois do fetch do sync: cria a branch local a partir do origin
        g(&user, &["fetch", "-q", "origin"]);
        let v1 = g(&author, &["rev-parse", "HEAD"]);
        assert_eq!(checkout_synced(&user, "repo", "PROJ-1").await.unwrap(), v1);

        // novo commit + novo sync: a branch local, atrasada, avança por fast-forward
        g(&author, &["commit", "-q", "--allow-empty", "-m", "pr v2"]);
        g(&author, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        g(&user, &["fetch", "-q", "origin"]);
        let v2 = g(&author, &["rev-parse", "HEAD"]);
        g(&user, &["checkout", "-q", "dev"]);
        assert_eq!(checkout_synced(&user, "repo", "PROJ-1").await.unwrap(), v2);

        // alteração sem commit: recusa e não troca de branch
        std::fs::write(user.join("f.txt"), "mudou\n").unwrap();
        let err = checkout_synced(&user, "repo", "PROJ-1").await.unwrap_err().to_string();
        assert!(err.contains("sem commit"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn branches_protegidas() {
        for b in ["dev", "DEV", "main", "release/1.2", "hotfix", "homologacao"] {
            assert!(is_protected(b), "{b}");
        }
        for b in ["PROJ-1676", "feature/dev-x", "devops", "hotfix-1"] {
            assert!(!is_protected(b), "{b}");
        }
    }
}
