#!/usr/bin/env bash
# Pack the AgentOS layer into the update bundle that `agentos-update`
# installs: agentosd, agentos-shell, image/rootfs/usr and the AgentOS-owned
# systemd units and udev rules. Writes
#   $OUT/agentos-update-x86_64.tar.gz and .sha256
#
# Environment:
#   AGENTOSD_BIN       agentosd binary        (default target/release/agentosd)
#   AGENTOS_SHELL_BIN  agentos-shell binary   (default target/release/agentos-shell)
#   AGENTOS_RELEASE    release tag            (default: git describe)
#   OUT                output directory       (default out/)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
OUT="${OUT:-$REPO/out}"
BIN="${AGENTOSD_BIN:-$REPO/target/release/agentosd}"
SHELL_BIN="${AGENTOS_SHELL_BIN:-$REPO/target/release/agentos-shell}"
[[ -x "$BIN" ]] || { echo "找不到 agentosd：$BIN" >&2; exit 1; }
[[ -x "$SHELL_BIN" ]] || { echo "找不到 agentos-shell：$SHELL_BIN" >&2; exit 1; }
RELEASE="${AGENTOS_RELEASE:-$(git -C "$REPO" describe --tags --always 2>/dev/null || echo unknown)}"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -a "$HERE/rootfs/usr" "$stage/"
mkdir -p "$stage/etc/systemd/system" "$stage/etc/udev/rules.d"
cp -a "$HERE"/rootfs/etc/systemd/system/agentos-*.service "$stage/etc/systemd/system/"
cp -a "$HERE"/rootfs/etc/udev/rules.d/*agentos* "$stage/etc/udev/rules.d/"
install -m 0755 "$BIN" "$stage/usr/local/bin/agentosd"
install -m 0755 "$SHELL_BIN" "$stage/usr/local/bin/agentos-shell"
chmod 0755 "$stage"/usr/local/bin/*
echo "$RELEASE" >"$stage/usr/share/agentos/release"
git -C "$REPO" rev-parse --short HEAD >"$stage/usr/share/agentos/version" 2>/dev/null ||
  echo unknown >"$stage/usr/share/agentos/version"

mkdir -p "$OUT"
name=agentos-update-x86_64.tar.gz
tar --owner=0 --group=0 --numeric-owner --sort=name -czf "$OUT/$name" -C "$stage" usr etc
(cd "$OUT" && sha256sum "$name" >"$name.sha256")
echo "更新包：$OUT/$name（$RELEASE）"
