# AgentOS

本文介绍任务内核 `agentos-taskd` 以及它的客户端：`agentos` CLI、`agentos-web-shell` 和原生桌面 `agentos-shell`。多轮规划、子 Agent、黑板、MCP、插件、智能家居、PIN、按住说话和朗读都由这个服务提供；它是唯一的守护进程，见[架构](architecture.md)。

AgentOS 探索以人的意图为入口、以 Agent 为执行者的操作系统：人交付目标，系统协调模型、能力、设备和权限，并持续解释执行状态。目标架构是 **Linux 内核 + Rust 用户态 Agent OS 层**；当前是可运行的单用户本地任务服务。

**v0.4：并行任务、会话线程、提问、预算、预授权和 MCP server，见[设计](v0.4-design.md)。v0.3：后台系统服务与独立终端工作台。** Unix socket 将任务内核与交互界面分开，支持多个客户端、实时变更通知、分页历史和跨连接批准。退出工作台后，后台任务继续执行。

- [v0.3 设计与实现边界](v0.3-design.md)
- [v0.4 设计：让 Agent 成为一等公民](v0.4-design.md)
- [Unix socket 协议 v1](protocol-v1.md)
- [架构概要](architecture.md) · [v0.2 审查与持久任务设计](v0.2-design.md)
- [情景式图形 Shell：设计、启动与边界](ambient-shell.md)

## 在 AgentOS 镜像里

镜像里已经装好 `agentos-taskd`、`agentos` 和 `agentos-web-shell`。任务服务是桌面用户的 systemd 用户服务，登录后自动启动，读取 `/etc/agentos/agentos.env` 里的规划器和密钥设置，状态放在 `~/.local/state/agentos`。在终端（桌面里的 foot）直接用：

```sh
agentos                                   # 终端工作台
agentos submit --wait 记一下 买牛奶
systemctl --user status agentos-taskd     # 服务状态
journalctl --user -u agentos-taskd        # 服务日志
```

修改 `agentos.env` 后用 `systemctl --user restart agentos-taskd` 生效。设置助理里设置的 PIN 就是任务服务的 PIN。图形桌面和文本模式都连接这个服务。`agentos-web-shell` 不会自动启动。

从带 `agentosd` 的旧版本升级上来时，任务服务第一次启动会把 `~/.local/share/agentos` 里的笔记、PIN、插件、黑板、`mcp.json` 和自定义子 Agent 迁到 `~/.local/state/agentos`：已有的不覆盖，旧目录不改动，做了什么写在 `~/.local/state/agentos/.migrated-from-agentosd` 里，也会出现在服务日志里。

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

规则规划器只认识这些固定说法。听不懂的句子会成为“未能处理”的任务，并列出当前能做的事；它不会被记成“已完成”。问时间得到的是本机时区的日期和时间。

笔记与任务立即持久保存。清空保留一份恢复快照，恢复会保留清空后新增的笔记；恢复之前不能再次清空非空笔记，以免覆盖唯一备份。正常输入下，这些 L1 可恢复动作自动执行。

工作台控制命令：

| 命令 | 作用 |
| --- | --- |
| `/tasks`、`/refresh` | 刷新当前任务页和详情 |
| `/next`、`/prev`、`/latest` | 更早历史、返回上一页、最新任务 |
| `/task task-1` | 选中任务，显示意图、计划参数、步骤结果与确认请求 |
| `/cancel task-1` | 停止后续步骤，不再等模型；已经开始的步骤可能仍完成 |
| `/approve task-1 1` | 批准当前已展示详情中的具体确认请求 |
| `/reply task-1 1 周三` | 回答任务在第 1 步提出的问题；任务在原地继续 |
| `/new` | 另起一段对话；不输入它时，工作台里的每句话都接着同一段对话说 |
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

连接后台后，界面通过只读接口直接读取后台笔记；打开“记忆”空间或在笔记变化后刷新都不会替你提交任务，任务轨迹里只有你自己表达过的意图。未连接后台时可体验本地笔记与专注计时；浏览器笔记与后台笔记独立，不自动同步。`/` 聚焦输入，`Alt+V` 连接或结束实时对话，`Esc` 打断回应，`Shift+Esc` 结束对话，`Alt+1–4` 切换空间。打断声音不取消已接收任务，待批准动作仍需在界面明确批准。完整行为与实现边界见[图形 Shell 设计](ambient-shell.md)。

## 独立 CLI 与脚本

