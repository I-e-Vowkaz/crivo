# Factory de provedores (Bitbucket e GitHub)

Data: 2026-11-26. Repositório principal: `~/Projects/crivo`.

## Por quê

O PR #1 trouxe o GitHub, mas a escolha do provedor ficou espalhada em 15 comparações `provider == "github"` por 8 arquivos (`commands`, `fix`, `review`, `review/chat`, `diff`, `publish`, `cli/mod`, `cli/comments`), e os tipos do Bitbucket (`bitbucket::sync::RepoSync`, `SyncResult`, `bitbucket::comments::Thread`) viraram tipos genéricos por acaso. O pedido é que a consulta aos provedores funcione como uma factory: um ponto único que devolve o provedor certo, com as mesmas funções para os dois.

## Objetivos

- Um módulo `hosting/` com os tipos comuns e um `enum` por provedor; o construtor é a factory.
- Nenhum `provider == "github"` (nem comparação de texto de provedor) fora de `hosting/`.
- O GitHub com as mesmas funções do Bitbucket: lista (para revisar e meus), revisores, aprovação, pedido de mudança, contagem de comentários, comentários abertos, checkout, review, diff, chat, correção com push, CLI (`-p`, `prs`, `comments`).
- **Escrita só no GitHub** (decisão do usuário, 2026-11-26): o token do Bitbucket volta a ser só de leitura (qualquer escopo que não seja `read:` é recusado) e o Crivo não publica nada no Bitbucket. No GitHub, publicar achados e pedir mudanças continua.
- Busca do GitHub com menos chamadas: hoje é uma chamada de reviews por PR aberto do repositório.

## Fora do escopo

- GitLab ou outro provedor (o desenho deixa o caminho aberto: uma variante a mais).
- Mudar o modelo de dados além do que já veio na migration 0013 (`repositories.provider`). Sem migration nova.
- Mudar o pipeline de review, os agentes ou a UI além do botão de publicar.

## Abordagens

- **A. Enum por provedor (escolhida).** `enum Kind` e `enum Provider { Bitbucket(..), Github(..) }`, com os métodos num lugar só. O compilador obriga cada método a tratar cada provedor; sem `async-trait`.
- B. Trait com `Box<dyn Provider>`: o padrão clássico, mas métodos assíncronos em trait dinâmica pedem `async-trait` ou futures em caixa. Só valeria para plugins de terceiros.
- C. Funções livres de despacho por texto: menos trabalho, mas os tipos continuam os do Bitbucket e a escolha continua por string.

O usuário escolheu A.

## Desenho

### `hosting::Kind`

`Bitbucket | Github`, com `as_str()`/`parse()` (o texto gravado em `repositories.provider`) e `can_write()` (`false` no Bitbucket). Serializa como `"bitbucket"`/`"github"`, então a UI continua recebendo o mesmo `provider`.

### `hosting::RepoRef` (sem rede, sem token)

Identifica um repositório: `kind`, `owner` (workspace no Bitbucket) e `slug`. Concentra o que hoje depende do provedor sem falar com a API:

- `clone_path(root)`: Bitbucket `root/slug`; GitHub `root/owner/slug` se existir, senão `root/slug` (o `github::clone_path` de hoje);
- `label(number)`: `slug#N` ou `owner/slug#N`;
- `matches(filter)`: o `--repo` da CLI (`slug`, ou `owner/slug` no GitHub);
- `fetch_tips(repo, number, src, dest)`: `review::fresh_tips` ou `review::github_tips`;
- `checkout(repo, number, branch)`: `git::checkout_synced` ou `git::checkout_github`;
- `head_ref(number, src)`: `origin/<src>` ou `origin/pr/<N>` (aba Diff);
- `RepoRef::load(conn, repository_id)` e um construtor a partir das colunas `provider, workspace, slug`.

### `hosting::Provider` (a factory)

`Provider::connect(kind, db, store)` lê as configurações e o segredo do provedor e devolve o cliente autenticado. Métodos:

- `sync(db, root) -> Vec<RepoSync>`: busca os PRs dos repositórios configurados do provedor, grava, e faz o `git fetch --prune` de cada clone (o que hoje está em `commands::sync_bitbucket` e no trecho GitHub de `sync_all`);
- `me()`: identidade gravada (`bitbucket.me` / `github.me`), usada no "já aprovei";
- `open_threads(repo, number)`: comentários abertos de outras pessoas;
- `push(wt, repo, source_repo, branch)`: `git push origin HEAD:refs/heads/<branch>` no Bitbucket; `github::push` com o token no GitHub;
- `extra_context(pr)`: as issues citadas no PR (só GitHub; Bitbucket devolve vazio);
- `publish(...)`: GitHub publica; Bitbucket devolve erro "o Crivo não escreve no Bitbucket";
- `requests()`.

`commands::sync_all` vira: para cada provedor configurado, `Provider::connect` e `sync`, juntando os `RepoSync`. A falha de um provedor vira linha de erro no resultado, como hoje.

### Tipos comuns

`hosting::{RepoSync, SyncResult, Thread, Reply}` saem de `bitbucket::sync` e `bitbucket::comments` (lá ficam `pub use` para os testes antigos, ou os testes passam a usar os de `hosting`).

### Escrita no Bitbucket

- `bitbucket/scopes.rs` volta à regra anterior ao PR #1: só `read:*`; o teste `escrita_em_pr_e_aceita` volta a ser `escopo_de_escrita_e_recusado`.
- O caminho Bitbucket de `publish.rs` sai (`Provider::publish` recusa antes de qualquer chamada).
- UI: os botões de publicar e a caixa "Publicar" de cada achado só aparecem em PR do GitHub (`pr.provider === 'github'`).
- README: a seção de segurança diz que o Bitbucket é só leitura e que a escrita no GitHub é explícita.

### GitHub com menos chamadas

`sync` do GitHub: lista os PRs abertos (paginado, como hoje), seleciona os meus e os que me pedem review direto da lista, e uma busca `is:pr is:open repo:<o>/<r> reviewed-by:@me` traz os que eu já revisei (e saíram de `requested_reviewers`). Só os PRs selecionados têm os reviews lidos. De 1 + N chamadas para 2 + (relevantes).

## Para quem for construir

- Os testes atuais (175) continuam passando; acrescentar testes de `RepoRef` (caminho, rótulo, filtro, `head_ref`), de `Kind::parse`, do Bitbucket recusando publicar e do escopo de escrita recusado.
- Critério de pronto: `grep -rn '"github"' src --include=*.rs` só acha `hosting/` e `github.rs` (e os testes); `cargo test` e `npm run build` sem erro nem aviso.
- A ordem é livre; a sugestão é tipos e `RepoRef` primeiro (sem rede), depois `Provider` e o sync, depois os chamadores.
