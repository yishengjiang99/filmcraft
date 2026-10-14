#!/usr/bin/env bash
# Build the web (wasm) build and publish it to https://grepawk.com/filmcraft/.
# See docs/deploy-grepawk.md. Usage: scripts/deploy-grepawk.sh [--threads] [--skip-build]
set -euo pipefail
HOST="${FILMCRAFT_DEPLOY_HOST:-root@64.23.239.208}"
DIR="${FILMCRAFT_DEPLOY_DIR:-/var/www/filmcraft}"
build_args=(); skip=0
for a in "$@"; do case "$a" in --threads) build_args+=(--threads);; --skip-build) skip=1;; *) echo "unknown arg $a" >&2; exit 2;; esac; done
cd "$(dirname "$0")/.."
[[ $skip == 1 ]] || cargo xtask web "${build_args[@]}"
test -s target/web/dist/index.html && test -s target/web/dist/filmcraft_web_bg.wasm
rsync -a --delete --chmod=D755,F644 target/web/dist/ "$HOST:$DIR.new/"
ssh "$HOST" "set -e; rm -rf '$DIR.prev'; if [ -d '$DIR' ]; then mv '$DIR' '$DIR.prev'; fi; mv '$DIR.new' '$DIR'"
code=$(curl -s -o /dev/null -w '%{http_code}' https://grepawk.com/filmcraft/ || true)
echo "https://grepawk.com/filmcraft/ -> $code"; [[ "$code" == 200 ]]
