#!/usr/bin/env bash
# Install the AgentOS task kernel (agentos-taskd and the agentos command) for
# the current user on an existing Linux machine, without the disk image.
#
#   ./install.sh                 build from this checkout, install, start
#   ./install.sh --from DIR      copy prebuilt binaries from DIR instead
#   ./install.sh --uninstall     remove what this script installed
#
# Everything is per user and needs no sudo: binaries in PREFIX/bin (default
# ~/.local/bin), a wrapper in PREFIX/libexec/agentos, settings in
# ~/.config/agentos/agentos.env and a systemd user unit named like the
# image's, so `systemctl --user ... agentos-taskd` works the same in both.
#
# The state directory (~/.local/state/agentos: tasks, PIN, notes and the
# append-only audit log) belongs to agentos-taskd. This script never creates,
# changes or removes anything in it, uninstall included: losing the audit log
# would break the one record of what the agent did.
#
# Written for bash 3.2 too, so it can be exercised on macOS (without systemd).
if [ -z "${BASH_VERSION:-}" ]; then
  echo "错误：请用 bash 运行：bash install.sh" >&2
  exit 2
fi
set -euo pipefail

UNIT_NAME=agentos-taskd.service
DOC_URL=https://github.com/Integ/agentos/blob/main/docs/install-standalone.md

usage() {
  cat <<'EOF'
AgentOS 独立安装：不写镜像，把任务服务 agentos-taskd 和 agentos 命令装给当前用户。

用法：./install.sh [选项]

  --prefix 目录    装到 目录/bin（默认 ~/.local）
  --from 目录      从这个目录复制已经编译好的 agentos-taskd 和 agentos，不编译
  --no-start       只安装文件，不启用也不启动用户服务
  --uninstall      卸载：停止并删除用户服务和程序；设置文件和状态目录都保留
  --help           查看帮助

装好的文件：
  目录/bin/agentos-taskd、目录/bin/agentos
  目录/libexec/agentos/agentos-taskd-service    读取设置，再启动任务服务
  ~/.config/agentos/agentos.env                 设置（规划器、API key）；已经存在时不覆盖
  ~/.config/systemd/user/agentos-taskd.service  用户服务

状态目录 ~/.local/state/agentos（任务、PIN、审计日志）由任务服务自己管理，安装和卸载
都不会动它。可以重复运行：没有变化时不会重启任务服务。

退出码：0 成功；1 安装失败或任务服务没有起来；2 用法错误。
EOF
}

say() { printf '%s\n' "$*"; }
warn() { printf '注意：%s\n' "$*" >&2; }
die() {
  printf '错误：%s\n' "$*" >&2
  exit 1
}

PREFIX=""
FROM=""
NO_START=0
UNINSTALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix | --from)
      [ $# -ge 2 ] && [ -n "$2" ] && [ "${2#--}" = "$2" ] || {
        printf '错误：%s 缺少值\n' "$1" >&2
        exit 2
      }
      if [ "$1" = --prefix ]; then PREFIX=$2; else FROM=$2; fi
      shift 2
      ;;
    --no-start) NO_START=1 && shift ;;
    --uninstall) UNINSTALL=1 && shift ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      printf '错误：不认识的参数 %s（--help 查看用法）\n' "$1" >&2
      exit 2
      ;;
  esac
done
if [ "$UNINSTALL" = 1 ] && { [ -n "$FROM" ] || [ "$NO_START" = 1 ]; }; then
  printf '错误：--uninstall 只能和 --prefix 一起用\n' >&2
  exit 2
fi

# A root install would put the service in root's home and run the agent as
# root; that is never what a per-user install means.
[ "$(id -u)" != 0 ] || die "不要用 root 或 sudo 运行：这是装给当前用户的，不需要管理员权限"
[ -n "${HOME:-}" ] || die "没有设置 HOME"

