#!/usr/bin/env bash
# Gera o .deb num container Ubuntu 22.04 (podman). O binário compilado no Fedora
# exige a glibc do Fedora e não abre no Ubuntu/Debian; o daqui usa a 2.35.
#
#   packaging/deb/build.sh
#
# Saída em src-tauri/target/deb-jammy/. Só entram os arquivos versionados e os
# novos que não estão no .gitignore (sem .env, node_modules nem target).
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$repo/src-tauri/target/deb-jammy"
image=pr-review-orchestrator-deb:jammy

podman build -t "$image" -f "$repo/packaging/deb/Containerfile" "$repo/packaging/deb"
mkdir -p "$out"
rm -f "$out"/*.deb

# Cache do registry do cargo e do target entre builds (volumes do podman).
git -C "$repo" ls-files -z -co --exclude-standard \
  | tar -C "$repo" --null -T - -cf - \
  | podman run --rm -i \
      -v pr-review-deb-cargo:/usr/local/cargo/registry \
      -v pr-review-deb-target:/target \
      -v "$out":/out:Z \
      -e CARGO_TARGET_DIR=/target \
      "$image" bash -euo pipefail -c '
        tar -xf - -C /build
        npm ci --no-audit --no-fund
        npm run tauri build -- --bundles deb
        cp /target/release/bundle/deb/*.deb /out/
      '

ls -lh "$out"/*.deb
