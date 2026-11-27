# 不写镜像，装到已有的 Linux 上

已经有一台在用的 Linux（Debian、Ubuntu、Fedora、Arch……），只想要 AgentOS 的任务内核：后台任务服务 `agentos-taskd` 和 `agentos` 命令（终端工作台、`submit`、MCP、微信通道）。仓库根目录的 `install.sh` 把它们装给当前用户，不需要 sudo，不碰系统目录，也不用重装系统。

图形桌面、按住说话、装到硬盘和 `agentos-update` 在线更新只在[镜像](install.md)里有。

## 一条命令

```sh
git clone https://github.com/Integ/agentos && cd agentos && ./install.sh
```

它会：

1. 编译 `agentos-taskd` 和 `agentos`（release，第一次要几分钟）；
2. 装到 `~/.local/bin`，再装一个读取设置后启动任务服务的小脚本；
3. 第一次运行时写一份设置模板 `~/.config/agentos/agentos.env`，以后不再覆盖；
4. 装上 systemd 用户服务 `agentos-taskd`，设为登录时启动，并马上启动它；
5. 运行 `agentos doctor`，列出还缺什么（模型 key、PIN、bubblewrap 沙箱……），每一条后面写着怎么办。

可以重复运行：文件没有变化时什么都不改，也不重启任务服务。

### 你需要

