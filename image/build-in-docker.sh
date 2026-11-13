#!/usr/bin/env bash
# Build the AgentOS image inside Docker, so the host only needs Docker
# (Linux x86-64 host recommended; on other hosts Docker emulates amd64 and the
# build is much slower).
#
#   image/build-in-docker.sh                 # Debian 13
#   DISTRO=ubuntu image/build-in-docker.sh   # Ubuntu 24.04
#   COMPRESS=1 image/build-in-docker.sh      # also write .img.xz
#   BUNDLE_ONLY=1 image/build-in-docker.sh   # only the agentos-update bundle
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DISTRO="${DISTRO:-debian}"
case "$DISTRO" in
  debian) BASE=debian:trixie ;;
  ubuntu) BASE=ubuntu:24.04 ;;
  *) echo "不支持的 DISTRO=$DISTRO" >&2; exit 1 ;;
esac

# The programs that go into the image and the update bundle, as
# "variable:package:binary". Give every variable to use prebuilt binaries.
PROGRAMS=(
  AGENTOSD_BIN:agentosd:agentosd
  AGENTOS_SHELL_BIN:agentos-shell:agentos-shell
  AGENTOS_TASKD_BIN:agentos-taskd:agentos-taskd
  AGENTOS_CLI_BIN:agentos-cli:agentos
  AGENTOS_WEB_SHELL_BIN:agentos-web-shell:agentos-web-shell
)
prebuilt=1
for p in "${PROGRAMS[@]}"; do var=${p%%:*}; [[ -n "${!var:-}" ]] || prebuilt=0; done

if [[ $prebuilt == 0 ]]; then
  packages=()
  for p in "${PROGRAMS[@]}"; do IFS=: read -r _ pkg _ <<<"$p"; packages+=(-p "$pkg"); done
  # Built against Debian 12's glibc and WebKitGTK, so the binaries run on both
  # bases (newer sonames-compatible libraries there).
  docker run --rm --platform linux/amd64 -v "$REPO:/src" -w /src \
    -e CARGO_TARGET_DIR=/src/target/image \
    rust:1-bookworm bash -euc '
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends libwebkit2gtk-4.1-dev libgtk-3-dev >/dev/null
      cargo build --release --locked "$@"' _ "${packages[@]}"
  for p in "${PROGRAMS[@]}"; do
    IFS=: read -r var _ bin <<<"$p"
    printf -v "$var" '%s' "target/image/release/$bin"
  done
fi

# Each binary as a path on the host, and as the same path inside /src.
host_bins=() container_bins=()
for p in "${PROGRAMS[@]}"; do
  var=${p%%:*}
  rel=${!var#"$REPO"/}
  host_bins+=("$var=$REPO/$rel")
  container_bins+=(-e "$var=/src/$rel")
done

if [[ "${BUNDLE_ONLY:-0}" == 1 ]]; then
  env "${host_bins[@]}" "$REPO/image/make-bundle.sh"
  exit 0
fi

docker run --rm --privileged --platform linux/amd64 -v "$REPO:/src" -w /src \
  -e DISTRO -e MIRROR -e EXTRA_MB -e COMPRESS -e AGENTOS_RELEASE "${container_bins[@]}" \
  "$BASE" bash -euc '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      debootstrap fdisk dosfstools mtools e2fsprogs xz-utils git ca-certificates >/dev/null
    git config --global --add safe.directory /src
    image/build.sh'
