# 更新 AgentOS

已经装好的机器不用重新写镜像。`agentos-update` 从 GitHub Release 下载 AgentOS 这一层的更新包，验证、自检通过后才替换文件，出问题可以一步回滚。基础系统（内核、驱动、库）仍然由发行版的 apt 负责。

```sh
agentos-update --check            # 只看看有没有新版本
sudo agentos-update               # 安装最新版本
sudo agentos-update --to v0.4.0   # 安装指定版本（也可以用来降级）
sudo agentos-update --system      # 再顺便 apt full-upgrade 基础系统
sudo agentos-update --rollback    # 回到更新前的版本
sudo agentos-update --file agentos-update-x86_64.tar.gz   # 离线安装（旁边要有 .sha256）
```

图形桌面里也可以点左上角 AgentOS 图标 →「检查更新…」。

## 更新时发生什么

1. 查询 `https://api.github.com/repos/Integ/agentos/releases/latest`（或 `--to` 指定的版本），和本机的 `/usr/share/agentos/release` 比较。
2. 下载 `agentos-update-x86_64.tar.gz` 和它的 `.sha256`，校验不一致就停止。
3. 解包到临时目录并检查内容：只接受 `usr/…`、`etc/systemd/system/agentos-*` 和 `etc/udev/rules.d/*agentos*` 下的普通文件，有符号链接、设备文件或其他路径就整个拒绝；去掉 setuid 位。
4. **自检**：启动一个临时的新 `agentos-taskd`（离线规划器），用新的 `agentos` 命令提交一条意图并等它完成。更新包里没有任务服务或 `agentos` 命令、或者自检失败，就停止，本机什么都没改。
5. 把即将被覆盖的文件备份到 `/var/lib/agentos-update/rollback/`（只保留上一个版本），然后逐个文件"写到旁边再改名"地替换，正在运行的程序不会读到写了一半的文件。
6. 重新加载 systemd 和 udev 配置，重启正在运行的 `agentos-taskd`（它的用户单元拉起新版本；重启前未完成的任务标为“已中断”）。图形桌面 `agentos-shell` 在下次登录或重启后换成新版本。

从 v0.4 及更早的版本更新上来时，跑的是机器上原有的 `agentos-update`：它要求更新包里有 `agentosd`，并拿它做自检。所以更新包里还放着一个同名的垫片脚本，它用更新包自己的任务服务完成这次自检；更新完成后，重新登录之前的旧桌面会话调用它时，它只是安静等待。重新登录后桌面连接任务服务，任务服务第一次启动时把 `~/.local/share/agentos` 里的数据迁过来（见[持久任务](persistent-workspace.md)）。旧的 `agentos-daemon` 脚本会留在 `/usr/local/bin` 里（更新不删除文件），已经没有谁调用它。垫片会在下一个版本删除。

从没有任务服务的版本（v0.2.x）更新上来时，任务服务在下次登录后才开始运行：它由登录会话启动，更新包里只能带单元文件，带不了“开机启用”需要的符号链接。

`/etc/agentos/agentos.env`（API key、规划器、Home Assistant 等设置）、PIN、笔记、审计日志和插件都不在更新包里，更新和回滚都不会动它们。

## 发布流程

发布 GitHub Release 时，Image 工作流除了两份磁盘镜像，还会构建更新包（和镜像用同一套在 Debian 12 上编译的二进制），用 `image/test/update-test.sh` 测一遍，再附到 Release 上。本地生成更新包：

```sh
BUNDLE_ONLY=1 image/build-in-docker.sh   # out/agentos-update-x86_64.tar.gz
```

`image/test/update-test.sh` 不需要 root：它用一个假的安装目录和本地的假 GitHub API，测试检查、安装、已是最新、回滚、校验失败、非法路径、符号链接、缺少任务服务、新版本自检失败和离线安装，还用 v0.4.0 原样的 `agentos-update`（`image/test/agentos-update-v0.4.0`）安装同一个更新包。CI 每次都会跑。

## 已知限制

- 真实性靠 HTTPS 和 GitHub 账号安全：`.sha256` 和更新包来自同一个 Release，能防传输损坏，防不了 Release 本身被篡改。后续计划给更新包加离线签名（minisign / ed25519），本机只信任镜像里内置的公钥。
- 不会删除新版本里不再需要的旧文件（只覆盖和新增）。
- **v0.1.0 不能原地升级，请重新写入 v0.2.0 镜像。** v0.1.0 镜像有一个属主错误（根文件系统沿用了 CI 的 uid 1001），`sudo` 会要密码，而 `agent` 账户根本没有密码，所以无法以 root 运行 `agentos-update`。v0.2.0 起镜像构建时会检查 `agent` 能免密 sudo，之后的版本都可以用 `agentos-update` 在线升级。v0.1.0 里值得留的数据在 `~/.local/share/agentos/`（笔记、插件）和 `/var/lib/agentos/audit.jsonl`（审计日志），重装前可以拷到 U 盘上，重装后放回同一位置。
- 更新包只覆盖 `usr/…` 和 AgentOS 自己的 systemd 单元、udev 规则。`/etc` 下其他由镜像提供的文件（`sudoers.d`、`journald.conf.d`、`modules-load.d`、终端 `foot.ini`）在线更新不会改动，要换这些得重新写镜像。
- 新的 `agentos-update` 要求更新包里有任务服务，所以不能用 `--to` 退回 v0.2.x；而且任务快照的格式只升不降，退回旧版本前要先移走状态目录。
- 还没有 A/B 分区：内核和基础系统的更新仍是 apt 的原地升级。
