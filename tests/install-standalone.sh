#!/usr/bin/env bash
# End-to-end test of install.sh against a real systemd user manager: install,
# a task through the installed service, an idempotent re-run, uninstall.
#
#   tests/install-standalone.sh [directory with agentos-taskd and agentos]
#
# It installs into this user's home, enables lingering and changes the user
# manager's environment, so it runs only on a throwaway Linux machine where
# CI=true (GitHub Actions sets it), never on a developer's.
set -euo pipefail
if [ "${CI:-}" != true ] || [ "$(uname -s)" != Linux ]; then
  echo "拒绝运行：这个测试会装进当前用户的主目录并启用用户服务，只在 Linux 的 CI（CI=true）里运行" >&2
  exit 2
fi

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$(cd "${1:-$REPO/target/debug}" && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fail() {
  echo "FAIL: $*" >&2
  journalctl --user -u agentos-taskd -n 30 --no-pager >&2 2>/dev/null || true
  exit 1
}

AGENTOS=$HOME/.local/bin/agentos
ENV_FILE=${XDG_CONFIG_HOME:-$HOME/.config}/agentos/agentos.env
STATE=$HOME/.local/state/agentos

# What a login session would provide: a running user manager and its bus.
sudo loginctl enable-linger "$(id -un)"
export XDG_RUNTIME_DIR=/run/user/$(id -u)
for _ in $(seq 1 50); do
  systemctl --user show-environment >/dev/null 2>&1 && break
  sleep 0.2
done
systemctl --user show-environment >/dev/null || fail "no systemd user manager"

# What a desktop that imports the whole login environment hands the user
# manager: keys meant for other tools. The wrapper must drop them; if one got
# through, the service would plan with a model nobody configured and the task
# below would fail instead of being handled by the rule planner.
ANTHROPIC_API_KEY=sk-ant-fake OPENAI_API_KEY=sk-fake OPENAI_MODEL=fake AGENTOS_PLANNER=openai \
  systemctl --user import-environment ANTHROPIC_API_KEY OPENAI_API_KEY OPENAI_MODEL AGENTOS_PLANNER

echo "== install"
"$REPO/install.sh" --from "$BIN" | tee "$T/install.out" || fail "install.sh failed"
systemctl --user is-enabled --quiet agentos-taskd || fail "service not enabled"
systemctl --user is-active --quiet agentos-taskd || fail "service not running after install"
grep -qE 'agentos doctor|agentos 能连上' "$T/install.out" || fail "install did not end with a report"
"$AGENTOS" submit --wait "记一下 独立安装测试" ||
  fail "the task did not succeed with the rule planner (did a key from the environment reach the service?)"

echo "== again, nothing changed: settings kept, service not restarted"
echo '# kept by the test' >>"$ENV_FILE"
pid=$(systemctl --user show -p MainPID --value agentos-taskd)
"$REPO/install.sh" --from "$BIN" | tee "$T/again.out" || fail "re-running install.sh failed"
grep -q '没有变化，不重启' "$T/again.out" || fail "re-run did not notice nothing changed"
[ "$(systemctl --user show -p MainPID --value agentos-taskd)" = "$pid" ] ||
  fail "re-run restarted the service although nothing changed"
grep -qx '# kept by the test' "$ENV_FILE" || fail "re-run overwrote the settings"

echo "== uninstall"
cp "$STATE/audit.jsonl" "$T/audit.before"
"$REPO/install.sh" --uninstall || fail "uninstall failed"
! systemctl --user is-active --quiet agentos-taskd || fail "service still running after uninstall"
[ ! -e "$HOME/.local/bin/agentos-taskd" ] && [ ! -e "$AGENTOS" ] || fail "binaries left behind"
[ ! -e "$HOME/.local/libexec/agentos/agentos-taskd-service" ] || fail "wrapper left behind"
[ ! -e "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/agentos-taskd.service" ] || fail "unit left behind"
[ -f "$ENV_FILE" ] || fail "uninstall removed the settings"
# The audit log is append-only and outlives the install: every record written
# before uninstall must still be there, byte for byte.
cmp -n "$(wc -c <"$T/audit.before")" "$T/audit.before" "$STATE/audit.jsonl" ||
  fail "uninstall changed the audit log"

systemctl --user unset-environment ANTHROPIC_API_KEY OPENAI_API_KEY OPENAI_MODEL AGENTOS_PLANNER
echo "install.sh: all checks passed"
