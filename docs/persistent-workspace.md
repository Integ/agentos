# AgentOS

本文介绍 `agentos-taskd`、`agentos-web-shell` 和 `agentos` CLI 组成的持久任务工作台。原有 `agentosd` 与原生 `agentos-shell` 继续提供多轮规划、MCP、插件和智能家居，参见[主项目说明](../README.md)。两组入口使用独立的 socket、状态和审计格式，不能互换连接或共用数据目录。镜像默认保留原生桌面。

AgentOS 探索以人的意图为入口、以 Agent 为执行者的操作系统：人交付目标，系统协调模型、能力、设备和权限，并持续解释执行状态。目标架构是 **Linux 内核 + Rust 用户态 Agent OS 层**；当前是可运行的单用户本地任务服务。

**v0.3：后台系统服务与独立终端工作台。** Unix socket 将任务内核与交互界面分开，支持多个客户端、实时变更通知、分页历史和跨连接批准。退出工作台后，后台任务继续执行。

- [v0.3 设计与实现边界](v0.3-design.md)
- [Unix socket 协议 v1](protocol-v1.md)
- [架构概要](architecture.md) · [v0.2 审查与持久任务设计](v0.2-design.md)
- [情景式图形 Shell：设计、启动与边界](ambient-shell.md)

## 快速体验

需要 **Rust 1.88+**，支持 Linux 和 macOS。无需模型服务即可运行完整核心体验。

终端一启动后台服务：

```sh
cargo run -p agentos-taskd -- --planner rule --state-dir /tmp/agentos-demo
```

终端二连接独立工作台：

```sh
cargo run -p agentos-cli -- --state-dir /tmp/agentos-demo workbench
```

工作台显示连接状态、任务页、步骤进度及选中任务详情，并随后台变化刷新。输入自然语言提交任务：

```text
记一下 买牛奶
看看笔记
清空笔记
恢复笔记
/task task-1
```

笔记与任务立即持久保存。清空保留一份恢复快照，恢复会保留清空后新增的笔记；恢复之前不能再次清空非空笔记，以免覆盖唯一备份。正常输入下，这些 L1 可恢复动作自动执行。

工作台控制命令：

| 命令 | 作用 |
| --- | --- |
| `/tasks`、`/refresh` | 刷新当前任务页和详情 |
| `/next`、`/prev`、`/latest` | 更早历史、返回上一页、最新任务 |
| `/task task-1` | 选中任务，显示意图、计划参数、步骤结果与确认请求 |
| `/cancel task-1` | 停止后续步骤；已经开始的步骤可能仍完成 |
| `/approve task-1 1` | 批准当前已展示详情中的具体确认请求 |
| `/capabilities`、`/help` | 查看能力与帮助 |
| `/quit` | 退出工作台，任务服务继续运行 |

服务不读取 stdin，关闭工作台或终端输入不会取消任务。向服务发送 SIGINT/SIGTERM 会停止接收新连接，最多等待五秒排空已接收工作，再把剩余任务标为已中断；重启不会自动重复执行。

## 语音优先的图形 Shell

保持上面的 `agentos-taskd` 运行，在另一终端启动情景式界面：

```sh
cargo run -p agentos-web-shell -- --state-dir /tmp/agentos-demo
```

在浏览器打开终端输出的完整 `http://127.0.0.1:4317/#session=…` 链接；片段是本次启动的本地访问凭证，请勿分享。点击麦克风并允许后，实时语音支持连续交流、自动回应和开口打断，不需要逐次提交转录文字。音频通过 WebRTC 直接传给实时模型，字幕只供查看；明确的系统操作经受限工具交给现有后台执行。

Shell 读取当前进程的 `OPENAI_API_KEY`，通过服务端 Responses API 生成受限场景 JSON；模型根据已提交意图、真实任务与笔记摘要、专注目标和屏幕尺寸选择组件、顺序、宽度与布局。模型依次取 `AGENTOS_UI_MODEL`、`OPENAI_MODEL`，默认 `gpt-5.6-terra`；支持 `OPENAI_BASE_URL`。不需要把密钥写入仓库。可在偏好中关闭“AI 情景布局”，或以 `AGENTOS_UI_MODEL=off` 启动；未配置、失败或响应无效时保留本地情景布局，并显示当前状态。输入草稿、原始音频和批准令牌不参与布局生成，任务数据和批准控件始终绑定真实状态。

实时语音独立读取 `AGENTOS_VOICE_MODEL`，默认使用 `gpt-realtime-2.1`；`AGENTOS_VOICE_MODEL=off` 可禁用，`AGENTOS_VOICE_VOICE` 可选择声音，默认为 `marin`。它复用 Shell 进程的 `OPENAI_API_KEY`，不把项目密钥传给浏览器。结束或隐藏页面会关闭麦克风，连接中断后不自动重连。

未连接后台时可体验本地笔记与专注计时；浏览器笔记与后台笔记独立，不自动同步。`/` 聚焦输入，`Alt+V` 连接或结束实时对话，`Esc` 打断回应，`Shift+Esc` 结束对话，`Alt+1–4` 切换空间。打断声音不取消已接收任务，待批准动作仍需在界面明确批准。完整行为与实现边界见[图形 Shell 设计](ambient-shell.md)。

