# Factory de provedores — plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** tirar a escolha de provedor dos chamadores e pô-la num módulo `hosting/` (enum por provedor + factory), com o Bitbucket só de leitura e o GitHub com as mesmas funções.

**Architecture:** `hosting::Kind` e `hosting::RepoRef` resolvem tudo que não usa rede (caminho do clone, rótulo, filtro, refs do git); `hosting::Provider` é o cliente autenticado criado por `Provider::connect` e concentra sync, comentários, push, contexto extra e publicação. Os chamadores (`commands`, `fix`, `review`, `diff`, `publish`, `cli`) só falam com esses tipos.

**Tech Stack:** Rust (Tauri 2, rusqlite, reqwest, tokio), Vue 3 + TypeScript.

**Spec:** `docs/superpowers/specs/2026-11-26-hosting-factory-design.md`

## Global Constraints

- Sem migration nova; `repositories.provider` (0013) guarda `'bitbucket'` ou `'github'`.
- Nenhuma comparação de texto de provedor fora de `src-tauri/src/hosting/` e `src-tauri/src/github.rs` (testes à parte).
- Bitbucket: só escopos `read:*`; nada é escrito no Bitbucket.
- O JSON para a UI continua com `provider: "bitbucket" | "github"`.
- Commits: `<branch> <type>: <descrição>`, na branch `feat/hosting-factory`, sem linha de atribuição.
- Pronto = `cargo test` e `npm run build` sem erro nem aviso.

## Review Focus

- PR de fork no GitHub: o push vai para `source_repo` (o fork), não para `owner/slug`; `Provider::push` recebe o `source_repo`.
- Repositório GitHub com o mesmo nome de um do Bitbucket: `RepoRef::matches` e `clone_path` não podem confundir `acme/app` (GitHub) com `app` (Bitbucket).
- Provedor sem configuração: `sync_all` só conecta os provedores configurados e não falha por causa do outro.
- PR que eu já aprovei no GitHub sai de `requested_reviewers`: continua listado (busca `reviewed-by:@me`).
- Publicar num PR do Bitbucket por chamada direta ao comando (sem a UI): recusado antes de qualquer requisição.

---

### Task 1: `hosting` com `Kind`, `RepoRef` e os tipos comuns

**Files:**
- Create: `src-tauri/src/hosting/mod.rs`
- Modify: `src-tauri/src/lib.rs` (`mod hosting;`), `src-tauri/src/bitbucket/sync.rs`, `src-tauri/src/bitbucket/comments.rs`, `src-tauri/src/github.rs`, `src-tauri/src/commands.rs`, `src-tauri/src/fix/mod.rs`

**Interfaces:**
- Produces:
  - `pub enum Kind { Bitbucket, Github }` (Serialize como `"bitbucket"`/`"github"`), `Kind::parse(&str) -> Option<Kind>`, `Kind::as_str(self) -> &'static str`, `Kind::can_write(self) -> bool`.
  - `pub struct RepoRef { pub kind: Kind, pub owner: String, pub slug: String }` com `RepoRef::new(provider: &str, owner, slug) -> AppResult<RepoRef>`, `clone_path(&self, root: &Path) -> PathBuf`, `label(&self, number: i64) -> String`, `matches(&self, filter: &str) -> bool`, `head_ref(&self, number: i64, src_branch: &str) -> String`, `async fetch_tips(&self, repo: &Path, number: i64, src: &str, dest: &str) -> AppResult<(String, String)>`, `async checkout(&self, repo: &Path, number: i64, branch: &str) -> AppResult<(String, String)>` (branch local, head).
  - `hosting::{RepoSync, SyncResult, Thread, Reply}` (os structs de hoje, movidos; `bitbucket::sync` e `bitbucket::comments` reexportam).

- [ ] **Step 1: testes em `hosting/mod.rs`**

```rust
#[test] fn kind_ida_e_volta() { assert_eq!(Kind::parse("github"), Some(Kind::Github)); assert_eq!(Kind::Bitbucket.as_str(), "bitbucket"); assert_eq!(Kind::parse("gitlab"), None); assert!(!Kind::Bitbucket.can_write() && Kind::Github.can_write()); }
#[test] fn rotulo_e_filtro() {
    let bb = RepoRef::new("bitbucket", "acme", "app").unwrap(); let gh = RepoRef::new("github", "acme", "app").unwrap();
    assert_eq!(bb.label(7), "app#7"); assert_eq!(gh.label(7), "acme/app#7");
    assert!(bb.matches("app") && !bb.matches("acme/app"));
    assert!(gh.matches("app") && gh.matches("acme/app") && !gh.matches("outro/app"));
}
#[test] fn head_ref_por_provedor() { assert_eq!(RepoRef::new("bitbucket","a","r").unwrap().head_ref(3,"PROJ-1"), "origin/PROJ-1"); assert_eq!(RepoRef::new("github","a","r").unwrap().head_ref(3,"PROJ-1"), "origin/pr/3"); }
#[test] fn clone_do_github_prefere_owner_repo() { /* tmp com root/acme/app/.git → root/acme/app; sem ele → root/app; Bitbucket sempre root/app */ }
#[test] fn provedor_desconhecido_e_erro() { assert!(RepoRef::new("gitlab","a","r").is_err()); }
```

