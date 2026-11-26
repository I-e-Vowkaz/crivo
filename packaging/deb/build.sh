#!/usr/bin/env bash
# Gera o .deb num container Ubuntu 22.04 (Docker ou Podman). O binário compilado no Fedora
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
if [[ -n "${CRIVO_CONTAINER_ENGINE:-}" ]]; then
  container_engine=$CRIVO_CONTAINER_ENGINE
elif command -v podman >/dev/null 2>&1; then
  container_engine=podman
elif command -v docker >/dev/null 2>&1; then
  container_engine=docker
else
  echo 'Instale Docker ou Podman para compilar o .deb.' >&2
  exit 1
fi

"$container_engine" build -t "$image" -f "$repo/packaging/deb/Containerfile" "$repo/packaging/deb"
mkdir -p "$out"
rm -f "$out"/*.deb

# Cache do registry do cargo e do target entre builds (volumes do container).
git -C "$repo" ls-files -z -co --exclude-standard \
  | tar -C "$repo" --null -T - -cf - \
  | "$container_engine" run --rm -i \
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
