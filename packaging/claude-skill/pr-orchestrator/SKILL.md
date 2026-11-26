---
name: pr-orchestrator
description: Revisa o PR aberto de um card rodando o PR Review Orchestrator pelo terminal (`pr-review-orchestrator -p <branch>`), com o mesmo pipeline do app (Sonnet, validação, melhorias do Haiku com juiz e auditoria do Opus). Use quando o usuário pedir para revisar um PR pelo card, ex. "revisa pr PROJ-1000", "revisa o PR do PROJ-1000 no back", "roda o orchestrator no PROJ-1000". Para revisar código local ainda não commitado, use a skill review-branch.
---

# PR Review Orchestrator pelo terminal

Esta skill não revisa o código. Ela dispara o orquestrador, espera o fim e mostra o
relatório que ele gera. O review é o mesmo do app e fica gravado no banco do app.

## 1. Montar o comando

- A branch é a chave do card, em maiúsculas: "proj-1000" vira `PROJ-1000`. O nome da branch é
  sempre igual ao card.
- Se o usuário disser "front" ou "back", acrescente `--repo acme-front` ou
  `--repo acme-back`. Sem isso, o orquestrador revisa todos os PRs abertos da
  branch (front e back juntos, quando houver os dois).
- `--sync` só se o usuário disser que o PR acabou de ser aberto ou atualizado. Sem PR no
  banco, o orquestrador já faz o sync sozinho.

```
pr-review-orchestrator -p PROJ-1000 [--repo acme-back]
```

## 2. Rodar em segundo plano

O review leva de 1 a 6 minutos, perto do limite de 10 minutos do Bash em primeiro plano.
Rode com `run_in_background: true` e espere a notificação de fim. Não fique lendo o arquivo
de saída enquanto isso.

Diga ao usuário, em uma linha, que o review começou e quanto costuma levar.

Se o comando não existir, ou se `pr-review-orchestrator --help` não mostrar a opção `-p`, o
app instalado está numa versão antiga. Use o binário de desenvolvimento, se existir:
`~/Projects/pr-review-orchestrator/src-tauri/target/debug/pr-review-orchestrator`. Sem ele,
peça ao usuário para reinstalar o app (`npm run tauri build` e `sudo dnf reinstall` do `.rpm`).

Se o comando sair com "o banco está na versão N, mais nova que este app", o app instalado é mais velho que o banco: rode de novo com o binário de `target/debug` e diga ao usuário, em uma linha, para reinstalar o `.rpm` (`npm run tauri build` e `sudo dnf reinstall` do pacote em `src-tauri/target/release/bundle/rpm/`).

Para cancelar, pare a tarefa em segundo plano. O orquestrador recebe o SIGTERM, mata os
processos do Claude e remove o worktree.

## 3. Mostrar o resultado

O stdout é o relatório em markdown: um bloco por PR, com os achados ordenados por
severidade e as melhorias pela nota do juiz. O stderr tem o andamento.

| Código de saída | Significado |
|---|---|
| 0 | todos os reviews concluíram |
| 1 | algum review falhou (o motivo está em `Erro:` no relatório ou no stderr) |
| 2 | erro de uso, ou nenhum PR aberto nem branch local com esse nome |
| 130 | cancelado |

- Comece com uma linha de resumo por PR: quantos achados, a maior severidade e quantas
  melhorias. Depois mostre o relatório como veio. Não reescreva os achados, não acrescente
  achados seus e não rode o review de novo por conta própria.
- Os achados podem ser discutidos na conversa. Para conferir um deles, leia o código no
  commit revisado; a pasta da execução (`arquivos em …`) tem o `findings.json`, o `card.md`
  e o `run.log`.
- Nada é publicado no Bitbucket. Se o usuário quiser comentar no PR, o texto de cada achado
  já está no formato de comentário.
- O teste no navegador não roda pelo terminal. Para ele, o usuário usa o app.