- [ ] **Step 2:** `cargo test hosting` falha (módulo inexistente).
- [ ] **Step 3:** criar o módulo; `fetch_tips` chama `review::fresh_tips` / `review::github_tips`, `checkout` chama `git::checkout_synced` (devolve `(branch.to_owned(), head)`) / `git::checkout_github`; `clone_path` do GitHub reusa a regra de `github::clone_path` (mover a função para cá e apagar a de `github.rs`). Mover os quatro structs e trocar os caminhos `bitbucket::sync::RepoSync` etc. pelos de `hosting`.
- [ ] **Step 4:** `cargo test` passa (175 + os novos).
- [ ] **Step 5:** commit `feat/hosting-factory refactor: hosting com Kind, RepoRef e tipos comuns`.

### Task 2: chamadores sem rede usam `RepoRef`

**Files:**
- Modify: `src-tauri/src/review/mod.rs` (struct do PR: `provider`/`owner` viram `repo: RepoRef`; linhas ~482, ~755), `src-tauri/src/review/chat.rs` (~250), `src-tauri/src/diff.rs` (~59–80), `src-tauri/src/commands.rs` (`checkout_pr` ~746, `pr_list` ~419–441), `src-tauri/src/fix/mod.rs` (`Target`, `repo_of_target` ~280, `prepare_pr` ~845), `src-tauri/src/cli/mod.rs` (~97–107), `src-tauri/src/cli/comments.rs` (~153–179)

**Interfaces:**
- Consumes: Task 1.
- Produces: `fix::Target.repo: Option<RepoRef>` (`None` na branch local, no lugar de `provider: "local"`); `review` usa `pr.repo`. O `provider` serializado para a UI sai de `repo.kind`.

- [ ] **Step 1:** nos filtros SQL da CLI, trocar `(r.provider = 'github' AND r.workspace || '/' || r.slug = ?2)` por filtrar em Rust com `RepoRef::matches` depois da consulta (a consulta traz todos os PRs abertos da branch). Teste em `cli/comments.rs`: com PR do Bitbucket `app` e do GitHub `acme/app` na mesma branch, `--repo acme/app` acha só o do GitHub e `--repo app` acha os dois (e então pede desambiguação, como hoje com front/back).
- [ ] **Step 2:** substituir cada `== "github"` desses arquivos pelo método de `RepoRef` correspondente (`clone_path`, `label`, `head_ref`, `fetch_tips`, `checkout`).
- [ ] **Step 3:** `grep -rnE '"github"|provider ==' src-tauri/src --include=*.rs | grep -vE 'hosting/|github.rs|publish.rs|commands.rs.*(sync|github_)'` não acha nada nos arquivos desta task.
- [ ] **Step 4:** `cargo test` e `npm run build` passam.
- [ ] **Step 5:** commit `feat/hosting-factory refactor: review, diff, fix e cli resolvem o repositório pelo RepoRef`.

### Task 3: `Provider` (factory) com sync, comentários, push e contexto

**Files:**
- Create: `src-tauri/src/hosting/provider.rs`
- Modify: `src-tauri/src/commands.rs` (`sync_all`, `sync_bitbucket` sai), `src-tauri/src/fix/mod.rs` (`add_threads` ~861, push ~1754, credenciais ~807), `src-tauri/src/review/mod.rs` (issues ~817), `src-tauri/src/github.rs` (`sync_repo` recebe `RepoRef`/root)

**Interfaces:**
- Consumes: Task 1.
- Produces: `pub enum Provider { Bitbucket(BitbucketSession), Github(GithubSession) }` com
  - `async Provider::connect(kind: Kind, db: &Mutex<Connection>, store: &dyn SecretStore) -> AppResult<Provider>`;
  - `Provider::configured(conn: &Connection) -> AppResult<Vec<Kind>>` (Bitbucket com `bitbucket.workspace` preenchido; GitHub com `github.repos` não vazio);
  - `async sync(&self, db: &Mutex<Connection>) -> AppResult<Vec<RepoSync>>` (grava `bitbucket.me`/`github.me`, `sync_runs`, PRs, e faz o `git fetch --prune` de cada clone);
  - `fn user(&self) -> String`; `fn requests(&self) -> u32`;
  - `async open_threads(&self, repo: &RepoRef, number: i64) -> AppResult<Vec<Thread>>`;
  - `async push(&self, wt: &Path, repo: &RepoRef, source_repo: &str, branch: &str) -> AppResult<()>`;
  - `async extra_context(&self, repo: &RepoRef, title: &str, body: &str) -> String` (Bitbucket: vazio).

