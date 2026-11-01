#!/usr/bin/env bash
# Build the AgentOS image inside Docker, so the host only needs Docker
# (Linux x86-64 host recommended; on other hosts Docker emulates amd64 and the
# build is much slower).
#
#   image/build-in-docker.sh                 # Debian 13
#   DISTRO=ubuntu image/build-in-docker.sh   # Ubuntu 24.04
#   COMPRESS=1 image/build-in-docker.sh      # also write .img.xz
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DISTRO="${DISTRO:-debian}"
case "$DISTRO" in
  debian) BASE=debian:trixie ;;
  ubuntu) BASE=ubuntu:24.04 ;;
  *) echo "不支持的 DISTRO=$DISTRO" >&2; exit 1 ;;
esac

if [[ -z "${AGENTOSD_BIN:-}" || -z "${AGENTOS_SHELL_BIN:-}" ]]; then
  # Built against Debian 12's glibc and WebKitGTK, so the binaries run on both
  # bases (newer sonames-compatible libraries there).
  docker run --rm --platform linux/amd64 -v "$REPO:/src" -w /src \
    -e CARGO_TARGET_DIR=/src/target/image \
    rust:1-bookworm bash -euc '
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends libwebkit2gtk-4.1-dev libgtk-3-dev >/dev/null
      cargo build --release --locked -p agentosd -p agentos-shell'
  AGENTOSD_BIN=target/image/release/agentosd
  AGENTOS_SHELL_BIN=target/image/release/agentos-shell
fi

docker run --rm --privileged --platform linux/amd64 -v "$REPO:/src" -w /src \
  -e DISTRO -e MIRROR -e EXTRA_MB -e COMPRESS -e AGENTOSD_BIN="/src/${AGENTOSD_BIN#"$REPO"/}" \
  -e AGENTOS_SHELL_BIN="/src/${AGENTOS_SHELL_BIN#"$REPO"/}" \
  "$BASE" bash -euc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      debootstrap fdisk dosfstools mtools e2fsprogs xz-utils git ca-certificates >/dev/null
    git config --global --add safe.directory /src
    image/build.sh'
