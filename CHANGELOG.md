# 更新日志

## 未发布

让结果可信：任务的结局、时间和任务历史都如实反映发生了什么。

- **“没听懂”不再记成“已完成”**：规划器没有办法处理的意图现在以独立的结局结束。`agentos-taskd` 新增任务终态 `unhandled`（CLI 显示“未能处理”，Web Shell 显示“没有听懂”）；`agentosd` 的任务新增 `unhandled` 和 `failed`，`done` 事件带上 `state`。规划器直接给出回答的任务仍是成功。
- **规划失败是错误，不是回复**：`agentosd` 的 Claude 和 OpenAI 兼容规划器在接口不可达、被拒绝或响应无效时，任务记为失败并以错误报告；此前会被当成普通回复、任务记为完成。子 Agent 规划失败或无法处理时，交还给管家的结果标记为失败。
- **`agentos-taskd` 的 `clock.now` 返回本机时区的日期和时间**，带星期和 UTC 偏移；此前只返回 UTC 时刻。结果里保留 `unix_ms`。
- **协议 v1 新增 `read`**：直接查询已授权的只读（L0）能力，不创建任务、不经过规划器。有副作用的能力和输出不可信的能力会被拒绝。新增错误码 `not_readable`、`capability_failed`。见 [docs/protocol-v1.md](docs/protocol-v1.md#直接读取)。
- **Web Shell 不再替用户提交意图**：读取后台笔记改走 `read`。此前连接后台、打开“记忆”空间或笔记变化后，界面会自动提交一条“看看笔记”任务，留在任务历史和审计日志里，接入模型规划器时还会多一次模型调用。
- **CLI**：`agentos submit --wait [--timeout 秒]` 等到任务结束再返回，退出码区分成功、未成功、等待批准和超时；`agentos read <能力>` 直接读取；`tasks --state unhandled` 可筛选；`capabilities` 的风险等级写作 L0–L3。
- **`--help` 和 `--version`**：`agentosd`、`agentos-shell`、`agentos-taskd`、`agentos-web-shell` 都支持。此前 `agentosd --help` 会直接启动守护进程，`agentos-shell --help` 会直接打开桌面窗口。`agentosd` 和 `agentos-shell` 遇到不认识的参数现在报错退出，不再忽略。

**兼容性**：`unhandled` 是协议 v1 任务状态的新增取值，`read` 是新增方法。严格解析状态的旧客户端需要升级后才能读取含 `unhandled` 的任务；新快照不保证旧程序可读。

## v0.3.0

- 新增 `agentos-taskd` 持久任务服务、协议 v1、独立 `agentos` CLI 和终端工作台。
- 新增 `agentos-web-shell`：基于真实任务上下文生成受限布局，支持 WebRTC 连续语音与随时打断。
- 保留 `agentosd` / `agentos-shell` 的多轮规划、MCP、插件、智能家居、PIN、哈希链审计、设置助理和镜像更新。新旧入口使用独立状态目录与协议。
- IPC 无字段命令同样严格拒绝未知字段，新增全命令校验、线格式兼容与真实 socket 回归测试。
- 最低 Rust 版本更新为 1.88。

## v0.2.2（2026-11-02）

这一版主要是真机测试（Intel N100 小主机、笔记本）发现的问题修复，外加图形设置助理。没有 v0.2.1，版本号直接从 v0.2.0 跳到 v0.2.2。

**从 v0.2.0 升级：** `sudo agentos-update` 会更新 AgentOS 自己的文件（桌面、设置助理、会话脚本、sway 配置），但不会安装新的系统软件包。显卡固件和输入法候选窗口这两个修复靠新软件包，在线升级拿不到。建议重新写入镜像；不想重装的话，在线升级后再运行（Ubuntu 镜像不需要第一条）：

```
sudo apt install firmware-intel-graphics firmware-intel-misc firmware-nvidia-graphics firmware-mediatek firmware-sof-signed intel-microcode amd64-microcode
sudo apt install fcitx5-frontend-gtk3 fbterm
```

### 改进

- **全新的设置助理**：首次进入桌面时不再弹出终端里的对话框，而是一个 Liquid Glass 风格的图形设置助理：一页一件事，用鼠标点选；Wi‑Fi 列表点一下就能加入；OpenAI 兼容接口带 DeepSeek、通义千问、Ollama 预设；Home Assistant 可以当场测试连接；输入错误就地提示；已保存的密钥留空即保持不变，手动加在 env 文件里的设置会保留。系统菜单「网络和 AI 设置…」打开同一个界面。文本模式仍用原来的向导。见 [docs/shell.md](docs/shell.md#设置助理)。（#30）
- **笔记本触摸板轻触点击**：默认开启，单指点按是左键、双指右键、三指中键。（#31）

### 修复

- **Debian 镜像在 Intel 核显（如 N100）上进不了桌面**：Debian 13 把显卡固件拆成了单独的包，镜像里缺 i915 GuC 固件，sway 启动失败。现在镜像带上 Intel、NVIDIA、MediaTek 固件、声卡 SOF 固件和 CPU 微码。（#29）
- **桌面起不来时多一次机会**：先等显卡设备就绪；GPU 渲染失败时用 CPU 渲染再试一次；失败原因写入日志（`journalctl -t agentos-sway`）。（#29）
- **文本模式中文显示成方块**：文本模式改在 fbterm 里运行，中文正常显示。（#29）
- **拼音输入法不显示候选字**：补上 fcitx5 的 GTK 输入模块。（#29）

## v0.2.0（2026-11-02）

**从 v0.1.0 升级：请重新写入镜像。** v0.1.0 镜像里 `sudo` 会要密码（账户没有密码），无法以 root 运行 `agentos-update`。从 v0.2.0 开始可以用 `sudo agentos-update` 在线升级。详见 [docs/update.md](docs/update.md)。

### 新功能

- **超级管家**：管家把任务拆开派给子 Agent；任务即进程，有检查点、可恢复；共享黑板；可选的模型裁判（`AGENTOS_JUDGE=1`）在执行有影响的步骤前复核。见 [docs/agents.md](docs/agents.md)。（#16）
- **Home Assistant 智能家居**：`home.list` / `home.state` / `home.control` / `home.run` / `home.secure`，开锁、开车库门等按 L3 处理，每次确认。见 [docs/smart-home.md](docs/smart-home.md)。（#20）
- **MCP 能力**：`~/.local/share/agentos/mcp.json` 里配置的 MCP 服务器，其工具自动注册为能力，按风险分级、输出视为不可信。见 [docs/mcp.md](docs/mcp.md)。（#21）
- **L3 PIN**：设置 PIN 后，不可逆操作确认时要输入 PIN（`agentosd --set-pin`，或设置向导）。PIN 不写入审计日志。（#18、#22）
- **设置向导**：新增 Home Assistant 连接和 PIN 两步。（#22）
- **Caps Lock 按键说话**：按住 Caps Lock 说话，键盘指示灯作为录音灯。（#19）
- **在线更新（OTA）**：`agentos-update` 从 GitHub Release 下载更新包，校验 sha256、先用新版本自检再替换，可 `--rollback`；系统菜单里有「检查更新…」。（#25）

### 安全与稳健性

- **污点传播**：网络插件、未沙箱插件和 MCP 工具的输出会把本次任务标记为不可信，之后有影响的操作一律要确认。（#18）
- **审计日志哈希链**：每条记录带上一条的 SHA-256，`agentosd --verify-audit` 检查是否被改动；v0.1.0 的旧日志照常读取。（#23）
- **插件资源限制**：内存 1 GiB、文件 64 MiB、CPU 10 秒、打开文件 256 个。（#24）
- **崩溃日志**：panic 报告写入 journal 和 `crash.txt`，下次启动在活动面板里显示；守护进程反复崩溃时退避重启；journal 跨重启保留。（#26）
- 笔记重启后保留、`clock.now` 用本地时间、审计日志容忍写了一半的行、socket 单行 64 KiB 上限。（#17）
- 规划器对 408/429/5xx/529 自动重试。（#18）
- CI 加入 cargo-deny：已知漏洞、许可证和依赖来源检查，每周跑一次。（#27）

### 修复

- 镜像里 `sudo` 要密码：根文件系统不再沿用 CI 的 uid，构建时检查 `agent` 能免密 sudo。（#15）

### 界面

- 玻璃风格终端和设置向导配色。（#19）

### 尚未在真机上验证

- Caps Lock 按键说话和指示灯、真实麦克风和扬声器
- 连接真实的 Home Assistant 和真实的 MCP 服务器
- 从一个真实发布版本在线升级到下一个版本（目前只有 CI 里的模拟测试）

## v0.1.0（2026-11-01）

第一个预览版：管家核心、Claude 和 OpenAI 兼容规划器、可启动安装镜像（Debian 13 / Ubuntu 24.04）、自我进化插件、玻璃风格桌面、语音交互。