## 独立 CLI 与脚本

`agentos-cli` 包生成 `agentos` 可执行文件。源码运行时用 `cargo run -p agentos-cli --`，安装二进制后直接用 `agentos`：

```sh
agentos submit 记一下 买牛奶
agentos tasks --limit 10
agentos tasks --before task-20 --state succeeded
agentos task task-1
agentos cancel task-2
agentos --json tasks
agentos watch
```

默认连接相同的状态目录；可在命令前指定 `--state-dir` 或 `--socket`。`--json` 适合脚本，`watch --after <revision>` 接收实时变更与重新同步通知。

独立批准命令要求先读取任务详情，再显式提供其中的确认令牌：

```sh
agentos approve task-3 1 --confirmation <令牌>
```

批准绑定任务、步骤、随机令牌、同一系统用户和五分钟有效期；入队及执行时都校验，随后仍重新检查能力授权。普通 `y` 只会产生新任务。没有可信身份核验设备的 L3 操作继续拒绝执行。

工作台断线后自动重连并重新获取视图，**不会自动重发提交或批准**。若发送请求后没有收到响应，操作可能已经生效，请先检查历史与实际结果。

## 本地状态与连接

状态目录依次取 `--state-dir`、`AGENTOS_STATE_DIR`、绝对路径 `$XDG_STATE_HOME/agentos`、`$HOME/.local/state/agentos`。socket 依次取 `--socket`、`AGENTOS_TASK_SOCKET`、状态目录内的 `control.sock`。

| 文件 | 内容 |
| --- | --- |
| `control.sock` | 仅本用户可访问的本地控制端点，权限 `0600` |
| `tasks.json` | 稳定任务 ID、持久 revision、生命周期、步骤与结果 |
| `notes.json` | 笔记与清空恢复快照 |
| `audit.jsonl` | 意图、策略、调用开始、结果和表达记录 |
| `.lock` | 状态目录的进程独占锁 |

服务及客户端通过内核 peer credentials 校验本机 UID。多个相同用户的终端共享同一任务空间，来源身份跨连接稳定；同一 UID 下的进程属于同一信任域。自定义 socket 应放在本人拥有、其他用户不可写的目录，不能直接放在共享可写的 `/tmp` 根目录；路径也需符合系统 Unix socket 长度限制。

任务与笔记原子替换并同步落盘，审计同步只追加。能力调用前保存调用开始与运行状态；关键持久化失败停止后续执行。坏数据拒绝加载且保留原文件。系统不承诺跨任务、日志和能力数据的原子事务，以中断态表达结果不确定性。

审计路径可用 `--audit` 或 `AGENTOS_TASK_AUDIT` 指定。状态目录与审计文件都限制单一写入者；不同状态目录应使用独立审计文件。

v0.2 快照自动升级，保留任务、结果和笔记；未完成任务中断、旧确认作废。新版快照不承诺旧程序可读。v0.1 的内存笔记无法恢复，原仓库 `agentos-audit.jsonl` 不会自动导入或重放。当前数据均为本地明文。

## 模型规划器

保留 Claude、OpenAI 兼容接口和离线规则规划器：

```sh
cargo run -p agentos-taskd -- --planner rule
cargo run -p agentos-taskd -- --planner claude
cargo run -p agentos-taskd -- --planner openai
```

`--planner` 优先于 `AGENTOS_PLANNER`。未指定时，有 `ANTHROPIC_API_KEY` 则选 Claude，否则有完整 OpenAI 兼容配置则选该接口，其余使用规则规划器。显式指定但配置不完整会报错。

| 规划器 | 配置 |
| --- | --- |
| Claude | `ANTHROPIC_API_KEY`；可选 `AGENTOS_MODEL`、`ANTHROPIC_BASE_URL` |
| OpenAI 兼容 | `OPENAI_MODEL`，以及 `OPENAI_API_KEY` 或 `OPENAI_BASE_URL`；本地兼容服务可不设 key |
| Rule | 无需配置，支持上面的笔记和时间示例 |

模型一次返回顺序工具列表，所有调用仍经过运行时校验与策略。网络失败、截断或畸形工具响应形成失败任务，不执行半个计划。当前不支持依赖上一步结果的多轮规划。

## 仓库结构与开发

```text
crates/
  core/      意图、表达、能力和设备类型
  runtime/   持久任务、变更序号、分页、通知、取消与恢复
  ipc/       协议 v1、受限帧读写与本机路径约定
  policy/    能力授权、风险与污点标记策略
  audit/     严格校验与同步落盘的只追加日志
  taskd/  Unix socket 服务、单执行 worker、规划器与持久笔记
  cli/       独立 agentos 命令与实时终端任务工作台
  web-shell/     语音优先情景画布与本地 HTTP / IPC 桥
```

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test crates/web-shell/assets/*.test.mjs
```

测试覆盖持久化、快照迁移、取消竞态、批准与撤权、审计故障、协议帧限制、真实 Unix socket 交互和客户端断线行为。CI 配置 Linux/macOS 检查及 Rust 1.88 最低版本编译。

当前为单用户、单 worker 的用户态服务，能力仍在守护进程内运行。下一阶段推进能力进程监督、MCP 与 Linux 资源隔离，再发展多轮规划、预算和并发调度。
