---
name: pr-painel
description: Mostra o painel do dia dos PRs pelo PR Review Orchestrator no terminal (`pr-review-orchestrator prs`): os PRs para o usuário revisar, com o último review de cada um, e os PRs dele, com pedidos de mudança, aprovações, comentários e correção. Use quando o usuário perguntar algo como "o que tenho pra revisar?", "o que tenho pra revisar hoje", "como estão meus PRs?", "painel do dia", "tem PR novo pra mim?". Para revisar um PR, use a skill pr-orchestrator; para os comentários de um PR do usuário, a pr-comentarios.
---

# Painel do dia pelo terminal

Esta skill só lê. Ela mostra o resumo das listas do app e sugere o próximo passo; não revisa nem
analisa nada por conta própria.

## 1. Rodar

```
pr-review-orchestrator prs --sync
```

- Primeiro plano: o sync leva alguns segundos (Bitbucket e `git fetch` nos clones).
- `--all` só se o usuário pedir também os PRs que ele já aprovou.
- O stdout é o painel em markdown; o stderr tem o andamento do sync (não mostre).
- Se o comando não existir ou `pr-review-orchestrator prs --help` falhar, o app instalado é
  antigo: use `~/Projects/pr-review-orchestrator/src-tauri/target/debug/pr-review-orchestrator`.
- Se o comando sair com "o banco está na versão N, mais nova que este app", o app instalado é mais velho que o banco: rode de novo com o binário de `target/debug` e diga ao usuário, em uma linha, para reinstalar o `.rpm` (`npm run tauri build` e `sudo dnf reinstall` do pacote em `src-tauri/target/release/bundle/rpm/`).
- Saída 0 ok; 2 erro de uso ou banco não encontrado (o usuário precisa abrir o app uma vez).
  Falha no sync aparece como `AVISO` no stderr e o painel sai do banco: diga em uma linha que os
  dados podem estar desatualizados.

## 2. Mostrar

- Comece com uma linha de resumo: quantos PRs para revisar (quantos nunca revisados e quantos
  com review desatualizado) e quantos PRs dele (quantos com pedido de mudança e quantos com
  sugestão esperando aprovação).
- Depois mostre o painel como veio, sem reescrever.
- Termine com uma linha oferecendo o próximo passo, sem rodar nada:
  - PR nunca revisado ou com review desatualizado: "revisa o PROJ-1000" (skill pr-orchestrator);
  - PR dele com comentários ou pedido de mudança: "comentários do meu PR PROJ-1000" (skill
    pr-comentarios).
- Front e back do mesmo card aparecem como dois PRs com a mesma branch; o review e os
  comentários tratam os dois juntos.
