#!/usr/bin/env bash
# Pack the AgentOS layer into the update bundle that `agentos-update`
# installs: agentos-taskd, agentos, agentos-shell, agentos-web-shell,
# image/rootfs/usr (scripts, the agentos-taskd user unit, and the agentosd
# stand-in that older agentos-update versions require) and the AgentOS-owned
# systemd units and udev rules. Writes
#   $OUT/agentos-update-x86_64.tar.gz and .sha256
#
# Environment:
#   AGENTOS_SHELL_BIN  agentos-shell binary   (default target/release/agentos-shell)
#   AGENTOS_TASKD_BIN  agentos-taskd binary   (default target/release/agentos-taskd)
#   AGENTOS_CLI_BIN    agentos binary         (default target/release/agentos)
#   AGENTOS_WEB_SHELL_BIN agentos-web-shell   (default target/release/agentos-web-shell)
#   AGENTOS_RELEASE    release tag            (default: git describe)
#   OUT                output directory       (default out/)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
OUT="${OUT:-$REPO/out}"
SHELL_BIN="${AGENTOS_SHELL_BIN:-$REPO/target/release/agentos-shell}"
TASKD_BIN="${AGENTOS_TASKD_BIN:-$REPO/target/release/agentos-taskd}"
CLI_BIN="${AGENTOS_CLI_BIN:-$REPO/target/release/agentos}"
WEB_SHELL_BIN="${AGENTOS_WEB_SHELL_BIN:-$REPO/target/release/agentos-web-shell}"
for b in "$SHELL_BIN" "$TASKD_BIN" "$CLI_BIN" "$WEB_SHELL_BIN"; do
  [[ -x "$b" ]] || { echo "找不到可执行文件：$b" >&2; exit 1; }
done
RELEASE="${AGENTOS_RELEASE:-$(git -C "$REPO" describe --tags --always 2>/dev/null || echo unknown)}"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -a "$HERE/rootfs/usr" "$stage/"
mkdir -p "$stage/etc/systemd/system" "$stage/etc/udev/rules.d"
cp -a "$HERE"/rootfs/etc/systemd/system/agentos-*.service "$stage/etc/systemd/system/"
cp -a "$HERE"/rootfs/etc/udev/rules.d/*agentos* "$stage/etc/udev/rules.d/"
install -m 0755 "$SHELL_BIN" "$stage/usr/local/bin/agentos-shell"
install -m 0755 "$TASKD_BIN" "$stage/usr/local/bin/agentos-taskd"
install -m 0755 "$CLI_BIN" "$stage/usr/local/bin/agentos"
install -m 0755 "$WEB_SHELL_BIN" "$stage/usr/local/bin/agentos-web-shell"
chmod 0755 "$stage"/usr/local/bin/*
echo "$RELEASE" >"$stage/usr/share/agentos/release"
git -C "$REPO" rev-parse --short HEAD >"$stage/usr/share/agentos/version" 2>/dev/null ||
  echo unknown >"$stage/usr/share/agentos/version"

mkdir -p "$OUT"
name=agentos-update-x86_64.tar.gz
tar --owner=0 --group=0 --numeric-owner --sort=name -czf "$OUT/$name" -C "$stage" usr etc
(cd "$OUT" && sha256sum "$name" >"$name.sha256")
echo "更新包：$OUT/$name（$RELEASE）"
