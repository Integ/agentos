# 把 AgentOS 安装到一台新 PC

AgentOS 以一个**可启动的磁盘镜像**发布：写进 U 盘，从 U 盘启动就能直接用；想长期使用时，一条命令（或首次设置里点一下）就能装到电脑的内置硬盘上。

## 你需要

- 一台 x86-64 PC，UEFI 启动（2012 年以后的电脑基本都是）。**安全启动（Secure Boot）可以保持开启。**
- 一个 8GB 以上的 U 盘（内容会被清空）。
- 安装到硬盘时：一块至少 8GB、可以被整盘清空的硬盘。
- 网络（有线或 Wi-Fi）和一个模型 API key（Claude 或任意 OpenAI 兼容接口）；不联网也能用离线规则规划器试用。

## 三步安装

### 1. 下载镜像并写入 U 盘

从 GitHub Releases（打了 `v*` 标签的版本），或者 Actions 里 **Image** 工作流的产物下载：

| 文件 | 说明 |
| --- | --- |
| `agentos-debian-x86_64.img.xz` | 默认，基于 Debian 13 |
| `agentos-ubuntu-x86_64.img.xz` | 基于 Ubuntu 24.04，内核对很新的硬件支持更好 |

用 [balenaEtcher](https://etcher.balena.io/) 直接选 `.img.xz` 写入 U 盘即可（它能直接读 xz 压缩的镜像，Windows 上也可以用 Rufus）。命令行方式：

```sh
# Linux（把 /dev/sdX 换成你的 U 盘，务必确认！）
xz -dc agentos-debian-x86_64.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync status=progress

# macOS（diskutil list 找到 U 盘，例如 /dev/disk4）
diskutil unmountDisk /dev/disk4
xz -dc agentos-debian-x86_64.img.xz | sudo dd of=/dev/rdisk4 bs=4m
```

### 2. 从 U 盘启动

插上 U 盘开机，按启动菜单键（常见的是 F12、F11、F8 或 Esc），选择 U 盘。几秒后会自动登录并进入 AgentOS：

1. **欢迎页**
2. **网络**：插网线会自动连接；要连 Wi-Fi 就选「连接 Wi-Fi」
3. **AI 规划器**：选 Claude / OpenAI 兼容接口 / 离线规则规划器，再填 API key 和模型

设置完就进入管家 Agent，可以直接输入意图（拼音输入法用 **Ctrl+空格** 切换）。第一次从 U 盘启动时，系统会自动把分区扩展到占满整个 U 盘。

### 3. 安装到硬盘

首次设置最后会问「要把它安装到这台电脑的硬盘上吗？」，选「安装到硬盘」，再选一块硬盘、输入 `yes` 确认。以后也可以在管家 Agent 退出后的菜单里按 `i`，或者在命令行运行：

```sh
sudo agentos-install                 # 交互式选择硬盘
sudo agentos-install /dev/nvme0n1    # 直接指定
```

安装器会：清空目标硬盘并分区（512MB EFI 分区 + ext4 根分区）→ 把当前系统复制过去（包括刚才填好的设置和 API key）→ 写入带签名的 shim + grub 引导 → 在 UEFI 里添加「AgentOS」启动项。完成后拔掉 U 盘重启即可。

## 使用小贴士

- 管家 Agent 里按 **Ctrl-D** 退出，会出现菜单：回车重新启动管家、`s` 打开命令行、`c` 重新设置、`r` 重启、`p` 关机。
- **Win+回车** 再开一个终端。
- 设置保存在 `/etc/agentos/agentos.env`（可以手动编辑），审计日志在 `/var/lib/agentos/audit.jsonl`。
- 图形界面起不来时，在开机菜单选「排障模式」，会直接在文本控制台运行管家 Agent。
- 系统更新：`sudo apt update && sudo apt full-upgrade`，内核升级后无需额外操作。

## 自己构建镜像

在 Linux x86-64 上只需要 Docker：

```sh
image/build-in-docker.sh                   # Debian 13，输出 out/agentos-debian-x86_64.img
DISTRO=ubuntu image/build-in-docker.sh     # Ubuntu 24.04
COMPRESS=1 image/build-in-docker.sh        # 同时生成 .img.xz
```

或者在 Debian/Ubuntu 主机上直接运行（需要 root、debootstrap、fdisk、dosfstools、mtools、e2fsprogs）：

```sh
sudo image/build.sh
```

在 QEMU 里做完整冒烟测试（安全启动开启 → U 盘自检 → 自动装到第二块盘 → 从硬盘启动再自检 + 截图）：

```sh
sudo apt install qemu-system-x86 qemu-utils ovmf
sudo image/test/qemu-smoke.sh out/agentos-debian-x86_64.img
```

## 镜像里有什么

| 部分 | 内容 |
| --- | --- |
| 底层 | Debian 13 或 Ubuntu 24.04 最小系统，发行版内核和固件，NetworkManager |
| 引导 | 发行版签名的 shim + grub（支持安全启动），根分区按 UUID 挂载 |
| 会话 | tty1 自动登录用户 `agent` → sway（kiosk 模式）→ foot 全屏终端 → `agentos-shell` |
| 中文 | Noto CJK 字体，fcitx5 拼音输入法，`zh_CN.UTF-8`，时区 Asia/Shanghai |
| AgentOS | `/usr/local/bin/agentosd`，以及 `agentos-setup`、`agentos-install`、`agentos-session`、`agentos-shell` |
| 开机服务 | `agentos-growroot`（首次启动扩展分区）、`agentos-selfcheck`（用离线规划器跑一遍 agentosd） |

## 目前的限制

- 只支持 UEFI，不支持老式 BIOS/CSM 启动。
- 整盘安装，暂不支持和 Windows 双系统共存。
- P0 阶段 `agent` 用户无需密码即可 `sudo`，开机自动登录；Agent 能做什么由 AgentOS 策略引擎把关，但这台机器本身没有登录保护。
- 拼音输入法依赖 sway 的输入法协议，需要在真机上进一步验证。
