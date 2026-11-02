# 更新日志

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