- Linux。任务服务自动运行需要 systemd 用户实例（主流桌面发行版都有）；没有也能装，只是要自己启动。
- Rust 1.88 或更新的版本（[rustup](https://rustup.rs)），以及 C 编译器。不想装 Rust，见下面的「用编译好的程序」。
- 推荐装 bubblewrap。没有它，插件每次调用都要你确认，MCP server 默认不启动，见 [mcp.md](mcp.md#沙箱)。
- 推荐装 Python 3：Agent 给自己写的插件用它运行，见[自我进化](self-evolution.md)。
- 模型 API key（Claude 或任意 OpenAI 兼容接口）可选；没有 key 时用离线规则规划器。

各发行版的包名：

| | Debian / Ubuntu | Arch |
| --- | --- | --- |
| C 编译器 | `build-essential` | `base-devel` |
| bubblewrap | `bubblewrap` | `bubblewrap` |
| Python 3 | `python3` | `python` |
| Rust | 发行版自带的版本通常低于 1.88，用 rustup | `rust`，跟着 Rust 稳定版更新；和 `rustup` 二选一 |

```sh
sudo apt install build-essential bubblewrap python3          # Debian / Ubuntu
sudo pacman -S --needed base-devel bubblewrap python rust    # Arch（已经用 rustup 时去掉 rust）
```

### 选项

```sh
./install.sh --prefix /opt/agentos   # 装到 /opt/agentos/bin（默认 ~/.local）
./install.sh --from 目录             # 复制目录里编译好的 agentos-taskd 和 agentos，不编译
./install.sh --no-start              # 只装文件，不启用也不启动用户服务
./install.sh --uninstall             # 卸载，见下文
```

不要用 sudo 运行，脚本会拒绝：以 root 安装会让 Agent 以 root 身份运行。已经装了镜像的机器上脚本也会停下，那里请用 `sudo agentos-update`。

### 用编译好的程序

每个版本的 [Releases](https://github.com/Integ/agentos/releases) 里有给镜像在线更新用的 `agentos-update-x86_64.tar.gz`，里面就有这两个程序（x86-64，在 Debian 12 上编译）：

```sh
sha256sum -c agentos-update-x86_64.tar.gz.sha256
mkdir agentos-bundle && tar -xzf agentos-update-x86_64.tar.gz -C agentos-bundle
./install.sh --from agentos-bundle/usr/local/bin
```

`install.sh` 本身从仓库里拿（`git clone`，或者只下载这一个文件）。脚本会先试运行这两个程序，架构或系统库不对就停下，不会覆盖已经装好的版本。

## 装好的文件

| 文件 | 用途 |
| --- | --- |
| `~/.local/bin/agentos-taskd`、`~/.local/bin/agentos` | 任务服务和命令 |
| `~/.local/libexec/agentos/agentos-taskd-service` | 读取设置文件，再启动任务服务 |
| `~/.config/agentos/agentos.env` | 设置，权限 600；已经存在时不覆盖 |
| `~/.config/systemd/user/agentos-taskd.service` | 用户服务；重新安装时会重写，本地改动用 `systemctl --user edit agentos-taskd` |
| `~/.local/state/agentos/` | 任务、笔记、PIN、插件和审计日志。由任务服务自己创建和管理，安装和卸载都不会动它 |

`~/.local/bin` 不在 `PATH` 里时，脚本最后会提示要加的那一行。

## 设置

设置都在 `~/.config/agentos/agentos.env`，改完重启任务服务：

```sh
systemctl --user restart agentos-taskd
```

接 Claude：

```sh
ANTHROPIC_API_KEY=sk-ant-...
```

接 OpenAI 兼容接口（本机的 Ollama 等也可以，没有 key 就不写）：

```sh
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_API_KEY=sk-...
OPENAI_MODEL=gpt-4.1
```

不写 `AGENTOS_PLANNER` 时，填了哪种 key 就用哪种模型规划器，都没填就用离线规则规划器；写 `AGENTOS_PLANNER=rule` 可以在留着 key 的同时不联网。这个文件按 shell 语法读取，值里有空格或特殊字符时加单引号；镜像里的 `/etc/agentos/agentos.env` 可以直接复制过来用。

**任务服务只认这个文件。** 有的桌面会把整个登录环境导入 systemd 用户实例，shell 里为别的工具设置的 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY` 就会被任务服务看到，让它不声不响地改用付费模型。所以启动脚本先清掉环境里所有 `AGENTOS_`、`ANTHROPIC_`、`OPENAI_`、`HASS_` 开头的变量，再读取设置文件。

想换状态目录，在设置文件里写 `AGENTOS_STATE_DIR=...`；这时在 shell 里也要设置同样的 `AGENTOS_STATE_DIR`（或者给 `agentos` 加 `--state-dir`），否则 `agentos` 和 `agentos-taskd --set-pin` 找的还是默认目录。

## 装好以后

`./install.sh` 最后打印的就是 `agentos doctor` 的报告，之后随时可以再运行它。通常还要处理：

- **PIN**：不可逆的操作需要 PIN。运行 `agentos-taskd --set-pin` 设置，正在运行的任务服务马上就会用上，不需要重启。
- **沙箱**：装上 bubblewrap 后重启任务服务。装了但内核不让它用时，doctor 会指出来：有的发行版限制非特权用户命名空间，例如 Ubuntu 24.04 起用 AppArmor 限制，Arch 的 `linux-hardened` 内核默认禁止（默认的 `linux` 内核没有这个限制）。
- **注销后继续运行**：systemd 默认在你最后一个会话结束时停掉用户服务，定时任务也就不会运行。想让它一直运行：`loginctl enable-linger`。

日常使用：

```sh
agentos                                  # 终端工作台
agentos submit --wait "现在几点"          # 提交一个任务
journalctl --user -u agentos-taskd       # 任务服务的日志，包括崩溃报告（看不到时试 journalctl -t agentos-taskd）
systemctl --user status agentos-taskd    # 有没有在运行
agentos activity                         # 最近活动：调用、批准、崩溃、自动采用的经验（没有桌面时看这里）
```

## 更新

```sh
cd agentos && git pull && ./install.sh
```

程序有变化时脚本会替换它们并重启任务服务，没有变化就不动它。状态目录原样保留；新版本可能会升级里面的格式，升级以后旧版本会拒绝打开它，所以不能带着同一个状态目录退回旧版本。

## 卸载

```sh
./install.sh --uninstall
```

停止并停用用户服务，删除用户服务文件、两个程序和启动脚本。装的时候用了 `--prefix`，卸载时不写也能从用户服务文件里找到位置。

**保留**设置文件（里面可能有 API key，不再需要就删掉它）和状态目录 `~/.local/state/agentos`。状态目录里有只追加的审计日志，记录 Agent 做过的每一件事，脚本永远不会删除它；确实不要了，自己删除。

## 和镜像的区别

| | 镜像 | 独立安装 |
| --- | --- | --- |
| 设置文件 | `/etc/agentos/agentos.env`，设置助理写入 | `~/.config/agentos/agentos.env`，自己编辑 |
| 启动任务服务 | 登录时由 `agentos-session` 启动 | systemd 用户服务，登录时启动 |
| 更新 | `sudo agentos-update`，可回滚 | `git pull && ./install.sh` |
| 沙箱 | 自带 bubblewrap | 自己安装 bubblewrap |
| 图形桌面、语音、装到硬盘 | 有 | 没有 |

两种方式用同一个用户服务名 `agentos-taskd` 和同一个默认状态目录，所以 `systemctl --user`、`journalctl --user -u agentos-taskd` 和 `agentos` 的用法一样。
