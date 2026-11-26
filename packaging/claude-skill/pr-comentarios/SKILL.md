---
name: pr-comentarios
description: Lista e resolve os comentários dos PRs que o próprio usuário abriu, pelo PR Review Orchestrator no terminal (`pr-review-orchestrator comments <branch>`), com o mesmo fluxo da tela "Meus PRs" do app (análise do Sonnet, conversa, aprovação, commit e push). Use quando o usuário falar dos comentários do PR dele, ex. "comentários do meu PR PROJ-1000", "resolve os comentários do PR PROJ-1000", "o que pediram no meu PR do back", "analisa o C3 do PROJ-1000". Para revisar o PR de outra pessoa, use a skill pr-orchestrator.
---

# Comentários dos meus PRs pelo terminal

Esta skill não analisa os comentários nem propõe código. Ela roda o orquestrador, que usa o
agente `comment-fixer` do app, e mostra o que ele devolve. Tudo fica no banco do app: o que se
faz aqui aparece na tela Meus PRs, e vice-versa.

## 1. Montar o comando

- A branch é a chave do card, em maiúsculas: "proj-1000" vira `PROJ-1000`.
- "front" ou "back" → `--repo acme-front` ou `--repo acme-back`. Se a branch
  tiver PR do usuário nos dois, as ações (tudo menos listar) exigem o `--repo`; o comando sai
  com 2 e diz isso.
- `--sync` só se o usuário disser que o PR acabou de ser aberto.
- Os comentários se chamam `C1`, `C2`… (por PR), como no app.

| Pedido | Comando | Como rodar |
|---|---|---|
| ver os comentários | `pr-review-orchestrator comments PROJ-1000` | primeiro plano (segundos) |
| analisar | `… comments PROJ-1000 analyze C1 C3` ou `analyze all` | `run_in_background: true` |
| conversar sobre um | `… comments PROJ-1000 ask C3 <mensagem>` | `run_in_background: true` |
| aprovar a sugestão | `… comments PROJ-1000 approve C3` | primeiro plano |
| commit e push | `… comments PROJ-1000 commit` | `run_in_background: true` |
| descartar a correção | `… comments PROJ-1000 discard` | primeiro plano |

`analyze` e `ask` levam de 1 a alguns minutos por comentário; `commit` roda o committer e o
push. Em segundo plano, espere a notificação de fim e não fique lendo o arquivo de saída.

Se o comando não existir, ou `pr-review-orchestrator comments --help` falhar, o app instalado é
antigo. Use `~/Projects/pr-review-orchestrator/src-tauri/target/debug/pr-review-orchestrator`.
Nesse caso, peça em uma linha para o usuário **fechar o app instalado antes** e não abri-lo
enquanto o terminal roda: a versão antiga não enxerga o terminal, roda o Claude na mesma
correção e, ao abrir, marca como interrompida a correção que o terminal está processando. Isso
some quando ele reinstalar o `.rpm`.

Se o comando sair com "o banco está na versão N, mais nova que este app", o app instalado é mais velho que o banco: rode de novo com o binário de `target/debug` e diga ao usuário, em uma linha, para reinstalar o `.rpm` (`npm run tauri build` e `sudo dnf reinstall` do pacote em `src-tauri/target/release/bundle/rpm/`).

## 2. Regras

- **Listar é o padrão.** "Comentários do meu PR" é só `comments <branch>`. Não analise nada sem
  o usuário pedir; os resumos de bots (Qodo, coderabbit) costumam ficar sem análise.
- **Mostre a saída como veio**, depois de uma linha de resumo por PR (quantos comentários,
  quantos não analisados, quantos com sugestão esperando aprovação). Não reescreva a análise, não
  acrescente sugestão sua e não proponha código: quem propõe é o `comment-fixer`.
- **`ask` leva o texto do usuário sem reescrever.** Se o usuário discordar de uma análise ou
  quiser outra sugestão, a mensagem dele vai pelo `ask`; a resposta volta com a sugestão nova.
- **`approve` só com pedido explícito para aqueles comentários** ("aprova o C3", "pode aplicar o
  C2 e o C4"). Concordar com uma explicação ("faz sentido") não é aprovação. A aprovação grava no
  worktree da correção, não no checkout do usuário.
- **`commit` só com pedido explícito.** Antes de rodar, diga em uma linha que vai fazer commit e
  push na branch do PR e quais comentários entram (os aplicados). É publicar no Bitbucket. O
  commit só sai com todos os comentários analisados resolvidos (aplicados, "não procede" ou
  "resposta"); os não analisados não bloqueiam.
- **`discard` só com pedido explícito.** Ele apaga as sugestões aprovadas e ainda sem commit.
- **Nada vai para o Bitbucket além do push.** A explicação de "não procede" e a de "resposta"
  são para o usuário copiar e responder no PR.

## 3. Códigos de saída

| Código | Significado |
|---|---|
| 0 | tudo certo |
| 1 | a etapa falhou (comentário com `falhou`, committer ou push com erro; o motivo está em `Erro:`, no item ou no stderr) |
| 2 | erro de uso, nenhum PR do usuário na branch, comentário inexistente, ou a correção está em outro estado (ocupada pelo app ou por outro terminal, já commitada) |
| 130 | cancelado |

Saída 2 com "o PR dessa branch é de outra pessoa" quer dizer que o pedido era de review: use a
skill pr-orchestrator.

Se o push falhar depois do commit, o commit fica no worktree da correção; rodar `commit` de novo
tenta só o push.
