# Crivo

O Crivo revisa PRs do Bitbucket e do GitHub.com com agentes do Claude Code e usa cards do Jira e issues GitHub referenciadas como spec. O Sonnet analisa o código, o Haiku aponta complexidade desnecessária, um juiz dá nota a cada melhoria e o Opus confere os bugs encontrados. O relatório sai em Rust, de forma determinística, e a revisão nunca grava no repositório. Também ajuda a resolver os comentários dos seus PRs, revisa branches locais e sobe os serviços do dia a dia. Funciona pela janela ou pelo terminal.

App desktop em Tauri 2, com backend em Rust e interface em Vue 3. A interface e os textos dos agentes estão em português.

## O que faz

- Lista os PRs abertos em que você está envolvido: os que você precisa revisar e os que você abriu, com quem pediu mudanças e quem aprovou.
- No GitHub, publica os achados que você selecionar como um review na PR, com comentário ou pedido de mudanças. No Bitbucket o Crivo só lê: nada é publicado lá.
- Revisa um PR sob demanda, num worktree separado no commit exato do PR. Seu checkout nunca é tocado.
  1. O Sonnet roda a skill `review-branch`, que lança o agente `branch-reviewer` e classifica cada achado por severidade.
  2. O app confere cada `arquivo:linha` no commit revisado.
  3. O Haiku procura complexidade desnecessária, consultas repetidas e testes que falham ou são instáveis, respeitando as convenções do repositório (`CLAUDE.md`, `AGENTS.md`, `.cursor/rules`, `.claude/rules`).
  4. Um juiz (Sonnet) dá nota de 0 a 10 a cada melhoria e descarta as que mudam o comportamento.
  5. O Opus confere todos os bugs válidos e marca cada um como confirmado, severidade ajustada, falso positivo ou evidência insuficiente.
  6. Opcionalmente, o Haiku testa o PR no navegador pelo Claude in Chrome, só em hosts permitidos e com usuários de desenvolvimento.
- Chat sobre o review, com memória e com acesso de leitura ao projeto do outro lado da task (front ↔ back).
- Meus PRs: busca os comentários abertos, analisa cada um com o Sonnet (procede, não procede ou resposta) e aplica só as sugestões que você aprovar, num worktree próprio. Commit pelo agente `committer` e push sem force.
- Minhas branches: o mesmo review e a mesma correção para branches locais, ainda sem PR.
- Gatilhos: sobe e para os serviços do dia a dia (scripts npm ou comandos shell), como as run configurations de uma IDE.
- Uso: custo médio por review e por agente.

## Requisitos

- Linux. O código usa `/proc`, `~/.local/share` e o systemd do usuário; não há build para macOS nem Windows.
- [Claude Code](https://code.claude.com/docs/en/overview) instalado e autenticado (`claude` no PATH). Para uso com token de execução, inicie o Crivo com `CLAUDE_CODE_OAUTH_TOKEN` no ambiente; o app repassa somente essa credencial ao processo do Claude.
- Um API token do Bitbucket só de leitura, com `read:pullrequest`, `read:repository` e `read:user`. O app recusa token com qualquer escopo de escrita.
- Para GitHub.com, um token com acesso aos repositórios (Pull requests: escrita para publicar; Contents: escrita para push) ou o GitHub CLI `gh` autenticado.
- Jira é opcional. Para ler seus cards, configure o site do Jira no app e o MCP do Atlassian no `~/.claude.json` com o nome `atlassian`. Sem Jira, o review usa o título e a descrição da PR e, no GitHub, as issues referenciadas.
- Os repositórios clonados localmente em `<pasta>/<repo>`; para GitHub, também é aceito `<pasta>/<owner>/<repo>`. O `origin` do clone GitHub deve apontar para o repositório da PR.
- Para `npm run tauri build` no sistema: Rust (stable), Node 22 e as dependências do Tauri 2 para Linux (WebKitGTK 4.1). Para gerar o `.deb` sem instalar Rust ou bibliotecas de desenvolvimento no sistema, use Docker ou Podman.

## Compilar e instalar

```bash
npm install
npm run tauri build      # gera o .rpm em src-tauri/target/release/bundle/rpm/
npm run build:deb         # gera o .deb num container Ubuntu 22.04 (Docker ou Podman)
```

Para desenvolver: `npm run tauri dev`. Testes: `cd src-tauri && cargo test` e `npm run build`.

## Configurar

Abra o app, vá em Configurações e preencha:

- email da conta Atlassian, workspace do Bitbucket e os repositórios monitorados, se usar Bitbucket;
- os repositórios GitHub no formato `owner/repo` e autenticação por token ou `gh`, se usar GitHub;
- a pasta dos clones locais;
- opcionalmente, o site do Jira (ex.: `empresa.atlassian.net`) para ler cards pelo MCP;
- o API token do Bitbucket, que vai para o cofre do sistema (Secret Service) e nunca volta para a tela;
- opcionalmente, os ambientes de teste do navegador (URL local, usuário e senha de desenvolvimento, hosts permitidos).

Para buscar um card do Jira, inclua a chave (`PROJ-123`) na branch ou no título da PR. No GitHub, issues referenciadas explicitamente no título ou na descrição da PR (como `#123` ou `owner/repo#123`) também entram na spec do review.

A skill `review-branch` e os agentes `branch-reviewer` e `committer` vão no pacote, em `claude/user/`. Se você tiver versões próprias em `~/.claude/skills/review-branch` e `~/.claude/agents/`, o app usa as suas.

## Pelo terminal

```bash
pr-review-orchestrator -p PROJ-123            # revisa o PR da branch e imprime o relatório
pr-review-orchestrator prs --sync             # painel do dia: o que revisar e como estão os seus PRs
pr-review-orchestrator comments PROJ-123      # comentários abertos dos seus PRs da branch
pr-review-orchestrator --sync                 # só atualiza a lista de PRs
```

Cada comando tem `--help`. Em `packaging/claude-skill/` há skills do Claude Code que chamam esses comandos, e em `packaging/systemd/` um timer que atualiza a lista todo dia.

## Segurança

- O review só lê. O hook `claude/hooks/guard-git.py` bloqueia commit, push, merge, rebase, tag e checkout ou reset de branch protegida.
- O teste no navegador só navega em `localhost` e nos hosts configurados (`claude/hooks/guard-browser.py`), sem upload de arquivo.
- Só o backend usa os tokens; os agentes recebem conteúdo, nunca credenciais. Os logs passam por um filtro que mascara segredos.
- A correção de comentários é a única parte que grava código: só no worktree dela, só o que você aprovou, sem force e nunca em branch protegida.
- Escrita no provedor só no GitHub e só quando você pede (publicar achados, pedir mudanças). O Bitbucket é só leitura: o token não pode ter escopo de escrita e o app não tem caminho para publicar lá.

## Custo

Medido em uso real: um review típico custa entre US$ 0,25 e US$ 0,35 (Sonnet), mais cerca de US$ 0,32 quando o Opus entra. O Haiku fica em torno de US$ 0,03. A aba Uso mostra os números das suas execuções.

## Apoie

Se o Crivo te ajuda, considere contribuir via Pix: `8759bf8d-e16f-4646-ab96-8807a3997b0c`
