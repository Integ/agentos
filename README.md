# AgentOS

一个以 Agent 为一等公民的操作系统，目标是让人和 AI 更顺畅地协作：Agent 通过各种输入设备接收人的意图，通过输出设备与人沟通并展示自己的工作。

[![AgentOS 介绍视频（1 分 42 秒，点击播放）](docs/media/agentos-intro-poster.jpg)](docs/media/agentos-intro.mp4)

<sub>▶ 点击封面播放 [介绍视频](docs/media/agentos-intro.mp4)（1 分 42 秒，720p）：功能一览与安装方法。</sub>

AgentOS 构建在 Linux 内核之上，在用户态自建完整的 Agent OS 层：意图总线、Agent 运行时、统一的感知/表达设备抽象，以及基于能力令牌和风险分级的权限模型。

当前版本：**v0.3.0**（[更新日志](CHANGELOG.md)）。

新增持久任务工作台和语音优先的情景式 Web Shell，与现有系统服务及原生桌面并行提供。多轮规划、MCP、插件、智能家居、PIN 和镜像更新继续由 `agentosd` / `agentos-shell` 提供；新的 `agentos-taskd` / `agentos-web-shell` 使用独立协议和状态目录。见[持久任务与 Web Shell](docs/persistent-workspace.md)。

## 现在能做什么

