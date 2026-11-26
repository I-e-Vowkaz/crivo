#!/usr/bin/env bash
# Copia do ~/.claude para claude/user/ a skill review-branch e os agentes branch-reviewer e
# committer. As cópias vão no pacote (.rpm e .deb) para quem não tem esses arquivos; quem tem
# continua usando os do próprio ~/.claude. Só lê o ~/.claude: nada é gravado lá.
#
#   packaging/sync-user-claude.sh
#
# Rodar depois de mexer na skill ou nos agentes, antes de gerar os pacotes.
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
src=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
dst=$repo/claude/user

for f in skills/review-branch/SKILL.md agents/branch-reviewer.md agents/committer.md; do
  install -D -m 644 "$src/$f" "$dst/$f"
done
git -C "$repo" status --short -- claude/user