`agentos-cli` 包生成 `agentos` 可执行文件。源码运行时用 `cargo run -p agentos-cli --`，安装二进制后直接用 `agentos`：

```sh
agentos submit 记一下 买牛奶
agentos submit --wait --timeout 60 现在几点
agentos read notes.list
agentos tasks --limit 10
agentos tasks --before task-20 --state succeeded
agentos task task-1
agentos cancel task-2
agentos --json tasks
agentos watch
```

默认连接相同的状态目录；可在命令前指定 `--state-dir` 或 `--socket`。`--json` 适合脚本，`watch --after <revision>` 接收实时变更与重新同步通知。

`submit` 默认只返回“已接收”。加上 `--wait` 会等到任务结束再返回，并打印最终任务（`--json` 时与 `agentos --json task` 的结构相同）；`--timeout 秒` 设置最长等待时间，默认 300 秒。这两个选项要写在意图之前，意图里出现的同名文字不会被当成选项。等待只读取任务，不会重发、批准或取消；放弃等待后任务仍在后台继续。

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功；`submit --wait` 时表示任务 `succeeded` |
| 1 | 连接、协议或后台返回的错误 |
| 2 | 命令行用法错误 |
| 3 | `submit --wait`：任务结束但没有成功（`unhandled`、`failed`、`cancelled`、`interrupted`） |
| 4 | `submit --wait`：任务在等待批准，需要人来处理 |
| 5 | `submit --wait`：等待超时，任务仍在后台继续 |
| 6 | `submit --wait`：任务在等待回答，需要人用 `agentos reply` 回答 |

`submit --thread task-N` 接着一段对话说（`task-N` 是这段对话第一个任务的编号，`submit` 的输出里会给出）；`tasks --thread task-N` 只列出这段对话的任务。Agent 提问时任务显示为“等待回答”，用 `agentos reply task-N 步骤 <回答>` 回答。

`preauthorize <能力>` 事先许可一类调用，之后符合规则的调用不再逐次确认；只适用于 L1/L2，不适用于读过外部内容的任务。例如只允许给自己发邮件、两小时内有效：

```sh
agentos preauthorize mail.send --one-of to=me@example.com --any body --expires 2h
agentos preauthorizations
agentos revoke pre-1
```

规则有 `--equals 字段=值`、`--one-of 字段=值1,值2`、`--prefix 字段=文本`、`--under 字段=目录`、`--any 字段`。没有写到的参数默认不允许出现，`--any-other-args` 才放开。必须至少限定一个参数或用 `--expires` 设置期限（最长 30 天）。设置了 PIN 时加 `--pin-stdin`。

`caller add <id> --name 名字 [--allow 能力]…` 注册一个外部调用方，`agentos mcp --caller <id>` 以它的身份提供 MCP 服务；见 [mcp.md](mcp.md)。给调用方的预授权用 `preauthorize … --caller <id>`。

`read <能力> [JSON 参数]` 直接查询一个只读（L0）能力，例如 `notes.list`、`clock.now`。它不创建任务、不经过规划器；有副作用的能力会被拒绝，需要用 `submit` 提交意图。`capabilities` 列出的风险等级写作 L0–L3。

独立批准命令要求先读取任务详情，再显式提供其中的确认令牌：

```sh
agentos approve task-3 1 --confirmation <令牌>
```

批准绑定任务、步骤、随机令牌、同一系统用户和五分钟有效期；入队及执行时都校验，随后仍重新检查能力授权。普通 `y` 只会产生新任务。

不可逆（L3）的步骤还需要 PIN 来证明是谁在批准。先用 `agentos-taskd --state-dir <目录> --set-pin` 设置；没有设置 PIN 时，L3 步骤直接拒绝执行。批准时加 `--pin-stdin`，PIN 从标准输入读取，不会出现在命令行里：

```sh
agentos approve task-3 1 --confirmation <令牌> --pin-stdin
```

工作台不读取 PIN，会提示改用上面的命令；Web Shell 和原生桌面在批准面板里输入。连续输错 5 次锁定 5 分钟，每次核对都记入审计（不含 PIN 本身）。

工作台断线后自动重连并重新获取视图，**不会自动重发提交或批准**。若发送请求后没有收到响应，操作可能已经生效，请先检查历史与实际结果。

## 本地状态与连接

状态目录依次取 `--state-dir`、`AGENTOS_STATE_DIR`、绝对路径 `$XDG_STATE_HOME/agentos`、`$HOME/.local/state/agentos`。socket 依次取 `--socket`、`AGENTOS_TASK_SOCKET`、状态目录内的 `control.sock`。

