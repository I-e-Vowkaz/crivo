//! Diff do PR para a aba "Diff" do painel do review: `git diff` no clone local, sem rede e sem
//! worktree. A lista vem só do `--numstat`; o texto, um arquivo por vez e com limite de tamanho
//! (há PR com 60 MB de diff).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::review::git;

/// Qual diff mostrar: o que o Claude revisou ou a ponta atual da branch (último sync).
#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Reviewed,
    Current,
}

/// Onde e entre o quê rodar o `git diff` (`base...head`).
pub struct Target {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    /// Commit revisado (`source_sha` do review), para contar o que chegou depois.
    pub reviewed_head: String,
}

#[derive(Serialize, Debug)]
pub struct DiffSummary {
    pub base_sha: String,
    pub head_sha: String,
    pub reviewed_sha: String,
    /// Commits na ponta depois do revisado (só na visão atual e se o revisado existe).
    pub ahead: Option<u32>,
    pub same_as_reviewed: bool,
    /// Motivo de não haver diff (commit ou branch que não está no clone).
    pub missing: Option<String>,
    pub files: Vec<DiffFile>,
}

/// Lê o review e monta o intervalo da visão pedida: PR (`pr_id`) ou branch local
/// (`repo_slug` + `branch`, base `origin/<base_branch>`).
pub fn target(conn: &Connection, run_id: &str, view: View) -> AppResult<Target> {
    let (pr_id, slug, branch, base_branch, source_sha, dest_sha): (
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT pr_id, repo_slug, branch, base_branch, source_sha, dest_sha FROM review_runs WHERE id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    let root = crate::review::repos_root(conn)?;
    let (repo, cur_base, cur_head) = match pr_id {
        Some(id) => {
            let (rr, src, dst, number): (crate::hosting::RepoRef, String, String, i64) = conn.query_row(
                "SELECT r.provider, r.workspace, r.slug, p.source_branch, p.dest_branch, p.bb_id FROM pull_requests p
                 JOIN repositories r ON r.id = p.repository_id WHERE p.id = ?1",
                [id],
                |r| Ok((crate::hosting::RepoRef::from_row(&r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
            let repo = rr.clone_path(&root);
            if !repo.join(".git").exists() {
                return Err(AppError::Invalid(format!("{} não é um clone git", repo.display())));
            }
            (repo, format!("origin/{dst}"), rr.head_ref(number, &src))
        }
        None => {
            let slug = slug.ok_or_else(|| AppError::Invalid("review sem projeto".into()))?;
            let branch = branch.ok_or_else(|| AppError::Invalid("review sem branch".into()))?;
            let base = base_branch.unwrap_or_else(|| "dev".into());
            (crate::branches::repo_dir(&root, &slug)?, format!("origin/{base}"), format!("refs/heads/{branch}"))
        }
    };
    let reviewed_head = source_sha.unwrap_or_default();
    let (base, head) = match view {
        View::Reviewed => (dest_sha.unwrap_or_default(), reviewed_head.clone()),
        View::Current => (cur_base, cur_head),
    };
    Ok(Target { repo, base, head, reviewed_head })
}

async fn commit_of(repo: &Path, rev: &str) -> Option<String> {
    if rev.is_empty() {
        return None;
    }
    git::run(repo, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).await.ok()
}

/// Prende a visão aos commits do resumo que a tela mostra: um sync com a aba aberta não muda
/// o commit dos arquivos que ainda vão carregar. Só aceita SHA em hexadecimal.
pub fn pin(mut t: Target, base_sha: &str, head_sha: &str) -> AppResult<Target> {
    let ok = |s: &str| (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok(base_sha) || !ok(head_sha) {
        return Err(AppError::Invalid(format!("commit inválido: {base_sha}...{head_sha}")));
    }
    t.base = base_sha.to_owned();
    t.head = head_sha.to_owned();
    Ok(t)
}

/// Lista da visão, ou o motivo de não haver diff.
pub async fn summary(t: &Target) -> AppResult<DiffSummary> {
    let reviewed = commit_of(&t.repo, &t.reviewed_head).await;
    let current = t.head != t.reviewed_head;
    let (base_sha, head_sha) = (commit_of(&t.repo, &t.base).await, commit_of(&t.repo, &t.head).await);
    let mut s = DiffSummary {
        base_sha: base_sha.clone().unwrap_or_default(),
        head_sha: head_sha.clone().unwrap_or_default(),
        reviewed_sha: t.reviewed_head.clone(),
        ahead: None,
        same_as_reviewed: false,
        missing: None,
        files: vec![],
    };
    let (Some(base_sha), Some(head_sha)) = (base_sha, head_sha) else {
        let sha7: String = t.reviewed_head.chars().take(7).collect();
        s.missing = Some(if current {
            let gone = if s.head_sha.is_empty() { &t.head } else { &t.base };
            let gone = gone.strip_prefix("refs/heads/").unwrap_or(gone);
            format!("A branch {gone} não existe no clone (PR já integrado ou branch apagada?). Clique em Atualizar.")
        } else {
            format!("O commit revisado ({sha7}) não está mais no clone (a branch foi reescrita?). Use a visão atual.")
        });
        return Ok(s);
    };
    s.same_as_reviewed = reviewed.as_deref() == Some(head_sha.as_str());
    if current && let Some(rev) = &reviewed {
        s.ahead = git::run(&t.repo, &["rev-list", "--count", &format!("{rev}..{head_sha}")]).await?.parse().ok();
    }
    s.files = list_files(&t.repo, &base_sha, &head_sha, &[]).await?;
    Ok(s)
}

/// Diff de um arquivo da visão; recusa caminho que não está na lista dela. A conferência lista
/// só esse arquivo (com o caminho antigo, se renomeado), não o PR inteiro.
pub async fn one_file(t: &Target, path: &str, old_path: Option<&str>) -> AppResult<FileDiff> {
    let paths: Vec<&str> = old_path.into_iter().chain([path]).collect();
    // o git recusaria com a mensagem dele; aqui a recusa é a mesma de qualquer caminho fora da lista
    if paths.iter().any(|p| p.is_empty() || p.starts_with('/') || p.split('/').any(|c| c == "..")) {
        return Err(AppError::Invalid(format!("arquivo fora do diff: {path}")));
    }
    let files = list_files(&t.repo, &t.base, &t.head, &paths).await?;
    let file = files
        .iter()
        .find(|f| f.path == path && f.old_path.as_deref() == old_path)
        .ok_or_else(|| AppError::Invalid(format!("arquivo fora do diff: {path}")))?;
    file_diff(&t.repo, &t.base, &t.head, file).await
}

/// Limite do texto de um arquivo; o resto é cortado na última quebra de linha.
pub const MAX_FILE_BYTES: usize = 1_000_000;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct DiffFile {
    pub path: String,
    /// Caminho antigo, só em renomeação.
    pub old_path: Option<String>,
    /// `added`, `deleted`, `renamed` ou `modified`.
    pub status: String,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct HunkLine {
    /// `ctx`, `del` ou `add`.
    pub kind: String,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Hunk {
    /// A linha `@@ … @@` inteira.
    pub header: String,
    pub lines: Vec<HunkLine>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    pub truncated: bool,
}

/// `git diff` sem depender da configuração do usuário (cor, diff externo, caminho citado).
fn diff_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    // `--literal-pathspecs`: `pages/[id].vue` é o arquivo, não um padrão que casa com `pages/i.vue`
    let mut a = vec!["--literal-pathspecs", "-c", "core.quotePath=false", "diff", "--no-color", "--no-ext-diff", "-M"];
    a.extend_from_slice(extra);
    a
}

/// Arquivos alterados entre a base e a ponta (`base...head`), na ordem do git.
pub async fn list_files(repo: &Path, base: &str, head: &str, paths: &[&str]) -> AppResult<Vec<DiffFile>> {
    let range = format!("{base}...{head}");
    let mut args = diff_args(&["-z", "--numstat", &range, "--"]);
    args.extend_from_slice(paths);
    let numstat = git::run(repo, &args).await?;
    let mut args = diff_args(&["-z", "--name-status", &range, "--"]);
    args.extend_from_slice(paths);
    let names = git::run(repo, &args).await?;

    // name-status: `X\0path\0`, ou `R<nn>\0old\0new\0` (C também traz dois caminhos)
    let mut status: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut it = names.split('\0').filter(|s| !s.is_empty());
    while let Some(code) = it.next() {
        let two = code.starts_with('R') || code.starts_with('C');
        let first = it.next().unwrap_or_default().to_owned();
        let (path, old) = if two { (it.next().unwrap_or_default().to_owned(), Some(first)) } else { (first, None) };
        let st = match code.chars().next() {
            Some('A') => "added",
            Some('D') => "deleted",
            Some('R') => "renamed",
            _ => "modified",
        };
        status.insert(path, (st.to_owned(), old));
    }

    // numstat: `A\tR\tpath\0`, ou `A\tR\t\0old\0new\0` em renomeação; binário com `-\t-`
    let mut out = Vec::new();
    let mut it = numstat.split('\0');
    while let Some(rec) = it.next() {
        if rec.is_empty() {
            continue;
        }
        let mut parts = rec.splitn(3, '\t');
        let (a, r, p) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let path = if p.is_empty() {
            it.next(); // caminho antigo; o status já o tem
            it.next().unwrap_or_default().to_owned()
        } else {
            p.to_owned()
        };
        let binary = a == "-" && r == "-";
        let (st, old_path) = status.remove(&path).unwrap_or_else(|| ("modified".to_owned(), None));
        out.push(DiffFile {
            path,
            old_path,
            status: st,
            added: a.parse().unwrap_or(0),
            removed: r.parse().unwrap_or(0),
            binary,
        });
    }
    Ok(out)
}

/// Diff de um arquivo, cortado em [`MAX_FILE_BYTES`].
pub async fn file_diff(repo: &Path, base: &str, head: &str, file: &DiffFile) -> AppResult<FileDiff> {
    let range = format!("{base}...{head}");
    let mut args = diff_args(&[&range, "--"]);
    if let Some(old) = &file.old_path {
        args.push(old);
    }
    args.push(&file.path);
    let o = git::raw(repo, &args).await?;
    if o.code != 0 {
        return Err(crate::error::AppError::Invalid(format!("git diff {}: {}", file.path, o.stderr)));
    }
    let mut bytes = &o.stdout[..];
    let truncated = bytes.len() > MAX_FILE_BYTES;
    if truncated {
        let cut = bytes[..MAX_FILE_BYTES].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        bytes = &bytes[..cut];
    }
    let text = String::from_utf8_lossy(bytes);
    let binary = text.lines().take_while(|l| !l.starts_with("@@")).any(|l| l.starts_with("Binary files "));
    Ok(FileDiff { hunks: parse_unified(&text), binary, truncated })
}

/// Lê o formato unificado do `git diff` (um arquivo): trechos `@@ -a,b +c,d @@` com as linhas
/// numeradas nos dois lados. O cabeçalho antes do primeiro `@@` e as linhas `\ No newline…` saem.
pub fn parse_unified(text: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    for line in text.lines() {
        if line.starts_with("@@") {
            let (o, n) = hunk_starts(line);
            old_no = o;
            new_no = n;
            hunks.push(Hunk { header: line.to_owned(), lines: Vec::new() });
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };
        let (kind, old, new) = match line.as_bytes().first() {
            Some(b' ') | None => ("ctx", Some(old_no), Some(new_no)),
            Some(b'-') => ("del", Some(old_no), None),
            Some(b'+') => ("add", None, Some(new_no)),
            _ => continue, // `\ No newline at end of file`
        };
        if old.is_some() {
            old_no += 1;
        }
        if new.is_some() {
            new_no += 1;
        }
        h.lines.push(HunkLine { kind: kind.to_owned(), old_no: old, new_no: new, text: line.get(1..).unwrap_or("").to_owned() });
    }
    hunks
}

/// `@@ -a[,b] +c[,d] @@` → (a, c).
fn hunk_starts(header: &str) -> (u32, u32) {
    let mut o = 0;
    let mut n = 0;
    for tok in header.split_whitespace().skip(1).take(2) {
        let num = |s: &str| s.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        if let Some(rest) = tok.strip_prefix('-') {
            o = num(rest);
        } else if let Some(rest) = tok.strip_prefix('+') {
            n = num(rest);
        }
    }
    (o, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::{Path, PathBuf};

    fn g(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=dev"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    }

    /// Repo com um commit base em `dev`; devolve (pasta, sha da base).
    fn repo(name: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("prr-diff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        g(&dir, &["init", "-q"]);
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        g(&dir, &["add", "-A"]);
        g(&dir, &["commit", "-q", "-m", "base"]);
        let base = g(&dir, &["rev-parse", "HEAD"]);
        (dir, base)
    }

    fn commit(dir: &Path) -> String {
        g(dir, &["add", "-A"]);
        g(dir, &["commit", "-q", "-m", "pr"]);
        g(dir, &["rev-parse", "HEAD"])
    }

    const DOIS_TRECHOS: &str = "diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,4 @@
 um
-dois
+DOIS
+dois e meio
 tres
@@ -10,2 +11,2 @@ fn x()
 dez
-onze
+ONZE
";

    #[test]
    fn parse_dois_trechos() {
        let h = parse_unified(DOIS_TRECHOS);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].header, "@@ -1,3 +1,4 @@");
        let nums: Vec<_> = h[0].lines.iter().map(|l| (l.kind.as_str(), l.old_no, l.new_no, l.text.as_str())).collect();
        assert_eq!(
            nums,
            vec![
                ("ctx", Some(1), Some(1), "um"),
                ("del", Some(2), None, "dois"),
                ("add", None, Some(2), "DOIS"),
                ("add", None, Some(3), "dois e meio"),
                ("ctx", Some(3), Some(4), "tres"),
            ]
        );
        assert!(h[1].header.starts_with("@@ -10,2 +11,2 @@"));
        let l = &h[1].lines;
        assert_eq!((l[0].old_no, l[0].new_no), (Some(10), Some(11)));
        assert_eq!((l[1].kind.as_str(), l[1].old_no), ("del", Some(11)));
        assert_eq!((l[2].kind.as_str(), l[2].new_no), ("add", Some(12)));
    }

    #[test]
    fn sem_newline_no_fim() {
        let h = parse_unified("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n");
        assert_eq!(h.len(), 1);
        let kinds: Vec<_> = h[0].lines.iter().map(|l| (l.kind.as_str(), l.text.as_str())).collect();
        assert_eq!(kinds, vec![("del", "a"), ("add", "b")]);
    }

    #[tokio::test]
    async fn lista_e_numstat() {
        let (dir, _) = repo("lista");
        std::fs::write(dir.join("m.txt"), "1\n2\n3\n").unwrap();
        std::fs::write(dir.join("apagar.txt"), "x\n").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2]).unwrap();
        let base = commit(&dir);
        std::fs::write(dir.join("m.txt"), "1\nDOIS\n3\n4\n").unwrap();
        std::fs::write(dir.join("novo.txt"), "n\n").unwrap();
        std::fs::remove_file(dir.join("apagar.txt")).unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        let head = commit(&dir);

        let mut files = list_files(&dir, &base, &head, &[]).await.unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let got: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.status.as_str(), f.added, f.removed, f.binary)).collect();
        assert_eq!(
            got,
            vec![
                ("apagar.txt", "deleted", 0, 1, false),
                ("bin.dat", "modified", 0, 0, true),
                ("m.txt", "modified", 2, 1, false),
                ("novo.txt", "added", 1, 0, false),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn arquivo_renomeado() {
        let (dir, _) = repo("ren");
        let texto: String = (1..=20).map(|i| format!("linha {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &texto).unwrap();
        let base = commit(&dir);
        g(&dir, &["mv", "a.txt", "b.txt"]);
        std::fs::write(dir.join("b.txt"), texto.replace("linha 10\n", "linha DEZ\n")).unwrap();
        let head = commit(&dir);

        let files = list_files(&dir, &base, &head, &[]).await.unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!((f.path.as_str(), f.old_path.as_deref(), f.status.as_str()), ("b.txt", Some("a.txt"), "renamed"));
        let d = file_diff(&dir, &base, &head, f).await.unwrap();
        let mudou: Vec<_> = d.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind != "ctx").map(|l| l.text.as_str()).collect();
        assert_eq!(mudou, vec!["linha 10", "linha DEZ"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn caminho_com_espaco_e_acento() {
        let (dir, base) = repo("acento");
        std::fs::create_dir_all(dir.join("pasta x")).unwrap();
        std::fs::write(dir.join("pasta x/ação.txt"), "oi\n").unwrap();
        let head = commit(&dir);

        let files = list_files(&dir, &base, &head, &[]).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "pasta x/ação.txt");
        let d = file_diff(&dir, &base, &head, &files[0]).await.unwrap();
        assert_eq!(d.hunks.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Origin, autor e o clone do usuário em `<root>/r` (slug `r`), com o banco apontando para
    /// `<root>`. Base `dev` e PR `PROJ-1` com `f.txt` alterado; o clone já fez fetch.
    /// Devolve (root, autor, conexão, sha da base, sha do PR).
    fn cenario(name: &str) -> (PathBuf, PathBuf, Connection, String, String) {
        let root = std::env::temp_dir().join(format!("prr-dview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (origin, author, user) = (root.join("o.git"), root.join("a"), root.join("r"));
        g(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), author.to_str().unwrap()]);
        std::fs::write(author.join("f.txt"), "1\n").unwrap();
        std::fs::write(author.join("nao-mudou.txt"), "x\n").unwrap();
        let dest = commit(&author);
        g(&author, &["push", "-q", "origin", "HEAD:dev"]);
        std::fs::write(author.join("f.txt"), "1\n2\n").unwrap();
        let src = commit(&author);
        g(&author, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        g(&root, &["clone", "-q", origin.to_str().unwrap(), user.to_str().unwrap()]);

        let conn = crate::db::open_in_memory().unwrap();
        crate::db::set_setting(&conn, "repos.root", &serde_json::json!(root.to_str().unwrap())).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO repositories (id, workspace, slug) VALUES (1, 'w', 'r');
             INSERT INTO pull_requests (id, repository_id, bb_id, title, url, author_uuid, source_branch, dest_branch,
               source_sha, dest_sha, state, created_on, updated_on, category)
             VALUES (1, 1, 10, 't', 'u', 'a', 'PROJ-1', 'dev', '{src}', '{dest}', 'OPEN', 'c', 'u', 'review_requested');
             INSERT INTO review_runs (id, pr_id, source_sha, dest_sha, status, run_dir)
             VALUES ('run', 1, '{src}', '{dest}', 'completed', '/x');"
        ))
        .unwrap();
        (root, author, conn, dest, src)
    }

    #[tokio::test]
    async fn visao_revisada_do_pr() {
        let (root, _, conn, dest, src) = cenario("rev");
        let t = target(&conn, "run", View::Reviewed).unwrap();
        assert_eq!((t.base.as_str(), t.head.as_str()), (dest.as_str(), src.as_str()));
        let s = summary(&t).await.unwrap();
        assert_eq!(s.missing, None);
        assert!(s.same_as_reviewed);
        assert_eq!(s.ahead, None);
        assert_eq!(s.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["f.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn visao_atual_conta_commits_novos() {
        let (root, author, conn, _, src) = cenario("atual");
        std::fs::write(author.join("f.txt"), "1\n2\n3\n").unwrap();
        commit(&author);
        std::fs::write(author.join("g.txt"), "g\n").unwrap();
        let tip = commit(&author);
        g(&author, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        g(&root.join("r"), &["fetch", "-q", "origin"]);

        let t = target(&conn, "run", View::Current).unwrap();
        assert_eq!(t.reviewed_head, src);
        let s = summary(&t).await.unwrap();
        assert_eq!(s.head_sha, tip);
        assert_eq!(s.ahead, Some(2));
        assert!(!s.same_as_reviewed);
        assert_eq!(s.files.len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn branch_local() {
        let (root, _, conn, dest, src) = cenario("local");
        g(&root.join("r"), &["branch", "PROJ-2", "origin/PROJ-1"]);
        conn.execute_batch(&format!(
            "INSERT INTO review_runs (id, repo_slug, branch, base_branch, source_sha, dest_sha, status, run_dir)
             VALUES ('loc', 'r', 'PROJ-2', 'dev', '{src}', '{dest}', 'completed', '/y');"
        ))
        .unwrap();
        let t = target(&conn, "loc", View::Current).unwrap();
        assert_eq!((t.base.as_str(), t.head.as_str()), ("origin/dev", "refs/heads/PROJ-2"));
        let s = summary(&t).await.unwrap();
        assert!(s.same_as_reviewed);
        assert_eq!(s.ahead, Some(0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn commit_revisado_sumiu() {
        let (root, _, conn, _, _) = cenario("sumiu");
        conn.execute("UPDATE review_runs SET source_sha = ?1", [&"0".repeat(40)]).unwrap();
        let s = summary(&target(&conn, "run", View::Reviewed).unwrap()).await.unwrap();
        assert!(s.missing.as_deref().unwrap_or("").starts_with("O commit revisado"), "{:?}", s.missing);
        assert!(s.files.is_empty());
        // na visão atual, o revisado sumido só tira a contagem
        let s = summary(&target(&conn, "run", View::Current).unwrap()).await.unwrap();
        assert_eq!((s.missing, s.ahead), (None, None));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn ponta_sumiu() {
        let (root, _, conn, _, _) = cenario("ponta");
        conn.execute("UPDATE pull_requests SET source_branch = 'PROJ-9'", []).unwrap();
        let s = summary(&target(&conn, "run", View::Current).unwrap()).await.unwrap();
        assert!(s.missing.as_deref().unwrap_or("").contains("não existe no clone"), "{:?}", s.missing);
        assert!(s.files.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn caminho_fora_da_lista() {
        let (root, _, conn, _, _) = cenario("fora");
        let t = target(&conn, "run", View::Reviewed).unwrap();
        for p in ["../../etc/passwd", "nao-mudou.txt"] {
            let err = one_file(&t, p, None).await.unwrap_err().to_string();
            assert!(err.contains("arquivo fora do diff"), "{err}");
        }
        assert_eq!(one_file(&t, "f.txt", None).await.unwrap().hunks.len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Um sync com a aba aberta não pode trocar o commit dos arquivos que ainda vão carregar.
    #[tokio::test]
    async fn diff_preso_aos_shas_do_resumo() {
        let (root, author, conn, _, _) = cenario("preso");
        let s = summary(&target(&conn, "run", View::Current).unwrap()).await.unwrap();
        std::fs::write(author.join("f.txt"), "1\n2\nnovo\n").unwrap();
        commit(&author);
        g(&author, &["push", "-q", "origin", "HEAD:refs/heads/PROJ-1"]);
        g(&root.join("r"), &["fetch", "-q", "origin"]);

        let t = pin(target(&conn, "run", View::Current).unwrap(), &s.base_sha, &s.head_sha).unwrap();
        let d = one_file(&t, "f.txt", None).await.unwrap();
        let adds: Vec<_> = d.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == "add").map(|l| l.text.as_str()).collect();
        assert_eq!(adds, vec!["2"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn pin_recusa_sha_invalido() {
        let (root, _, conn, dest, src) = cenario("pin");
        for (b, h) in [("--output=/tmp/x", src.as_str()), (dest.as_str(), "HEAD"), ("", src.as_str())] {
            let err = pin(target(&conn, "run", View::Current).unwrap(), b, h).err().expect("deveria recusar").to_string();
            assert!(err.contains("commit inválido"), "{err}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Só o arquivo pedido é listado; com o caminho antigo, a renomeação continua reconhecida.
    #[tokio::test]
    async fn um_arquivo_renomeado_pelo_caminho_antigo() {
        let (dir, _) = repo("um-ren");
        let texto: String = (1..=20).map(|i| format!("linha {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &texto).unwrap();
        let base = commit(&dir);
        g(&dir, &["mv", "a.txt", "b.txt"]);
        std::fs::write(dir.join("b.txt"), texto.replace("linha 10\n", "linha DEZ\n")).unwrap();
        std::fs::write(dir.join("outro.txt"), "o\n").unwrap();
        let head = commit(&dir);

        let only = list_files(&dir, &base, &head, &["a.txt", "b.txt"]).await.unwrap();
        assert_eq!(only.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["b.txt"]);
        let t = Target { repo: dir.clone(), base: base.clone(), head: head.clone(), reviewed_head: head.clone() };
        let d = one_file(&t, "b.txt", Some("a.txt")).await.unwrap();
        let mudou: Vec<_> = d.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind != "ctx").map(|l| l.text.as_str()).collect();
        assert_eq!(mudou, vec!["linha 10", "linha DEZ"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `pages/[id].vue` não pode casar com `pages/i.vue` (caminho literal, não padrão).
    #[tokio::test]
    async fn caminho_com_colchetes() {
        let (dir, base) = repo("colchete");
        std::fs::create_dir_all(dir.join("pages")).unwrap();
        std::fs::write(dir.join("pages/[id].vue"), "a\n").unwrap();
        std::fs::write(dir.join("pages/i.vue"), "b\n").unwrap();
        let head = commit(&dir);

        let t = Target { repo: dir.clone(), base: base.clone(), head: head.clone(), reviewed_head: head.clone() };
        let d = one_file(&t, "pages/[id].vue", None).await.unwrap();
        let textos: Vec<_> = d.hunks.iter().flat_map(|h| &h.lines).map(|l| l.text.as_str()).collect();
        assert_eq!(textos, vec!["a"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn corta_em_um_mega() {
        let (dir, base) = repo("mega");
        let linha = "x".repeat(49);
        let texto: String = (0..30_000).map(|_| format!("{linha}\n")).collect();
        std::fs::write(dir.join("grande.txt"), texto).unwrap();
        let head = commit(&dir);

        let files = list_files(&dir, &base, &head, &[]).await.unwrap();
        let d = file_diff(&dir, &base, &head, &files[0]).await.unwrap();
        assert!(d.truncated);
        let lines: Vec<_> = d.hunks.iter().flat_map(|h| &h.lines).collect();
        let bytes: usize = lines.iter().map(|l| l.text.len() + 2).sum();
        assert!(bytes <= MAX_FILE_BYTES, "{bytes}");
        assert!(lines.len() > 1000);
        assert!(lines.iter().all(|l| l.text == linha), "linha cortada no meio");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
