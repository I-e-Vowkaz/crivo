# Timer das 10h (systemd do usuário)

Atualiza a lista de PRs todo dia às 10h, mesmo com o app fechado (o PC precisa estar ligado e com você logado; se estiver desligado às 10h, roda ao logar). Só busca os PRs: não dispara review.

Instalar (depois de instalar o pacote `.rpm`):

    mkdir -p ~/.config/systemd/user
    cp pr-review-sync.service pr-review-sync.timer ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable --now pr-review-sync.timer

Conferir e rodar à mão:

    systemctl --user list-timers pr-review-sync.timer
    systemctl --user start pr-review-sync.service
    journalctl --user -u pr-review-sync.service -n 20

Remover:

    systemctl --user disable --now pr-review-sync.timer
    rm ~/.config/systemd/user/pr-review-sync.{service,timer}