| 文件 | 内容 |
| --- | --- |
| `control.sock` | 仅本用户可访问的本地控制端点，权限 `0600` |
| `tasks.json` | 任务快照：稳定任务 ID、持久 revision、生命周期、步骤与结果 |
| `tasks.journal` | 快照之后的改动，每行是一个任务改动后的样子；攒到和任务数差不多多时并入快照，启动和正常退出时也会并入 |
| `notes.json` | 笔记与清空恢复快照 |
| `audit.jsonl` | 意图、策略、调用开始、结果和表达记录 |
| `blackboard.json` | 所有任务和 Agent 共享的黑板 |
| `pin.json` | PIN 的加盐哈希（0600） |
| `evolve/` | 系统自己生成的插件及其版本 |
| `mcp.json`、`agents/*.json` | 可选配置：MCP server、自定义子 Agent |
| `.lock` | 状态目录的进程独占锁 |

服务及客户端通过内核 peer credentials 校验本机 UID。多个相同用户的终端共享同一任务空间，来源身份跨连接稳定；同一 UID 下的进程属于同一信任域。自定义 socket 应放在本人拥有、其他用户不可写的目录，不能直接放在共享可写的 `/tmp` 根目录；路径也需符合系统 Unix socket 长度限制。

任务与笔记原子替换并同步落盘，审计同步只追加并带哈希链（`agentos-taskd --verify-audit` 校验）。能力调用前保存调用开始与运行状态；关键持久化失败停止后续执行。损坏的任务或笔记文件拒绝加载且保留原文件；无法续写的审计日志会被改名保留，另开一份新日志并在第一条说明原因。系统不承诺跨任务、日志和能力数据的原子事务，以中断态表达结果不确定性。

审计路径可用 `--audit` 或 `AGENTOS_TASK_AUDIT` 指定。状态目录与审计文件都限制单一写入者；不同状态目录应使用独立审计文件。

v0.2 快照自动升级，保留任务、结果和笔记；未完成任务中断、旧确认作废。新版快照不承诺旧程序可读。v0.1 的内存笔记无法恢复，原仓库 `agentos-audit.jsonl` 不会自动导入或重放。当前数据均为本地明文。

## 模型规划器

保留 Claude、OpenAI 兼容接口和离线规则规划器：

```sh
cargo run -p agentos-taskd -- --planner rule
cargo run -p agentos-taskd -- --planner claude
cargo run -p agentos-taskd -- --planner openai
```

每个任务的预算用环境变量设置：`AGENTOS_TASK_TOKENS`（输入加输出 token，默认 2000000）和 `AGENTOS_TASK_SECONDS`（执行时长，默认 1800），`0` 表示不限制，写错会拒绝启动而不是当作不限制。任务详情里的“用量”一行显示它已经消耗了多少。

`--planner` 优先于 `AGENTOS_PLANNER`。未指定时，有 `ANTHROPIC_API_KEY` 则选 Claude，否则有完整 OpenAI 兼容配置则选该接口，其余使用规则规划器。显式指定但配置不完整会报错。

| 规划器 | 配置 |
| --- | --- |
| Claude | `ANTHROPIC_API_KEY`；可选 `AGENTOS_MODEL`、`ANTHROPIC_BASE_URL` |
| OpenAI 兼容 | `OPENAI_MODEL`，以及 `OPENAI_API_KEY` 或 `OPENAI_BASE_URL`；本地兼容服务可不设 key |
| Rule | 无需配置，支持上面的笔记和时间示例 |

规划是多轮的：模型每轮返回一组工具调用，执行结果交还给它，由它决定下一轮或给出回答，最多 8 轮。所有调用仍经过运行时校验与策略。网络失败、截断或畸形工具响应使任务失败，不执行半个计划。

## 仓库结构与开发

仓库结构见 [README](../README.md#仓库结构)，各 crate 的职责见[架构](architecture.md#一个内核)。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test crates/web-shell/assets/*.test.mjs
```

测试覆盖持久化、快照迁移、取消竞态、批准与撤权、审计故障、协议帧限制、真实 Unix socket 交互和客户端断线行为。CI 配置 Linux/macOS 检查及 Rust 1.88 最低版本编译。

当前为单用户的用户态服务，任务并行执行，能力仍在守护进程内运行（插件在沙箱里）。完整的结构见[架构](architecture.md)。