- [ ] **Step 1: testes** — `configured`: banco só com `bitbucket.workspace` → `[Bitbucket]`; só com `github.repos = ["acme/app"]` → `[Github]`; vazio → `[]`. `sync_all` com nenhum configurado devolve o erro de hoje ("configure ao menos um repositório Bitbucket ou GitHub"). Os testes de sync do Bitbucket com o servidor falso (`bitbucket::client` test server) passam a ir por `Provider::Bitbucket`.
- [ ] **Step 2:** `cargo test` falha.
- [ ] **Step 3:** implementar movendo o código de `commands::sync_bitbucket`, do trecho GitHub de `sync_all`, de `fix::add_threads`, do push em `fix` e das issues em `review`. `sync_all` = para cada `Kind` de `configured`, `connect` + `sync`, erro de um provedor vira `RepoSync { slug: "<Provedor>", error }`.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** commit `feat/hosting-factory refactor: Provider como factory dos clientes do Bitbucket e do GitHub`.

### Task 4: escrita só no GitHub

**Files:**
- Modify: `src-tauri/src/bitbucket/scopes.rs`, `src-tauri/src/publish.rs`, `src-tauri/src/hosting/provider.rs`, `src/views/ReviewPanel.vue`, `README.md`

**Interfaces:**
- Produces: `async Provider::publish(&self, repo: &RepoRef, number: i64, sha: &str, body: &str, inline: &[Value], request_changes: bool) -> AppResult<Vec<Published>>`; no Bitbucket devolve `AppError::Invalid("o Crivo não escreve no Bitbucket")` sem requisição.

- [ ] **Step 1: testes** — `scopes`: `check(&["read:pullrequest:bitbucket", "write:pullrequest:bitbucket", ...])` → `!ok()` e `forbidden == ["write:pullrequest:bitbucket"]` (volta o `escopo_de_escrita_e_recusado`); `publish_findings` num run de PR do Bitbucket devolve o erro acima (banco em memória, sem servidor).
- [ ] **Step 2:** falham.
- [ ] **Step 3:** reverter a regra do PR #1 em `scopes.rs` (só `read:`); `publish.rs` chama `Provider::publish` e perde o caminho `api.bitbucket.org`; na UI, a área de publicar e a caixa "Publicar" de cada achado ficam sob `pr.provider === 'github'`; README: Bitbucket só leitura, GitHub com publicação explícita.
- [ ] **Step 4:** `cargo test` e `npm run build` passam.
- [ ] **Step 5:** commit `feat/hosting-factory fix: Bitbucket volta a ser só leitura e publicar fica só no GitHub`.

### Task 5: sync do GitHub com menos chamadas

**Files:**
- Modify: `src-tauri/src/github.rs` (`sync_repo`, `Api`)

**Interfaces:**
- Produces: `async Api::reviewed_by_me(&self, owner: &str, repo: &str) -> AppResult<HashSet<i64>>` (GET `/search/issues?q=is:pr+is:open+repo:<owner>/<repo>+reviewed-by:@me&per_page=100`, números de `items[].number`).

- [ ] **Step 1: teste** — com o servidor falso do GitHub (ou um `Api` com base configurável, como `bitbucket::Client::with_base`): 3 PRs abertos, 1 meu, 1 com review pedido a mim, 1 que eu revisei (só aparece na busca) e 1 de outra pessoa sem relação → grava 3 e faz exatamente 1 (lista) + 1 (busca) + 3 (reviews) requisições.
- [ ] **Step 2:** falha.
- [ ] **Step 3:** selecionar pelos campos da lista + o conjunto da busca antes de ler reviews; `Api::with_base` para o teste.
- [ ] **Step 4:** `cargo test` passa.
- [ ] **Step 5:** commit `feat/hosting-factory perf: sync do GitHub só lê os reviews dos PRs relevantes`.

### Task 6: fechamento

- [ ] **Step 1:** `grep -rnE '== "github"|"github" =>|provider ==' src-tauri/src --include=*.rs` só acha `hosting/` e `github.rs`.
- [ ] **Step 2:** `cargo test` e `npm run build` sem erro nem aviso.
- [ ] **Step 3:** README: seção curta sobre provedores (configuração do GitHub por token ou `gh`) se ainda não houver.
- [ ] **Step 4:** commit `feat/hosting-factory docs: provedores no README`.