- **管家 Agent + 子 Agent**：用自然语言下达意图，管家拆解任务、派给子 Agent；任务即进程，每一步写检查点，守护进程重启后接着做；子 Agent 通过共享黑板协作，可选模型裁判在有影响的步骤前复核。见 [docs/agents.md](docs/agents.md)。
- **云端大模型规划**：Claude（默认 `claude-opus-5-5`）或任意 OpenAI 兼容接口（OpenAI、DeepSeek、通义千问、Ollama 等）；也有离线规则规划器。限流和 5xx 自动重试。
- **玻璃风格桌面**：WebKit 写的 `agentos-shell`，带玻璃风格终端和确认弹窗。见 [docs/shell.md](docs/shell.md)。
- **图形设置助理**：首次进入桌面时出现 Liquid Glass 风格的设置助理，一页一件事：Wi‑Fi、模型接口（带 DeepSeek/通义千问/Ollama 预设）、语音、Home Assistant（可当场测试连接）、PIN、安装到硬盘。系统菜单「网络和 AI 设置…」可随时重新打开。见 [docs/shell.md](docs/shell.md#设置助理)。
- **语音输入与朗读**：按住麦克风按钮或 **Caps Lock** 说话，松开发送，Caps Lock 指示灯就是录音灯；回复可以读出来。只在按住时录音，没有唤醒词。见 [docs/voice.md](docs/voice.md)。
- **智能家居**：接入 Home Assistant，开灯、调空调是 L1，场景脚本是 L2，开锁、撤防、开车库门是 L3。见 [docs/smart-home.md](docs/smart-home.md)。
- **MCP 工具**：`mcp.json` 里配置的 MCP server，工具自动成为管家的能力。见 [docs/mcp.md](docs/mcp.md)。
- **自我进化**：让 AgentOS 给自己写新能力，以沙箱插件形式安装，可回滚。见 [docs/self-evolution.md](docs/self-evolution.md)。
- **安全模型**：能力令牌 + 风险分级确认（L0–L3）；污点追踪（网络插件、MCP 输出会让后续有影响的操作都要确认）；L3 操作可要求 PIN；审计日志带 SHA-256 哈希链，可校验；插件有 bubblewrap 沙箱和资源上限（内存 1 GiB、CPU 10 秒等）。
- **稳健性与运维**：守护进程崩溃时写入 journal 和 `crash.txt`、退避重启，并在桌面「最近活动」里提示；journal 跨重启保留；`sudo agentos-update` 在线更新，先自检再替换，可回滚。

更多文档：

- 设计草案：[AgentOS 架构设计草案](https://claude.ai/artifact/RzRDjHkBbJVbnyFDRSZ4BN)
- 架构概要与进度：[docs/architecture.md](docs/architecture.md)
- 安装：[docs/install.md](docs/install.md)，在线更新：[docs/update.md](docs/update.md)

## 安装到 PC

到 [Releases](https://github.com/Integ/agentos/releases) 下载镜像（Debian 13 或 Ubuntu 24.04 两种），写进 U 盘，从 U 盘启动即可使用；设置助理里可以一键装到硬盘。需要 x86-64、UEFI 启动，安全启动可以保持开启。详见 [docs/install.md](docs/install.md)。

- 镜像自带 Intel、NVIDIA、MediaTek 显卡固件、声卡 SOF 固件和 CPU 微码，Intel N100 这类核显小主机可以直接进桌面。
- 内置 fcitx5 拼音输入法；笔记本触摸板默认开启轻触点击。
- 图形桌面起不来时自动退回文本模式（fbterm，中文正常显示），原因记在 `journalctl -t agentos-sway`。

### 升级

已经装好的机器运行 `sudo agentos-update`（或系统菜单「检查更新…」），可 `--rollback`。详见 [docs/update.md](docs/update.md)。

- **从 v0.1.0 升级**：必须重新写入镜像（v0.1.0 的 `sudo` 会要密码，无法运行更新）。
- **从 v0.2.0 升级**：建议重新写入镜像。在线更新只更新 AgentOS 自己的文件，不会安装新的系统软件包，所以 v0.2.2 的显卡固件和输入法候选窗口修复拿不到。不想重装的话，在线更新后再运行（Ubuntu 镜像不需要第一条）：

  ```sh
  sudo apt install firmware-intel-graphics firmware-intel-misc firmware-nvidia-graphics firmware-mediatek firmware-sof-signed intel-microcode amd64-microcode
  sudo apt install fcitx5-frontend-gtk3 fbterm
  ```

## 仓库结构

```
crates/
  core/      核心类型：Intent、Percept、Expression、Capability，Sensor/Output trait
  policy/    策略引擎：能力授权 + 风险分级确认 + 污点追踪
  audit/     只追加审计日志（JSON Lines），可回放
  agentosd/  P0 守护进程：感知中枢 → 管家 Agent → 表达中枢；插件运行时与自我进化（evolve）
  shell/     图形界面 agentos-shell：玻璃风格桌面，通过 Unix socket 连接 agentosd
  taskd/     独立持久任务服务 agentos-taskd（协议 v1）
  web-shell/ 情景式浏览器界面 agentos-web-shell：模型布局与连续实时语音
  cli/       agentos 命令与独立终端任务工作台
  runtime/   持久任务、分页、通知、取消与中断恢复
  ipc/       严格校验、受限帧与本机连接协议
image/       可启动磁盘镜像：构建脚本、系统集成文件（设置助理、会话脚本、agentos-update）、OTA 更新包、QEMU 冒烟测试
```

## 快速开始

需要 Rust 1.88+。

```sh
cargo run -p agentosd
```

各程序都支持 `--help` 和 `--version`（例如 `cargo run -p agentosd -- --help`），写错的参数会报错退出，而不是被忽略。

然后输入意图，例如：

```
记一下 买牛奶
看看笔记
现在几点
清空笔记      # L3 不可逆操作，会先请求确认
y
```

### 图形界面

```sh
cargo run -p agentosd -- --listen $XDG_RUNTIME_DIR/agentos.sock
cargo run -p agentos-shell -- --windowed   # 需要 libwebkit2gtk-4.1-dev
```

按住界面上的麦克风按钮（或 Caps Lock）可以语音输入，配置方法见 [docs/voice.md](docs/voice.md)。

### 持久任务与情景式 Web Shell

在三个终端中分别启动（同一个状态目录）：

```sh
cargo run -p agentos-taskd -- --planner rule --state-dir /tmp/agentos-workspace
cargo run -p agentos-web-shell -- --state-dir /tmp/agentos-workspace
cargo run -p agentos-cli -- --state-dir /tmp/agentos-workspace workbench
```

打开 Web Shell 输出的完整本机链接。它支持模型根据任务和上下文生成受限布局、连续实时语音及开口打断；API 密钥仅由服务端读取。完整配置见[情景式 Shell](docs/ambient-shell.md)。

这组入口当前提供时钟与笔记能力；MCP、插件、智能家居和多轮规划使用上面的原生桌面入口。两组服务的 socket、任务恢复语义、笔记和审计格式各自独立，不互相读取或迁移状态。现有镜像仍默认启动原生桌面。

![AgentOS 桌面](docs/images/shell-desktop.jpg)

详见 [docs/shell.md](docs/shell.md)。

### 用 Claude 做规划

设置 `ANTHROPIC_API_KEY` 后，`agentosd` 自动改用 Claude 规划器（默认模型 `claude-opus-5-5`）：每个意图调用一次 Messages API，所有能力作为工具提供，模型返回的工具调用按顺序成为计划步骤，仍然逐步经过策略引擎。

```sh
export ANTHROPIC_API_KEY=...
cargo run -p agentosd                    # 自动使用 Claude
cargo run -p agentosd -- --planner rule  # 强制使用离线规则规划器
```

可选环境变量：`AGENTOS_MODEL`（换模型）、`ANTHROPIC_BASE_URL`（换 API 地址）。

### 用 OpenAI 兼容接口做规划

任何兼容 OpenAI Chat Completions 的服务都可以（OpenAI、DeepSeek、通义千问、Moonshot，或本地的 Ollama、vLLM、LM Studio），需要支持 function calling。

```sh
# 云端服务
export OPENAI_API_KEY=...
export OPENAI_BASE_URL=https://api.deepseek.com/v1   # 不设则为 https://api.openai.com/v1
export OPENAI_MODEL=deepseek-chat
cargo run -p agentosd -- --planner openai

# 本地 Ollama（不需要 key）
OPENAI_BASE_URL=http://localhost:11434/v1 OPENAI_MODEL=qwen3 cargo run -p agentosd
```

规划器选择：`--planner claude|openai|rule`（或环境变量 `AGENTOS_PLANNER`）。不指定时，设置了 `ANTHROPIC_API_KEY` 就用 Claude，否则配置了 OpenAI 兼容接口就用它，都没有则用离线规则规划器。

### 自我进化

配置了大模型（Claude 或 OpenAI 兼容接口）后，可以让 AgentOS 给自己写新能力：

```
学会：把摄氏度换算成华氏度   # 生成插件、沙箱自测，展示代码和权限
y                             # 确认安装（L3）
37 摄氏度是多少华氏度          # 新能力立即可用（需要 LLM 规划器）
看看插件
回滚 unit.c_to_f
```

插件在 bubblewrap 沙箱中运行（`apt install bubblewrap python3`），存放在 `~/.local/share/agentos/evolve`（`--home` 或 `AGENTOS_HOME` 可改）。详见 [docs/self-evolution.md](docs/self-evolution.md)。

数据目录默认是 `~/.local/share/agentos`（`--home` 或 `AGENTOS_HOME` 可改），笔记保存在其中的 `notes.json`，重启后还在。审计日志默认写到数据目录里的 `audit.jsonl`，可用 `--audit <路径>` 或环境变量 `AGENTOS_AUDIT` 指定。每条记录都带上一条的 SHA-256（哈希链），`agentosd --verify-audit` 可以检查中间有没有记录被改过、删过或插入过；想防止整段重写或截掉末尾，可以把它输出的“链头”另外记下来。

### 智能家居

设置 `HASS_URL` 和 `HASS_TOKEN`（Home Assistant 长期访问令牌）后，管家可以查看和控制家里的设备：开灯、调空调、拉窗帘属于 L1，运行脚本和场景属于 L2，开锁、撤防、开车库门属于 L3。详见 [docs/smart-home.md](docs/smart-home.md)。

### MCP 工具

在数据目录放一个 `mcp.json`（格式同 Claude Desktop 的 `mcpServers`），MCP server 的工具就会成为管家的能力。没有特别设置时，每次调用都要确认，结果也当作不可信。详见 [docs/mcp.md](docs/mcp.md)。

### 不可逆操作的 PIN

L3 操作（删除数据、安装插件等）默认只要点“确认执行”。设置 PIN 后，确认框会要求输入 PIN，键盘回复 y 或语音“确认”都不能单独通过：

```bash
agentosd --set-pin      # 输入两次 4–12 位数字
agentosd --clear-pin    # 取消
```

装机镜像的设置向导里也有设置 PIN 这一步。PIN 以加盐迭代哈希存在数据目录的 `pin.json`（0600），连续输错 5 次锁定 5 分钟，每次核对都记入审计日志（不含 PIN 本身）。控制台模式下先输入 `pin 你的PIN` 再回复 y。它能挡住提示注入、旁人和随手乱试，但挡不住已经能以桌面用户身份运行代码的人。

## 开发

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo deny check     # 依赖漏洞（RustSec）、许可证、来源；cargo install --locked cargo-deny
```

CI 每次提交和每周一都会跑 `cargo deny check`（配置见 [deny.toml](deny.toml)），新披露的依赖漏洞会让 CI 变红。