case "${XDG_CONFIG_HOME:-}" in
  /*) CONFIG_HOME=$XDG_CONFIG_HOME ;;
  *) CONFIG_HOME=$HOME/.config ;;
esac
ENV_DIR=$CONFIG_HOME/agentos
ENV_FILE=$ENV_DIR/agentos.env
UNIT_DIR=$CONFIG_HOME/systemd/user
UNIT=$UNIT_DIR/$UNIT_NAME

# Paths end up unquoted in the unit file, where whitespace splits arguments
# and % starts a specifier; refuse them rather than escape them.
plain_path() {
  case "$1" in
    *[[:space:]%\"\'\\\$\`]*) die "路径里不能有空格、引号、%、\$ 或反斜杠：$1" ;;
  esac
}

# The prefix an earlier run installed to, read back from its unit file.
installed_prefix() {
  [ -f "$UNIT" ] || return 0
  sed -n 's|^ExecStart=\(/.*\)/libexec/agentos/agentos-taskd-service$|\1|p' "$UNIT" | head -n 1
}

# Where the service keeps its state, resolved like agentos-taskd does it
# (AGENTOS_STATE_DIR from the settings, else XDG_STATE_HOME, else HOME), so
# messages name the real directory. Only printed, never touched.
state_dir() {
  (
    # The file is sourced the way the wrapper does it, without -e or -u.
    set +eu
    clear_agentos_env
    # shellcheck disable=SC1090
    [ -r "$ENV_FILE" ] && . "$ENV_FILE" >/dev/null 2>&1
    if [ -n "${AGENTOS_STATE_DIR:-}" ]; then
      printf '%s\n' "$AGENTOS_STATE_DIR"
    else
      case "${XDG_STATE_HOME:-}" in
        /*) printf '%s\n' "$XDG_STATE_HOME/agentos" ;;
        *) printf '%s\n' "$HOME/.local/state/agentos" ;;
      esac
    fi
  )
}

have_systemd() {
  command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}

# The same variables the wrapper clears, so the agentos command here looks
# for the socket where the service will put it.
clear_agentos_env() {
  local v
  for v in $(compgen -e); do
    case "$v" in
      AGENTOS_* | ANTHROPIC_* | OPENAI_* | HASS_*) unset "$v" ;;
    esac
  done
}

# Run the installed agentos command with the service's settings.
run_cli() {
  (
    set +eu
    clear_agentos_env
    set -a
    # shellcheck disable=SC1090
    [ -r "$ENV_FILE" ] && . "$ENV_FILE"
    set +a
    exec "$BIN_DIR/agentos" "$@"
  )
}

pkg_install_hint() {
  if command -v apt-get >/dev/null 2>&1; then
    say "sudo apt install $1"
  elif command -v dnf >/dev/null 2>&1; then
    say "sudo dnf install $1"
  elif command -v pacman >/dev/null 2>&1; then
    say "sudo pacman -S $1"
  elif command -v zypper >/dev/null 2>&1; then
    say "sudo zypper install $1"
  else
    say "用发行版的包管理器安装 $1"
  fi
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

CHANGED=0
UNIT_CHANGED=0
# Copy src to dst through a temporary file in the same directory: rename is
# atomic, and it works while the old binary is running, where writing over
# it would fail with "text file busy".
put() {
  local src=$1 dst=$2 mode=$3 tmp
  if [ -f "$dst" ] && cmp -s "$src" "$dst"; then
    chmod "$mode" "$dst"
    return 1
  fi
  tmp=$dst.new.$$
  if ! { cp "$src" "$tmp" && chmod "$mode" "$tmp" && mv -f "$tmp" "$dst"; }; then
    rm -f "$tmp"
    die "无法写入 $dst"
  fi
  CHANGED=1
  say "  已写入 $dst"
  return 0
}

# ---------------------------------------------------------------- uninstall

uninstall() {
  local recorded
  recorded=$(installed_prefix)
  if [ -z "$PREFIX" ]; then PREFIX=${recorded:-$HOME/.local}; fi
  case "$PREFIX" in /*) ;; *) die "--prefix 要写绝对路径：$PREFIX" ;; esac
  BIN_DIR=$PREFIX/bin
  say "卸载 AgentOS（$PREFIX）…"

  if have_systemd; then
    if systemctl --user cat "$UNIT_NAME" >/dev/null 2>&1; then
      systemctl --user disable --now "$UNIT_NAME" >/dev/null 2>&1 ||
        warn "停止用户服务 $UNIT_NAME 时出错，继续删除文件"
      say "  已停止并停用用户服务"
    fi
  elif [ -f "$UNIT" ]; then
    warn "连不上 systemd 用户实例，没法停止用户服务；文件照样删除，还在运行的任务服务要你在能用 systemctl --user 的会话里停止：systemctl --user stop $UNIT_NAME"
  fi
  # Remove the enable link by hand too, in case systemctl could not.
  rm -f "$UNIT" "$UNIT_DIR/default.target.wants/$UNIT_NAME"
  if have_systemd; then
    systemctl --user daemon-reload || true
    systemctl --user reset-failed "$UNIT_NAME" >/dev/null 2>&1 || true
  fi

  local f
  for f in "$BIN_DIR/agentos-taskd" "$BIN_DIR/agentos" \
    "$PREFIX/libexec/agentos/agentos-taskd-service"; do
    if [ -e "$f" ] || [ -L "$f" ]; then
      rm -f "$f"
      say "  已删除 $f"
    fi
  done
  rmdir "$PREFIX/libexec/agentos" 2>/dev/null || true
  if [ -n "$recorded" ] && [ "$recorded" != "$PREFIX" ]; then
    warn "之前的安装在 $recorded，这次删除的是 $PREFIX；那边的程序还在，用 --prefix $recorded 再卸载一次"
  fi

  say ""
  say "保留了："
  say "  设置 $ENV_FILE（里面可能有 API key；不再需要就删掉它）"
  say "  状态目录 $(state_dir)（任务、笔记、PIN 和只追加的审计日志）"
  say "自己启动过 agentos-taskd 的话，记得停止它。"
}

if [ "$UNINSTALL" = 1 ]; then
  uninstall
  exit 0
fi

# ------------------------------------------------------------------ install

# On the image the system units and agentos-update own these programs; a
# second copy under ~/.local would race them for the same socket and state.
if [ -e /usr/local/bin/agentos-session ]; then
  die "这台机器装的是 AgentOS 镜像，任务服务已经由系统管理；更新请运行 sudo agentos-update"
fi

if [ "$(uname -s)" != Linux ]; then
  warn "AgentOS 只支持 Linux。在 $(uname -s) 上可以装上文件，但没有 bubblewrap 沙箱，也没有 systemd 用户服务"
fi

PREFIX=${PREFIX:-$HOME/.local}
case "$PREFIX" in /*) ;; *) PREFIX=$PWD/$PREFIX ;; esac
plain_path "$PREFIX"
plain_path "$ENV_FILE"
BIN_DIR=$PREFIX/bin
LIBEXEC=$PREFIX/libexec/agentos
WRAPPER=$LIBEXEC/agentos-taskd-service

# 1. Binaries: prebuilt, or built from this checkout.
if [ -n "$FROM" ]; then
  [ -d "$FROM" ] || die "--from 指定的目录不存在：$FROM"
  SRC=$(cd "$FROM" && pwd)
else
  REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  if [ ! -f "$REPO/Cargo.toml" ] || [ ! -d "$REPO/crates/taskd" ]; then
    die "$REPO 不是 AgentOS 仓库；先 git clone https://github.com/Integ/agentos，或者用 --from 指定编译好的程序"
  fi
  command -v cargo >/dev/null 2>&1 ||
    die "没有找到 cargo。安装 Rust 1.88 或更新的版本（https://rustup.rs），或者用 --from 指定编译好的程序"
  command -v cc >/dev/null 2>&1 ||
    warn "没有找到 C 编译器 cc，编译多半会失败；安装方法：$(pkg_install_hint build-essential)"
  say "编译 agentos-taskd 和 agentos（release；第一次要几分钟）…"
  (cd "$REPO" && cargo build --release --locked -p agentos-taskd -p agentos-cli) ||
    die "编译失败，见上面 cargo 的输出"
  target=${CARGO_TARGET_DIR:-target}
  case "$target" in /*) SRC=$target/release ;; *) SRC=$REPO/$target/release ;; esac
fi
for b in agentos-taskd agentos; do
  [ -f "$SRC/$b" ] && [ -x "$SRC/$b" ] || die "在 $SRC 里没有找到可执行的 $b"
  # Catches a binary for another architecture or C library before it is
  # installed over a working one.
  "$SRC/$b" --version >/dev/null 2>&1 || die "$SRC/$b 在这台机器上运行不了（架构或系统库不对？）"
done
taskd_version=$("$SRC/agentos-taskd" --version | awk '{print $NF}')
cli_version=$("$SRC/agentos" --version | awk '{print $NF}')
[ "$taskd_version" = "$cli_version" ] ||
  warn "agentos-taskd（$taskd_version）和 agentos（$cli_version）版本不同，最好用同一次编译的程序"

previous=$(installed_prefix)
if [ -n "$previous" ] && [ "$previous" != "$PREFIX" ]; then
  warn "之前装在 $previous，用户服务改用 $PREFIX 里的程序；旧的不会删除，不需要就删掉 $previous/bin/agentos-taskd、$previous/bin/agentos 和 $previous/libexec/agentos"
fi
say "安装到 $PREFIX …"
mkdir -p "$BIN_DIR" "$LIBEXEC" "$UNIT_DIR"
put "$SRC/agentos-taskd" "$BIN_DIR/agentos-taskd" 0755 || true
put "$SRC/agentos" "$BIN_DIR/agentos" 0755 || true

# 2. Wrapper: the env file is written with shell quoting (like the image's
# /etc/agentos/agentos.env, so a copy of that works here), hence sourced by
# bash rather than given to systemd as an EnvironmentFile.
{
  cat <<'EOF'
#!/usr/bin/env bash
# Run agentos-taskd for this user with the settings in agentos.env. Installed
# by the AgentOS standalone install.sh; started by agentos-taskd.service.
#
# That file is the only source of AgentOS settings: some desktops import the
# whole login environment into the user manager, and a key found there would
# otherwise switch the service to a paid model nobody configured for it.
for v in $(compgen -e); do
  case "$v" in
    AGENTOS_* | ANTHROPIC_* | OPENAI_* | HASS_*) unset "$v" ;;
  esac
done
EOF
  printf 'ENV_FILE=%q\n' "$ENV_FILE"
  cat <<'EOF'
set -a
# shellcheck disable=SC1090
[ -r "$ENV_FILE" ] && . "$ENV_FILE"
set +a
EOF
  printf 'exec %q "$@"\n' "$BIN_DIR/agentos-taskd"
} >"$WORK/wrapper"
put "$WORK/wrapper" "$WRAPPER" 0755 || true

# 3. Settings: written once, then the user's. Mode 0600 because API keys go
# in here.
if [ -e "$ENV_FILE" ]; then
  say "  保留已有的设置 $ENV_FILE"
  if [ -n "$(find "$ENV_FILE" -prune \( -perm -004 -o -perm -040 \) 2>/dev/null)" ]; then
    warn "$ENV_FILE 别的用户也能读，里面的 API key 会泄露；运行 chmod 600 $ENV_FILE"
  fi
else
  (umask 077 && mkdir -p "$ENV_DIR")
  cat >"$WORK/agentos.env" <<'EOF'
# AgentOS 任务服务的设置（独立安装）。agentos-taskd 启动时读取这个文件，
# 改完运行 systemctl --user restart agentos-taskd 生效。
#
# 按 shell 语法读取：值里有空格或特殊字符时加单引号。任务服务只认这里的设置，
# 登录环境里的 ANTHROPIC_API_KEY、OPENAI_API_KEY 等变量不会传给它。

# 规划器：rule（离线规则，不联网）、claude、openai。
# 不写时：下面填了 key 就用对应的模型规划器，否则用离线规则规划器。
#AGENTOS_PLANNER=claude

# Claude
#ANTHROPIC_API_KEY=
#AGENTOS_MODEL=

# OpenAI 兼容接口（也可以是本机的 Ollama 等，没有 key 时留空）
#OPENAI_BASE_URL=https://api.openai.com/v1
#OPENAI_API_KEY=
#OPENAI_MODEL=

# Home Assistant
#HASS_URL=http://homeassistant.local:8123
#HASS_TOKEN=

# Agent 的文件工作区（默认 ~/AgentOS）
#AGENTOS_FILES_DIR=
EOF
  (umask 077 && cp "$WORK/agentos.env" "$ENV_FILE")
  say "  已写入 $ENV_FILE"
fi

# 4. The user unit, after the image's agentos-taskd.service.
cat >"$WORK/unit" <<EOF
# AgentOS task service for this user (standalone install, no disk image).
# Written by install.sh; running it again rewrites this file, so put local
# changes in a drop-in (systemctl --user edit agentos-taskd).
# Settings: $ENV_FILE
# State: ~/.local/state/agentos unless the settings set AGENTOS_STATE_DIR.
[Unit]
Description=AgentOS task service (agentos-taskd)
Documentation=$DOC_URL

[Service]
ExecStart=$WRAPPER
SyslogIdentifier=agentos-taskd
# Restart after crashes, backing off when it keeps failing. systemd older
# than 254 ignores the RestartSteps lines (with a warning) and restarts
# every second until its start limit.
Restart=always
RestartSec=1
RestartSteps=5
RestartMaxDelaySec=30
# On SIGTERM it drains admitted work for up to five seconds.
TimeoutStopSec=15

[Install]
WantedBy=default.target
EOF
if put "$WORK/unit" "$UNIT" 0644; then UNIT_CHANGED=1; fi
[ "$CHANGED" = 1 ] || say "  文件都是最新的"

# 5. Start (or restart, when something changed) the service.
STARTED=0
FAILED=0
if [ "$NO_START" = 1 ]; then
  say "按 --no-start，没有启用或启动用户服务。"
elif have_systemd; then
  [ "$UNIT_CHANGED" = 0 ] || systemctl --user daemon-reload
  systemctl --user enable --quiet "$UNIT_NAME"
  if systemctl --user is-active --quiet "$UNIT_NAME"; then
    if [ "$CHANGED" = 1 ]; then
      say "重启任务服务，换上新的文件…"
      systemctl --user restart "$UNIT_NAME" || FAILED=1
    else
      say "任务服务已经在运行，没有变化，不重启。"
    fi
  else
    say "启动任务服务…"
    # A unit that hit its start limit refuses to start until reset.
    systemctl --user reset-failed "$UNIT_NAME" >/dev/null 2>&1 || true
    systemctl --user start "$UNIT_NAME" || FAILED=1
  fi
  STARTED=1
elif [ -n "$previous" ]; then
  # Installed and enabled by an earlier run, from a session that had a user
  # bus; this one (ssh, su) cannot reach it.
  warn "连不上 systemd 用户实例（例如 ssh 或 su 进来的 shell），没法重启任务服务。"
  [ "$CHANGED" = 0 ] || say "新文件要等任务服务下次启动才用上；在图形会话里运行 systemctl --user restart $UNIT_NAME，或者注销后重新登录。"
else
  warn "连不上 systemd 用户实例（没有 systemd，或者这个会话没有用户总线，例如 ssh 或 su 进来的 shell）。文件已经装好，用户服务没有启用。"
  say "在图形会话里再运行一次 ./install.sh 就会启用；也可以先手动运行：$WRAPPER"
fi

# Wait until the service answers, so doctor reports on it rather than on a
# socket that is about to appear.
if [ "$STARTED" = 1 ] && [ "$FAILED" = 0 ]; then
  i=0
  until run_cli capabilities >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 40 ] || ! systemctl --user is-active --quiet "$UNIT_NAME"; then
      FAILED=1
      break
    fi
    sleep 0.5
  done
fi
if [ "$FAILED" = 1 ]; then
  warn "任务服务没有起来。最近的日志："
  journalctl --user -u "$UNIT_NAME" -n 20 --no-pager >&2 2>/dev/null || true
fi

# 6. What is still missing, in the service's own words.
say ""
if "$BIN_DIR/agentos" --help 2>/dev/null | grep -q '^  doctor '; then
  say "—— agentos doctor ——"
  set +e
  run_cli doctor
  code=$?
  set -e
  say ""
  case "$code" in
    0) say "一切正常。" ;;
    3) say "上面是还要处理的事（规划器、PIN、沙箱等），每条后面写了怎么办。" ;;
    *) say "doctor 连不上任务服务；问题和办法见上面。" ;;
  esac
elif run_cli capabilities >/dev/null 2>&1; then
  say "任务服务在运行，agentos 能连上。（这个版本的 agentos 还没有 doctor 命令。）"
else
  say "agentos 连不上任务服务。（这个版本的 agentos 还没有 doctor 命令。）"
fi

# Things doctor cannot see from inside the service.
if [ "$(uname -s)" = Linux ]; then
  if ! command -v bwrap >/dev/null 2>&1; then
    say "沙箱：没有 bubblewrap，插件每次调用都要你确认，MCP server 不启动。安装：$(pkg_install_hint bubblewrap)"
  fi
  if have_systemd && command -v loginctl >/dev/null 2>&1 &&
    [ "$(loginctl show-user "$(id -un)" --property=Linger --value 2>/dev/null)" = no ]; then
    say "注销后任务服务会停止，定时任务也不会运行；想让它一直运行：loginctl enable-linger"
  fi
fi
case ":$PATH:" in
  *":$BIN_DIR:"*)
    found=$(command -v agentos || true)
    if [ "$found" != "$BIN_DIR/agentos" ]; then
      warn "PATH 里先找到的是 $found，不是刚装的 $BIN_DIR/agentos"
    fi
    ;;
  *) say "PATH 里没有 $BIN_DIR；把 export PATH=\"$BIN_DIR:\$PATH\" 加进 ~/.profile 或 shell 的配置文件。" ;;
esac

say ""
say "装好了：$("$BIN_DIR/agentos-taskd" --version)"
say "  打开工作台：agentos"
say "  修改设置：  编辑 $ENV_FILE，然后 systemctl --user restart agentos-taskd"
say "  设置 PIN：  agentos-taskd --set-pin"
say "  查看日志：  journalctl --user -u agentos-taskd"
say "  卸载：      ./install.sh --uninstall（保留设置和状态目录）"
say "  说明：      $DOC_URL"

[ "$FAILED" = 0 ] || exit 1
