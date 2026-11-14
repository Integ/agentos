#!/usr/bin/env bash
# Exercise agentos-update end to end without root: a fake install root, a
# fake GitHub releases API on localhost, and a real update bundle.
#
#   image/test/update-test.sh [bundle.tar.gz]
#
# Without an argument a bundle is made from target/debug/agentos-taskd and
# agentos (cargo build -p agentos-taskd -p agentos-cli first).
#
# agentos-update-v0.4.0 beside this script is the updater as v0.4.0 shipped
# it, unchanged: machines on that version run it to install the next one.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UPDATE="$REPO/image/rootfs/usr/local/bin/agentos-update"
T=$(mktemp -d)
cleanup() {
  [ -n "${server:-}" ] && kill "$server" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT

pass=0
ok() { echo "ok - $*"; pass=$((pass + 1)); }
fail() { echo "FAIL - $*" >&2; exit 1; }

if [ $# -ge 1 ]; then
  bundle="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
else
  OUT="$T/out" AGENTOS_RELEASE=v9.9.9 AGENTOS_SHELL_BIN=/usr/bin/true AGENTOS_TASKD_BIN="$REPO/target/debug/agentos-taskd" \
    AGENTOS_CLI_BIN="$REPO/target/debug/agentos" AGENTOS_WEB_SHELL_BIN=/usr/bin/true \
    "$REPO/image/make-bundle.sh" >/dev/null
  bundle="$T/out/agentos-update-x86_64.tar.gz"
fi
new_release=$(tar -xzOf "$bundle" usr/share/agentos/release)

# A tiny static "GitHub": releases/latest plus the two assets.
web="$T/web"
mkdir -p "$web/repos/t/agentos/releases" "$web/dl"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
publish() { # publish <bundle> <tag>
  cp "$1" "$web/dl/agentos-update-x86_64.tar.gz"
  (cd "$web/dl" && sha256sum agentos-update-x86_64.tar.gz >agentos-update-x86_64.tar.gz.sha256)
  cat >"$web/repos/t/agentos/releases/latest" <<EOF
{"tag_name": "$2", "assets": [
 {"name": "agentos-update-x86_64.tar.gz", "browser_download_url": "http://127.0.0.1:$port/dl/agentos-update-x86_64.tar.gz"},
 {"name": "agentos-update-x86_64.tar.gz.sha256", "browser_download_url": "http://127.0.0.1:$port/dl/agentos-update-x86_64.tar.gz.sha256"}]}
EOF
}
publish "$bundle" "$new_release"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$web" >/dev/null 2>&1 &
server=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/" >/dev/null && break; sleep 0.1; done

# An "old" installation.
root="$T/root"
mkdir -p "$root/usr/local/bin" "$root/usr/share/agentos" "$root/etc/agentos"
echo v0.0.1 >"$root/usr/share/agentos/release"
printf '#!/bin/sh\necho old\n' >"$root/usr/local/bin/agentosd"
chmod 0755 "$root/usr/local/bin/agentosd"
echo 'ANTHROPIC_API_KEY=keep-me' >"$root/etc/agentos/agentos.env"

export AGENTOS_UPDATE_ROOT="$root" AGENTOS_UPDATE_API="http://127.0.0.1:$port" AGENTOS_UPDATE_REPO=t/agentos

out=$("$UPDATE" --check)
grep -q "有新版本：$new_release" <<<"$out" || fail "check should see $new_release: $out"
ok "--check reports the new release"

"$UPDATE" >"$T/log" 2>&1 || { cat "$T/log"; fail "update failed"; }
[ "$(cat "$root/usr/share/agentos/release")" = "$new_release" ] || fail "release file not updated"
cmp -s "$root/usr/local/bin/agentosd" <(tar -xzOf "$bundle" usr/local/bin/agentosd) ||
  fail "agentosd not replaced by its stand-in"
[ -x "$root/usr/local/bin/agentos-update" ] || fail "scripts not installed"
[ -f "$root/etc/systemd/system/agentos-selfcheck.service" ] || fail "units not installed"
cmp -s "$root/usr/local/bin/agentos-taskd" <(tar -xzOf "$bundle" usr/local/bin/agentos-taskd) ||
  fail "agentos-taskd not installed"
[ -x "$root/usr/local/bin/agentos" ] || fail "agentos command not installed"
[ -f "$root/usr/lib/systemd/user/agentos-taskd.service" ] || fail "task service user unit not installed"
grep -q keep-me "$root/etc/agentos/agentos.env" || fail "settings touched"
[ -z "$(find "$root" -name '*.agentos-new')" ] || fail "temp files left behind"
ok "update installs the bundle and keeps settings"

out=$("$UPDATE" --check)
grep -q "已是最新版本" <<<"$out" || fail "check after update: $out"
out=$("$UPDATE")
grep -q "已是最新版本" <<<"$out" || fail "second update should be a no-op: $out"
ok "up to date afterwards"

"$UPDATE" --rollback >"$T/log" 2>&1 || { cat "$T/log"; fail "rollback failed"; }
[ "$(cat "$root/usr/share/agentos/release")" = v0.0.1 ] || fail "release not rolled back"
grep -q 'echo old' "$root/usr/local/bin/agentosd" || fail "agentosd not rolled back"
[ ! -e "$root/usr/local/bin/agentos-update" ] || fail "new-only files not removed"
[ ! -e "$root/usr/local/bin/agentos-taskd" ] || fail "agentos-taskd left behind by rollback"
[ ! -e "$root/usr/lib/systemd/user/agentos-taskd.service" ] || fail "user unit left behind by rollback"
if "$UPDATE" --rollback >/dev/null 2>&1; then fail "second rollback should fail"; fi
ok "rollback restores the previous version"

# Tampered download: checksum mismatch, nothing changes.
echo "0000000000000000000000000000000000000000000000000000000000000000  agentos-update-x86_64.tar.gz" \
  >"$web/dl/agentos-update-x86_64.tar.gz.sha256"
if "$UPDATE" >"$T/log" 2>&1; then fail "bad checksum accepted"; fi
grep -q "校验失败" "$T/log" || { cat "$T/log"; fail "wrong error for bad checksum"; }
[ "$(cat "$root/usr/share/agentos/release")" = v0.0.1 ] || fail "bad checksum changed files"
ok "checksum mismatch is refused"

# Bundles with files outside the allowed paths, or a broken task service.
evil() { # evil <name> <setup-cmd>
  local d="$T/evil-$1"
  mkdir -p "$d"
  tar -xzf "$bundle" -C "$d"
  (cd "$d" && eval "$2")
  tar --owner=0 --group=0 -czf "$T/$1.tar.gz" -C "$d" .
  publish "$T/$1.tar.gz" "v-$1"
}
evil passwd 'mkdir -p etc && echo "root::0:0::/root:/bin/sh" > etc/passwd'
if "$UPDATE" >"$T/log" 2>&1; then fail "etc/passwd accepted"; fi
grep -q "不允许的路径" "$T/log" || { cat "$T/log"; fail "wrong error for bad path"; }
[ ! -e "$root/etc/passwd" ] || fail "bad path written"
ok "paths outside usr/ and AgentOS units are refused"

evil link 'ln -s /etc/shadow usr/share/agentos/shadow'
if "$UPDATE" >"$T/log" 2>&1; then fail "symlink accepted"; fi
grep -q "不允许的文件类型" "$T/log" || { cat "$T/log"; fail "wrong error for symlink"; }
ok "symlinks are refused"

evil no-taskd 'rm usr/local/bin/agentos-taskd'
if "$UPDATE" >"$T/log" 2>&1; then fail "bundle without the task service accepted"; fi
grep -q "没有 agentos-taskd" "$T/log" || { cat "$T/log"; fail "wrong error for a missing task service"; }
grep -q 'echo old' "$root/usr/local/bin/agentosd" || fail "refused update changed files"
ok "a bundle without the task service is refused"

evil broken-taskd 'printf "#!/bin/sh\nexit 1\n" > usr/local/bin/agentos-taskd'
if "$UPDATE" >"$T/log" 2>&1; then fail "broken agentos-taskd accepted"; fi
grep -q "任务服务自检失败" "$T/log" || { cat "$T/log"; fail "wrong error for broken agentos-taskd"; }
[ ! -e "$root/usr/local/bin/agentos-taskd" ] || fail "broken task service update changed files"
ok "a new agentos-taskd that fails its self-check is not installed"

evil no-cli 'rm usr/local/bin/agentos'
if "$UPDATE" >"$T/log" 2>&1; then fail "task service without its command accepted"; fi
grep -q "没有 agentos 命令" "$T/log" || { cat "$T/log"; fail "wrong error for a missing agentos command"; }
ok "a bundle with the task service but without its command is refused"

# Offline install from a local file.
cp "$bundle" "$T/offline.tar.gz"
"$UPDATE" --file "$T/offline.tar.gz" >/dev/null 2>&1 && fail "--file without .sha256 accepted"
(cd "$T" && sha256sum offline.tar.gz >offline.tar.gz.sha256)
"$UPDATE" --file "$T/offline.tar.gz" >"$T/log" 2>&1 || { cat "$T/log"; fail "offline update failed"; }
[ "$(cat "$root/usr/share/agentos/release")" = "$new_release" ] || fail "offline update not applied"
ok "offline --file install"

# A machine still on v0.4.0 updates with the updater it has: that one wants
# usr/local/bin/agentosd in the bundle and runs it as a self-test. The
# stand-in must pass it, by running the bundle's own task service.
old_root="$T/old-root"
mkdir -p "$old_root/usr/local/bin" "$old_root/usr/share/agentos" "$old_root/etc/agentos"
echo v0.4.0 >"$old_root/usr/share/agentos/release"
printf '#!/bin/sh\necho old\n' >"$old_root/usr/local/bin/agentosd"
chmod 0755 "$old_root/usr/local/bin/agentosd"
AGENTOS_UPDATE_ROOT="$old_root" "$REPO/image/test/agentos-update-v0.4.0" --file "$T/offline.tar.gz" \
  >"$T/log" 2>&1 || { cat "$T/log"; fail "the v0.4.0 updater refused the new bundle"; }
[ "$(cat "$old_root/usr/share/agentos/release")" = "$new_release" ] || fail "v0.4.0 updater did not install"
grep -q 'agentosd 已移除' "$old_root/usr/local/bin/agentosd" || fail "stand-in not installed by the v0.4.0 updater"
ok "the v0.4.0 updater installs the new bundle"

echo "all $pass agentos-update tests passed"
